//! Deterministic fast path for precise commands. When the caller (an LLM)
//! already knows exactly what to do — `click e12`, `type "ada@x.io" into e5`,
//! `select "M" from "Size"` — no decision model is needed. Anything that doesn't
//! parse, or whose target is ambiguous, falls back to Jev.

use crate::snapshot::{El, Kind, Snapshot};

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Id(usize),
    Name(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Click(Target),
    Type(Target, String),
    Select(Target, String),
    Check(Target, bool),
    Enter(Option<Target>),
}

/// Rewrites typographic quotes and single-quoted literals ('ada@x.io') as
/// double-quoted ones. A single quote only opens after a space or bracket and
/// only closes before a space, punctuation or the end, so apostrophes
/// ("O'Brien", "Ada's") are left alone.
fn normalize_quotes(s: &str) -> String {
    let s = s.replace(['\u{201C}', '\u{201D}'], "\"");
    if s.contains('"') {
        return s;
    }
    let cs: Vec<char> = s.chars().map(|c| if c == '\u{2018}' || c == '\u{2019}' { '\'' } else { c }).collect();
    let opens = |i: usize| i == 0 || cs[i - 1].is_whitespace() || "([{,:=".contains(cs[i - 1]);
    let closes = |i: usize| i + 1 == cs.len() || cs[i + 1].is_whitespace() || ",.;:)]}!?".contains(cs[i + 1]);
    let mut out: Vec<char> = cs.clone();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\'' && opens(i) && i + 1 < cs.len() && !cs[i + 1].is_whitespace() {
            if let Some(j) = (i + 2..cs.len()).find(|&j| cs[j] == '\'' && !cs[j - 1].is_whitespace() && closes(j)) {
                out[i] = '"';
                out[j] = '"';
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out.into_iter().collect()
}

/// Removes wrappers LLMs add (`act("…")`, `run("…", "…")`) and escaped quotes.
pub fn sanitize(s: &str) -> String {
    let mut s = normalize_quotes(&s.trim().replace("\\\"", "\"").replace("\\'", "'"));
    for w in ["act(", "run(", "act (", "run ("] {
        if s.to_lowercase().starts_with(w) && s.ends_with(')') {
            s = s[w.len()..s.len() - 1].trim().to_string();
        }
    }
    // Paren-less wrappers: run "a", "b" / act "a"
    for w in ["run ", "act "] {
        if s.to_lowercase().starts_with(w) && s[w.len()..].trim_start().starts_with('"') {
            s = s[w.len()..].trim().to_string();
        }
    }
    // run("a", "b") → a; b
    if s.starts_with('"') && s.ends_with('"') && s.contains("\", \"") {
        s = s[1..s.len() - 1].replace("\", \"", "; ");
    } else if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') && s[1..s.len() - 1].matches('"').count() % 2 == 0 {
        s = s[1..s.len() - 1].to_string();
    }
    s
}

const VERBS: &[&str] = &[
    "click", "tap", "press", "open", "type", "enter", "fill", "input", "write", "select", "choose", "pick", "check",
    "uncheck", "tick", "untick", "set",
];

/// Splits "type … into e3, then click e4" into sub-commands.
fn split(s: &str) -> Vec<String> {
    let mut parts = vec![];
    let mut cur = String::new();
    let mut in_q = false;
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '"' {
            in_q = !in_q;
        }
        if !in_q && (c == ';' || c == '\n' || c == ',') {
            let rest: String = cs[i + 1..].iter().collect();
            let rest_l = rest.trim_start().to_lowercase();
            let rest_l = rest_l.trim_start_matches("then ").trim_start_matches("and ").trim_start_matches("and then ");
            if c != ',' || VERBS.iter().any(|v| rest_l.starts_with(v)) {
                parts.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
        }
        cur.push(c);
        i += 1;
    }
    parts.push(cur);
    let mut out = vec![];
    for p in parts {
        // " then " / " and then " also separate commands.
        for q in p.split(" and then ").flat_map(|x| x.split(" then ")) {
            let q = q.trim().trim_start_matches("and ").trim().trim_end_matches('.').trim();
            if !q.is_empty() {
                out.push(q.to_string());
            }
        }
    }
    out
}

fn quoted_first(s: &str) -> Option<(String, &str)> {
    let a = s.find('"')?;
    let b = a + 1 + s[a + 1..].find('"')?;
    Some((s[a + 1..b].to_string(), &s[b + 1..]))
}

/// Role words allowed around a precise target: `click the "Save" button`.
const TARGET_WORDS: &[&str] = &[
    "on", "the", "element", "field", "button", "link", "textbox", "checkbox", "box", "tab", "option", "menu", "input",
    "radio", "switch", "dropdown", "select",
];

/// A precise target is `eN` or a quoted name, optionally surrounded by role
/// words. Anything else (`the details of "X"`) is a description, not a target.
fn parse_target(s: &str) -> Option<Target> {
    let s = s.trim().trim_end_matches('.').trim();
    let mut rest = s;
    loop {
        let lower = rest.to_lowercase();
        let Some(w) = TARGET_WORDS.iter().find(|w| lower.starts_with(&format!("{w} "))) else { break };
        rest = rest[w.len()..].trim_start();
    }
    let l = rest.to_lowercase();
    if let Some(n) = l.strip_prefix('e') {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return n.parse().ok().map(Target::Id);
        }
    }
    if !rest.starts_with('"') {
        return None;
    }
    let (q, after) = quoted_first(rest)?;
    let after = after.trim().to_lowercase();
    after.split_whitespace().all(|w| TARGET_WORDS.contains(&w)).then_some(Target::Name(q))
}

