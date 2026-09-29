//! Decision VM: decides one step on one observation with as few Jev rounds as
//! the evidence allows, under the license Λ and risk-tiered commit rules.
//!
//! Round 1 is the fused legacy question batch (fields, click, done, final)
//! plus same-request VERIFY Nouls for the risky candidates the prior ranks
//! highest, so verification costs no extra round. The decision rule commits a
//! plan when the click is licensed and confident enough for its risk class;
//! otherwise one refine round reranks the nucleus with richer context and a
//! VERIFY per candidate; otherwise it escalates with ranked alternatives.
//! Choice (relative) and Noul (absolute) answers are never pooled numerically:
//! Nouls act as gates.

pub mod data;
pub mod goal;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::Knobs;
use crate::decide::{self, Plan};
use crate::jev::{Answers, Oracle, Questions};
use crate::license::{self, Risk};
use crate::snapshot::{El, Kind, Snapshot, score_all, truncate};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Outcome {
    /// Execute `plan`.
    Commit,
    /// The instruction is already carried out.
    Done,
    /// Hand back to the caller with a reason.
    Escalate(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub plan: Plan,
    pub outcome: Outcome,
    pub rounds: u32,
    /// Ranked (element id, p) alternatives for escalations.
    pub alternatives: Vec<(String, f64)>,
    #[serde(skip)]
    pub answers: Vec<Answers>,
    pub trace: Vec<Value>,
}

// Literal reading: say that fields are filled first, or Jev answers "no, pick
// the size first" for a correct submit click.
const VERIFY_Q: &str = "The fields listed in `auto_filled` are filled in first. After that, is clicking `candidate` the next step that `instruction` asks for?";

fn el<'a>(snap: &'a Snapshot, i: usize) -> Option<&'a El> {
    snap.els.iter().find(|e| e.i == i)
}

/// The record line an element belongs to, for richer descriptions.
fn record_text(snap: &Snapshot, e: &El) -> Option<String> {
    let rc = e.rc?;
    snap.text_lines(|t| t.rc == Some(rc)).into_iter().next().map(|l| truncate(&l, 160))
}

fn rich(snap: &Snapshot, e: &El) -> String {
    match record_text(snap, e) {
        Some(r) => format!("{} | row: {r}", e.line(true)),
        None => e.line(true),
    }
}

/// Does the task's wording license some other visible commit control?
fn lexicon_points_elsewhere(snap: &Snapshot, licence: &str, c: usize) -> bool {
    snap.els.iter().any(|x| {
        x.i != c && !x.latent() && !x.has_flag("disabled") && license::risk(x).0 >= Risk::R2 && license::licensed(licence, x).is_ok()
    })
}

fn verify_p(a: &Answers, i: usize) -> Option<f64> {
    a.yes(&format!("verify_e{i}"))
}

/// Offered click candidates of a built question batch.
fn offered(q: &Questions) -> Vec<usize> {
    q.0.get("click")
        .and_then(|c| c["criteria"].as_object())
        .map(|m| m.keys().filter_map(|k| k.strip_prefix('e').and_then(|s| s.parse().ok())).collect())
        .unwrap_or_default()
}

