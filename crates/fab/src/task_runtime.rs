//! Durable admission and conservative recovery for application tool calls.
//! A lost response never grants permission to repeat browser work.
use std::{collections::HashMap, sync::{Arc, Mutex, OnceLock}, time::Duration};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use crate::{api, events::{ErrorCode, Failure}, pool, task_store::{Checkpoint, EffectKind, PendingEffect, Store, TaskGuard, TaskId, TaskOutcome, TaskRecord, TaskState}, tools};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool { Do, Run, Step }

impl Tool {
    fn parse(name: &str) -> Result<Self> {
        Ok(match name { "do" | "task" => Self::Do, "run" => Self::Run, "step" => Self::Step, _ => anyhow::bail!("unknown execution tool {name}") })
    }
    fn name(&self) -> &'static str { match self { Self::Do => "do", Self::Run => "run", Self::Step => "step" } }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request { tool: Tool, args: Value }

/// Validate scripts before leasing or navigating a page.
pub fn validate(tool: &str, args: &Value) -> Result<(), Failure> {
    let invalid = |message: String| Failure::new(ErrorCode::InvalidArgs, message);
    if !args.is_object() {
        return Err(invalid("arguments must be an object".into()));
    }
    api::shape_of(args)?;
    if let Some(id) = args.get("request_id") {
        if !id.as_str().is_some_and(|id| !id.trim().is_empty()) {
            return Err(invalid("request_id must be a nonempty string".into()));
        }
    }
    match Tool::parse(tool).map_err(|e| invalid(format!("{e:#}")))? {
        Tool::Run => {
            let script = args["script"].as_str().ok_or_else(|| invalid("script is required".into()))?;
            fab_core::script::parse(script).map_err(|e| Failure::new(ErrorCode::InvalidProgram, format!("script error, {e} (nothing was run)")))?;
        }
        Tool::Do | Tool::Step => {
            if !args["step"].as_str().or_else(|| args["task"].as_str()).is_some_and(|s| !s.trim().is_empty()) {
                return Err(invalid("step is required".into()));
            }
        }
    }
    Ok(())
}

pub fn store() -> Result<Store> { Store::open(Store::default_path()?) }

#[derive(Clone)]
enum Stop { Cancel(String), Interrupt(String) }
type Cancellation = tokio::sync::watch::Sender<Option<Stop>>;
fn active() -> &'static Mutex<HashMap<TaskId, Cancellation>> {
    static ACTIVE: OnceLock<Mutex<HashMap<TaskId, Cancellation>>> = OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}
struct Registration(TaskId);
impl Drop for Registration {
    fn drop(&mut self) { active().lock().unwrap().remove(&self.0); }
}

/// Signal the owning runner first, then apply journal-only controls once it
/// has released its task lock. The runner records terminal uncertainty.
pub async fn control(scope: &str, command: crate::task_commands::TaskCommand) -> Result<crate::task_commands::TaskCommandOutcome> {
    let store = store()?;
    if let crate::task_commands::TaskCommand::Cancel { task, reason } = &command {
        ensure!(!reason.trim().is_empty(), crate::events::invalid("cancellation requires a reason"));
        ensure!(store.get(task)?.session == scope, crate::events::invalid("task does not belong to this session"));
        let sender = active().lock().unwrap().get(task).cloned();
        if let Some(sender) = sender {
            sender.send(Some(Stop::Cancel(reason.clone()))).context("runner stopped during cancellation")?;
            // Until the runner finishes the task, or stops without finishing it
            // (it paused before seeing the signal): then the cancel applies here.
            let finished = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let record = store.get(task)?;
                    if matches!(record.state, TaskState::Finished { .. }) { return Ok::<_, anyhow::Error>(true); }
                    if !active().lock().unwrap().contains_key(task) { return Ok(false); }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.context("cancellation requested; inspect the task for its final state")??;
            if !finished {
                return crate::task_commands::execute_in_session(&store, scope, command);
            }
            // The runner may have finished its work before the signal arrived.
            let record = store.get(task)?;
            let cancelled = matches!(&record.state, TaskState::Finished { outcome: TaskOutcome::Cancelled(_) });
            return Ok(if cancelled {
                crate::task_commands::TaskCommandOutcome::Cancelled(record)
            } else {
                crate::task_commands::TaskCommandOutcome::AlreadyFinished(record)
            });
        }
    }
    crate::task_commands::execute_in_session(&store, scope, command)
}

pub fn interrupt_scope(scope: &str) -> Result<()> {
    let store = store()?;
    for task in store.list(Some(scope))? {
        if let Some(sender) = active().lock().unwrap().get(&task.id).cloned() {
            let _ = sender.send(Some(Stop::Interrupt("session closed".into())));
        }
    }
    Ok(())
}

pub struct ResultRecord { pub task: TaskId, pub done: pool::Done }

/// The effect a durable `do`/`step` has in flight: the whole call until its
/// planner's first tool call, then each planner call in turn.
struct Flight { pending: Option<PendingEffect>, whole: bool, calls: u64 }

