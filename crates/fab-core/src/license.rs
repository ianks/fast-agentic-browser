//! The license Λ(σ): which effects an instruction permits. Commit and
//! destructive actions (R2/R3) happen only when the instruction asks for them,
//! so a mis-resolved or injected decision can't save, delete or pay on its own.

use crate::snapshot::El;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum Risk {
    /// Reveal, type (unsubmitted), open menus/tabs.
    R0,
    /// Navigate.
    R1,
    /// Commit: save, submit, send, add to cart…
    R2,
    /// Destructive or irreversible: delete, revoke, pay…
    R3,
}

/// (element verb, risk, instruction verbs that license it). Matching is on whole
/// words; multi-word entries match as phrases.
const LEXICON: &[(&str, Risk, &[&str])] = &[
    ("delete", Risk::R3, &["delete", "remove", "trash", "erase", "discard", "wipe", "destroy", "get rid"]),
    ("remove", Risk::R3, &["remove", "delete", "trash", "erase", "discard", "take off", "get rid"]),
    ("revoke", Risk::R3, &["revoke", "disable", "deactivate"]),
    ("unlink", Risk::R3, &["unlink", "remove", "disconnect", "detach", "delete"]),
    ("disconnect", Risk::R3, &["disconnect", "unlink", "remove", "detach", "revoke"]),
    ("detach", Risk::R3, &["detach", "unlink", "remove", "disconnect"]),
    ("deactivate", Risk::R3, &["deactivate", "disable", "revoke", "turn off"]),
    ("disable", Risk::R3, &["disable", "deactivate", "turn off", "revoke"]),
    ("pay", Risk::R3, &["pay", "purchase", "buy", "checkout", "check out", "place"]),
    ("purchase", Risk::R3, &["purchase", "buy", "pay", "order", "checkout", "check out"]),
    ("buy", Risk::R3, &["buy", "purchase", "pay", "order", "checkout", "check out"]),
    ("transfer", Risk::R3, &["transfer", "send", "move"]),
    ("refund", Risk::R3, &["refund"]),
    ("archive", Risk::R3, &["archive"]),
    ("unsubscribe", Risk::R3, &["unsubscribe", "opt out"]),
    ("terminate", Risk::R3, &["terminate", "end", "stop"]),
    ("reset", Risk::R3, &["reset", "clear", "restore"]),
    ("deploy", Risk::R3, &["deploy", "release", "ship", "roll out"]),
    ("place order", Risk::R2, &["place", "order", "checkout", "check out", "buy", "purchase"]),
    ("checkout", Risk::R2, &["checkout", "check out", "order", "buy", "purchase", "pay"]),
    ("check out", Risk::R2, &["checkout", "check out", "order", "buy", "purchase", "pay"]),
    ("add to cart", Risk::R2, &["add", "cart", "buy", "purchase"]),
    ("add to bag", Risk::R2, &["add", "bag", "buy", "purchase"]),
    ("wishlist", Risk::R2, &["wishlist", "wish list", "favorite", "favourite", "save for later"]),
    ("save", Risk::R2, &["save", "update", "apply", "change", "edit", "set", "rename", "modify", "store", "keep", "persist", "switch", "turn", "enable", "disable", "record"]),
    ("update", Risk::R2, &["update", "save", "change", "edit", "set", "modify", "apply"]),
    ("apply", Risk::R2, &["apply", "save", "use", "redeem", "set"]),
    ("submit", Risk::R2, &["submit", "send", "file", "post", "complete", "finish", "request", "create", "report", "sign up", "register", "leave", "write", "publish", "apply"]),
    ("send", Risk::R2, &["send", "submit", "post", "message", "email", "share", "reply", "contact"]),
    ("post", Risk::R2, &["post", "publish", "submit", "send", "reply", "comment"]),
    ("publish", Risk::R2, &["publish", "post", "release", "go live", "share"]),
    // Comments and replies publish under the user's name ("Add comment", "Reply").
    ("comment", Risk::R2, &["comment", "post", "publish", "submit", "reply", "send"]),
    ("reply", Risk::R2, &["reply", "respond", "answer", "post", "comment", "submit", "send"]),
    ("create", Risk::R2, &["create", "add", "new", "make", "open", "start", "set up"]),
    ("approve", Risk::R2, &["approve", "accept", "allow", "grant", "authorize", "authorise", "sign off", "okay", "ok"]),
    ("accept", Risk::R2, &["accept", "approve", "agree", "allow", "consent", "say yes", "ok"]),
    ("reject", Risk::R2, &["reject", "decline", "deny", "refuse", "turn down", "do not accept", "don't accept", "dont accept", "opt out", "say no"]),
    ("decline", Risk::R2, &["decline", "reject", "deny", "refuse", "turn down", "do not accept", "don't accept", "opt out", "say no"]),
    ("subscribe", Risk::R2, &["subscribe", "sign up", "newsletter", "join", "register", "enroll", "enrol"]),
    ("sign up", Risk::R2, &["sign up", "register", "create", "join", "account"]),
    ("register", Risk::R2, &["register", "sign up", "create", "join", "account"]),
    ("follow", Risk::R2, &["follow"]),
    ("bookmark", Risk::R2, &["bookmark", "save"]),
    ("invite", Risk::R2, &["invite", "add"]),
    ("upload", Risk::R2, &["upload", "attach", "add"]),
    ("enable", Risk::R2, &["enable", "activate", "turn on", "switch on"]),
    ("activate", Risk::R2, &["activate", "enable", "turn on"]),
    ("upgrade", Risk::R2, &["upgrade"]),
    ("start trial", Risk::R2, &["trial"]),
    ("start free trial", Risk::R2, &["trial"]),
    ("hold", Risk::R2, &["hold", "pause", "suspend"]),
    ("assign", Risk::R2, &["assign", "reassign", "give", "transfer", "move"]),
    ("book", Risk::R2, &["book", "reserve", "schedule"]),
    ("reserve", Risk::R2, &["reserve", "book", "hold"]),
    ("confirm", Risk::R2, &["confirm", "yes", "proceed"]),
    ("message", Risk::R2, &["message", "send", "write to", "contact", "dm", "chat", "text", "reach out"]),
    ("call", Risk::R2, &["call", "phone", "ring", "dial"]),
    ("notify", Risk::R2, &["notify", "notification", "alert me", "let me know", "tell me when"]),
    ("remind", Risk::R2, &["remind", "reminder"]),
    ("waitlist", Risk::R2, &["waitlist", "wait list", "waiting list"]),
    ("donate", Risk::R3, &["donate", "donation", "give"]),
    ("pre order", Risk::R3, &["pre order", "preorder", "pre-order"]),
    ("preorder", Risk::R3, &["pre order", "preorder", "pre-order"]),
    ("email me", Risk::R2, &["email me", "notify", "subscribe", "let me know"]),
    ("share", Risk::R2, &["share", "send"]),
    ("favorite", Risk::R2, &["favorite", "favourite", "star", "like"]),
];