/// Round 1 of a decision, built without any network: the fused legacy batch
/// plus (outside parity mode) the goal context, same-request VERIFY Nouls for
/// the prior's top risky candidates, and the impossible gate. Pure, so parity
/// with `decide::build` is testable offline.
pub fn round1(
    snap: &Snapshot,
    instr: &str,
    goal: Option<&str>,
    history: &[String],
    k: &Knobs,
) -> (Value, Questions, decide::Mapping, Vec<usize>) {
    let (mut state, mut q, map) = decide::build(snap, instr, history, k);
    let mut verified: Vec<usize> = Vec::new();
    if k.dvm_parity {
        return (state, q, map, verified);
    }
    // The overall goal gives a step its referents ("click Send" → whose message?).
    if let Some(g) = goal.filter(|g| *g != instr) {
        state["goal"] = json!(g);
    }
    let cands: Vec<&El> = offered(&q).into_iter().filter_map(|i| el(snap, i)).collect();
    let scores = score_all(instr, &cands.iter().map(|e| e.desc(true)).collect::<Vec<_>>());
    let mut risky: Vec<(f64, &El)> = cands
        .iter()
        .zip(scores)
        // Licensed commits, plus R2 ones the lexicon rejects: their VERIFY can
        // license them semantically (a paraphrase without the verb).
        .filter(|(e, _)| {
            let r = license::risk(e).0;
            r >= Risk::R2 && (license::licensed(instr, e).is_ok() || (r == Risk::R2 && k.sem_license <= 1.0))
        })
        .map(|(e, s)| (s, *e))
        .collect();
    risky.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, e) in risky.into_iter().take(3) {
        q.gate(
            format!("verify_e{}", e.i),
            json!({"candidate": e.line(k.ctx), "question": VERIFY_Q}),
            "Yes, that click is the right next step.",
            "No, that click is not what the instruction asks for next.",
            k.gates_as_choice,
        );
        verified.push(e.i);
    }
    q.gate(
        "impossible",
        "Does the current page show that what `instruction` asks for cannot be done (the requested item or option is disabled, sold out, out of stock, unavailable, or the action is not allowed)?",
        "The page explicitly shows the requested thing is unavailable or not allowed.",
        "Nothing on the page says the requested thing is unavailable.",
        k.gates_as_choice,
    );
    (state, q, map, verified)
}

pub async fn decide<O: Oracle>(
    snap: &Snapshot,
    instr: &str,
    goal: Option<&str>,
    history: &[String],
    k: &Knobs,
    jev: &O,
) -> Result<Decision> {
    decide_with(snap, instr, goal, goal, history, k, jev).await
}

/// `shown`: goal text included in the decision state; `ground`: goal text used
/// only to check which rows a commit may touch.
pub async fn decide_with<O: Oracle>(
    snap: &Snapshot,
    instr: &str,
    shown: Option<&str>,
    ground: Option<&str>,
    history: &[String],
    k: &Knobs,
    jev: &O,
) -> Result<Decision> {
    let goal = ground;
    let (state, q, map, verified) = round1(snap, instr, shown, history, k);
    let mut trace = Vec::new();

    let a1 = jev.answer(&state, &q).await?;
    let mut plan = decide::decode(&a1, &map, snap);
    decide::dedupe_values(&mut plan, instr);
    let mut d = Decision {
        plan,
        outcome: Outcome::Commit,
        rounds: 1,
        alternatives: a1.ranked("click").into_iter().take(4).map(|(k, p)| (k.to_string(), p)).collect(),
        answers: vec![],
        trace: vec![],
    };
    trace.push(json!({
        "round": 1, "questions": q.len(), "tokens": a1.usage.input_tokens, "ms": a1.latency.as_secs_f64() * 1e3,
        "click": d.alternatives, "done": d.plan.done, "final": d.plan.final_p, "impossible": a1.yes("impossible"),
        "verify": verified.iter().map(|i| (format!("e{i}"), verify_p(&a1, *i))).collect::<Vec<_>>(),
    }));

    let impossible = a1.yes("impossible").unwrap_or(0.0);
    // Stop rule shared with the legacy loop.
    let step0 = history.is_empty();
    let done = d.plan.done >= k.done_threshold && (!step0 || d.plan.is_noop() || d.plan.done >= 0.9);
    if done {
        d.outcome = Outcome::Done;
    } else if impossible >= k.impossible && !k.dvm_parity {
        d.outcome = Outcome::Escalate(format!("impossible: the page shows that what was asked for is unavailable (p={impossible:.2})"));
    } else if d.plan.is_noop() {
        d.outcome = Outcome::Escalate(if step0 {
            "no applicable action found on this page".into()
        } else {
            format!("nothing further to do, but the instruction does not look done (p={:.2})", d.plan.done)
        });
    } else if !k.dvm_parity {
        if let Some(c) = d.plan.click {
            judge_click(snap, instr, goal, k, jev, &state, &a1, c, &mut d, &mut trace).await?;
        }
    }
    d.answers.push(a1);
    d.trace = trace;
    Ok(d)
}

