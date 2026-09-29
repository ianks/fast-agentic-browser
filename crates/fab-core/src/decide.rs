//! Turns (instruction, snapshot, history) into one batch of typed questions and
//! decodes the answers into an executable step plan.

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::config::Knobs;
use crate::jev::{Answers, Jev, Questions};
use crate::snapshot::{El, Kind, Snapshot, truncate};
use crate::spans;

/// The decision model. Jev only; kept as a type so a different System One
/// model can be dropped in.
pub struct Decider(pub Jev);

impl Decider {
    pub async fn ask(&self, state: &Value, q: &Questions) -> Result<Answers> {
        self.0.ask(state, q).await
    }
}

pub const NO_CHANGE: &str = "(no change)";
const NONE: &str = "none";
const ENTER: &str = "press_enter";

#[derive(Debug, Clone, Serialize)]
pub enum FillValue {
    Text(String),
    Select(String),
    Check(bool),
    /// Click this radio element.
    Radio(usize),
}

#[derive(Debug, Clone, Serialize)]
pub struct Fill {
    pub el: usize,
    pub value: FillValue,
    pub p: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    pub fills: Vec<Fill>,
    pub click: Option<usize>,
    pub enter: bool,
    pub click_p: f64,
    pub click_conf: f64,
    pub done: f64,
    pub final_p: f64,
    /// Top alternatives for the click, when it's not a clear call.
    pub alternatives: Vec<(String, f64)>,
    /// Executing the plan finishes the step (its commit was refused, and its
    /// fills type every value the instruction gave).
    pub last: bool,
}

impl Plan {
    pub fn is_noop(&self) -> bool {
        self.fills.is_empty() && self.click.is_none() && !self.enter
    }
}

/// How to map answers back to elements.
#[derive(Default)]
pub struct Mapping {
    /// question id -> (element index for text/select/check) | radio group members
    fields: Vec<(String, FieldQ)>,
}

enum FieldQ {
    Text(usize),
    Select(usize),
    Check(usize),
    Radio(Vec<(String, usize)>),
}

fn field_obj(line: String, question: &str) -> Value {
    json!({"field": line, "question": question})
}

/// Builds the decision state and the question batch for one step.
/// "Type UNLINK to confirm", "type “acme/api” below to confirm": the text a
/// confirmation field asks for. Only quoted, all-caps or path-like tokens, so
/// "type the repository name" gives nothing.
fn confirm_token(e: &El) -> Option<String> {
    let hay = format!("{} | {} | {}", e.n, e.p.as_deref().unwrap_or(""), e.c.as_deref().unwrap_or(""));
    // ASCII lowercasing keeps byte offsets, so they index `hay` too.
    let low = hay.to_ascii_lowercase();
    if !low.contains("confirm") {
        return None;
    }
    let mut from = 0;
    while let Some(k) = low[from..].find("type ") {
        let at = from + k + 5;
        let rest = hay[at..].trim_start();
        let first = rest.chars().next()?;
        let quoted = matches!(first, '"' | '\'' | '\u{201C}' | '\u{2018}' | '`');
        let tok: String = if quoted {
            rest[first.len_utf8()..].chars().take_while(|c| !matches!(c, '"' | '\'' | '\u{201D}' | '\u{2019}' | '`')).collect()
        } else {
            rest.chars().take_while(|c| !c.is_whitespace()).collect::<String>().trim_end_matches(['.', ',', ':', ';', ')']).to_string()
        };
        let letters: Vec<char> = tok.chars().filter(|c| c.is_alphabetic()).collect();
        let upper = !letters.is_empty() && letters.iter().all(|c| c.is_uppercase());
        if !tok.is_empty() && tok.chars().count() <= 64 && (quoted || upper || tok.contains('/')) {
            return Some(tok);
        }
        from = at;
    }
    None
}