/// Journals each tool call of the planner loop that ends a `do`/`step`.
/// The planner starts only once the work before it returned confirmed
/// (autopilot stops on an unconfirmed engine result, and a program's own
/// planners are not journaled), so its first call closes the whole-call
/// effect. Checkpoints count confirmed calls and carry the conversation
/// with them, so a task stopped inside its loop continues from the last
/// confirmed call instead of pausing (see `planner_prior`).
struct PlannerJournal {
    store: Store,
    guard: Arc<TaskGuard>,
    flight: Arc<Mutex<Flight>>,
    /// What the task was doing when it was interrupted, if it was.
    saved: Option<crate::planner::Prior>,
    /// Where `fixture:` pages are served in this daemon: the loop's address
    /// is kept by name, since the server's port does not outlive it.
    fixtures: crate::api::Fixtures,
}

impl crate::planner::CallJournal for PlannerJournal {
    fn before(&self, name: &str, args: &Value) -> Result<()> {
        let mut flight = self.flight.lock().unwrap();
        if flight.whole {
            let whole = flight.pending.clone().context("the whole-call effect is not in flight")?;
            self.store.complete_effect(&self.guard, &whole, &json!({"before_planner": "returned"}), Checkpoint::new(json!({"planner_calls": 0})), None)?;
            (flight.pending, flight.whole) = (None, false);
        }
        ensure!(flight.pending.is_none(), "a planner call is already in flight");
        let mut request = json!({"tool": name, "args": args});
        fab_core::secrets::redact::value(&mut request);
        let prepared = self.store.begin_effect(&self.guard, self.store.get(self.guard.task_id())?.revision, EffectKind::PlannerCall, &request)?;
        flight.pending = Some(self.store.mark_dispatched(&self.guard, &prepared)?);
        Ok(())
    }
    fn after(&self, name: &str, text: &str, status: tools::Status, prior: &crate::planner::Prior) -> Result<()> {
        let status = match status { tools::Status::Ok => "ok", tools::Status::Failed => "failed", tools::Status::Uncertain => anyhow::bail!("an uncertain call stays unconfirmed") };
        let mut flight = self.flight.lock().unwrap();
        let pending = flight.pending.clone().filter(|_| !flight.whole).context("no planner call is in flight")?;
        let mut receipt = json!({"tool": name, "text": crate::trunc(text, RECEIPT_TEXT), "status": status});
        fab_core::secrets::redact::value(&mut receipt);
        let calls = flight.calls + 1;
        // The conversation is committed with the receipt, never beside it:
        // a checkpoint that counted a call the transcript does not contain
        // would make a resumed loop repeat it.
        let mut saved = prior.clone();
        saved.calls = calls;
        if let Some(name) = self.fixtures.get().and_then(|srv| srv.portable(&saved.url)) {
            saved.url = name;
        }
        let mut loop_state = serde_json::to_value(&saved)?;
        fab_core::secrets::redact::value(&mut loop_state);
        self.store.complete_effect(&self.guard, &pending, &receipt, Checkpoint::new(json!({"planner_calls": calls, "planner": loop_state})), None)?;
        (flight.pending, flight.calls) = (None, calls);
        Ok(())
    }
    fn resume(&self) -> Option<crate::planner::Prior> {
        let mut saved = self.saved.clone()?;
        if let (Some(name), Some(srv)) = (saved.url.strip_prefix("fixture:"), self.fixtures.get()) {
            saved.url = srv.url(name);
        }
        Some(saved)
    }
}

/// How much of a tool result a receipt keeps: enough to see what the call
/// did, not enough to grow the journal with a whole page.
const RECEIPT_TEXT: usize = 2000;

/// A planner loop a paused task can continue from: the conversation its
/// checkpoint holds, checked against the call count beside it. A checkpoint
/// that counts planner calls but holds no conversation cannot be continued
/// (rerunning the call would repeat them), and is refused.
fn planner_prior(task: &TaskRecord) -> Result<Option<crate::planner::Prior>> {
    match task.checkpoint.as_ref() {
        None => Ok(None),
        Some(cp) => planner_prior_of(cp.payload()),
    }
}

/// The conversation a checkpoint payload holds, or `None` when the task never
/// reached its planner.
fn planner_prior_of(payload: &Value) -> Result<Option<crate::planner::Prior>> {
    let Some(calls) = payload.get("planner_calls").and_then(Value::as_u64) else { return Ok(None) };
    let held = payload.get("planner").cloned().context("the planner checkpoint holds no conversation")?;
    let saved: crate::planner::Prior = serde_json::from_value(held).context("the planner checkpoint's conversation is unreadable")?;
    ensure!(saved.calls == calls, "the planner checkpoint counts {calls} calls but its conversation holds {}", saved.calls);
    Ok(Some(saved))
}