/// Applies the license and risk-tiered commit rule to the chosen click; may run
/// one refine round.
#[allow(clippy::too_many_arguments)]
async fn judge_click<O: Oracle>(
    snap: &Snapshot,
    instr: &str,
    goal: Option<&str>,
    k: &Knobs,
    jev: &O,
    state1: &Value,
    a1: &Answers,
    c: usize,
    d: &mut Decision,
    trace: &mut Vec<Value>,
) -> Result<()> {
    let Some(e) = el(snap, c) else { return Ok(()) };
    // The user's task licenses commits; a program step only scopes the decision.
    let licence = format!("{instr} {}", goal.unwrap_or(""));
    if let Err(why) = license::licensed(&licence, e) {
        // Semantic license: the task asks for this commit in other words. Only
        // where the lexicon is silent: if the task's verbs license another
        // commit control on the page, that one is what it means.
        let semantic = license::risk(e).0 == Risk::R2
            && verify_p(a1, c).is_some_and(|v| v >= k.sem_license)
            && !lexicon_points_elsewhere(snap, &licence, c);
        if !semantic {
            d.outcome = Outcome::Escalate(format!("not permitted: {why}. Ask for it explicitly if that's intended"));
            return Ok(());
        }
        trace.push(json!({"licensed_by_verify": format!("e{c}"), "verify": verify_p(a1, c)}));
    }
    let context = format!("{instr} {}", goal.unwrap_or(""));
    if let Err(why) = license::grounded(&context, e, snap) {
        d.outcome = Outcome::Escalate(why);
        return Ok(());
    }
    let risk = license::risk(e).0;
    let tau = k.tau[risk as usize];
    let v = verify_p(a1, c);
    // R3 needs a positive VERIFY; R2 is blocked only by a clear "no".
    let verify_ok = match risk {
        Risk::R3 => v.is_some_and(|v| v >= k.nu),
        Risk::R2 => v.is_none_or(|v| v >= k.nu2),
        _ => true,
    };
    if d.plan.click_p >= tau && verify_ok {
        return Ok(());
    }
    if d.rounds > k.r_max {
        d.outcome = Outcome::Escalate(format!("not confident enough to click {} \"{}\" (p={:.2})", e.r, e.n, d.plan.click_p));
        return Ok(());
    }
    // Refine: rerank the nucleus with record context, VERIFY each member.
    let mut mass = 0.0;
    let mut nucleus: Vec<&El> = Vec::new();
    for (key, p) in a1.ranked("click") {
        let Some(i) = key.strip_prefix('e').and_then(|s| s.parse::<usize>().ok()) else { continue };
        if let Some(x) = el(snap, i) {
            nucleus.push(x);
            mass += p;
        }
        if nucleus.len() >= 5 || mass >= 0.95 {
            break;
        }
    }
    let mut q = Questions::default();
    let opts = nucleus
        .iter()
        .map(|x| (x.id(), Some(rich(snap, x))))
        .chain(std::iter::once(("none".to_string(), Some("None of these is the right next click.".to_string()))));
    q.choice(
        "pick",
        "Which element in `candidates` should be clicked next to carry out `instruction`? The fields in `auto_filled` are filled in automatically before the click; `actions_taken` lists what is already done.",
        opts,
    );
    for x in &nucleus {
        q.gate(
            format!("verify_e{}", x.i),
            json!({"candidate": rich(snap, x), "question": VERIFY_Q}),
            "Yes, that click is the right next step.",
            "No, that click is not what the instruction asks for next.",
            k.gates_as_choice,
        );
    }
    // Same context as round 1 (history, auto-filled fields, page text), plus
    // the nucleus with record context.
    let mut state = state1.clone();
    state["candidates"] = json!(nucleus.iter().map(|x| rich(snap, x)).collect::<Vec<_>>());
    let a2 = jev.answer(&state, &q).await?;
    d.rounds += 1;
    let ranked: Vec<(String, f64)> = a2.ranked("pick").into_iter().take(4).map(|(k, p)| (k.to_string(), p)).collect();
    trace.push(json!({
        "round": 2, "questions": q.len(), "tokens": a2.usage.input_tokens, "ms": a2.latency.as_secs_f64() * 1e3,
        "pick": ranked,
        "verify": nucleus.iter().map(|x| (x.id(), verify_p(&a2, x.i))).collect::<Vec<_>>(),
    }));
    d.alternatives = ranked;
    let pick = a2.choice("pick").and_then(|(k, p, _)| Some((k.strip_prefix('e')?.parse::<usize>().ok()?, p)));
    let outcome = match pick.and_then(|(i, p)| Some((el(snap, i)?, p))) {
        None => Outcome::Escalate("no candidate fits the instruction well enough".into()),
        Some((x, p)) => {
            let risk = license::risk(x).0;
            let v = verify_p(&a2, x.i).unwrap_or(0.0);
            let floor = if risk >= Risk::R3 { k.nu } else if risk >= Risk::R2 { k.nu2 } else { 0.0 };
            let semantic = risk == Risk::R2 && v >= k.sem_license && !lexicon_points_elsewhere(snap, &format!("{instr} {}", goal.unwrap_or("")), x.i);
            if license::licensed(&format!("{instr} {}", goal.unwrap_or("")), x).is_err() && !semantic {
                Outcome::Escalate(format!("not permitted: would click {} \"{}\"", x.r, x.n))
            } else if let Err(why) = license::grounded(&format!("{instr} {}", goal.unwrap_or("")), x, snap) {
                Outcome::Escalate(why)
            } else if p >= k.tau[risk as usize] && v >= floor {
                d.plan.click = Some(x.i);
                d.plan.click_p = p;
                Outcome::Commit
            } else {
                Outcome::Escalate(format!("ambiguous: best guess {} \"{}\" (p={p:.2}, verify={v:.2})", x.r, x.n))
            }
        }
    };
    d.answers.push(a2);
    d.outcome = outcome;
    Ok(())
}