pub fn build(snap: &Snapshot, instr: &str, history: &[String], k: &Knobs) -> (Value, Questions, Mapping) {
    // Latent elements need a reveal first: only offered when the engine can
    // execute reveal macros (`k.latent`).
    let usable = |e: &El| !e.has_flag("disabled") && (k.latent || !e.latent());
    let mut q = Questions::default();
    let mut map = Mapping::default();
    let values = spans::spans(instr, 200);

    // ----- fields -----
    let fields = snap.prune_els(instr, k.max_fields, |e| {
        usable(e) && matches!(e.kind(), Kind::Text | Kind::Select | Kind::Check | Kind::Radio)
    });
    let mut radio_groups: Vec<(String, Vec<&El>)> = Vec::new();
    let mut shown: Vec<&El> = Vec::new();
    for e in &fields {
        // A confirmation field asks for a word the page gives ("Type UNLINK to
        // confirm"), not the task: offer it too.
        let token = if e.kind() == Kind::Text { confirm_token(e) } else { None };
        match e.kind() {
            Kind::Text if token.is_some() || values.iter().any(|v| spans::fits(e.t.as_deref(), v)) => {
                let id = format!("f{}", e.i);
                // Only values that fit the field's input type (I6: code filters what code can).
                let opts = std::iter::once((
                    NO_CHANGE.to_string(),
                    Some("The instruction gives no value for this field, or the field already holds it.".to_string()),
                ))
                .chain(values.iter().filter(|v| spans::fits(e.t.as_deref(), v) && Some(*v) != token.as_ref()).map(|v| (v.clone(), None)))
                .chain(token.iter().map(|t| (t.clone(), Some("The confirmation text the page asks for.".to_string()))));
                q.choice(
                    &id,
                    field_obj(
                        e.line(k.ctx),
                        "What should be typed into `field` to carry out `instruction`? Pick `(no change)` if `instruction` does not give a value for this field.",
                    ),
                    opts,
                );
                map.fields.push((id, FieldQ::Text(e.i)));
                shown.push(e);
            }
            Kind::Select => {
                let Some(o) = &e.o else { continue };
                let mut opts: Vec<(String, Option<String>)> = vec![(NO_CHANGE.into(), Some("Leave the current selection.".into()))];
                for x in o.iter().take(250) {
                    if !x.is_empty() && !opts.iter().any(|(k, _)| k == x) {
                        opts.push((x.clone(), None));
                    }
                }
                if opts.len() < 2 {
                    continue;
                }
                let id = format!("f{}", e.i);
                q.choice(
                    &id,
                    field_obj(e.line(k.ctx), "Which option should be selected in `field` to carry out `instruction`? Pick `(no change)` if `instruction` does not say."),
                    opts,
                );
                map.fields.push((id, FieldQ::Select(e.i)));
                shown.push(e);
            }
            Kind::Check => {
                let id = format!("f{}", e.i);
                q.choice(
                    &id,
                    field_obj(e.line(k.ctx), "Should `field` be checked or unchecked to carry out `instruction`? Pick `(no change)` if `instruction` does not say."),
                    [
                        (NO_CHANGE, Some("The instruction says nothing about this checkbox.".into())),
                        ("check", Some("Turn it on.".into())),
                        ("uncheck", Some("Turn it off.".into())),
                    ],
                );
                map.fields.push((id, FieldQ::Check(e.i)));
                shown.push(e);
            }
            Kind::Radio => {
                let g = e.g.clone().unwrap_or_else(|| e.id());
                match radio_groups.iter_mut().find(|(n, _)| *n == g) {
                    Some((_, v)) => v.push(e),
                    None => radio_groups.push((g, vec![e])),
                }
            }
            _ => {}
        }
    }
    for (gi, (g, members)) in radio_groups.iter().enumerate() {
        let id = format!("r{gi}");
        let mut opts: Vec<(String, Option<String>)> = vec![(NO_CHANGE.into(), Some("The instruction says nothing about this choice.".into()))];
        let mut idx = Vec::new();
        for m in members {
            let mut key = if m.n.is_empty() { m.id() } else { truncate(&m.n, 60) };
            if opts.iter().any(|(k, _)| *k == key) {
                key = format!("{key} ({})", m.id());
            }
            let desc = if m.has_flag("checked") { Some("currently selected".to_string()) } else { None };
            opts.push((key.clone(), desc));
            idx.push((key, m.i));
            shown.push(m);
        }
        q.choice(
            &id,
            json!({"group": g, "question": "Which option in the radio group `group` should be selected to carry out `instruction`? Pick `(no change)` if `instruction` does not say."}),
            opts,
        );
        map.fields.push((id, FieldQ::Radio(idx)));
    }

    // ----- click -----
    let clickables = snap.prune_els(instr, k.prune_k, |e| usable(e) && e.kind() == Kind::Click);
    let mut opts: Vec<(String, Option<String>)> = vec![(
        NONE.into(),
        Some("No click is needed now: `instruction` is already fully done (see `actions_taken`), or it only asks to fill in fields.".into()),
    )];
    if map.fields.iter().any(|(_, f)| matches!(f, FieldQ::Text(_))) {
        opts.push((
            ENTER.into(),
            Some("Press Enter in the field just typed into, to submit it when there is no button for that.".into()),
        ));
    }
    for e in &clickables {
        opts.push((e.id(), k.opt_desc.then(|| e.desc(k.ctx))));
    }
    q.choice(
        "click",
        "Which element in `elements` should be clicked next to carry out `instruction`? Only the fields listed in `auto_filled` get filled in automatically before this click; every other element (dropdowns, pickers, tabs, buttons, links) has to be clicked to be used. Pick the first click the instruction still needs.",
        opts,
    );
    q.gate(
        "done",
        "Has `instruction` already been fully carried out? Judge from `actions_taken` and the current page.",
        "Every part of the instruction is already done.",
        "Some part of the instruction still has to be done.",
        k.gates_as_choice,
    );
    if k.trust_final <= 1.0 {
        // Predicts whether the upcoming click finishes the instruction, so the
        // verification round-trip can be skipped. Probed: Choice beats Noul here.
        q.choice(
            "final",
            "How many more clicks does `instruction` still need, starting from the current page?",
            [
                ("one", Some("Exactly one more click finishes it.".to_string())),
                ("several", Some("The next click is only an intermediate step; more clicks will follow (open a menu or picker, go to another page, confirm a dialog).".to_string())),
            ],
        );
    }

    // ----- state -----
    let mut lines: Vec<&El> = clickables.iter().copied().chain(shown.iter().copied()).collect();
    lines.sort_by_key(|e| e.i);
    lines.dedup_by_key(|e| e.i);
    let auto: Vec<String> = shown.iter().map(|e| e.id()).collect();
    let mut state = json!({
        "instruction": instr,
        "actions_taken": history,
        "auto_filled": auto,
        "page": format!("{} — {}", snap.title, snap.url),
        "page_text": snap.text_context(instr, k.max_text),
        "elements": lines.iter().map(|e| e.line(k.ctx)).collect::<Vec<_>>(),
    });
    if snap.modal {
        state["note"] = "A modal dialog is open; elements marked (covered) are behind it.".into();
    }
    (state, q, map)
}

