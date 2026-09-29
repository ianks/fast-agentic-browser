//! Candidate values for text fields. Jev can only pick, never generate, so every
//! value we might type has to be offered as an option: quoted strings when the
//! instruction has them, otherwise short n-grams of the instruction.

const EDGE_STOP: &[&str] = &[
    "a", "an", "the", "to", "of", "in", "on", "for", "and", "or", "with", "into", "as", "at", "by", "is", "it", "my",
    "me", "then", "from", "using", "via", "type", "enter", "fill", "set", "put", "write", "search", "click", "select",
    "choose", "log", "sign", "field", "box", "value", "be", "should", "under", "named", "called", "i", "we", "you",
];

/// Quoted substrings: "x", “x”, 'x' (single quotes only when not an apostrophe).
pub fn quoted(s: &str) -> Vec<String> {
    let cs: Vec<char> = s.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let close = match cs[i] {
            '"' => Some('"'),
            '“' => Some('”'),
            '‘' => Some('’'),
            '`' => Some('`'),
            '\'' if i == 0 || !cs[i - 1].is_alphanumeric() => Some('\''),
            _ => None,
        };
        if let Some(close) = close {
            let mut j = i + 1;
            while j < cs.len() {
                let end_ok = close != '\'' || j + 1 == cs.len() || !cs[j + 1].is_alphanumeric();
                if cs[j] == close && end_ok {
                    break;
                }
                j += 1;
            }
            if j < cs.len() && j > i + 1 {
                out.push(cs[i + 1..j].iter().collect());
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    // `{{card number}}`: a value from the password manager is a literal, quoted or not.
    for (_, name) in crate::secrets::placeholders(s) {
        let p = format!("{{{{{name}}}}}");
        if !out.iter().any(|q| q.contains(&p)) {
            out.push(p);
        }
    }
    out
}

fn clean(w: &str) -> &str {
    let w = w
        .trim_matches(|c: char| matches!(c, ',' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\'' | '“' | '”'))
        .trim_end_matches('.');
    // Possessives: "bob's" names "bob".
    w.strip_suffix("'s").or_else(|| w.strip_suffix("’s")).unwrap_or(w)
}

const NUMBER_WORDS: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen",
    "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen", "twenty",
];

/// "two" → "2" (number words are for people; fields want digits).
pub fn number_word(w: &str) -> Option<String> {
    let l = w.to_lowercase();
    if l == "a dozen" || l == "dozen" {
        return Some("12".into());
    }
    NUMBER_WORDS.iter().position(|n| *n == l).map(|i| i.to_string())
}

/// Whether a candidate value is plausible for a field of input type `t`.
pub fn fits(t: Option<&str>, v: &str) -> bool {
    // A password-manager value's shape is unknown until it is typed.
    if crate::secrets::has_placeholder(v) {
        return true;
    }
    let digits = v.chars().filter(|c| c.is_ascii_digit()).count();
    match t {
        Some("number") | Some("range") => {
            let x = v.replace([',', '$', '€', '£'], "");
            !x.is_empty() && x.trim().parse::<f64>().is_ok()
        }
        Some("email") => v.contains('@'),
        Some("tel") => digits >= 5 && v.chars().all(|c| c.is_ascii_digit() || " +-().".contains(c)),
        Some("url") => v.contains('.') || v.contains('/'),
        _ => true,
    }
}

pub fn spans(instr: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        let s = s.trim().to_string();
        if !s.is_empty() && !out.iter().any(|x| x == &s) {
            out.push(s);
        }
    };
    let q = quoted(instr);
    if !q.is_empty() {
        for s in &q {
            push(s.clone());
        }
        // Unquoted typed tokens still count: `set quantity to 2`, `phone 555-0192`.
        let mut rest = instr.to_string();
        for s in &q {
            rest = rest.replace(&format!("\"{s}\""), " ");
            if s.starts_with("{{") {
                rest = rest.replace(s.as_str(), " ");
            }
        }
        for w in rest.split_whitespace().map(clean) {
            let typed = w.chars().any(|c| c.is_ascii_digit()) || w.contains('@') || w.contains("://");
            if typed && !w.is_empty() {
                push(w.to_string());
            }
            if let Some(d) = number_word(w) {
                push(d);
            }
        }
        return out.into_iter().take(max).collect();
    }
    let words: Vec<&str> = instr.split_whitespace().map(clean).filter(|w| !w.is_empty()).collect();
    let stop = |w: &str| EDGE_STOP.contains(&w.to_lowercase().as_str());
    for n in 1..=4 {
        for win in words.windows(n) {
            if stop(win[0]) || stop(win[n - 1]) {
                continue;
            }
            push(win.join(" "));
            if n == 1 {
                if let Some(d) = number_word(win[0]) {
                    push(d);
                }
            }
        }
    }
    out.truncate(max);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes() {
        assert_eq!(quoted(r#"type "alice" and 'hunter 2' but don't"#), vec!["alice", "hunter 2"]);
        assert_eq!(quoted("it's bob's"), Vec::<String>::new());
    }

    #[test]
    fn possessives_numbers_and_types() {
        let s = spans("sign in with bob's account and add two shirts", 100);
        assert!(s.contains(&"bob".to_string()) && !s.contains(&"bob's".to_string()));
        assert!(s.contains(&"2".to_string()));
        assert!(fits(Some("number"), "2") && !fits(Some("number"), "two"));
        assert!(fits(Some("email"), "a@b.io") && !fits(Some("email"), "alice"));
        assert!(fits(Some("tel"), "+358 40 555 0192") && !fits(Some("tel"), "Helsinki"));
    }

    #[test]
    fn placeholders_are_literals() {
        assert_eq!(quoted("pay with {{card number}} and \"{{CVC}}\""), vec!["{{CVC}}", "{{card number}}"]);
        assert!(fits(Some("email"), "{{username}}"));
        let s = spans("sign in as {{username}} with {{password}}", 10);
        assert!(s.contains(&"{{username}}".to_string()) && s.contains(&"{{password}}".to_string()));
        assert!(!s.iter().any(|x| x.contains("{{") && !x.ends_with("}}")));
    }

    #[test]
    fn quoted_plus_unquoted_numbers() {
        let s = spans(r#"select "M" from "Size", set quantity to 2, and click "Add to cart""#, 100);
        assert!(s.contains(&"M".to_string()) && s.contains(&"2".to_string()));
        let s = spans(r#"set the quantity to two and click "Add""#, 100);
        assert!(s.contains(&"2".to_string()));
    }

    #[test]
    fn ngrams_skip_stopword_edges() {
        let s = spans("log in as alice with password hunter2", 100);
        assert!(s.contains(&"alice".to_string()));
        assert!(s.contains(&"hunter2".to_string()));
        assert!(!s.contains(&"as alice".to_string()));
        let s = spans("email bob@example.com, then submit.", 100);
        assert!(s.contains(&"bob@example.com".to_string()));
    }
}