/// Latent targets of a plan: (target, trigger) pairs to reveal before acting.
/// Result of the pre-commit form audit.
#[derive(Debug, Clone, Serialize)]
pub struct Audit {
    /// P(the form carries out exactly what the task asks).
    pub p: f64,
    /// The field most likely to disagree with the task, and its probability.
    pub wrong: Option<(usize, f64)>,
    /// On a review page: the summary line most likely to disagree.
    pub line: Option<String>,
    #[serde(skip)]
    pub answers: Option<Answers>,
}

/// One line per form field as it will be submitted: value, checked state and
/// label context, without option lists.
fn audit_line(e: &El) -> String {
    let mut s = format!("{} {} \"{}\"", e.id(), e.r, e.n);
    match e.kind() {
        Kind::Text | Kind::Select => s.push_str(&format!(" value=\"{}\"", e.v.as_deref().unwrap_or(""))),
        Kind::Check | Kind::Radio => s.push_str(if e.has_flag("checked") { " (checked)" } else { " (not checked)" }),
        _ => {}
    }
    if let Some(g) = &e.g {
        s.push_str(&format!(" group=\"{g}\""));
    }
    if let Some(c) = &e.c {
        s.push_str(&format!(" — in: {}", truncate(c, 80)));
    }
    s
}