pub fn decode(a: &Answers, map: &Mapping, snap: &Snapshot) -> Plan {
    let by_i: HashMap<usize, &El> = snap.els.iter().map(|e| (e.i, e)).collect();
    let final_p = match a.answers.get("final") {
        Some(crate::jev::Answer::Choice { probabilities, .. }) => probabilities.get("one").copied().map(f64::from).unwrap_or(0.0),
        _ => 0.0,
    };
    let mut plan = Plan { done: a.yes("done").unwrap_or(0.0), final_p, ..Default::default() };
    for (id, f) in &map.fields {
        let Some((choice, p, _)) = a.choice(id) else { continue };
        if choice == NO_CHANGE {
            continue;
        }
        let fill = match f {
            FieldQ::Text(i) => {
                let cur = by_i.get(i).and_then(|e| e.v.as_deref()).unwrap_or("");
                if cur == choice {
                    continue;
                }
                Fill { el: *i, value: FillValue::Text(choice.to_string()), p }
            }
            FieldQ::Select(i) => {
                if by_i.get(i).and_then(|e| e.v.as_deref()) == Some(choice) {
                    continue;
                }
                Fill { el: *i, value: FillValue::Select(choice.to_string()), p }
            }
            FieldQ::Check(i) => {
                let want = choice == "check";
                let is = by_i.get(i).is_some_and(|e| e.has_flag("checked"));
                if want == is {
                    continue;
                }
                Fill { el: *i, value: FillValue::Check(want), p }
            }
            FieldQ::Radio(members) => {
                let Some((_, i)) = members.iter().find(|(k, _)| k == choice) else { continue };
                if by_i.get(i).is_some_and(|e| e.has_flag("checked")) {
                    continue;
                }
                Fill { el: *i, value: FillValue::Radio(*i), p }
            }
        };
        plan.fills.push(fill);
    }
    plan.fills.sort_by_key(|f| f.el);
    if let Some((choice, p, conf)) = a.choice("click") {
        plan.click_p = p;
        plan.click_conf = conf;
        if choice == ENTER {
            plan.enter = true;
        } else if let Some(i) = choice.strip_prefix('e').and_then(|s| s.parse().ok()) {
            plan.click = Some(i);
        }
        if conf < 0.8 {
            plan.alternatives = a.ranked("click").into_iter().take(4).map(|(k, p)| (k.to_string(), p)).collect();
        }
    }
    plan
}

