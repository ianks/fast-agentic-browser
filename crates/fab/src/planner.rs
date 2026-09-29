//! The real benchmark: an LLM agent completes a task from a natural-language
//! goal. Only the toolset differs between arms:
//! - control:    chrome-devtools-mcp (snapshot + uid-based click/fill/...)
//! - experiment: fab (act / run / extract / read, decided by Jev)
//! Same model, same turn budget, same initial observation, same checks.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Instant;
use fab_core::llm::Llm;
use fab_core::{Session, Timings};

use crate::bench::StepLog;
use crate::mcp_client::McpClient;
use crate::tools;

pub const MAX_TURNS: u32 = 30;
const MAX_NUDGES: u32 = 2;
const NUDGE: &str = "Continue the task: call a tool to act on the page, or call `finish` with your final answer if the goal is complete or impossible.";

#[derive(Debug, Clone, Default, Serialize)]
pub struct PlannerStats {
    pub arm: String,
    pub model: String,
    pub turns: u32,
    pub llm_ms: f64,
    pub tool_ms: f64,
    pub tool_calls: u32,
    pub cost: f64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Text-only turns answered with a "call a tool or finish" nudge.
    pub nudges: u32,
    pub answer: Option<String>,
}

/// A live event from an agent run (for watching runs as they happen).
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub arm: usize,
    /// Milliseconds since the agent started working on the goal.
    pub ms: f64,
    /// "llm" (model replied), "call" (tool starts), "result" (tool finished), "finish", "error".
    pub kind: &'static str,
    pub text: String,
}

#[derive(Clone)]
pub struct Emitter {
    pub arm: usize,
    pub tx: tokio::sync::mpsc::UnboundedSender<Event>,
    pub t0: Instant,
}

impl Emitter {
    pub fn emit(&self, kind: &'static str, text: String) {
        let _ = self.tx.send(Event { arm: self.arm, ms: self.t0.elapsed().as_secs_f64() * 1e3, kind, text });
    }
}