fn strip_verb<'a>(s: &'a str, verbs: &[&str]) -> Option<&'a str> {
    let l = s.to_lowercase();
    verbs.iter().find_map(|v| {
        let v = format!("{v} ");
        l.starts_with(&v).then(|| &s[v.len()..])
    })
}

fn parse_one(s: &str) -> Option<Cmd> {
    let l = s.to_lowercase();
    if l == "press enter" || l == "hit enter" || l == "submit with enter" {
        return Some(Cmd::Enter(None));
    }
    if let Some(r) = l.strip_prefix("press enter in ").or_else(|| l.strip_prefix("press enter on ")) {
        let off = s.len() - r.len();
        return Some(Cmd::Enter(Some(parse_target(&s[off..])?)));
    }
    if let Some(rest) = strip_verb(s, &["type", "enter", "input", "write"]) {
        let (v, after) = quoted_first(rest)?;
        let after = after.trim();
        let al = after.to_lowercase();
        let t = ["into ", "in ", "on "].iter().find_map(|p| al.starts_with(p).then(|| &after[p.len()..]))?;
        return Some(Cmd::Type(parse_target(t)?, v));
    }
    if let Some(rest) = strip_verb(s, &["fill", "set"]) {
        // fill <target> with "v" / set <target> to "v"
        let rl = rest.to_lowercase();
        let (tpart, vpart) = [" with ", " to "].iter().find_map(|k| rl.rfind(k).map(|i| (&rest[..i], &rest[i + k.len()..])))?;
        let (v, tail) = quoted_first(vpart)?;
        if !tail.trim().is_empty() {
            return None;
        }
        return Some(Cmd::Type(parse_target(tpart)?, v));
    }
    if let Some(rest) = strip_verb(s, &["select", "choose", "pick"]) {
        let (v, after) = quoted_first(rest)?;
        let after = after.trim();
        let al = after.to_lowercase();
        let t = ["from ", "in ", "for "].iter().find_map(|p| al.starts_with(p).then(|| &after[p.len()..]))?;
        return Some(Cmd::Select(parse_target(t)?, v));
    }
    if let Some(rest) = strip_verb(s, &["uncheck", "untick"]) {
        return Some(Cmd::Check(parse_target(rest)?, false));
    }
    if let Some(rest) = strip_verb(s, &["check", "tick"]) {
        return Some(Cmd::Check(parse_target(rest)?, true));
    }
    if let Some(rest) = strip_verb(s, &["click", "tap", "press", "open"]) {
        return Some(Cmd::Click(parse_target(rest)?));
    }
    None
}

/// Canonical text of one command, e.g. `type "x" into e5`.
pub fn describe(c: &Cmd) -> String {
    let t = |t: &Target| match t {
        Target::Id(i) => format!("e{i}"),
        Target::Name(n) => format!("\"{n}\""),
    };
    match c {
        Cmd::Click(x) => format!("click {}", t(x)),
        Cmd::Type(x, v) => format!("type \"{v}\" into {}", t(x)),
        Cmd::Select(x, v) => format!("select \"{v}\" from {}", t(x)),
        Cmd::Check(x, true) => format!("check {}", t(x)),
        Cmd::Check(x, false) => format!("uncheck {}", t(x)),
        Cmd::Enter(Some(x)) => format!("press enter in {}", t(x)),
        Cmd::Enter(None) => "press enter".into(),
    }
}

/// Parses an instruction into precise commands, or None if any part is fuzzy.
pub fn parse(instr: &str) -> Option<Vec<Cmd>> {
    let s = sanitize(instr);
    let parts = split(&s);
    if parts.is_empty() {
        return None;
    }
    parts.iter().map(|p| parse_one(p)).collect()
}