/// Confirmed planner work: the task stopped inside its loop. It can only
/// continue from a checkpoint that holds the conversation (see
/// [`planner_prior`]); an older one has to be resolved by hand.
fn planner_checkpoint(task: &TaskRecord) -> bool {
    task.checkpoint.as_ref().is_some_and(|cp| cp.payload().get("planner_calls").is_some())
}

/// Whether a call leaves its task uncertain, and so has to pause for an
/// explicit resolution instead of finishing as a failure or a success.
///
/// Uncertain when the call was stopped midway (`interrupted`), or when a
/// journaled call failed after something a rerun would change again: an
/// action the engine committed, or a planner call it made. A failure decided
/// between actions (`definite`) is final whatever ran before it. A journaled
/// call that committed nothing and never reached the planner did nothing: it
/// failed, and reporting it as "paused" would ask the user to resolve work
/// that never ran.
pub(crate) fn leaves_uncertain(journaled: bool, failed: bool, definite: bool, acted: bool, planned: bool, interrupted: bool) -> bool {
    interrupted || (journaled && failed && !definite && (acted || planned))
}

/// Resolves a paused run's uncertain `items` or `next page` request from
/// what the page it ran on shows now (the client's page).
pub async fn observe(pool: &Arc<pool::Browser>, client: &str, task: &TaskId, effect: &crate::task_store::EffectId, reason: &str) -> Result<TaskRecord> {
    ensure!(!active().lock().unwrap().contains_key(task), Failure::new(ErrorCode::InvalidArgs, format!("task {task} is running")));
    let mut lease = pool.lease(client).await?;
    let resolved = crate::durable_program::resolve_observed(&store()?, task, effect, reason, &mut lease).await;
    lease.release();
    resolved
}

pub async fn call(
    pool: &Arc<pool::Browser>, ctx: &api::Ctx, scope: &str, client: &str,
    tool: &str, args: &Value, live: Option<Arc<dyn Fn(String) + Send + Sync>>, limit: Duration,
) -> Result<ResultRecord> {
    validate(tool, args)?;
    let store = store()?;
    let request = Request { tool: Tool::parse(tool)?, args: args.clone() };
    let mut payload = serde_json::to_value(request)?;
    // Request identity belongs to admission, not program semantics.
    payload["args"].as_object_mut().context("arguments must be an object")?.remove("request_id");
    let record = store.submit(scope, &payload, args["request_id"].as_str())?;
    execute(&store, record, pool, ctx, client, live, limit, false, false).await
}

pub async fn resume(
    record: TaskRecord, adopt_page: bool, pool: &Arc<pool::Browser>, ctx: &api::Ctx,
    client: &str, live: Option<Arc<dyn Fn(String) + Send + Sync>>, limit: Duration,
) -> Result<ResultRecord> {
    execute(&store()?, record, pool, ctx, client, live, limit, true, adopt_page).await
}

fn reply_done(reply: api::Reply) -> pool::Done {
    pool::Done { reply, home: None, acted: false, disposition: pool::Disposition::Completed }
}

/// The reply a finished task gave: its committed `end`, replayed.
fn finished_reply(store: &Store, task: &TaskRecord, outcome: &TaskOutcome) -> Result<api::Reply> {
    let outputs = store.output(&task.id, None)?;
    let records = outputs.iter().filter(|o| o.value["t"] == "record").count() as u64;
    let end = outputs.iter().rev().find(|o| o.value["t"] == "end").map(|o| serde_json::from_value::<crate::events::End>(o.value.clone())).transpose()?;
    let mut reply = match (end, outcome) {
        (Some(end), _) => api::Reply { ok: end.ok, value: end.value, error: end.error, url: end.url, ..Default::default() },
        (None, TaskOutcome::Completed(value)) => serde_json::from_value(value.clone())?,
        (None, TaskOutcome::Failed(reason)) => api::Reply::fail(ErrorCode::StepFailed, reason.clone()),
        (None, TaskOutcome::Cancelled(reason)) => api::Reply::fail(ErrorCode::Cancelled, reason.clone()),
    };
    reply.records = records;
    Ok(reply)
}

/// Streams a task's committed records to the caller, each with its `seq`.
fn replay(store: &Store, task: &TaskId, live: Option<&Arc<dyn Fn(String) + Send + Sync>>) -> Result<()> {
    let Some(live) = live else { return Ok(()) };
    for o in store.output(task, None)? {
        if o.value["t"] == "record" {
            let mut line = o.value;
            line["seq"] = json!(o.sequence);
            live(format!("{}{line}", api::RECORD));
        }
    }
    Ok(())
}