/// Before an R2/R3 commit: does what is about to be submitted carry out exactly
/// what `task` asks (every value as given, every choice made, nothing extra)?
/// One Jev round over the live field values plus the page's text, so review
/// and confirmation pages (a summary, no fields) are checked too.
pub async fn audit<O: Oracle>(snap: &Snapshot, task: &str, commit: usize, jev: &O) -> Result<Option<Audit>> {
    let fields: Vec<&El> = snap
        .els
        .iter()
        .filter(|e| matches!(e.kind(), Kind::Text | Kind::Select | Kind::Check | Kind::Radio) && !e.latent() && !e.has_flag("disabled"))
        // Unselected radio options are noise; the selected one says what was chosen.
        .filter(|e| e.kind() != Kind::Radio || e.has_flag("checked"))
        .take(80)
        .collect();
    let button = snap.els.iter().find(|e| e.i == commit);
    let mut text = String::new();
    for l in snap.text_lines(|_| true) {
        if text.len() + l.len() > 2500 {
            break;
        }
        text.push_str(&l);
        text.push('\n');
    }
    let state = json!({
        "task": task,
        "form": fields.iter().map(|e| audit_line(e)).collect::<Vec<_>>(),
        "page_text": text,
        "about_to_click": button.map(|e| e.line(true)),
    });
    let mut q = Questions::default();
    // A contradiction check, not a completeness check: a commit is often one
    // step of a longer task, so values meant for later steps don't count.
    q.noul_criteria(
        "conflict",
        "Clicking `about_to_click` submits `form` as it is now (on a review page, what `page_text` summarizes). Does any field, or anything the summary shows, contradict `task`: a value, quantity, date or option different from what the task gives for it (same words, order and format), or an option turned on that the task did not ask for? Values the task gives for other forms or later steps don't count, and fields the task says nothing about may keep their defaults.",
        "At least one field or summary line disagrees with what the task says for it, or has something turned on that the task didn't ask for.",
        "Nothing contradicts the task (fields the task doesn't mention, and values meant for later steps, don't count).",
    );
    if !fields.is_empty() {
        let opts = fields
            .iter()
            .map(|e| (e.id(), Some(audit_line(e))))
            .chain(std::iter::once(("none".to_string(), Some("Every field agrees with the task.".to_string()))));
        q.choice("wrong", "Which field in `form` disagrees with `task` most clearly?", opts);
    }
    // On a review page (no fields) name the summary line instead.
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(120).collect();
    if fields.is_empty() && lines.len() >= 2 {
        let opts = lines
            .iter()
            .enumerate()
            .map(|(i, l)| (format!("l{i}"), Some(truncate(l, 160))))
            .chain(std::iter::once(("none".to_string(), Some("Nothing in the summary disagrees with the task.".to_string()))));
        q.choice("line", "Which line of `page_text` disagrees with `task` most clearly?", opts);
    }
    let a = jev.answer(&state, &q).await?;
    let p = 1.0 - a.yes("conflict").unwrap_or(0.0);
    let wrong = a.choice("wrong").and_then(|(c, p, _)| c.strip_prefix('e').and_then(|n| n.parse().ok()).map(|i| (i, p)));
    let line = a
        .choice("line")
        .and_then(|(c, _, _)| c.strip_prefix('l').and_then(|n| n.parse::<usize>().ok()))
        .and_then(|i| lines.get(i).map(|l| truncate(l, 160)));
    Ok(Some(Audit { p, wrong, line, answers: Some(a) }))
}

/// A dialog is open where a run would otherwise end: which of its buttons
/// does the task call for? Returns (element, p), or None for "none of them".
pub async fn dialog_choice<O: Oracle>(snap: &Snapshot, task: &str, jev: &O) -> Result<Option<(usize, f64, Answers)>> {
    let btns: Vec<&El> = snap
        .els
        .iter()
        .filter(|e| e.kind() == Kind::Click && !e.has_flag("covered") && !e.has_flag("disabled") && !e.latent())
        .take(24)
        .collect();
    if btns.is_empty() {
        return Ok(None);
    }
    let mut text = String::new();
    for l in snap.text_lines(|_| true) {
        if text.len() + l.len() > 1500 {
            break;
        }
        text.push_str(&l);
        text.push('\n');
    }
    let state = json!({"task": task, "dialog_and_page_text": text, "buttons": btns.iter().map(|e| e.line(true)).collect::<Vec<_>>()});
    let mut q = Questions::default();
    let opts = btns
        .iter()
        .map(|e| (e.id(), Some(e.desc(true))))
        .chain(std::iter::once(("none".to_string(), Some("None of these: the task does not call for any of them.".to_string()))));
    q.choice("button", "A dialog is open on the page. Which of `buttons` does `task` call for now?", opts);
    let a = jev.answer(&state, &q).await?;
    let pick = a.choice("button").and_then(|(c, p, _)| c.strip_prefix('e').and_then(|n| n.parse::<usize>().ok()).map(|i| (i, p)));
    Ok(pick.map(|(i, p)| (i, p, a)))
}

