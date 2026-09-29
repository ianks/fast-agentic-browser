//! Agent mode: fab receives the task itself. Jev decides every action
//! from t=0; an LLM is a coprocessor that compiles a data program when the
//! task needs one, and takes over (the goal toolset loop) only when the
//! engine can't finish. Plain action tasks make no LLM call.
//!
//! At t=0 three things start together:
//! - the engine runs the task as written, behind a gate: it may decide at
//!   once, but executes nothing until the task is classified, and no commit
//!   (R2/R3) until it is known that no data program is needed;
//! - one Jev request classifies the task (question? needs data first?);
//! - when that says so, the LLM compiles a program (collect/extract steps and
//!   an answer template), which replaces the speculative run.

use anyhow::Result;
use serde_json::{Value, json};
use std::time::Instant;
use tokio::sync::watch;
use fab_core::jev::{Jev, Questions};
use fab_core::{Gate, Session, Timings};

use crate::bench::StepLog;
use crate::planner::{self, Emitter, PlannerStats};

const COMPILE_SYSTEM: &str = r#"You turn a browser task into a program for a fast browser engine. The engine carries out instructions on the current website by itself (navigation, forms, dialogs, retries). You only plan what must be read or computed first, and write the reply.
Return JSON only: {"steps": [...], "answer": "..."}
- "steps": [] when the task only asks for changes; the engine then runs the task as written. Otherwise a list of:
  - instruction strings, e.g. "open the Billing page", "place the account {top} on hold with the reason \"Overdue invoices\"";
  - {"collect": "<what the rows are>", "where": "<conditions in words; omit for all rows>", "op": "count|sum|max|min|argmax|argmin|list", "of": "<column to sum or compare>", "by": "<column that names what you want back>", "as": "<name>"}: reads a table or list across all its pages and computes exactly. With "by", rows are grouped by that column and {name} is the winning group's name: "the account with the most overdue invoices" → {"collect": "invoices", "where": "status is Overdue", "op": "argmax", "by": "account", "as": "acct"} (count per account); "the laptop with the most RAM under $1,500" → {"collect": "laptops", "where": "price under $1,500", "op": "argmax", "of": "RAM", "by": "name", "as": "pick"};
  - {"extract": "<question answered by the current page>", "as": "<name>"}: reads one value.
  Later steps and the answer use {name}.
- "answer": the reply to the user, with {name} where computed values go, e.g. "Refunded total: {refunds}". Empty ("") when the task only asks for changes.
Keep every literal value from the task exactly. Never do arithmetic yourself."#;

/// What the task needs, from one Jev request.
struct Class {
    question: f64,
    program: f64,
}

async fn classify(jev: &Jev, task: &str, page: &str) -> Result<(Class, fab_core::jev::Answers)> {
    let mut q = Questions::default();
    q.noul_criteria(
        "question",
        "Does `task` ask for information to be reported back (a question to answer), as opposed to only asking for changes to be made on the website?",
        "The task asks a question or asks to find out, report, list or tell something.",
        "The task only asks for changes (fill in, submit, book, save, delete, enable...) and asks nothing back.",
    );
    q.noul_criteria(
        "program",
        "Before any change can be made, does `task` require finding, comparing, counting or adding up information across several items, rows or pages (e.g. 'the cheapest', 'the one with the most', 'the total of')?",
        "Which item to act on, or the answer, depends on comparing or aggregating several items first.",
        "The task names what to act on directly, or is a single lookup.",
    );
    let state = json!({"task": task, "page": fab_core::snapshot::truncate(page, 1200)});
    let a = jev.ask(&state, &q).await?;
    Ok((Class { question: a.yes("question").unwrap_or(0.5), program: a.yes("program").unwrap_or(0.5) }, a))
}

struct Compiled {
    steps: Vec<Value>,
    answer: String,
}

fn parse_compiled(text: &str) -> Option<Compiled> {
    let a = text.find('{')?;
    let b = text.rfind('}')?;
    let v: Value = serde_json::from_str(&text[a..=b]).ok()?;
    Some(Compiled {
        steps: v["steps"].as_array().cloned().unwrap_or_default(),
        answer: v["answer"].as_str().unwrap_or("").to_string(),
    })
}