async fn execute(
    store: &Store, record: TaskRecord, pool: &Arc<pool::Browser>, ctx: &api::Ctx,
    client: &str, live: Option<Arc<dyn Fn(String) + Send + Sync>>, limit: Duration, explicit: bool, adopt_page: bool,
) -> Result<ResultRecord> {
    let id = record.id;
    if let Some(f) = &live { f(format!("{}{id}", api::START)); }
    let guard = Arc::new(store.acquire(&id)?);
    let (cancel, mut cancelled) = tokio::sync::watch::channel(None);
    active().lock().unwrap().insert(id.clone(), cancel);
    let _registration = Registration(id.clone());
    let mut task = store.recover_interrupted(&guard)?;
    if let TaskState::Finished { outcome } = &task.state {
        // A duplicate submission: the same records and answer, not the work.
        if let (Some(f), Some(schema)) = (&live, task.request["args"].get("records").filter(|s| !s.is_null())) {
            f(format!("{}{schema}", api::SCHEMA));
        }
        replay(store, &id, live.as_ref())?;
        return Ok(ResultRecord { task: id.clone(), done: reply_done(finished_reply(store, &task, outcome)?) });
    }
    ensure!(explicit || matches!(task.state, TaskState::Queued), Failure::new(ErrorCode::Paused, format!("task {id} requires explicit resume; inspect it with tasks show")));
    ensure!(task.pending_effect.is_none(), Failure::new(ErrorCode::Paused, format!("task {id} has an uncertain effect; resolve it before resuming")));
    // A task stopped inside its planner loop continues from the conversation
    // its checkpoint holds; one whose checkpoint cannot be continued is
    // refused, because rerunning the call would repeat confirmed work.
    let prior = planner_prior(&task).map_err(|why| Failure::new(ErrorCode::Paused, format!("task {id} stopped inside its planner loop and cannot continue from it: {why:#}")))?;
    ensure!(!planner_checkpoint(&task) || prior.is_some(), Failure::new(ErrorCode::Paused, format!("task {id} stopped inside its planner loop, which cannot continue from a checkpoint yet; resolve it applied with the call's reply or cancel it")));
    task = store.resume(&guard, task.revision)?;
    // A crash after committing the receipt needs only the terminal transition.
    if let Some(value) = task.checkpoint.as_ref().and_then(|cp| cp.payload().get("completed")) {
        let mut reply: api::Reply = serde_json::from_value(value.clone())?;
        store.finish(&guard, task.revision, TaskOutcome::Completed(value.clone()))?;
        reply.records = store.output(&id, None)?.iter().filter(|o| o.value["t"] == "record").count() as u64;
        return Ok(ResultRecord { task: id, done: reply_done(reply) });
    }
    let request: Request = serde_json::from_value(task.request.clone())?;
    validate(request.tool.name(), &request.args)?;
    let mut ctx = ctx.clone();
    let dispatched = if matches!(request.tool, Tool::Run) {
        ctx.program_journal = Some(Arc::new(crate::durable_program::Journal::new(self::store()?, guard.clone(), adopt_page)));
        None
    } else if let Some(saved) = prior.clone() {
        // Continuing a loop: the engine's work before it and every call it
        // confirmed are already on record, so the run opens no new whole-call
        // effect and the planner's next call is the one in flight.
        let flight = Arc::new(Mutex::new(Flight { pending: None, whole: false, calls: saved.calls }));
        ctx.call_journal = Some(Arc::new(PlannerJournal { store: self::store()?, guard: guard.clone(), flight: flight.clone(), saved: Some(saved), fixtures: ctx.fixtures() }));
        Some(flight)
    } else {
        let prepared = store.begin_effect(&guard, task.revision, EffectKind::ToolCall, &task.request)?;
        let flight = Arc::new(Mutex::new(Flight { pending: Some(store.mark_dispatched(&guard, &prepared)?), whole: true, calls: 0 }));
        ctx.call_journal = Some(Arc::new(PlannerJournal { store: self::store()?, guard: guard.clone(), flight: flight.clone(), saved: None, fixtures: ctx.fixtures() }));
        Some(flight)
    };
    // A whole call's records are committed as they stream, each taking the
    // next outbox position as its `seq` (against whichever effect is in
    // flight); a program run commits its own. A record that could not be
    // committed was not streamed: the call can't end as if it had been.
    let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let live = match (&dispatched, live) {
        (Some(pending), Some(forward)) => {
            let (pending, commit, guard, lost) = (pending.clone(), self::store()?, guard.clone(), lost.clone());
            Some(Arc::new(move |line: String| {
                let Some(body) = line.strip_prefix(api::RECORD) else { return forward(line) };
                let Ok(mut record) = serde_json::from_str::<Value>(body) else { return };
                if record.get("seq").is_none() {
                    let mut flight = pending.lock().unwrap();
                    let Some(effect) = flight.pending.as_mut() else {
                        lost.store(true, std::sync::atomic::Ordering::SeqCst);
                        return forward("record not committed: no effect in flight".into());
                    };
                    match commit.append_output(&guard, effect, record.clone()) {
                        Ok((next, seq)) => {
                            *effect = next;
                            record["seq"] = json!(seq);
                        }
                        // Not committed: not streamed either; the task pauses.
                        Err(e) => {
                            lost.store(true, std::sync::atomic::Ordering::SeqCst);
                            return forward(format!("record not committed: {e:#}"));
                        }
                    }
                }
                forward(format!("{}{record}", api::RECORD));
            }) as Arc<dyn Fn(String) + Send + Sync>)
        }
        (_, live) => live,
    };
    let done = tokio::select! {
        done = pool::call(pool, &ctx, client, request.tool.name(), &request.args, live, limit) => done,
        _ = cancelled.changed() => {
            let stopped = cancelled.borrow().clone().unwrap_or_else(|| Stop::Interrupt("runner stopped".into()));
            let revision = store.get(&id)?.revision;
            let failure = match stopped {
                Stop::Cancel(reason) => { store.cancel(&guard, revision, &reason)?; Failure::new(ErrorCode::Cancelled, reason) }
                Stop::Interrupt(reason) => { store.interrupt(&guard, revision, &reason)?; Failure::new(ErrorCode::Interrupted, reason) }
            };
            return Ok(ResultRecord { task: id, done: reply_done(api::Reply::failure(failure)) });
        }
    };
    let mut done = done;
    let current = store.get(&id)?;
    // The whole-call effect, while no planner call has closed it.
    let flight = dispatched.clone();
    let whole = flight.as_ref().and_then(|f| { let f = f.lock().unwrap(); f.pending.clone().filter(|_| f.whole) });
    // Whether the planner loop started: its first call closes the whole-call
    // effect, so an effect that is still open (or a call already confirmed)
    // means no planner call was made.
    let planned = flight.as_ref().is_some_and(|f| { let f = f.lock().unwrap(); !f.whole || f.calls > 0 });
    // Failures decided between actions (nothing was in flight) are final;
    // any other failure of a whole call may have left an action unconfirmed.
    let definite = done.reply.error.as_ref().is_some_and(|f| matches!(f.code, ErrorCode::InvalidArgs | ErrorCode::InvalidProgram | ErrorCode::SchemaMismatch | ErrorCode::BudgetExhausted));
    if lost.load(std::sync::atomic::Ordering::SeqCst) {
        done.reply.ok = false;
        done.reply.error = Some(Failure::new(ErrorCode::Internal, "a record could not be committed to the task"));
    }
    // A program that didn't reach its own end (no lease, a checkpoint it
    // couldn't load or save) keeps its continuation: it pauses, never finishes.
    let program_unfinished = dispatched.is_none() && !done.reply.ok && done.reply.error.as_ref().is_none_or(|f| matches!(f.code, ErrorCode::Internal | ErrorCode::Interrupted | ErrorCode::Paused));
    if program_unfinished && matches!(current.state, TaskState::Running) {
        store.pause(&guard, current.revision, "the program stopped before its end; its continuation is kept")?;
    } else if leaves_uncertain(dispatched.is_some(), !done.reply.ok, definite, done.acted, planned, done.disposition == pool::Disposition::Interrupted) {
        store.pause(&guard, current.revision, "execution stopped without confirming its outcome; browser effects may have executed")?;
        let why = done.reply.error.as_ref().map(|f| f.message.clone()).unwrap_or_else(|| "the call did not finish".into());
        done.reply.error = Some(Failure::new(ErrorCode::Paused, format!("{why}; paused: browser effects may have executed (fab tasks show {id})")));
        done.reply.ok = false;
    } else if dispatched.is_some() && !done.reply.ok {
        // Failed before anything was committed: the work is finished, not
        // uncertain, so the task ends as a failure instead of waiting to be
        // resolved.
        let value = serde_json::to_value(&done.reply)?;
        let mut end = serde_json::to_value(done.reply.end())?;
        end["t"] = json!("end");
        let completed = match whole {
            Some(whole) => store.complete_effect(&guard, &whole, &value, Checkpoint::new(json!({"completed": value})), Some(end))?,
            None => store.checkpoint(&guard, current.revision, Checkpoint::new(json!({"completed": value})), Some(end))?,
        };
        let outcome = match &done.reply.error {
            Some(f) => TaskOutcome::Failed(f.message.clone()),
            None => TaskOutcome::Completed(value.clone()),
        };
        store.finish(&guard, completed.revision, outcome)?;
    } else if matches!(current.state, TaskState::Running) {
        let value = serde_json::to_value(&done.reply)?;
        let mut end = serde_json::to_value(done.reply.end())?;
        end["t"] = json!("end");
        let completed = match whole {
            Some(whole) => store.complete_effect(&guard, &whole, &value, Checkpoint::new(json!({"completed": value})), Some(end))?,
            None => store.checkpoint(&guard, current.revision, Checkpoint::new(json!({"completed": value})), Some(end))?,
        };
        store.finish(&guard, completed.revision, TaskOutcome::Completed(value))?;
    }
    Ok(ResultRecord { task: id, done })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::CallJournal;
    use crate::task_commands::{ResolutionCommand, TaskCommand, TaskCommandOutcome, TaskReply, execute_in_session};
    use crate::task_store::{EffectState, Receipt};

    struct TestDb(std::path::PathBuf);
    impl TestDb {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("fab-task-runtime-{}", TaskId::new().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn store(&self) -> Result<Store> { Store::open(self.0.join("tasks.sqlite3")) }
    }
    impl Drop for TestDb {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    /// A running `do` task with its whole-call effect in flight, as `execute` starts it.
    fn start(db: &TestDb) -> Result<(TaskId, PlannerJournal, Arc<Mutex<Flight>>)> {
        let store = db.store()?;
        let task = store.submit("session", &json!({"tool": "do", "args": {"step": "buy it"}}), None)?;
        let guard = Arc::new(store.acquire(&task.id)?);
        let running = store.resume(&guard, task.revision)?;
        let prepared = store.begin_effect(&guard, running.revision, EffectKind::ToolCall, &running.request)?;
        let flight = Arc::new(Mutex::new(Flight { pending: Some(store.mark_dispatched(&guard, &prepared)?), whole: true, calls: 0 }));
        Ok((task.id, PlannerJournal { store, guard, flight: flight.clone(), saved: None, fixtures: Default::default() }, flight))
    }
    /// The conversation a planner call produced, for a journal.
    fn prior(turns: u32) -> crate::planner::Prior {
        crate::planner::Prior {
            messages: vec![json!({"role": "system", "content": "s"}), json!({"role": "user", "content": "goal"}), json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "function": {"name": "act", "arguments": "{}"}}]}), json!({"role": "tool", "tool_call_id": "c1", "content": "clicked"})],
            calls: 0,
            turns,
            page: "page-1".into(),
            document: "doc-1".into(),
            url: "https://shop.test/cart".into(),
            arm: "experiment".into(),
        }
    }
    /// Each effect's kind and whether it was confirmed applied.
    fn kinds(store: &Store, task: &TaskId) -> Result<Vec<(EffectKind, bool)>> {
        Ok(store.effects(task)?.into_iter().map(|e| (e.kind, matches!(e.state, EffectState::Confirmed { receipt: Receipt::Applied { .. } }))).collect())
    }

    #[test]
    fn each_planner_call_is_its_own_confirmed_effect() -> Result<()> {
        let db = TestDb::new();
        let (task, journal, flight) = start(&db)?;
        assert!(journal.after("read", "page", tools::Status::Ok, &prior(1)).is_err(), "no call is in flight");
        journal.before("act", &json!({"instruction": "click e3"}))?;
        let store = db.store()?;
        assert_eq!(kinds(&store, &task)?, [(EffectKind::ToolCall, true), (EffectKind::PlannerCall, false)], "the first call closes the whole call");
        assert!(journal.before("read", &json!({})).is_err(), "one call at a time");
        journal.after("act", "clicked", tools::Status::Ok, &prior(1))?;
        assert!(journal.after("act", "clicked", tools::Status::Ok, &prior(1)).is_err(), "a call completes once");
        journal.before("read", &json!({}))?;
        journal.after("read", "no such row", tools::Status::Failed, &prior(2))?;
        let record = store.get(&task)?;
        assert!(record.pending_effect.is_none() && planner_checkpoint(&record));
        assert_eq!(record.checkpoint.as_ref().unwrap().payload()["planner_calls"], json!(2));
        // The conversation is committed with the receipt, not beside it.
        let saved = planner_prior(&record)?.expect("the loop can continue");
        assert_eq!(saved.calls, 2);
        assert_eq!(saved.messages.len(), 4, "the goal, the model message and its result");
        assert_eq!(saved.turns, 2);
        assert_eq!((saved.page.as_str(), saved.document.as_str(), saved.arm.as_str()), ("page-1", "doc-1", "experiment"));
        assert_eq!(saved.url, "https://shop.test/cart", "the loop's address is kept, so a resumed run can reopen it");
        assert_eq!(journal.resume().map(|p| p.calls), None, "a fresh journal holds nothing to resume");
        let effects = store.effects(&task)?;
        assert_eq!(effects[1].request, json!({"tool": "act", "args": {"instruction": "click e3"}}));
        assert!(matches!(&effects[2].state, EffectState::Confirmed { receipt: Receipt::Applied { value, .. } } if value["status"] == "failed"));
        // A finished run commits its reply with no effect in flight.
        assert!(flight.lock().unwrap().pending.is_none());
        let done = store.checkpoint(&journal.guard, record.revision, Checkpoint::new(json!({"completed": {"ok": true}})), None)?;
        store.finish(&journal.guard, done.revision, TaskOutcome::Completed(json!({"ok": true})))?;
        Ok(())
    }

    /// A journaled `do` only pauses when it may have changed something. A
    /// call that fails before the engine commits anything and before the
    /// planner starts did nothing, so it finishes as a failure.
    #[test]
    fn only_a_call_that_acted_or_planned_leaves_its_task_uncertain() {
        // (journaled, failed, definite, acted, planned, interrupted)
        assert!(!leaves_uncertain(true, true, false, false, false, false), "failed before acting: reported as possibly executed");
        assert!(leaves_uncertain(true, true, false, true, false, false), "the engine committed an action");
        assert!(leaves_uncertain(true, true, false, false, true, false), "a planner call ran");
        assert!(leaves_uncertain(true, true, false, true, true, false), "acted and planned");
        assert!(!leaves_uncertain(true, false, false, true, false, false), "a successful call is not uncertain");
        // Stopped midway, whatever it had done.
        assert!(leaves_uncertain(true, true, false, false, false, true), "an interrupted call");
        assert!(!leaves_uncertain(false, true, false, false, false, false), "a program run has its own branch");
        // A failure decided between actions is final, whatever ran before it.
        assert!(!leaves_uncertain(true, true, true, true, true, false), "a definite failure");
    }

    #[test]
    fn a_crash_inside_a_planner_call_leaves_only_that_call_uncertain() -> Result<()> {
        let db = TestDb::new();
        let (task, journal, _) = start(&db)?;
        journal.before("act", &json!({"instruction": "type \"a\" into e1"}))?;
        journal.after("act", "typed", tools::Status::Ok, &prior(1))?;
        journal.before("act", &json!({"instruction": "click e9"}))?;
        assert!(journal.after("act", "lost", tools::Status::Uncertain, &prior(1)).is_err(), "an uncertain call is never confirmed");
        drop(journal);
        let store = db.store()?;
        let guard = store.acquire(&task)?;
        let recovered = store.recover_interrupted(&guard)?;
        assert!(matches!(recovered.state, TaskState::Interrupted { .. }));
        let pending = recovered.pending_effect.clone().unwrap();
        assert!(pending.kind == EffectKind::PlannerCall && matches!(pending.state, EffectState::Uncertain { .. }));
        assert_eq!(kinds(&store, &task)?, [(EffectKind::ToolCall, true), (EffectKind::PlannerCall, true), (EffectKind::PlannerCall, false)]);
        assert_eq!(recovered.checkpoint.as_ref().unwrap().payload()["planner_calls"], json!(1));
        // What the loop had confirmed is still resumable.
        assert_eq!(planner_prior(&recovered)?.map(|p| p.calls), Some(1));
        drop(guard);
        // Not applied settles the call, but the request still cannot rerun.
        let out = execute_in_session(&store, "session", TaskCommand::Resolve { task: task.clone(), effect: pending.id.clone(), resolution: ResolutionCommand::NotApplied { reason: "the button is still there".into() } })?;
        assert!(matches!(out, TaskCommandOutcome::Resolved(ref r) if r.pending_effect.is_none() && planner_checkpoint(r)));
        Ok(())
    }

    #[test]
    fn an_uncertain_planner_call_resolves_applied_with_the_whole_reply() -> Result<()> {
        let db = TestDb::new();
        let (task, journal, _) = start(&db)?;
        journal.before("act", &json!({"instruction": "click e9"}))?;
        let store = db.store()?;
        let stopped = store.interrupt(&journal.guard, store.get(&task)?.revision, "lost reply")?;
        drop(journal);
        let effect = stopped.pending_effect.unwrap().id;
        let reply = TaskReply { text: "bought".into(), ok: true, value: json!("order 7"), turns: 3, cost: 0.0 };
        let out = execute_in_session(&store, "session", TaskCommand::Resolve { task: task.clone(), effect, resolution: ResolutionCommand::Applied { reason: "the order page shows it".into(), result: reply.clone() } })?;
        let TaskCommandOutcome::Resolved(resolved) = out else { panic!("expected resolved task") };
        assert!(!planner_checkpoint(&resolved), "the reply replaces the planner checkpoint");
        assert_eq!(resolved.checkpoint.unwrap().payload(), &json!({"completed": reply}));
        Ok(())
    }

    /// A resumed loop carries on from where it stopped: its call numbers keep
    /// going (so a call that was confirmed is never issued again under a
    /// number the journal has already passed), and it can read back the
    /// conversation it continues.
    #[test]
    fn a_resumed_loop_continues_its_numbering() -> Result<()> {
        let db = TestDb::new();
        let (task, journal, _) = start(&db)?;
        journal.before("act", &json!({"instruction": "click e3"}))?;
        journal.after("act", "clicked", tools::Status::Ok, &prior(1))?;
        journal.before("read", &json!({}))?;
        journal.after("read", "the page", tools::Status::Ok, &prior(2))?;
        // The task is interrupted, and a fresh run picks it up.
        let store = db.store()?;
        let stopped = store.interrupt(&journal.guard, store.get(&task)?.revision, "the runner stopped")?;
        let saved = planner_prior(&stopped)?.expect("the loop can continue");
        // The resumed run puts the task back to running, as `execute` does.
        let running = store.resume(&journal.guard, stopped.revision)?;
        assert!(matches!(running.state, TaskState::Running));
        let flight = Arc::new(Mutex::new(Flight { pending: None, whole: false, calls: saved.calls }));
        let resumed = PlannerJournal { store: db.store()?, guard: journal.guard.clone(), flight: flight.clone(), saved: Some(saved.clone()), fixtures: Default::default() };
        // What the loop continues from is exactly what was saved.
        assert_eq!(resumed.resume().map(|p| (p.calls, p.turns, p.messages.len())), Some((saved.calls, saved.turns, saved.messages.len())));
        // Its next call is numbered after the confirmed ones, and it opens no
        // whole-call effect: the engine's work before the loop is confirmed.
        resumed.before("act", &json!({"instruction": "click e9"}))?;
        assert_eq!(kinds(&db.store()?, &task)?, [
            (EffectKind::ToolCall, true),
            (EffectKind::PlannerCall, true),
            (EffectKind::PlannerCall, true),
            (EffectKind::PlannerCall, false),
        ]);
        resumed.after("act", "bought", tools::Status::Ok, &prior(3))?;
        let record = db.store()?.get(&task)?;
        assert_eq!(record.checkpoint.as_ref().unwrap().payload()["planner_calls"], json!(3), "the count went backwards");
        assert_eq!(planner_prior(&record)?.map(|p| p.turns), Some(3));
        Ok(())
    }

    /// A checkpoint that counts planner calls but holds no conversation (an
    /// older one) cannot be continued: rerunning would repeat them.
    #[test]
    fn a_planner_checkpoint_without_a_conversation_is_refused() -> Result<()> {
        let db = TestDb::new();
        let store = db.store()?;
        let task = store.submit("session", &json!({"tool": "do", "args": {"step": "buy it"}}), None)?;
        let guard = Arc::new(store.acquire(&task.id)?);
        let running = store.resume(&guard, task.revision)?;
        let prepared = store.begin_effect(&guard, running.revision, EffectKind::ToolCall, &running.request)?;
        let flight = Arc::new(Mutex::new(Flight { pending: Some(store.mark_dispatched(&guard, &prepared)?), whole: true, calls: 0 }));
        let journal = PlannerJournal { store: db.store()?, guard, flight: flight.clone(), saved: None, fixtures: Default::default() };
        journal.before("act", &json!({"instruction": "click e3"}))?;
        journal.after("act", "clicked", tools::Status::Ok, &prior(1))?;
        let record = db.store()?.get(&task.id)?;
        assert!(planner_prior(&record)?.is_some());
        // What a checkpoint written before the conversation was kept holds.
        let calls = record.checkpoint.as_ref().unwrap().payload()["planner_calls"].clone();
        assert!(planner_prior_of(&json!({"planner_calls": calls})).is_err(), "a count without a conversation was accepted");
        // A conversation that disagrees with the count beside it is refused too.
        let wrong = json!({"planner_calls": 9, "planner": record.checkpoint.unwrap().payload()["planner"].clone()});
        assert!(planner_prior_of(&wrong).is_err(), "a mismatched count was accepted");
        // A checkpoint with no planner calls at all is not a planner loop.
        assert!(planner_prior_of(&json!({"completed": {"ok": true}}))?.is_none());
        Ok(())
    }

    /// The conversation a checkpoint keeps: the goal survives, the oldest
    /// turns fall off, and no tool result is left without its model message.
    #[test]
    fn a_saved_conversation_is_whole_and_bounded() {
        use crate::planner::capped;
        let mut messages = vec![json!({"role": "system", "content": "s"}), json!({"role": "user", "content": "the goal"})];
        // Twelve turns, each answer far larger than the cap.
        for i in 0..12 {
            messages.push(json!({"role": "assistant", "content": null, "tool_calls": [{"id": format!("c{i}"), "function": {"name": "act", "arguments": "{}"}}]}));
            messages.push(json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": "x".repeat(64 * 1024)}));
        }
        let kept = capped(&messages);
        assert_eq!((kept[0]["content"].as_str(), kept[1]["content"].as_str()), (Some("s"), Some("the goal")), "the goal is kept");
        assert!(kept.len() < messages.len(), "the conversation did not shrink");
        // A kept tool result always has the model message that called it.
        for (i, m) in kept.iter().enumerate() {
            if m["role"] == json!("tool") {
                assert_eq!(kept[i - 1]["role"], json!("assistant"), "a tool result without the model message that answered it");
            }
        }
        // The cut lands on a turn: nothing kept is half a turn.
        assert!(kept.iter().rev().take_while(|m| m["role"] != json!("user")).all(|m| m["role"] == json!("tool")), "the conversation was cut mid-turn");
        // Long results are shortened rather than dropped.
        assert!(kept.iter().filter(|m| m["role"] == json!("tool")).all(|m| m["content"].as_str().is_some_and(|c| c.len() <= 4200)));
        // Small conversations are kept whole.
        let small: Vec<Value> = messages[..4].to_vec();
        assert_eq!(capped(&small).len(), 4, "a short conversation was cut");
    }
}