/// Controls that commit whatever change the page is holding, not a change of
/// their own.
const GENERIC_COMMITS: &[&str] = &["save", "update", "apply", "submit", "confirm"];

/// Verbs of change beyond the lexicon's licensing words.
const CHANGE_VERBS: &[&str] = &[
    "make", "enter", "record", "log", "provide", "input", "put",
    "move", "change", "set", "edit", "reschedule", "rename", "reassign", "assign", "switch", "turn", "mark", "add", "update",
    "book", "request", "approve", "file", "report", "register", "enable", "disable", "grant", "give", "invite", "share",
    "hold", "return", "exchange", "cancel", "merge", "upgrade", "downgrade", "transfer", "rotate", "adjust", "correct", "fix",
    "complete", "finish", "replace", "reorder", "schedule", "allow", "restrict", "limit", "promote", "demote", "advance",
];

/// Does the task ask for any change to be made (as opposed to only reading)?
fn asks_for_change(ws: &[String]) -> bool {
    // The lexicon's element verbs, not its licensing families: those hold
    // broad words ("open", "new", "use") that plain questions contain too.
    CHANGE_VERBS.iter().any(|v| has_phrase(ws, v)) || LEXICON.iter().any(|(verb, r, _)| *r == Risk::R2 && has_phrase(ws, verb))
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Crude stem so "deletion", "deleting", "deleted" all match "delete".
fn stem(w: &str) -> String {
    for suf in ["ations", "ation", "ions", "ion", "ing", "ed", "es", "s", "e"] {
        if let Some(x) = w.strip_suffix(suf) {
            if x.chars().count() >= 3 {
                return x.to_string();
            }
        }
    }
    w.to_string()
}

/// Whole-word phrase match on stems ("confirm the deletion" matches "delete").
fn has_phrase(ws: &[String], phrase: &str) -> bool {
    let p: Vec<String> = words(phrase).iter().map(|w| stem(w)).collect();
    let ws: Vec<String> = ws.iter().map(|w| stem(w)).collect();
    !p.is_empty() && ws.windows(p.len()).any(|w| w == p.as_slice())
}

/// Risk class of clicking `e`, and the lexicon entry that set it.
pub fn risk(e: &El) -> (Risk, Option<&'static (&'static str, Risk, &'static [&'static str])>) {
    let ws = words(&e.n);
    // Expanders reveal; they commit nothing, whatever else their name says
    // ("Expand checkout").
    if e.has_flag("expanded") || e.has_flag("collapsed") || ws.first().is_some_and(|w| matches!(w.as_str(), "expand" | "collapse" | "show" | "hide")) {
        return (Risk::R0, None);
    }
    // Controls that only change what a list shows ("Apply filters", "Update
    // results") commit nothing, whatever their verb.
    if ws.iter().any(|w| matches!(w.as_str(), "filter" | "filters" | "results" | "search" | "sort" | "refresh")) {
        return (if e.r == "link" { Risk::R1 } else { Risk::R0 }, None);
    }
    // Multi-word entries first ("place order" before "order"-like words), then by order.
    let mut best: Option<&(&str, Risk, &[&str])> = None;
    for entry in LEXICON {
        if has_phrase(&ws, entry.0) && best.is_none_or(|b| words(entry.0).len() > words(b.0).len() || entry.1 > b.1) {
            best = Some(entry);
        }
    }
    match best {
        // A link that starts a flow ("Register a camper", "Create account",
        // "Book now") navigates to its form; the commit is that form's submit.
        Some(b) if e.r == "link" && b.1 == Risk::R2 && FLOW_STARTERS.contains(&b.0) => (Risk::R1, None),
        Some(b) => (b.1, Some(b)),
        None if e.r == "link" => (Risk::R1, None),
        None => (Risk::R0, None),
    }
}