/// Fills `{name}` holes from the program's results; None if a hole is unfilled.
fn fill(template: &str, results: &Value) -> Option<String> {
    let mut out = template.to_string();
    if let Some(m) = results.as_object() {
        for (k, v) in m {
            let val = match v {
                Value::String(s) => s.clone(),
                Value::Object(o) if o.contains_key("top") => match &o["top"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
                other => other.to_string(),
            };
            out = out.replace(&format!("{{{k}}}"), &val);
        }
    }
    (!out.contains('{') || !out.contains('}')).then_some(out)
}

/// The final reply written by code from the engine's result. "impossible"
/// ends only a single-part task: in a multi-part one, the other parts may
/// still be possible, so the LLM continues.
fn render(v: &Value, template: &str, task: &str) -> Option<String> {
    match v["status"].as_str().unwrap_or("") {
        // "done_unverified" (the engine's own final check disagrees) goes to the
        // LLM: on the blind held-out-2 suite all 4 such completions were wrong,
        // while on the tuned race suite only 2 correct ones end this way.
        "done" => {
            // A collect that matched nothing is more often a mis-bound
            // condition than a true zero: let the planner look.
            let empty_collect = v["actions"].as_array().into_iter().flatten().filter_map(Value::as_str).any(|a| a.starts_with("collected") && a.contains(" 0 matched"));
            if empty_collect {
                return None;
            }
            if !template.is_empty() {
                return fill(template, &v["results"]);
            }
            let actions: Vec<&str> = v["actions"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let mut s = format!("Done. {}", actions.join("; "));
            if let Some(r) = v["results"].as_object().filter(|m| !m.is_empty()) {
                s.push_str(&format!(" Results: {}", Value::Object(r.clone())));
            }
            Some(s)
        }
        "impossible" if fab_core::dvm::goal::clauses(task).len() <= 1 => {
            Some(format!("Not possible: {}", v["error"].as_str().unwrap_or("")))
        }
        _ => None,
    }
}

/// An engine result whose input may have executed without confirmation.
fn uncertain(v: &Value) -> bool {
    v["uncertain"] == json!(true) || v["status"] == "uncertain"
}

fn step_log(kind: &str, input: &str, v: &Value, ms: f64) -> StepLog {
    StepLog {
        kind: kind.into(),
        input: input.into(),
        ok: matches!(v["status"].as_str(), Some("done" | "done_unverified" | "impossible")),
        detail: Value::String(crate::trunc(&v.to_string(), 4000)),
        ms,
    }
}

/// Resumes a durable planner loop: the engine work before it and every call
/// it confirmed are already on record, so the conversation continues instead
/// of repeating them. The browser is put back where the loop left off.
async fn resume_loop(
    sess: &mut Session,
    task: &str,
    url: &str,
    page: &str,
    model: Option<&str>,
    ev: Option<&Emitter>,
    journal: Option<&dyn planner::CallJournal>,
    saved: &planner::Prior,
) -> Result<(PlannerStats, Vec<StepLog>, Timings)> {
    let emit = |kind: &'static str, text: String| { if let Some(e) = ev { e.emit(kind, text) } };
    let _stats = PlannerStats { arm: saved.arm.clone(), model: model.unwrap_or("none").to_string(), ..Default::default() };
    // A restarted daemon has a new page, and a session that was interrupted
    // in place may have none: open the address the loop was on. A GET is
    // safe to repeat, unlike the calls the loop already made.
    let here = sess.last_snapshot().map(|s| s.url.clone()).unwrap_or_default();
    if !saved.url.is_empty() && here != saved.url {
        emit("resume", format!("reopening {} (the browser is on {})", saved.url, if here.is_empty() { "no page" } else { &here }));
        sess.goto(&saved.url).await.map_err(|e| anyhow::anyhow!("cannot reopen the page the planner loop was on ({}): {e:#}", saved.url))?;
    }
    // The page as it is now, not as the history left it: the model decides
    // against what it can see, and the document it last saw may be gone.
    let page = sess.page_summary().await.unwrap_or_else(|_| page.to_string());
    let effort = std::env::var("FAB_FALLBACK_EFFORT").ok().filter(|e| !e.is_empty() && *e != "default");
    // The toolset the conversation was made with, not the current default.
    let ts = match saved.arm.as_str() {
        "goal" => planner::Toolset::Goal(sess),
        _ => planner::Toolset::Usebrowser(sess),
    };
    planner::drive_with(ts, task, url, &page, model.unwrap_or_default(), ev, None, effort.as_deref(), false, journal, Some(saved)).await
}

/// Runs `task` on the current page. `model` is the coprocessor LLM (None =
/// engine only, no LLM at all). `journal` (a durable call) journals each
/// call of the planner loop, which then ends the run.
pub async fn run(
    sess: &mut Session,
    task: &str,
    url: &str,
    page: &str,
    model: Option<&str>,
    ev: Option<&Emitter>,
    journal: Option<&dyn planner::CallJournal>,
) -> Result<(PlannerStats, Vec<StepLog>, Timings)> {
    let emit = |kind: &'static str, text: String| {
        if let Some(e) = ev {
            e.emit(kind, text);
        }
    };
    emit("start", String::new());
    let stats = PlannerStats { arm: "agent".into(), model: model.unwrap_or("none").to_string(), ..Default::default() };
    let llm = model.map(planner::llm_for).transpose()?;
    let jev = sess.decider.0.clone();
    sess.task = Some(task.to_string());
    // FAB_CLARIFY=1: uncertain engine decisions get one small-LLM clarification
    // (same model) before the planner takes over. Off by default: on held-out-2
    // it fired in 40/160 runs, the planner still took over in 32, and the 8 it
    // finished alone were 4/8 right (87.5% vs 88.1% overall, no speed gain).
    let saved_clarify = sess.k.clarify.clone();
    if let Some(m) = model {
        if sess.k.clarify.is_empty() && std::env::var("FAB_CLARIFY").as_deref() == Ok("1") {
            sess.k.clarify = m.to_string();
        }
    }
    let out = run_inner(sess, task, url, page, model, ev, emit, stats, llm, jev, journal).await;
    sess.task = None;
    sess.k.clarify = saved_clarify;
    // Engine-internal LLM calls count as LLM calls in the report.
    let count = |stats: &mut PlannerStats, t: &Timings| {
        stats.turns += t.llm_calls;
        stats.llm_ms += t.llm_ms;
        stats.cost += t.llm_cost;
    };
    match out {
        Ok((mut stats, logs, timings)) => {
            count(&mut stats, &timings);
            Ok((stats, logs, timings))
        }
        Err(e) => match e.downcast::<planner::AgentError>() {
            Ok(mut ae) => {
                let t = ae.timings.clone();
                count(&mut ae.stats, &t);
                Err(ae.into())
            }
            Err(e) => Err(e),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    sess: &mut Session,
    task: &str,
    url: &str,
    page: &str,
    model: Option<&str>,
    ev: Option<&Emitter>,
    emit: impl Fn(&'static str, String) + Copy,
    mut stats: PlannerStats,
    llm: Option<fab_core::llm::Llm>,
    jev: fab_core::jev::Jev,
    journal: Option<&dyn planner::CallJournal>,
) -> Result<(PlannerStats, Vec<StepLog>, Timings)> {
    let mut logs = Vec::new();
    let mut timings = Timings::default();
    // A durable run stopped inside its planner loop: the engine's work before
    // the loop and every call it confirmed are already confirmed, so nothing
    // here may run again. The conversation continues where it stopped.
    if let Some(saved) = journal.and_then(|j| j.resume()) {
        return resume_loop(sess, task, url, page, model, ev, journal, &saved).await;
    }
    // A site seen before: go straight to the page the task belongs on.
    let moved_page;
    let page: &str = if sess.shape_navigate(task, &mut timings).await.unwrap_or(false) {
        moved_page = sess.page_summary().await.unwrap_or_default();
        &moved_page
    } else {
        page
    };
    let (tx, rx) = watch::channel(Gate::Hold);
    sess.gate = Some(rx);
    let t_spec = Instant::now();
    let class_seen: std::sync::Mutex<Option<(f64, f64)>> = Default::default();
    let class_seen = &class_seen;
    // Tasks that need reading or computing go to the adaptive planner loop
    // (goal toolset); FAB_PROGRAM_PATH=compile uses a one-shot compile instead
    // (faster, measured less accurate on held-out tasks).
    let planner_path = std::env::var("FAB_PROGRAM_PATH").as_deref() != Ok("compile") && llm.is_some();
    let use_planner: std::sync::atomic::AtomicBool = Default::default();
    let use_planner = &use_planner;
    let commits0 = sess.commits;
    // Input an earlier step typed but did not submit lives only in this page:
    // reloading it (the fallback's clean start) would throw that work away.
    let unsaved0 = unsaved_input(sess).await;
    // Decides what the task needs while the engine is already deciding.
    let control = async {
        let mut llm_stats: Option<(f64, fab_core::llm::ChatUsage)> = None;
        let cls = classify(&jev, task, page).await;
        let (question, program, ans) = match &cls {
            Ok((c, a)) => (c.question, c.program, Some(a.clone())),
            Err(_) => (0.5, 0.5, None),
        };
        emit("jev", format!("task class: question p={question:.2} · needs data first p={program:.2}"));
        *class_seen.lock().unwrap() = Some((question, program));
        if question < 0.5 && program < 0.5 {
            let _ = tx.send(Gate::Release);
            return (None, ans, llm_stats, question >= 0.5);
        }
        let Some(llm) = &llm else {
            // Engine only: run the task as written.
            let _ = tx.send(Gate::Release);
            return (None, ans, llm_stats, question >= 0.5);
        };
        if planner_path {
            use_planner.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = tx.send(Gate::Abort);
            return (None, ans, llm_stats, question >= 0.5);
        }
        // A question's answer comes from the program, so the speculative run
        // stops; otherwise it may keep navigating while the program compiles.
        let _ = tx.send(if question >= 0.5 { Gate::Abort } else { Gate::ReadOnly });
        let body = json!({
            "messages": [
                {"role": "system", "content": COMPILE_SYSTEM},
                {"role": "user", "content": format!("Task: {task}\n\nThe browser is open at {url}. Current page:\n{page}")}
            ],
            "temperature": 0,
            // Same program, a third of the latency (measured: 2.4 s -> 1.0 s).
            "reasoning": {"effort": "low"},
        });
        let compiled = match llm.chat(body).await {
            Ok((msg, usage, dt)) => {
                llm_stats = Some((dt.as_secs_f64() * 1e3, usage));
                parse_compiled(msg["content"].as_str().unwrap_or(""))
            }
            Err(e) => {
                emit("error", format!("compile: {}", crate::trunc(&format!("{e:#}"), 160)));
                None
            }
        };
        let needs_program = compiled.as_ref().is_some_and(|c| !c.steps.is_empty()) || question >= 0.5;
        let _ = tx.send(if needs_program { Gate::Abort } else { Gate::Release });
        (compiled, ans, llm_stats, question >= 0.5)
    };
    let (v, (compiled, class_ans, llm_stats, is_question)) = tokio::join!(sess.do_goal(task, &[]), control);
    sess.gate = None;
    if let Some(a) = &class_ans {
        let mut t = Timings::default();
        t.decide_ms = a.latency.as_secs_f64() * 1e3;
        t.decide_calls = 1;
        t.input_tokens = a.usage.input_tokens;
        timings.merge(&t);
    }
    if let Some((ms, u)) = &llm_stats {
        stats.turns += 1;
        stats.llm_ms += ms;
        stats.cost += u.cost;
        stats.prompt_tokens += u.prompt_tokens;
        stats.completion_tokens += u.completion_tokens;
        emit(
            "llm",
            format!("compile {:.1}s → {} step(s)", ms / 1e3, compiled.as_ref().map(|c| c.steps.len()).unwrap_or(0)),
        );
    }
    let spec_ms = t_spec.elapsed().as_secs_f64() * 1e3;
    if let Ok(t) = serde_json::from_value::<Timings>(v["timings"].clone()) {
        timings.merge(&t);
    }
    let mut first = step_log("do", task, &v, spec_ms);
    if let Some((q, p)) = *class_seen.lock().unwrap() {
        first.input = json!({"goal": task, "class": {"question": q, "program": p}}).to_string();
    }
    logs.push(first);
    // A durable call's planner starts only from confirmed engine work: its
    // first call closes the whole-call effect (see `task_runtime`).
    let mut unconfirmed = uncertain(&v);
    let handoff = |unconfirmed: bool, stats: &PlannerStats, logs: &[StepLog], timings: &Timings| -> Option<Result<(PlannerStats, Vec<StepLog>, Timings)>> {
        (journal.is_some() && unconfirmed).then(|| {
            let msg = "the engine's actions may have taken effect but were not confirmed; stopped before the planner".to_string();
            emit("error", msg.clone());
            Err(planner::AgentError { stats: stats.clone(), logs: logs.to_vec(), timings: timings.clone(), msg }.into())
        })
    };

    // The program replaces the speculative run when there is one.
    // Default reasoning effort: low measured ~12 points less accurate on held-out.
    let effort_env = std::env::var("FAB_FALLBACK_EFFORT").unwrap_or_else(|_| "default".into());
    let effort = Some(effort_env.as_str()).filter(|e| !e.is_empty() && *e != "default");
    if use_planner.load(std::sync::atomic::Ordering::Relaxed) {
        emit("llm", "task needs reading or computing: planner loop".into());
        let model = model.unwrap_or_default();
        // Direct answers (no finish turn) measured worse: 77.5% vs 85.0% on
        // held-out, with wrong collect results returned unchecked. Giving the
        // planner the page's full text up front measured 77.3% vs 81.8% on
        // question tasks: not adopted.
        // The LLM works with the fine-grained toolset (it picks each element and
        // reads pages itself). Held-out, 80 runs: 91.2% at p50 9.3 s vs 83.8% /
        // 9.7 s with the goal toolset (`do`); questions alone 86.4% vs 81.8%.
        // FAB_PLANNER_TOOLSET=goal restores the goal toolset.
        let fine = std::env::var("FAB_PLANNER_TOOLSET").as_deref() != Ok("goal");
        let _ = is_question;
        if let Some(stop) = handoff(unconfirmed, &stats, &logs, &timings) { return stop; }
        let ts = if fine { planner::Toolset::Usebrowser(sess) } else { planner::Toolset::Goal(sess) };
        let out = planner::drive_with(ts, task, url, page, model, ev, None, effort, false, journal, None).await;
        return finish_planner(stats, logs, timings, out);
    }
    let mut result = v;
    let mut template = String::new();
    if let Some(c) = compiled {
        template = c.answer.clone();
        let mut steps = c.steps;
        if steps.is_empty() && is_question {
            steps = vec![json!({"extract": task, "as": "answer"})];
            if template.is_empty() {
                template = "{answer}".into();
            }
        }
        if !steps.is_empty() {
            let t = Instant::now();
            let input = json!({"goal": task, "steps": steps}).to_string();
            emit("call", format!("do  program of {} step(s)", steps.len()));
            let v2 = sess.do_goal(task, &steps).await;
            let ms = t.elapsed().as_secs_f64() * 1e3;
            if let Ok(t) = serde_json::from_value::<Timings>(v2["timings"].clone()) {
                timings.merge(&t);
            }
            logs.push(step_log("do", &input, &v2, ms));
            unconfirmed |= uncertain(&v2);
            result = v2;
        }
    }
    let status = result["status"].as_str().unwrap_or("").to_string();
    emit("result", format!("engine: {status}"));
    if std::env::var("FAB_DEBUG_FINAL_PAGE").is_ok() {
        let p = sess.page_summary().await.unwrap_or_default();
        eprintln!("---- final page after engine ({status}) for: {}\n{}\n----", crate::trunc(task, 80), crate::trunc(&p, 3500));
    }
    if let Some(answer) = render(&result, &template, task) {
        emit("finish", crate::trunc(&answer, 160));
        stats.answer = Some(answer);
        return Ok((stats, logs, timings));
    }

    // The engine couldn't finish: the LLM takes over from here, with the
    // goal toolset, starting from what was already done.
    let Some(model) = model else {
        let msg = format!("could not finish: {}", result["error"].as_str().unwrap_or(&status));
        return Err(planner::AgentError { stats, logs, timings, msg }.into());
    };
    if let Some(stop) = handoff(unconfirmed, &stats, &logs, &timings) { return stop; }
    // Nothing was committed: start the LLM from a clean page rather than the
    // engine's half-finished state (FAB_FALLBACK_RESET=0 keeps the state).
    let reset = sess.commits == commits0 && !unsaved0 && std::env::var("FAB_FALLBACK_RESET").as_deref() != Ok("0");
    let (prior, page_now) = if reset {
        emit("result", "no changes were committed: restarting from the start page".into());
        (None, sess.goto(url).await.unwrap_or_default())
    } else {
        (Some(crate::trunc(&result.to_string(), 3000)), sess.page_summary().await.unwrap_or_default())
    };
    // The fine-grained toolset (the LLM picks each element), measured better
    // than handing the goal back to `do` (see the planner path above);
    // FAB_FALLBACK=goal restores the goal toolset.
    let ts = match std::env::var("FAB_FALLBACK").as_deref() {
        Ok("goal") => planner::Toolset::Goal(sess),
        _ => planner::Toolset::Usebrowser(sess),
    };
    let out = planner::drive_with(ts, task, url, &page_now, model, ev, prior.as_deref(), effort, false, journal, None).await;
    finish_planner(stats, logs, timings, out)
}

/// Whether the page holds form input not yet submitted: a field whose value
/// differs from the one the page loaded with.
async fn unsaved_input(sess: &mut Session) -> bool {
    const JS: &str = "[...document.querySelectorAll('input,textarea,select')].some(e => e.type === 'checkbox' || e.type === 'radio' ? e.checked !== e.defaultChecked : e.tagName === 'SELECT' ? [...e.options].some(o => o.selected !== o.defaultSelected) : e.type !== 'hidden' && e.type !== 'submit' && e.value !== e.defaultValue)";
    sess.browser.eval(JS).await.ok().and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Adds a planner loop's stats, logs and timings to agent mode's own.
fn finish_planner(
    mut stats: PlannerStats,
    mut logs: Vec<StepLog>,
    mut timings: Timings,
    out: Result<(PlannerStats, Vec<StepLog>, Timings)>,
) -> Result<(PlannerStats, Vec<StepLog>, Timings)> {
    let merge = |stats: &mut PlannerStats, s: &PlannerStats| {
        stats.turns += s.turns;
        stats.llm_ms += s.llm_ms;
        stats.tool_ms += s.tool_ms;
        stats.tool_calls += s.tool_calls;
        stats.cost += s.cost;
        stats.prompt_tokens += s.prompt_tokens;
        stats.completion_tokens += s.completion_tokens;
        stats.nudges += s.nudges;
        stats.answer = s.answer.clone();
    };
    match out {
        Ok((s, l, t)) => {
            merge(&mut stats, &s);
            logs.extend(l);
            timings.merge(&t);
            Ok((stats, logs, timings))
        }
        Err(e) => {
            if let Some(ae) = e.downcast_ref::<planner::AgentError>() {
                merge(&mut stats, &ae.stats);
                logs.extend(ae.logs.clone());
                timings.merge(&ae.timings);
                return Err(planner::AgentError { stats, logs, timings, msg: ae.msg.clone() }.into());
            }
            Err(e)
        }
    }
}