/// One value, one field: when the same text is chosen for more fields than the
/// instruction mentions it, keep only the most probable assignments.
pub fn dedupe_values(plan: &mut Plan, instr: &str) {
    let mut by_val: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, f) in plan.fills.iter().enumerate() {
        if let FillValue::Text(v) = &f.value {
            by_val.entry(v.clone()).or_default().push(i);
        }
    }
    let mut drop: Vec<usize> = Vec::new();
    for (v, mut idx) in by_val {
        let allowed = instr.matches(v.as_str()).count().max(1);
        if idx.len() > allowed {
            idx.sort_by(|a, b| plan.fills[*b].p.total_cmp(&plan.fills[*a].p));
            drop.extend(idx.into_iter().skip(allowed));
        }
    }
    drop.sort_unstable();
    for i in drop.into_iter().rev() {
        plan.fills.remove(i);
    }
}

/// Human-readable description of an action for `actions_taken`.
pub fn describe_fill(f: &Fill, snap: &Snapshot) -> String {
    let e = snap.els.iter().find(|e| e.i == f.el);
    let what = e.map(|e| format!("{} \"{}\"", e.r, e.n)).unwrap_or_else(|| format!("e{}", f.el));
    match &f.value {
        FillValue::Text(v) => format!("typed \"{v}\" into {what}"),
        FillValue::Select(v) => format!("selected \"{v}\" in {what}"),
        FillValue::Check(true) => format!("checked {what}"),
        FillValue::Check(false) => format!("unchecked {what}"),
        FillValue::Radio(_) => format!("selected {what}"),
    }
}

pub fn describe_click(i: usize, snap: &Snapshot) -> String {
    match snap.els.iter().find(|e| e.i == i) {
        Some(e) => format!("clicked {} \"{}\"{}", e.r, e.n, e.c.as_ref().map(|c| format!(" (in: {c})")).unwrap_or_default()),
        None => format!("clicked e{i}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_tokens() {
        let f = |n: &str, c: Option<&str>| -> El {
            serde_json::from_value(serde_json::json!({"i": 1, "r": "textbox", "n": n, "c": c})).unwrap()
        };
        assert_eq!(confirm_token(&f("Type UNLINK to confirm", None)).as_deref(), Some("UNLINK"));
        assert_eq!(confirm_token(&f("Confirm", Some("To confirm, type \u{201C}acme/api\u{201D} below"))).as_deref(), Some("acme/api"));
        assert_eq!(confirm_token(&f("Type the repository name to confirm", None)), None);
        assert_eq!(confirm_token(&f("Type your message", None)), None);
    }
}
