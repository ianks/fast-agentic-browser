//! Goal runtime helpers: splitting a goal into checkable clauses, and the
//! clause gate that must pass before a goal is reported done.

use anyhow::Result;
use serde_json::json;

use crate::jev::{Answers, Jev, Questions};
use crate::snapshot::Snapshot;

/// Splits a goal into clauses that can be checked one by one. Conditionals
/// ("if X, do Y") stay whole; sentences and "and then"/"then" separate.
pub fn clauses(goal: &str) -> Vec<String> {
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
    fn splits_clauses() {
        let c = clauses("Deploy release v2.14.0 to staging, and make sure the deploy actually succeeded.");
        assert_eq!(c.len(), 1);
        let c = clauses("Open the Billing section and then put the account on hold. If you get signed out, sign back in.");
        assert_eq!(c.len(), 3, "{c:?}");
    }
}