/// Verbs that, on a link, open a form rather than commit anything.
const FLOW_STARTERS: &[&str] = &[
    "register", "sign up", "create", "book", "reserve", "apply", "checkout", "check out", "start trial", "start free trial", "invite",
    "upload", "submit", "request", "post", "message",
];

/// Does the fuzzy instruction `instr` license clicking `e`?
/// "Confirm" buttons are licensed when the instruction licenses any commit
/// (they finish an action the instruction asked for).
pub fn licensed(instr: &str, e: &El) -> Result<(), String> {
    let (r, entry) = risk(e);
    if r <= Risk::R1 {
        return Ok(());
    }
    // Quoted text is data (a value to type, a message to send), not the
    // user's words: a comment that says "submitted" doesn't ask to submit.
    // It counts only when it names this control exactly (click "Place order").
    let ws = words(&outside_quotes(instr));
    let name = e.n.trim();
    let quoted_name = crate::spans::quoted(instr).iter().any(|q| q.trim().eq_ignore_ascii_case(name));
    if name.chars().count() >= 3 && (has_phrase(&ws, name) || quoted_name) {
        return Ok(());
    }
    let Some((verb, _, family)) = entry else { return Ok(()) };
    if family.iter().any(|v| has_phrase(&ws, v)) {
        return Ok(());
    }
    if *verb == "confirm" && LEXICON.iter().any(|(_, r, fam)| *r >= Risk::R2 && fam.iter().any(|v| has_phrase(&ws, v))) {
        return Ok(());
    }
    // A task that asks for a change licenses the generic control that commits
    // it ("move the meeting to 15:30" → Save). Specific commits (send, delete,
    // pay, subscribe…) still need their own verb, and R3 never gets this.
    if r == Risk::R2 && GENERIC_COMMITS.contains(verb) && asks_for_change(&ws) {
        return Ok(());
    }
    Err(format!("the instruction doesn't ask to {verb} (would click {} \"{}\")", e.r, e.n))
}

/// The instruction as a request: its quoted values blanked out, and the
/// field a value goes "into" (the comment box, the message) dropped up to the
/// end of its clause, since naming a field doesn't ask to commit it.
fn outside_quotes(instr: &str) -> String {
    let mut cs: Vec<char> = instr.chars().collect();
    for (a, b) in crate::spans::quoted_ranges(instr) {
        cs[a..b].iter_mut().for_each(|c| *c = ' ');
    }
    let s: String = cs.into_iter().collect();
    let mut out = String::new();
    let mut rest = s.as_str();
    while let Some(i) = rest.to_lowercase().find(" into ") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + " into ".len()..];
        let lower = tail.to_lowercase();
        let end = [",", ";", "\n", " then ", " and "].iter().filter_map(|w| lower.find(w)).min().unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Record grounding: a commit on a per-row button that repeats across rows