fn brief(args: &Value) -> String {
    let s = match args {
        Value::Object(m) => m
            .iter()
            .filter(|(k, _)| k.as_str() != "pageId" && k.as_str() != "includeSnapshot")
            .map(|(k, v)| format!("{k}={}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    };
    crate::trunc(&s, 110)
}

pub enum Toolset<'a> {
    Usebrowser(&'a mut Session),
    /// fab with the goal-level `do` tool first.
    Goal(&'a mut Session),
    Cdt(&'a McpClient),
}

const FINISH_DESC: &str = "Call when the goal is complete, or when it cannot be completed. `answer` holds the requested information, a short confirmation, or why the task is impossible.";

pub const SYSTEM_EXPERIMENT: &str = "You are a browser agent. You control a web browser through a very fast tool and must complete the user's goal.
- `act` takes a plain-English instruction. Precise commands run instantly: `click e<id>`, `type \"<text>\" into e<id>`, `select \"<option>\" from e<id>`, `check e<id>`, `press enter`, or several joined with commas. Element ids (e12) come from the page listing. You can also give a high-level instruction (\"log in as \\\"<username>\\\" with password \\\"<password>\\\"\", \"add the <product> in size <size> to the cart\") and it works out the clicks. Always put literal text to type in double quotes.
- `run` takes a list of such instructions and executes them in order in one call. Prefer it whenever you can predict the next steps.
- `read` returns the page's text (tables as rows) for reading data. `extract` answers one question from the page verbatim.
- Every act/run result includes the resulting page. Check it: if something failed or the page shows an error, adapt.
- When the goal is achieved (or is impossible), call `finish`. Never claim success you did not observe.";

/// Turns are the cost (about 1.3 s each, whatever their size): do everything
/// already visible in one call.
pub const BATCH_HINT: &str = "- Each call costs a full turn, so batch: do everything you can already see to do in ONE call. Fill all the fields of a form in a single `act` (commands joined with commas) or `run`, and end it with the click that saves or moves to the next step when nothing else is needed first. Stop early only when a step depends on something not yet on the page.";

pub const SYSTEM_GOAL: &str = "You are a browser agent. A fast browser engine carries out goals for you.
- Call `do` with the user's goal, keeping every literal value exactly. The engine handles navigation, forms, dialogs, retries and verification, and reports done / incomplete / impossible / failed with evidence and the page.
- Questions that need counting, summing, comparing or finding the top item across rows or pages: use `collect` (or a `collect` step inside `do`). It is exact; never count or add numbers yourself.
- If `do` reports a problem, call `do` again with `steps`: short instructions, or precise commands using the element ids from the returned page (e.g. click e<id>; type \"<text>\" into e<id>).
- If `do` reports impossible, don't try alternatives the user didn't ask for; explain why in `finish`.
- If `do` reports done_unverified, the actions were carried out but the page shows no confirmation: don't repeat them; finish.
- If `do` reports steps_done, every step you gave worked; `unmet` lists the parts of the goal still open. Do only those next, never the finished steps again.
- Call `finish` with the answer written from the returned results and evidence. Never claim success you did not observe.";

pub const SYSTEM_CONTROL: &str = "You are a browser agent. You control a web browser through Chrome DevTools tools and must complete the user's goal.
- Use take_snapshot to see the page; elements have uids you pass to click/fill/fill_form etc.
- Check results: if something failed or the page shows an error, adapt.
- When the goal is achieved (or is impossible), call `finish`. Never claim success you did not observe.";

impl Toolset<'_> {
    fn arm(&self) -> &'static str {
        match self {
            Toolset::Usebrowser(_) => "experiment",
            Toolset::Goal(_) => "goal",
            Toolset::Cdt(_) => "control",
        }
    }

    /// The page the loop is on, for its checkpoint: page, document and
    /// address. Empty for the control arm, which is never journaled.
    async fn identity(&mut self) -> (String, String, String) {
        match self {
            Toolset::Usebrowser(s) | Toolset::Goal(s) => {
                let page = s.browser.page_id().await;
                let snap = s.last_snapshot();
                (page, snap.map(|x| x.doc_id.clone()).unwrap_or_default(), snap.map(|x| x.url.clone()).unwrap_or_default())
            }
            Toolset::Cdt(_) => (String::new(), String::new(), String::new()),
        }
    }

    fn defs(&self) -> Vec<Value> {
        match self {
            Toolset::Usebrowser(_) => tools::definitions().into_iter().filter(|t| t["name"] != "goto").collect(),
            // The VM carries the goal: `do` (whose steps may be precise
            // commands for corrections) plus the data tools; no fine-grained
            // act/run/observe, so the planner delegates instead of driving.
            Toolset::Goal(_) => std::iter::once(tools::do_definition())
                .chain(tools::definitions().into_iter().filter(|t| matches!(t["name"].as_str(), Some("collect" | "read" | "extract"))))
                .collect(),
            Toolset::Cdt(c) => c.tools.clone(),
        }
    }

    async fn call(&mut self, name: &str, args: &Value) -> (String, tools::Status, Option<Timings>) {
        match self {
            Toolset::Usebrowser(s) | Toolset::Goal(s) => tools::call(s, name, args).await,
            Toolset::Cdt(c) => {
                let (t, err) = c.call(name, args).await;
                (t, if err { tools::Status::Failed } else { tools::Status::Ok }, None)
            }
        }
    }
}

/// Write-ahead journal for a durable run's planner calls: `before` commits
/// the call's intent (an Err dispatches nothing), `after` its confirmed
/// result. An `Uncertain` call never reaches `after`: it stays unconfirmed.
pub trait CallJournal: Send + Sync {
    fn before(&self, name: &str, args: &Value) -> Result<()>;
    /// The call is confirmed; `prior` is the conversation it produced, saved
    /// with the receipt in the same commit.
    fn after(&self, name: &str, text: &str, status: tools::Status, prior: &Prior) -> Result<()>;
    /// The conversation a resumed loop continues from, when one was saved.
    fn resume(&self) -> Option<Prior> { None }
}

/// A planner loop's own state, kept with a task's checkpoint: the
/// conversation so far, the calls already confirmed, and the page it was
/// on. A resumed loop continues this instead of repeating the work
/// (INTENT I07, I08).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prior {
    /// The conversation, capped: the goal, then the newest whole turns.
    pub messages: Vec<Value>,
    /// Planner calls already confirmed, not to be repeated.
    #[serde(default)]
    pub calls: u64,
    /// LLM turns the loop has spent; the turn budget continues from here.
    #[serde(default)]
    pub turns: u32,
    /// The page and document the conversation describes, and its address.
    #[serde(default)]
    pub page: String,
    #[serde(default)]
    pub document: String,
    /// Where the browser was: a resumed run opens this again, so a fresh
    /// page is the page the conversation was having.
    #[serde(default)]
    pub url: String,
    /// Which toolset produced the conversation: resuming with another one
    /// would offer the model tools its history never called.
    #[serde(default)]
    pub arm: String,
}

/// How much of a conversation a checkpoint keeps. The goal always survives;
/// older turns fall off so the checkpoint does not grow with the loop.
const TRANSCRIPT_BYTES: usize = 32 * 1024;
/// A tool result kept whole; longer ones are cut (the shape stays).
const TOOL_BYTES: usize = 4000;

/// The conversation a checkpoint keeps: the first two messages (the system
/// prompt and the goal with the page it was given), then the newest whole
/// turns that fit in [`TRANSCRIPT_BYTES`].
///
/// The cut falls on a turn boundary: a kept slice that starts with a tool
/// message would leave a result without the model message it answers, and
/// providers reject that. Tool results are also shortened, so one huge page
/// cannot push the loop's own history out.
pub fn capped(messages: &[Value]) -> Vec<Value> {
    // Shorten first, so the budget is spent on whole turns rather than on
    // one page's worth of text.
    let mut all: Vec<Value> = messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            if m["role"] == json!("tool") && m["content"].as_str().is_some_and(|c| c.len() > TOOL_BYTES) {
                m["content"] = json!(crate::trunc(m["content"].as_str().unwrap_or_default(), TOOL_BYTES));
            }
            m
        })
        .collect();
    let head: usize = 2.min(all.len());
    let sizes: Vec<usize> = all.iter().map(|m| m.to_string().len()).collect();
    let mut total: usize = sizes.iter().sum();
    let mut end = all.len();
    while end > head && total > TRANSCRIPT_BYTES {
        end -= 1;
        total -= sizes[end];
    }
    // A turn that was cut in half: drop the model message whose results are
    // no longer all here, so no tool result is left unanswered.
    while end > head && end < all.len() && all[end]["role"] == json!("tool") {
        end -= 1;
    }
    all.truncate(end);
    if end < messages.len() {
        all.push(json!({"role": "user", "content": "(earlier turns of this task were left out to save space; their work is already done)"}));
    }
    all
}

/// One client per model for the whole process, so its connection pool (and
/// TLS session) is shared by every run instead of re-handshaking per task.
pub fn llm_for(model: &str) -> Result<Llm> {
    static CLIENTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Llm>>> = std::sync::OnceLock::new();
    let mut m = CLIENTS.get_or_init(Default::default).lock().unwrap();
    if let Some(l) = m.get(model) {
        return Ok(l.clone());
    }
    let l = Llm::from_env(model)?;
    m.insert(model.to_string(), l.clone());
    Ok(l)
}

/// Runs the agent loop. `page` is the initial observation (taken outside the timer).
pub async fn drive(
    ts: Toolset<'_>,
    goal: &str,
    url: &str,
    page: &str,
    model: &str,
    ev: Option<&Emitter>,
) -> Result<(PlannerStats, Vec<StepLog>, Timings)> {
    // FAB_PLANNER_EFFORT (e.g. "low") sets the reasoning effort of every planner
    // loop, control included, so baselines can be compared at equal settings.
    let effort = std::env::var("FAB_PLANNER_EFFORT").ok().filter(|e| !e.is_empty());
    drive_with(ts, goal, url, page, model, ev, None, effort.as_deref(), false, None, None).await
}

/// `drive`, where `prior` describes work already done on the page (agent mode's
/// automatic attempt), so the planner continues instead of starting over.
/// `journal` records each tool call of a durable run around its dispatch;
/// `saved` is the conversation a resumed loop continues from (see [`Prior`]).
#[allow(clippy::too_many_arguments)]
pub async fn drive_with(
    mut ts: Toolset<'_>,
    goal: &str,
    url: &str,
    page: &str,
    model: &str,
    ev: Option<&Emitter>,
    prior: Option<&str>,
    effort: Option<&str>,
    direct: bool,
    journal: Option<&dyn CallJournal>,
    saved: Option<&Prior>,
) -> Result<(PlannerStats, Vec<StepLog>, Timings)> {
    let emit = |kind: &'static str, text: String| {
        if let Some(e) = ev {
            e.emit(kind, text);
        }
    };
    emit("start", String::new());
    let llm = llm_for(model)?;
    let mut stats = PlannerStats { arm: ts.arm().into(), model: model.to_string(), ..Default::default() };
    let mut logs = Vec::new();
    let mut timings = Timings::default();
    let mut defs = ts.defs();
    defs.push(json!({
        "name": "finish",
        "description": FINISH_DESC,
        "inputSchema": {"type": "object", "properties": {"answer": {"type": "string"}}, "required": ["answer"]}
    }));
    let tool_specs: Vec<Value> = defs
        .iter()
        .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["inputSchema"]}}))
        .collect();
    let batch_hint = std::env::var("FAB_BATCH_HINT").as_deref() == Ok("1");
    let experiment_prompt = if batch_hint { format!("{SYSTEM_EXPERIMENT}\n{BATCH_HINT}") } else { SYSTEM_EXPERIMENT.to_string() };
    let system: &str = match ts {
        Toolset::Usebrowser(_) => &experiment_prompt,
        Toolset::Goal(_) => SYSTEM_GOAL,
        Toolset::Cdt(_) => SYSTEM_CONTROL,
    };
    // A resumed loop continues the conversation its checkpoint holds: the
    // goal, the page and every confirmed call are already in it, so the
    // model continues the work instead of repeating it.
    let (mut messages, resumed) = match saved.filter(|p| !p.messages.is_empty()) {
        Some(p) => {
            if !p.arm.is_empty() && p.arm != ts.arm() {
                let msg = format!("the saved planner loop used the {} toolset, not the {} one", p.arm, ts.arm());
                emit("error", msg.clone());
                return Err(AgentError { stats, logs, timings, msg }.into());
            }
            stats.turns = p.turns;
            emit("resume", format!("continuing the planner loop after {} confirmed call(s), {} turn(s)", p.calls, p.turns));
            let mut messages = p.messages.clone();
            // The page as it is now, not as the history left it: a restarted
            // daemon has a new document, and the model must decide against
            // what it can see, never against a page that is gone.
            messages.push(json!({"role": "user", "content": format!("The page is now:\n{page}")}));
            (messages, true)
        }
        None => (
            vec![
                json!({"role": "system", "content": system}),
                json!({"role": "user", "content": match prior {
                    None => format!("Goal: {goal}\n\nThe browser is open at {url}. Current page:\n{page}"),
                    Some(p) => format!("Goal: {goal}\n\nThe browser is open at {url}. An automatic attempt already ran:\n{p}\n\nCurrent page:\n{page}"),
                }}),
            ],
            false,
        ),
    };
    let _ = resumed;
    // The budget is for the whole loop, not for one attempt at it.
    for _ in 0..MAX_TURNS.saturating_sub(stats.turns) {
        let mut body = json!({"messages": messages, "tools": tool_specs, "temperature": 0});
        // "mixed": the first turn (the plan) at the model's default effort,
        // later turns (reacting to tool results) at low effort.
        let turn_effort = match effort {
            Some("mixed") => (stats.turns > 0).then_some("low"),
            other => other,
        };
        if let Some(e) = turn_effort {
            body["reasoning"] = json!({"effort": e});
        }
        let (msg, usage, dt) = match llm.chat(body).await {
            Ok(x) => x,
            Err(e) => {
                emit("error", crate::trunc(&format!("{e:#}"), 200));
                return Err(e);
            }
        };
        stats.turns += 1;
        stats.llm_ms += dt.as_secs_f64() * 1e3;
        stats.cost += usage.cost;
        stats.prompt_tokens += usage.prompt_tokens;
        stats.completion_tokens += usage.completion_tokens;
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        emit("llm", format!("LLM {:.1}s → {}", dt.as_secs_f64(), if calls.is_empty() { "text only".to_string() } else { format!("{} call(s)", calls.len()) }));
        messages.push(msg.clone());
        if calls.is_empty() {
            // A text-only turn is not a finish: models often narrate ("I will now…")
            // mid-task. Nudge up to twice before taking the text as the answer.
            if stats.nudges < MAX_NUDGES {
                emit("llm", format!("(text only, nudged) {}", crate::trunc(msg["content"].as_str().unwrap_or(""), 100)));
                stats.nudges += 1;
                messages.push(json!({"role": "user", "content": NUDGE}));
                continue;
            }
            stats.answer = msg["content"].as_str().map(str::to_string);
            emit("finish", crate::trunc(stats.answer.as_deref().unwrap_or(""), 160));
            return Ok((stats, logs, timings));
        }
        for c in calls {
            let name = c["function"]["name"].as_str().unwrap_or_default().to_string();
            let args: Value = serde_json::from_str(c["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
            if name == "finish" {
                stats.answer = args["answer"].as_str().map(str::to_string);
                emit("finish", crate::trunc(stats.answer.as_deref().unwrap_or(""), 160));
                return Ok((stats, logs, timings));
            }
            emit("call", format!("{name}  {}", brief(&args)));
            if let Some(Err(e)) = journal.map(|j| j.before(&name, &args)) {
                let msg = format!("{name} was not started: its intent could not be journaled ({e:#})");
                emit("error", msg.clone());
                return Err(AgentError { stats, logs, timings, msg }.into());
            }
            let t = Instant::now();
            let (text, status, tm) = ts.call(&name, &args).await;
            // The result joins the conversation before the receipt is
            // committed, so a saved transcript is always whole: a resumed
            // loop never sees a call without its result, and never a result
            // for a call that was not confirmed.
            messages.push(json!({"role": "tool", "tool_call_id": c["id"], "content": text}));
            let checkpoint = match journal {
                None => None,
                Some(_) => {
                    let (id_page, id_document, id_url) = ts.identity().await;
                    Some(Prior {
                        messages: capped(&messages),
                        calls: 0,
                        turns: stats.turns,
                        page: id_page,
                        document: id_document,
                        url: id_url,
                        arm: ts.arm().to_string(),
                    })
                }
            };
            // A result that cannot be journaled leaves the call unconfirmed.
            let unjournaled = journal
                .zip(checkpoint.as_ref())
                .filter(|_| status != tools::Status::Uncertain)
                .and_then(|(j, p)| j.after(&name, &text, status, p).err());
            let ok = status == tools::Status::Ok;
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            emit("result", format!("{} {name} {:.0} ms  {}", if ok { "✓" } else { "✗" }, ms, crate::trunc(first, 120)));
            stats.tool_ms += ms;
            stats.tool_calls += 1;
            if let Some(tm) = tm {
                timings.merge(&tm);
            }
            logs.push(StepLog {
                kind: name.clone(),
                input: args.to_string(),
                ok,
                detail: Value::String(crate::trunc(&text, 4000)),
                ms,
            });
            if let Some(e) = unjournaled {
                stats.answer = None;
                let msg = format!("{name} ran but its result could not be journaled; stopped unconfirmed ({e:#})");
                emit("error", msg.clone());
                return Err(AgentError { stats, logs, timings, msg }.into());
            }
            // Agent mode: a finished `do` whose answer template is filled is
            // the reply; no separate `finish` turn.
            if direct && ok {
                let first: Value = serde_json::from_str(text.lines().next().unwrap_or("")).unwrap_or(Value::Null);
                if let Some(a) = first["final_answer"].as_str() {
                    stats.answer = Some(a.to_string());
                    emit("finish", crate::trunc(a, 160));
                    return Ok((stats, logs, timings));
                }
            }
            // The model would see a failure and try again: an action whose
            // input may have executed ends the run instead, unconfirmed.
            if status == tools::Status::Uncertain {
                stats.answer = None;
                let msg = format!("{name} may have taken effect but was not confirmed; stopped rather than repeat it ({})", crate::trunc(first, 200));
                emit("error", msg.clone());
                return Err(AgentError { stats, logs, timings, msg }.into());
            }
        }
    }
    stats.answer = None;
    emit("error", format!("turn budget ({MAX_TURNS}) exhausted"));
    Err(AgentError { stats, logs, timings, msg: format!("agent turn budget ({MAX_TURNS}) exhausted") }.into())
}

/// An agent run that ended without finishing; keeps what happened for diagnosis.
#[derive(Debug)]
pub struct AgentError {
    pub stats: PlannerStats,
    pub logs: Vec<StepLog>,
    pub timings: Timings,
    pub msg: String,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for AgentError {}