fn norm(s: &str) -> String {
    s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a command needs its target to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Click,
    Text,
    Select,
    Check,
}

impl Need {
    pub fn of(c: &Cmd) -> Self {
        match c {
            Cmd::Click(_) => Need::Click,
            Cmd::Type(..) | Cmd::Enter(_) => Need::Text,
            Cmd::Select(..) => Need::Select,
            Cmd::Check(..) => Need::Check,
        }
    }

    pub fn accepts(self, e: &El) -> bool {
        if e.latent() {
            return false;
        }
        match self {
            Need::Click => true,
            // `type "UK" into "Country"` on a <select> is a selection.
            Need::Text => matches!(e.kind(), Kind::Text | Kind::Select),
            Need::Select => e.kind() == Kind::Select,
            Need::Check => matches!(e.kind(), Kind::Check | Kind::Radio),
        }
    }
}

/// Outcome of resolving a precise command's target.
#[derive(Debug)]
pub enum Resolved {
    Found(usize),
    /// Several elements fit a name; a narrow decision may pick among them.
    Ambiguous(Vec<usize>),
    /// Nothing fits; the reason names what the caller referred to.
    Missing(String),
}

/// Resolves a target against the current snapshot. Ids are stable node keys:
/// they stay valid while the node lives in the same document; if the document
/// changed since the caller's listing (`seen`), the id is remapped by
/// fingerprint (role, name, context) when that is unambiguous.
pub fn resolve(t: &Target, need: Need, snap: &Snapshot, seen: Option<&Snapshot>) -> Resolved {
    match t {
        Target::Id(i) => {
            let same_doc = seen.is_none_or(|p| p.doc_id == snap.doc_id);
            let was = seen.and_then(|p| p.els.iter().find(|e| e.i == *i));
            let cur = if same_doc {
                snap.els.iter().find(|e| e.i == *i)
            } else {
                let Some(old) = was else { return Resolved::Missing(format!("e{i} is not on this page")) };
                let key = |e: &El| (e.r.clone(), e.n.clone(), e.c.clone());
                let hits: Vec<&El> = snap.els.iter().filter(|e| key(e) == key(old)).collect();
                match hits.len() {
                    1 => Some(hits[0]),
                    0 => return Resolved::Missing(format!("the page changed and e{i} ({}) is gone", old.desc(true))),
                    _ => return Resolved::Missing(format!("the page changed and e{i} ({}) is ambiguous now", old.desc(true))),
                }
            };
            match cur {
                Some(e) if e.latent() => Resolved::Missing(format!("e{i} ({}) is hidden; reveal it first", e.desc(true))),
                Some(e) if !need.accepts(e) => {
                    Resolved::Missing(format!("e{i} is {} \"{}\", which can't take this command", e.r, e.n))
                }
                Some(e) => Resolved::Found(e.i),
                None => match was {
                    Some(old) => Resolved::Missing(format!("e{i} ({}) is no longer on the page", old.desc(true))),
                    None => Resolved::Missing(format!("there is no e{i} on this page")),
                },
            }
        }
        Target::Name(n) => {
            let n = norm(n);
            let ok = |e: &El| need.accepts(e) && !e.has_flag("disabled");
            let exact: Vec<usize> = snap.els.iter().filter(|e| ok(e) && norm(&e.n) == n).map(|e| e.i).collect();
            match exact.len() {
                1 => return Resolved::Found(exact[0]),
                0 => {}
                _ => return Resolved::Ambiguous(exact),
            }
            // Placeholder or prefix matches.
            let loose: Vec<usize> = snap
                .els
                .iter()
                .filter(|e| ok(e))
                .filter(|e| e.p.as_deref().is_some_and(|p| norm(p) == n) || norm(&e.n).starts_with(&n))
                .map(|e| e.i)
                .collect();
            match loose.len() {
                1 => Resolved::Found(loose[0]),
                0 => {
                    // Lexically closest candidates of the right kind, for a narrow decision.
                    let near: Vec<usize> = snap.prune_els(&n, 8, |e| ok(e)).into_iter().map(|e| e.i).collect();
                    let scores = crate::snapshot::score_all(
                        &n,
                        &near.iter().filter_map(|i| snap.els.iter().find(|e| e.i == *i)).map(|e| e.desc(true)).collect::<Vec<_>>(),
                    );
                    let near: Vec<usize> = near.into_iter().zip(scores).filter(|(_, s)| *s > 0.0).map(|(i, _)| i).collect();
                    if near.is_empty() {
                        Resolved::Missing(format!("nothing on the page is called \"{n}\""))
                    } else {
                        Resolved::Ambiguous(near)
                    }
                }
                _ => Resolved::Ambiguous(loose),
            }
        }
    }
}