/// (Message, Delete, Mark paid…) must be on a row the text refers to. Without
/// that, a step like "then click Send" picks an arbitrary row.
pub fn grounded(text: &str, e: &El, snap: &crate::snapshot::Snapshot) -> Result<(), String> {
    let Some(rc) = e.rc else { return Ok(()) };
    if risk(e).0 < Risk::R2 {
        return Ok(());
    }
    let repeated = snap.els.iter().filter(|x| x.rc.is_some() && x.rc != e.rc && x.r == e.r && x.n == e.n).count() >= 1;
    if !repeated {
        return Ok(());
    }
    let row_of = |rc: u32, x: &El| -> String {
        let row: String = snap.texts.iter().filter(|t| t.rc == Some(rc)).map(|t| t.x.as_str()).collect::<Vec<_>>().join(" ");
        if row.is_empty() { x.c.clone().unwrap_or_default() } else { row }
    };
    let row = row_of(rc, e);
    let name: std::collections::HashSet<String> = crate::snapshot::tokens(&e.n).into_iter().collect();
    let want: std::collections::HashSet<String> =
        crate::snapshot::tokens(text).into_iter().filter(|w| !name.contains(w) && w.chars().count() >= 2).collect();
    if !crate::snapshot::tokens(&row).iter().any(|w| want.contains(w)) {
        return Err(format!("which row? the instruction doesn't say which \"{}\" to click (would use the one in: {})", e.n, crate::snapshot::truncate(&row, 60)));
    }
    // Near-duplicates ("maren.lund@x.io" vs "maren.lundqvist@x.io", "Halvorsen
    // Freight AS" vs "Halvorsen Fresh Foods") share words; the row that holds
    // the instruction's exact literals is the one it means.
    let score = |r: &str| row_score(text, r, &want);
    let mine = score(&row);
    let best_other = snap
        .els
        .iter()
        .filter(|x| x.rc.is_some() && x.rc != e.rc && x.r == e.r && x.n == e.n)
        .map(|x| (score(&row_of(x.rc.unwrap(), x)), x))
        .max_by(|a, b| a.0.total_cmp(&b.0));
    match best_other {
        Some((s, x)) if s > mine => Err(format!(
            "the instruction matches another row better: \"{}\" in {} rather than {}",
            e.n,
            crate::snapshot::truncate(&row_of(x.rc.unwrap(), x), 60),
            crate::snapshot::truncate(&row, 60)
        )),
        _ => Ok(()),
    }
}

