//! Scrubs every concealed value fab has typed from anything it hands back:
//! page listings, tool replies, console and network logs, eval results,
//! errors. Pages echo what they are given (a URL query, a JSON body, a
//! base64 header), so each value is removed in those forms too.

use std::sync::RwLock;
use zeroize::Zeroizing;

/// What a scrubbed value becomes.
pub const MASK: &str = "••••";

/// Shorter values (a card's expiry month) are masked on the page but not
/// scrubbed from text, where they would erase unrelated digits.
const MIN_LEN: usize = 4;

/// A cut-off value (`abcdefghijkl…`, as page listings shorten long text) is
/// scrubbed when at least this much of it shows.
const MIN_PREFIX: usize = 16;

static FORMS: RwLock<Vec<Zeroizing<String>>> = RwLock::new(Vec::new());

/// Registers a concealed value (and its encoded forms) for scrubbing.
pub fn add(value: &str) {
    if value.chars().count() < MIN_LEN {
        return;
    }
    let mut forms = vec![value.to_string()];
    // Page listings collapse whitespace runs to one space.
    forms.push(value.split_whitespace().collect::<Vec<_>>().join(" "));
    let json = serde_json::to_string(value).unwrap_or_default();
    forms.push(json[1..json.len() - 1].to_string());
    forms.push(percent(value));
    forms.push(percent(value).replace("%20", "+"));
    forms.push(value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;"));
    use base64::Engine;
    forms.push(base64::engine::general_purpose::STANDARD.encode(value));
    forms.push(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value));
    let mut all = FORMS.write().unwrap();
    for f in forms {
        if f.chars().count() >= MIN_LEN && !all.iter().any(|x| **x == f) {
            all.push(Zeroizing::new(f));
        }
    }
    // Longest first, so a value's JSON form isn't half-replaced by a shorter one.
    all.sort_by_key(|f| std::cmp::Reverse(f.len()));
}

fn percent(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// True once any value is registered (until then scrubbing is free).
pub fn active() -> bool {
    !FORMS.read().unwrap().is_empty()
}

/// `s` with every registered value replaced by [`MASK`].
pub fn text(s: &str) -> String {
    let forms = FORMS.read().unwrap();
    let mut out = s.to_string();
    for f in forms.iter() {
        if out.contains(f.as_str()) {
            out = out.replace(f.as_str(), MASK);
        }
    }
    // A long value shown cut off: its prefix, then an ellipsis.
    if out.contains('…') {
        for f in forms.iter() {
            let ends: Vec<usize> = f.char_indices().map(|(i, _)| i).skip(MIN_PREFIX).collect();
            for &end in ends.iter().rev() {
                let cut = format!("{}…", &f[..end]);
                if out.contains(&cut) {
                    out = out.replace(&cut, MASK);
                }
            }
        }
    }
    out
}

/// Scrubs every string in a JSON value, in place.
pub fn value(v: &mut serde_json::Value) {
    if !active() {
        return;
    }
    match v {
        serde_json::Value::String(s) => {
            let t = text(s);
            if t != *s {
                *s = t;
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(value),
        serde_json::Value::Object(o) => o.values_mut().for_each(value),
        _ => {}
    }
}

#[cfg(test)]
pub fn clear() {
    FORMS.write().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_encoded_forms() {
        add("p@ss w\"rd&1");
        let url = format!("https://x.test/cb?pw={}", percent("p@ss w\"rd&1"));
        assert_eq!(text(&url), "https://x.test/cb?pw=••••");
        assert_eq!(text(r#"{"password":"p@ss w\"rd&1"}"#), r#"{"password":"••••"}"#);
        assert_eq!(text("typed p@ss w\"rd&1 ok"), "typed •••• ok");
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD.encode("p@ss w\"rd&1");
        assert_eq!(text(&format!("Basic {b}")), "Basic ••••");
        let mut v = serde_json::json!({"a": ["x p@ss w\"rd&1"]});
        value(&mut v);
        assert_eq!(v["a"][0], "x ••••");
        add("12");
        assert_eq!(text("12 items"), "12 items");
    }

    #[test]
    fn scrubs_collapsed_and_cut_off_forms() {
        let token = "tok_live_9f8e7d6c5b4a39281706f5e4d3c2b1a0ffeeddccbbaa99887766554433221100aabbccddeeff00112233";
        add(token);
        // A listing cuts long values to 79 characters and an ellipsis.
        let shown: String = token.chars().take(79).collect();
        assert_eq!(text(&format!("value: {shown}…")), "value: ••••");
        assert_eq!(text("tok_live_9f8…"), "tok_live_9f8…", "too little of it shows to be sure");
        add("two  spaced\n words here");
        assert_eq!(text("typed two spaced words here"), "typed ••••");
    }
}