pub fn reveals(plan: &Plan, snap: &Snapshot) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let targets = plan.fills.iter().map(|f| f.el).chain(plan.click);
    for t in targets {
        if let Some(rv) = el(snap, t).and_then(|e| e.rv) {
            if !out.iter().any(|(_, r)| *r == rv as usize) {
                out.push((t, rv as usize));
            }
        }
    }
    out
}

/// True if `e` is a field whose kind the plan writes to.
pub fn is_field(e: &El) -> bool {
    matches!(e.kind(), Kind::Text | Kind::Select | Kind::Check | Kind::Radio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::{Answer, Usage};
    use std::collections::HashMap;
    use std::time::Duration;

    /// Canned oracle: `f(question id, spec)` gives each answer.
    struct Canned<F: Fn(&str, &Value) -> Answer + Sync>(F);

    impl<F: Fn(&str, &Value) -> Answer + Sync> Oracle for Canned<F> {
        fn answer(&self, _state: &Value, q: &Questions) -> impl std::future::Future<Output = Result<Answers>> + Send {
            let answers: HashMap<String, Answer> = q.0.iter().map(|(id, spec)| (id.clone(), (self.0)(id, spec))).collect();
            std::future::ready(Ok(Answers { model: "canned".into(), answers, usage: Usage::default(), latency: Duration::ZERO }))
        }
    }

    fn choice(pick: &str, p: f64, spec: &Value) -> Answer {
        let mut probabilities: HashMap<String, f64> = spec["criteria"]
            .as_object()
            .map(|m| m.keys().map(|k| (k.clone(), 0.0)).collect())
            .unwrap_or_default();
        let n = probabilities.len().max(2) as f64;
        let rest = (1.0 - p) / (n - 1.0);
        for (k, v) in probabilities.iter_mut() {
            *v = if k == pick { p } else { rest };
        }
        Answer::Choice { choice: pick.into(), probabilities: probabilities.into_iter().map(|(key, value)| (key, value.try_into().unwrap())).collect(), confidence: ((n * p - 1.0) / (n - 1.0)).try_into().unwrap() }
    }

    /// Answers: click → `click` with p, `(no change)` for fields, done/impossible
    /// as given, every verify Noul → `verify`, final → several.
    fn oracle(click: &'static str, p: f64, verify: f64, impossible: f64) -> Canned<impl Fn(&str, &Value) -> Answer + Sync> {
        Canned(move |id: &str, spec: &Value| match id {
            "click" | "pick" => choice(click, p, spec),
            "done" => Answer::Noul { noul: 0.02.try_into().unwrap() },
            "impossible" => Answer::Noul { noul: impossible.try_into().unwrap() },
            "final" => choice("several", 0.9, spec),
            v if v.starts_with("verify_") => Answer::Noul { noul: verify.try_into().unwrap() },
            _ if spec["type"] == "choice" => choice("(no change)", 0.99, spec),
            _ => Answer::Noul { noul: crate::domain::Probability::ZERO },
        })
    }

    fn snap(els: Value, texts: Value) -> Snapshot {
        serde_json::from_value(json!({"docId": "d", "version": 1, "url": "http://x/", "title": "t", "els": els, "texts": texts})).unwrap()
    }

    fn knobs() -> Knobs {
        let mut k = Knobs::default();
        k.set("engine", "dvm").unwrap();
        k
    }

    fn run<O: Oracle>(s: &Snapshot, instr: &str, o: &O, k: &Knobs) -> Decision {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(decide(s, instr, None, &[], k, o)).unwrap()
    }

    #[test]
    fn license_blocks_unrequested_commit() {
        let s = snap(json!([{"i": 1, "r": "button", "n": "Add to cart"}, {"i": 2, "r": "button", "n": "Save to wishlist"}]), json!([]));
        let d = run(&s, "add the laptop to the cart", &oracle("e2", 0.99, 0.9, 0.0), &knobs());
        assert!(matches!(&d.outcome, Outcome::Escalate(w) if w.contains("not permitted")), "{:?}", d.outcome);
        let d = run(&s, "add the laptop to the cart", &oracle("e1", 0.99, 0.9, 0.0), &knobs());
        assert_eq!(d.outcome, Outcome::Commit);
    }

    #[test]
    fn verify_licenses_only_where_lexicon_is_silent() {
        // A paraphrase without the verb: nothing on the page is lexically
        // licensed, so a confident VERIFY licenses the checkout.
        let s = snap(json!([{"i": 1, "r": "button", "n": "Checkout securely"}, {"i": 2, "r": "link", "n": "Help"}]), json!([]));
        let d = run(&s, "send the Juniper bouquet to Hélène Marchetti", &oracle("e1", 0.99, 0.9, 0.0), &knobs());
        assert_eq!(d.outcome, Outcome::Commit, "{:?}", d.outcome);
        // A weak VERIFY doesn't.
        let d = run(&s, "send the Juniper bouquet to Hélène Marchetti", &oracle("e1", 0.99, 0.5, 0.0), &knobs());
        assert!(matches!(&d.outcome, Outcome::Escalate(w) if w.contains("not permitted")), "{:?}", d.outcome);
    }

    #[test]
    fn repeated_row_commit_needs_a_referent() {
        let s = snap(
            json!([
                {"i": 1, "r": "button", "n": "Message", "c": "Anthony Young", "rc": 10},
                {"i": 2, "r": "button", "n": "Message", "c": "Zelda Fitzgerald", "rc": 20}
            ]),
            json!([{"i": 3, "x": "Anthony Young", "rc": 10}, {"i": 4, "x": "Zelda Fitzgerald", "rc": 20}]),
        );
        let d = run(&s, "click Message and send it", &oracle("e1", 0.99, 0.9, 0.0), &knobs());
        assert!(matches!(&d.outcome, Outcome::Escalate(w) if w.contains("which row")), "{:?}", d.outcome);
        let d = run(&s, "message Zelda Fitzgerald", &oracle("e2", 0.99, 0.9, 0.0), &knobs());
        assert_eq!(d.outcome, Outcome::Commit);
    }

    #[test]
    fn impossible_gate_stops_before_acting() {
        let s = snap(json!([{"i": 1, "r": "button", "n": "Notify me"}, {"i": 2, "r": "button", "n": "Add to cart"}]), json!([]));
        let d = run(&s, "buy the lamp in Sage", &oracle("e2", 0.99, 0.9, 0.95), &knobs());
        assert!(matches!(&d.outcome, Outcome::Escalate(w) if w.starts_with("impossible")), "{:?}", d.outcome);
    }

    #[test]
    fn destructive_commit_needs_verify() {
        let s = snap(json!([{"i": 1, "r": "button", "n": "Delete", "c": "report.pdf"}]), json!([]));
        let mut k = knobs();
        let d = run(&s, "delete report.pdf", &oracle("e1", 0.99, 0.9, 0.0), &k);
        assert_eq!(d.outcome, Outcome::Commit);
        // A clear VERIFY "no" and no refine budget → escalate instead of deleting.
        k.r_max = 0;
        let d = run(&s, "delete report.pdf", &oracle("e1", 0.99, 0.1, 0.0), &k);
        assert!(matches!(d.outcome, Outcome::Escalate(_)), "{:?}", d.outcome);
    }

    /// Golden parity: in parity mode, round 1 is exactly the legacy question batch.
    #[test]
    fn parity_mode_matches_legacy_questions() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/decisions/scripted.jsonl");
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let mut k = Knobs::default();
        k.dvm_parity = true;
        let mut n = 0;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let c: crate::dataset::DecisionCase = serde_json::from_str(line).unwrap();
            let (s1, q1, _) = decide::build(&c.snap, &c.instr, &c.history, &k);
            let (s2, q2, _, v) = round1(&c.snap, &c.instr, Some("some goal"), &c.history, &k);
            assert_eq!(s1, s2, "state differs for {}", c.id);
            assert_eq!(q1.0, q2.0, "questions differ for {}", c.id);
            assert!(v.is_empty());
            n += 1;
        }
        assert!(n > 0);
    }
}