/// Rewrites `eN` references into element descriptions so the decision model
/// sees what the caller meant, e.g. `click e6` → `click button "Subscribe" (in: Newsletter)`.
pub fn describe_ids(instr: &str, prev: Option<&Snapshot>) -> String {
    let Some(p) = prev else { return instr.to_string() };
    let mut out = String::with_capacity(instr.len());
    let cs: Vec<char> = instr.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let boundary = i == 0 || !cs[i - 1].is_alphanumeric();
        if boundary && cs[i] == 'e' && i + 1 < cs.len() && cs[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < cs.len() && cs[j].is_ascii_digit() {
                j += 1;
            }
            let end_ok = j == cs.len() || !cs[j].is_alphanumeric();
            let n: usize = cs[i + 1..j].iter().collect::<String>().parse().unwrap_or(usize::MAX);
            if let (true, Some(e)) = (end_ok, p.els.iter().find(|e| e.i == n)) {
                // No quote characters: they would turn into typed-value literals.
                let ctx = e.c.as_deref().map(|c| format!(" in {c}")).unwrap_or_default();
                out.push_str(&format!("the {} named {}{ctx}", e.r, e.n).replace('"', ""));
                i = j;
                continue;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_precise_commands() {
        assert_eq!(parse("click e12"), Some(vec![Cmd::Click(Target::Id(12))]));
        assert_eq!(
            parse(r#"type "ada@example.com" into e1, then click "Place order""#),
            Some(vec![Cmd::Type(Target::Id(1), "ada@example.com".into()), Cmd::Click(Target::Name("Place order".into()))])
        );
        assert_eq!(
            parse(r#"act("select \"M\" from \"Size\"")"#),
            Some(vec![Cmd::Select(Target::Name("Size".into()), "M".into())])
        );
        assert_eq!(
            parse(r#"run("type \"grace@example.com\" into e5", "click e6")"#),
            Some(vec![Cmd::Type(Target::Id(5), "grace@example.com".into()), Cmd::Click(Target::Id(6))])
        );
        assert_eq!(parse(r#"fill "Full name" with "Ada Lovelace""#), Some(vec![Cmd::Type(Target::Name("Full name".into()), "Ada Lovelace".into())]));
        assert_eq!(parse("check e7"), Some(vec![Cmd::Check(Target::Id(7), true)]));
        assert_eq!(
            parse(r#"run "type \"alice\" into e3", "type \"hunter2\" into e4", "click e6""#),
            Some(vec![
                Cmd::Type(Target::Id(3), "alice".into()),
                Cmd::Type(Target::Id(4), "hunter2".into()),
                Cmd::Click(Target::Id(6))
            ])
        );
        assert_eq!(parse(r#"click on tab "Reviews (3)""#), Some(vec![Cmd::Click(Target::Name("Reviews (3)".into()))]));
    }

    #[test]
    fn single_and_typographic_quotes() {
        assert_eq!(parse("type 'ada@example.com' into e2"), Some(vec![Cmd::Type(Target::Id(2), "ada@example.com".into())]));
        assert_eq!(parse("type 'O'Brien' into e3"), Some(vec![Cmd::Type(Target::Id(3), "O'Brien".into())]));
        assert_eq!(parse("click \u{201C}Place order\u{201D}"), Some(vec![Cmd::Click(Target::Name("Place order".into()))]));
        assert_eq!(parse("select \u{2018}Express\u{2019} from e9"), Some(vec![Cmd::Select(Target::Id(9), "Express".into())]));
        // Apostrophes are not quotes.
        assert_eq!(sanitize("open Ada's profile"), "open Ada's profile");
        assert_eq!(sanitize("don't click 'Delete'"), "don't click \"Delete\"");
    }

    #[test]
    fn descriptions_are_not_targets() {
        assert_eq!(parse(r#"open the details of "Quantum Kettle""#), None);
        assert_eq!(parse(r#"click the "Save" button"#), Some(vec![Cmd::Click(Target::Name("Save".into()))]));
        assert_eq!(parse(r#"open the result "Logitech MX Master 3S""#), None);
        assert_eq!(parse(r#"type "x" into the "Email address" field"#), Some(vec![Cmd::Type(Target::Name("Email address".into()), "x".into())]));
    }

    #[test]
    fn fuzzy_falls_through() {
        assert_eq!(parse("log in as alice with password hunter2"), None);
        assert_eq!(parse("add the blue shirt to the cart"), None);
        assert_eq!(parse(r#"type "x" into e1 and pick the cheapest plan"#), None);
    }
}
