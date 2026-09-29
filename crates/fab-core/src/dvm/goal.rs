//! Goal runtime helpers: splitting a goal into checkable clauses, and the
//! clause gate that must pass before a goal is reported done.

use anyhow::Result;
use serde_json::json;

use crate::jev::{Answers, Jev, Questions};
use crate::snapshot::Snapshot;

/// Splits a goal into clauses that can be checked one by one. Conditionals
/// ("if X, do Y") stay whole; sentences and "and then"/"then" separate.
/// A quoted value is one piece of data, never split: the sentences of a
/// message to type are not parts of the goal.
pub fn clauses(goal: &str) -> Vec<String> {
    let cs: Vec<char> = goal.chars().collect();
    let values: Vec<String> = crate::spans::quoted_ranges(goal).into_iter().map(|(a, b)| cs[a..b].iter().collect()).collect();
    let mut masked = String::new();
    let mut at = 0;
    for (k, (a, b)) in crate::spans::quoted_ranges(goal).into_iter().enumerate() {
        masked.extend(&cs[at..a]);
        masked.push_str(&format!("\u{E000}{k}\u{E001}"));
        at = b;
    }
    masked.extend(&cs[at..]);
    let unmask = |c: String| {
        let mut c = c;
        for (k, v) in values.iter().enumerate() {
            c = c.replace(&format!("\u{E000}{k}\u{E001}"), v);
        }
        c
    };
    clauses_of(&masked).into_iter().map(unmask).collect()
}

fn clauses_of(goal: &str) -> Vec<String> {
    // Sentence ends: . ; ! ? followed by whitespace or the end ("v2.14.0" stays whole).
    let cs: Vec<char> = goal.chars().collect();
    let mut sents: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, c) in cs.iter().enumerate() {
        cur.push(*c);
        if matches!(c, '.' | ';' | '!' | '?') && cs.get(i + 1).is_none_or(|n| n.is_whitespace()) {
            sents.push(std::mem::take(&mut cur));
        }
    }
    sents.push(cur);
    let mut out = Vec::new();
    for sent in sents {
        let sent = sent.trim().trim_end_matches(['.', ';', '!']).trim();
        if sent.is_empty() {
            continue;
        }
        let l = sent.to_lowercase();
        if l.starts_with("if ") || l.contains(" if ") || l.starts_with("answer") || l.starts_with("tell me") {
            out.push(sent.to_string());
            continue;
        }
        for part in sent.split(", and then ").flat_map(|p| p.split(" and then ")).flat_map(|p| p.split(", then ")) {
            let part = part.trim();
            if part.split_whitespace().count() >= 2 {
                out.push(part.to_string());
            }
        }
    }
    if out.is_empty() {
        out.push(goal.trim().to_string());
    }
    out
}

/// Asks, in one request, whether each clause has been carried out. Returns
/// (clause, p) pairs plus the answers for accounting.
pub async fn check(jev: &Jev, goal: &str, cl: &[String], actions: &[String], snap: &Snapshot) -> Result<(Vec<(String, f64)>, Answers)> {
    let mut q = Questions::default();
    for (i, c) in cl.iter().enumerate() {
        q.noul_criteria(
            format!("c{i}"),
            json!({"clause": c, "question": "Has `clause` been carried out? Judge from `actions_taken` and the current page."}),
            "Yes: the actions and the page show this part of the goal is done (or it is a condition that did not apply).",
            "No: this part still has to be done, or the page shows it failed.",
        );
    }
    let state = json!({
        "goal": goal,
        "actions_taken": actions,
        "page": format!("{} — {}", snap.title, snap.url),
        "page_text": snap.text_lines(|t| t.a.is_some() || t.h.is_some()).into_iter().chain(snap.text_context(goal, 16)).take(30).collect::<Vec<_>>(),
    });
    let a = jev.ask(&state, &q).await?;
    let res = cl.iter().enumerate().map(|(i, c)| (c.clone(), a.noul(&format!("c{i}")).unwrap_or(0.0))).collect();
    Ok((res, a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_message_is_one_clause() {
        let g = "type \u{201c}First. Then second; and then third!\u{201d} into the comment box";
        assert_eq!(clauses(g), vec![g.to_string()]);
        assert_eq!(clauses("type \"a. b\" into e1. Then click Save"), vec!["type \"a. b\" into e1", "Then click Save"]);
    }

    #[test]
    fn splits_clauses() {
        let c = clauses("Deploy release v2.14.0 to staging, and make sure the deploy actually succeeded.");
        assert_eq!(c.len(), 1);
        let c = clauses("Open the Billing section and then put the account on hold. If you get signed out, sign back in.");
        assert_eq!(c.len(), 3, "{c:?}");
    }
}