/// How well a row's text matches an instruction: exact, whole occurrences of
/// its strong literals (quoted text, emails, ids with digits) weigh far more
/// than shared words.
fn row_score(text: &str, row: &str, want: &std::collections::HashSet<String>) -> f64 {
    let rl = row.to_lowercase();
    let whole = |lit: &str| {
        let l = lit.to_lowercase();
        let mut from = 0;
        while let Some(i) = rl[from..].find(&l) {
            let a = from + i;
            let b = a + l.len();
            let before = rl[..a].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
            let after = rl[b..].chars().next().is_none_or(|c| !c.is_alphanumeric());
            if before && after {
                return true;
            }
            from = a + l.len().max(1);
        }
        false
    };
    let mut lits: Vec<String> = crate::spans::quoted(text);
    for w in text.split_whitespace() {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric() && c != '@' && c != '.' && c != '-' && c != '_').trim_end_matches('.');
        if w.chars().count() >= 3 && (w.contains('@') || w.chars().any(|c| c.is_ascii_digit())) {
            lits.push(w.to_string());
        }
    }
    let exact: f64 = lits.iter().filter(|l| !l.trim().is_empty() && whole(l)).map(|l| 10.0 + l.len() as f64).sum();
    let shared = crate::snapshot::tokens(row).iter().filter(|w| want.contains(*w)).count() as f64;
    exact + shared
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(name: &str) -> El {
        serde_json::from_value(serde_json::json!({"i": 1, "r": "button", "n": name})).unwrap()
    }

    #[test]
    fn flow_starting_links_navigate() {
        let link = |n: &str| -> El { serde_json::from_value(serde_json::json!({"i": 1, "r": "link", "n": n})).unwrap() };
        assert_eq!(risk(&link("Register a camper")).0, Risk::R1);
        assert_eq!(risk(&link("Submit a meter reading")).0, Risk::R1);
        assert_eq!(risk(&b("Expand checkout")).0, Risk::R0);
        assert_eq!(risk(&b("Checkout")).0, Risk::R2);
        assert!(licensed("make Ngozi the primary driver of van LS70 KWD", &b("Save assignment")).is_ok());
        assert_eq!(risk(&b("Register")).0, Risk::R2);
        // Destructive and direct commits keep their class on links too.
        assert_eq!(risk(&link("Delete account")).0, Risk::R3);
        assert_eq!(risk(&link("Approve")).0, Risk::R2);
    }

    #[test]
    fn typed_text_does_not_license_a_commit() {
        assert_eq!(risk(&b("add comment")).0, Risk::R2);
        assert_eq!(risk(&b("Reply")).0, Risk::R2);
        assert!(licensed("type \"hi\" into the comment box, then post it", &b("add comment")).is_ok());
        assert!(licensed("type \"hi\" into the comment box", &b("add comment")).is_err());
        // The words of a value to type are data, not the user's request.
        assert!(licensed("type \"This post was submitted by fab. Add comment below\" into the comment box", &b("add comment")).is_err());
        assert!(licensed("type \u{201c}send it, then post it\u{201d} into the message", &b("Send")).is_err());
        assert!(licensed("type \u{201c}send it\u{201d} into the message, then send it", &b("Send")).is_ok());
        // A quote that names the control is still the user's word.
        assert!(licensed("type \"hi\" into e1, then click \"add comment\"", &b("add comment")).is_ok());
    }

    #[test]
    fn near_duplicate_rows() {
        let want = |t: &str| crate::snapshot::tokens(t).into_iter().collect::<std::collections::HashSet<_>>();
        let t = "make maren.lund@stornoway.io a Viewer";
        assert!(row_score(t, "maren.lund@stornoway.io Editor", &want(t)) > row_score(t, "maren.lundqvist@stornoway.io Editor", &want(t)));
        let t = "place \"Halvorsen Freight AS\" on hold";
        assert!(row_score(t, "Halvorsen Freight AS Active", &want(t)) > row_score(t, "Halvorsen Fresh Foods Active", &want(t)));
    }

    #[test]
    fn change_tasks_license_generic_commits() {
        assert!(licensed("move the review so it starts at 15:30", &b("Save")).is_ok());
        assert!(licensed("reassign the ticket to Priya", &b("Update")).is_ok());
        // Reading licenses nothing; specific commits still need their verb.
        assert!(licensed("how many tickets are open?", &b("Save")).is_err());
        assert!(licensed("move the review to 15:30", &b("Send")).is_err());
        assert!(licensed("move the review to 15:30", &b("Delete event")).is_err());
    }

    #[test]
    fn commit_needs_matching_verb() {
        assert!(licensed("mark invoice #1042 as paid", &b("Mark paid")).is_ok());
        assert!(licensed("mark invoice #1042 as paid", &b("Delete")).is_err());
        assert!(licensed("add the laptop with the most RAM to the cart", &b("Save to wishlist")).is_err());
        assert!(licensed("add the laptop with the most RAM to the cart", &b("Add to cart")).is_ok());
        assert!(licensed("choose the Basic plan and continue", &b("Upgrade now")).is_err());
        assert!(licensed("choose the Basic plan and continue", &b("Continue")).is_ok());
        assert!(licensed("delete the file \"report-2024.pdf\"", &b("Delete")).is_ok());
        assert!(licensed("delete the file \"report-2024.pdf\" and confirm", &b("Confirm")).is_ok());
        assert!(licensed("check out ... and place the order", &b("Place order")).is_ok());
        assert!(licensed("set the time zone to Europe/Berlin and save the settings", &b("Save settings")).is_ok());
        // "address" must not license "add to cart".
        assert!(licensed("type the address", &b("Add to cart")).is_err());
        assert!(licensed("Confirm the deletion of the file", &b("Delete")).is_ok());
        assert!(licensed("Turn down all cookies", &b("Reject all")).is_ok());
        assert!(licensed("Do not accept any cookies", &b("Reject all")).is_ok());
        assert!(licensed("Consent to all cookies", &b("Accept all")).is_ok());
        assert!(licensed("Buy the Aurora Desk Lamp in Sage", &b("Notify me")).is_err());
        assert!(licensed("notify me when Sage is back", &b("Notify me")).is_ok());
        assert!(licensed("search for Zelda Fitzgerald", &b("Message")).is_err());
        assert!(licensed("Send a message to Zelda Fitzgerald", &b("Message")).is_ok());
        assert!(licensed("message Zelda", &b("Call")).is_err());
    }
}
