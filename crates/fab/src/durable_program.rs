//! Program checkpoints are inert data. Resuming requires fresh page evidence.
use std::sync::Arc;
use anyhow::{Result, ensure};
use fab_core::{Session, program_machine::ProgramMachine, secrets::redact};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use crate::events::{ErrorCode, Failure};
use crate::task_store::{Checkpoint, EffectId, EffectKind, EffectState, PendingEffect, Resolution, Store, TaskGuard, TaskId, TaskRecord};

/// The page an effect ran on. `url` is evidence only: identity is the page,
/// document and revision.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PageBinding {
    pub(crate) page: String,
    pub(crate) document: String,
    revision: u64,
    #[serde(default)]
    url: Option<String>,
}
impl PartialEq for PageBinding {
    fn eq(&self, o: &Self) -> bool { (&self.page, &self.document, self.revision) == (&o.page, &o.document, o.revision) }
}
impl PageBinding {
    pub async fn observe(session: &mut Session) -> Result<Self> {
        let page = session.browser.page_id().await;
        let snapshot = session.snapshot().await?;
        ensure!(!snapshot.doc_id.is_empty(), "page observation has no document identity");
        Ok(Self { page, document: snapshot.doc_id.clone(), revision: snapshot.version, url: Some(snapshot.url.clone()) })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub machine: ProgramMachine,
    pub scraper: crate::scrape::Scraper,
    /// The newest progress lines; older ones are summarized in the first.
    pub log: Vec<String>,
    /// How many records the task has committed. The records themselves live
    /// only in the task outbox, so a checkpoint does not grow with them.
    pub emitted: usize,
    pub turns: u32,
    pub cost: f64,
    pub steps: usize,
    pub binding: Option<PageBinding>,
    pub navigation: Option<String>,
    /// An initial navigation asserted (not observed) to have arrived: the
    /// resumed run must find the browser there before it binds the page.
    #[serde(default)]
    pub arrived: Option<String>,
    /// While a detail page is open: the program as it was just before it
    /// opened the outermost one. A run adopting another page returns to the
    /// list and continues from here, so the item is opened again.
    #[serde(default)]
    pub reopen: Option<Reopen>,
}

/// The program suspended at the `open` of its outermost detail page.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reopen {
    pub machine: ProgramMachine,
    pub scraper: crate::scrape::Scraper,
    /// Records committed before the page was opened.
    pub emitted: usize,
    /// A step that may act on the page (`do`, or `read` in agent mode) was
    /// dispatched since: opening the page again could repeat it.
    pub acted: bool,
}

/// A saved program state, checked: the machine's item handles exist in its
/// scraper, and a reopen point is suspended at an `open` of the same program.
fn parse(data: &Value) -> Result<State> {
    use fab_core::program_machine::RequestKind;
    let state: State = serde_json::from_value(data.clone())?;
    ensure!(state.cost.is_finite() && state.cost >= 0.0, "invalid program accounting");
    ensure!(state.machine.item_handles().into_iter().all(|h| state.scraper.has_item(h)), "checkpoint refers to page items it does not hold");
    if let Some(r) = &state.reopen {
        ensure!(state.scraper.in_detail(), "checkpoint has a reopen point without an open detail page");
        ensure!(r.machine.program() == state.machine.program(), "checkpoint's reopen point belongs to a different program");
        ensure!(matches!(r.machine.pending().map(|p| &p.kind), Some(RequestKind::Open { .. })), "checkpoint's reopen point is not suspended at an open");
        ensure!(!r.scraper.in_detail() && r.emitted <= state.emitted, "checkpoint's reopen point is inconsistent");
        ensure!(r.machine.item_handles().into_iter().all(|h| r.scraper.has_item(h)), "checkpoint's reopen point refers to page items it does not hold");
    }
    Ok(state)
}

fn saved(task: &TaskRecord) -> Result<State> {
    parse(task.checkpoint.as_ref().and_then(|cp| cp.payload().get("program")).ok_or_else(|| anyhow::anyhow!("no saved program continuation"))?)
}

/// The state a return to the list page leaves, and the page (None: the
/// browser never left it; the detail page was fetched): the program as it
/// was before it opened the outermost detail page, so the item is opened
/// again. Refused when that could repeat work: a record emitted or a step
/// that may act dispatched since the page was opened.
pub(crate) fn returned(mut state: State) -> Result<(State, Option<String>)> {
    ensure!(state.scraper.in_detail(), "no detail page is open");
    let target = state.scraper.list_page().map(str::to_owned);
    let back = state.reopen.take().ok_or_else(|| anyhow::anyhow!("the checkpoint does not hold the program as it was before the detail page was opened"))?;
    ensure!(!back.acted, "a step that may act ran on the open detail page; opening it again could repeat it");
    ensure!(back.emitted == state.emitted, "{} record(s) came from the open detail page; opening it again would repeat them", state.emitted - back.emitted);
    let mut scraper = back.scraper;
    (scraper.turns, scraper.cost, scraper.llm_ms) = (state.scraper.turns, state.scraper.cost, state.scraper.llm_ms);
    // Back on the recorded list page, what was learned there still holds;
    // staying on the adopted page, it is adopted as the list page.
    if target.is_none() { scraper.adopt_page()?; }
    state.machine = back.machine;
    state.scraper = scraper;
    Ok((state, target))
}

pub struct Journal { store: Store, guard: Arc<TaskGuard>, pub(crate) adopt_page: bool }
impl Journal {
    pub fn new(store: Store, guard: Arc<TaskGuard>, adopt_page: bool) -> Self { Self { store, guard, adopt_page } }
    pub fn task(&self) -> Result<TaskRecord> { self.store.get(self.guard.task_id()) }
    pub(crate) fn load(&self) -> Result<Option<State>> {
        let task = self.task()?;
        let Some(data) = task.checkpoint.as_ref().and_then(|cp| cp.payload().get("program")) else { return Ok(None) };
        Ok(Some(parse(data)?))
    }
    /// Commits the state, and `output` with it; returns the output's
    /// position in the task's outbox.
    pub(crate) fn save(&self, state: &State, output: Option<Value>) -> Result<u64> {
        let emits = output.is_some();
        let task = self.store.checkpoint(&self.guard, self.task()?.revision, checkpoint(state)?, output)?;
        Ok(if emits { task.next_output() - 1 } else { task.next_output() })
    }
    pub(crate) fn begin(&self, state: &State, kind: EffectKind, request: &Value) -> Result<PendingEffect> {
        self.save(state, None)?;
        let pending = self.store.begin_effect(&self.guard, self.task()?.revision, kind, request)?;
        self.store.mark_dispatched(&self.guard, &pending)
    }
    pub(crate) fn complete(&self, pending: &PendingEffect, receipt: &Value, state: &State) -> Result<()> {
        self.store.complete_effect(&self.guard, pending, receipt, checkpoint(state)?, None)?;
        Ok(())
    }
    pub fn pause(&self, reason: &str) -> Result<()> {
        self.store.pause(&self.guard, self.task()?.revision, reason)?;
        Ok(())
    }
}

const LOG_TAIL: usize = 200;

/// The progress log as checkpointed: bounded, so saving after every request
/// stays linear in the run's length.
pub(crate) fn log_tail(log: &[String]) -> Vec<String> {
    if log.len() <= LOG_TAIL {
        return log.to_vec();
    }
    let dropped = log.len() - LOG_TAIL + 1;
    std::iter::once(format!("(… {dropped} earlier line{})", if dropped == 1 { "" } else { "s" }))
        .chain(log[dropped..].iter().cloned())
        .collect()
}

fn checkpoint(state: &State) -> Result<Checkpoint> {
    let payload = json!({"program": state});
    let mut scrubbed = payload.clone();
    redact::value(&mut scrubbed);
    ensure!(payload == scrubbed, "checkpoint contains concealed material; execution requires reconciliation");
    Ok(Checkpoint::new(payload))
}

/// An asserted leaf result advances the saved interpreter, never arbitrary
/// caller-supplied continuation data. Scraper mutations need their full receipt.
pub(crate) fn resolve_value(task: &TaskRecord, value: &Value) -> Result<(Checkpoint, Value)> {
    use fab_core::program_machine::{RequestKind, Response};
    let mut state = saved(task)?;
    let asked = state.machine.pending().ok_or_else(|| anyhow::anyhow!("program is not suspended"))?.clone();
    ensure!(matches!(asked.kind, RequestKind::Leaf { .. } | RequestKind::Effect { kind: fab_core::script::Eff::Test, .. }), "this request needs scraper recovery evidence; a scalar result is insufficient (resolve an `items` or `next page` request with --observe, or not_applied)");
    let effect = task.pending_effect.as_ref().ok_or_else(|| anyhow::anyhow!("task has no pending effect"))?;
    ensure!(serde_json::to_value(&asked)? == effect.request, "effect does not match the saved continuation");
    let result: Result<Response, String> = Ok(Response::Value(fab_core::script::Value::from_json(value)));
    ensure!(asked.kind.accepts(result.as_ref().expect("ok")), "a yes/no question needs a true or false result");
    state.machine.complete(&asked.token, result.clone())?;
    Ok((checkpoint(&state)?, serde_json::to_value(result)?))
}

/// An asserted arrival completes an uncertain initial navigation. The page
/// is not re-observed here: the resumed run checks it is on that address
/// (origin and path) before it binds the page, and pauses otherwise.
pub(crate) fn resolve_navigation(task: &TaskRecord, value: &Value) -> Result<(Checkpoint, Value)> {
    ensure!(value == &json!(true) || value == &json!({"arrived": true}), "a navigation resolution asserts arrival with `true`");
    let mut state = saved(task)?;
    let url = state.navigation.take().ok_or_else(|| anyhow::anyhow!("program has no pending navigation"))?;
    let effect = task.pending_effect.as_ref().ok_or_else(|| anyhow::anyhow!("task has no pending effect"))?;
    ensure!(effect.request == json!({"url": url}), "effect does not match the saved continuation");
    state.binding = None;
    state.arrived = Some(url);
    Ok((checkpoint(&state)?, json!({"arrived": true, "asserted": true})))
}

/// An asserted return to the list page: the program continues from before
/// it opened the detail page. A resumed run checks the browser is on the
/// list page (origin and path) before it binds the page, as for an asserted
/// navigation.
pub(crate) fn resolve_return(task: &TaskRecord, value: &Value) -> Result<(Checkpoint, Value)> {
    ensure!(value == &json!(true) || value == &json!({"returned": true}), "a return resolution asserts arrival with `true`");
    let state = saved(task)?;
    let effect = task.pending_effect.as_ref().ok_or_else(|| anyhow::anyhow!("task has no pending effect"))?;
    let (mut state, target) = returned(state)?;
    ensure!(effect.request == json!({"url": target}), "effect does not match the saved continuation");
    if target.is_some() {
        state.binding = None;
        state.arrived = target.clone();
    }
    Ok((checkpoint(&state)?, json!({"returned": target, "asserted": true})))
}

/// What a request's own page can say about whether it happened.
enum Observed {
    /// `items "…"`: the list's keys on the live page.
    Items(String),
    /// `next page`: the list moved on when the page shows unseen items.
    Next,
    /// `open`: the address it was opening, when it had one.
    Open(String),
    /// `back`: the list page it returns to, when it has one.
    Back(String),
}

/// Resolves an uncertain `items` or `next page` request from what the page
/// it ran on shows now, compared with the items the saved scraper produced.
/// `items` reads only, so it is answered with the unseen items the page
/// shows (read in code with the saved mapping). `next page` moved on when
/// the page shows unseen items: reading them next is right whether or not
/// the pager was used; when it shows only items already read, it did not,
/// and runs again. `open` and `back` only navigate, so the page can confirm
/// one: the address the page is on is the address it was going to. Anything
/// the page cannot tell (another page, the list gone, a list or fields never
/// mapped, an `open` whose page was fetched and so never moved the browser)
/// stays `not_applied`, which is always safe for a navigation.
pub(crate) async fn resolve_observed(store: &Store, task: &TaskId, effect: &EffectId, reason: &str, sess: &mut Session) -> Result<TaskRecord> {
    use fab_core::program_machine::{RequestKind, Response};
    use fab_core::script::{Eff, Value as V};
    let refuse = |message: String| anyhow::Error::new(Failure::new(ErrorCode::InvalidArgs, message));
    ensure!(!reason.trim().is_empty(), refuse("resolution requires a reason".into()));
    let guard = store.acquire(task)?;
    let current = store.recover_interrupted(&guard)?;
    let pending = current.pending_effect.as_ref().ok_or_else(|| refuse("task has no pending effect".into()))?;
    ensure!(&pending.id == effect, refuse("effect is not pending for this task".into()));
    ensure!(pending.kind == EffectKind::ProgramRequest && matches!(pending.state, EffectState::Uncertain { .. }), refuse("only a dispatched program request can be resolved from the page".into()));
    let mut state = saved(&current)?;
    let asked = state.machine.pending().ok_or_else(|| refuse("program is not suspended".into()))?.clone();
    ensure!(serde_json::to_value(&asked)? == pending.request, "effect does not match the saved continuation");
    let what = match &asked.kind {
        RequestKind::Effect { kind: Eff::Items, query } => Observed::Items(query.clone()),
        RequestKind::Effect { kind: Eff::Next, .. } => Observed::Next,
        RequestKind::Open { item, .. } => Observed::Open(
            state
                .scraper
                .open_target(item)
                .ok_or_else(|| refuse("`open` had no address to go to (the item has no link, or its page is a fetched copy that never moved the browser); resolve it not_applied to open it again".into()))?,
        ),
        RequestKind::Back => Observed::Back(
            state
                .scraper
                .list_page()
                .ok_or_else(|| refuse("`back` had no list page to return to (the browser never left it); resolve it not_applied".into()))?
                .to_string(),
        ),
        _ => return Err(refuse("only an `items`, `next page`, `open` or `back` request can be resolved from the page".into())),
    };
    let expected = state.binding.clone().ok_or_else(|| refuse("the page the request ran on was not recorded".into()))?;
    let here = PageBinding::observe(sess).await?;
    ensure!(here.page == expected.page, refuse("the browser page the request ran on is gone; resolve it not_applied, then resume".into()));
    let resolution = match what {
        Observed::Items(query) => {
            // `items` never leaves the page: another document means something else happened.
            ensure!(here.document == expected.document, refuse("the page has changed since the request; resolve it not_applied, then resume".into()));
            let wants = crate::api::loop_wants(state.machine.program()).remove(&state.machine.pc()).unwrap_or_default();
            let (seen, items) = state.scraper.items_seen_now(sess, &query, &wants).await.map_err(|e| refuse(format!("the page can't answer `items \"{query}\"` by itself ({e:#}); resolve it not_applied to read it again")))?;
            let response: Result<Response, String> = Ok(Response::Value(V::List(items)));
            state.machine.complete(&asked.token, response.clone())?;
            json!({"response": response, "evidence": seen})
        }
        Observed::Next => {
            ensure!(!state.scraper.is_virtual(), refuse("`next page` was paging a fetched page, which the browser does not show; resolve it not_applied".into()));
            // Paging keeps the site and the layout of the list's page (a query
            // or a page number may change): anything else is not this list.
            let same_list = match (expected.url.as_deref(), here.url.as_deref()) {
                (Some(a), Some(b)) => crate::scrape::same_layout(a, b),
                _ => false,
            };
            ensure!(same_list, refuse("the browser is not on the list's page any more, so whether it moved on can't be told; resolve it not_applied".into()));
            let seen = state.scraper.sight(sess, None).await.map_err(|e| refuse(format!("{e:#}; resolve it not_applied")))?;
            ensure!(!seen.shown.is_empty(), refuse(format!("the page shows none of the list {}, so whether it moved on can't be told", seen.list)));
            if seen.unseen.is_empty() {
                let evidence = serde_json::to_string(&seen)?;
                return Ok(store.resolve(&guard, current.revision, effect, Resolution::NotApplied { assertion: format!("{reason} (observed: the page shows only items already read, so `next page` runs again: {evidence})") })?);
            }
            let response: Result<Response, String> = Ok(Response::Value(V::Bool(true)));
            state.machine.complete(&asked.token, response.clone())?;
            json!({"response": response, "evidence": seen})
        }
        // A navigation the page can confirm: the address on screen is the one
        // the request was going to. The opposite answer is not proof that it
        // did not happen (the tab may have moved on since), so it stays
        // `not_applied`, which only repeats a GET.
        Observed::Open(target) | Observed::Back(target) => {
            let here_url = here.url.clone().unwrap_or_default();
            if !crate::scrape::same_layout(&here_url, &target) {
                let what = match asked.kind {
                    RequestKind::Back => "`back`",
                    _ => "`open`",
                };
                let evidence = json!({"expected": target, "here": here_url});
                return Ok(store.resolve(&guard, current.revision, effect, Resolution::NotApplied { assertion: format!("{reason} (observed: the browser is on {here_url}, not on the address {what} was going to, so it runs again: {evidence})") })?);
            }
            let response: Result<Response, String> = Ok(Response::Done);
            state.machine.complete(&asked.token, response.clone())?;
            json!({"response": response, "evidence": {"here": here_url, "expected": target}})
        }
    };
    state.log.extend(state.scraper.log.drain(..));
    state.log = log_tail(&state.log);
    state.binding = Some(here);
    let assertion = format!("{reason} (observed on the page)");
    Ok(store.resolve(&guard, current.revision, effect, Resolution::Applied { assertion, receipt: resolution, checkpoint: checkpoint(&state)?, output: None })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fab_core::{backend::{Browser, driver::{DriverFuture, PageDriver}}, decide::Decider, jev::Jev, Knobs, program_machine::Event};
    use crate::task_store::TaskState;

    struct Fake;
    impl PageDriver for Fake {
        fn name(&self) -> &'static str { "journal-test" }
        fn describe(&self) -> String { "journal test".into() }
        fn page_id(&self) -> String { "page-a".into() }
        fn eval<'a>(&'a mut self, _: &'a str) -> DriverFuture<'a, Result<Value>> {
            Box::pin(async { Ok(json!({"docId":"document-a","version":1,"url":"https://test.invalid/","title":"test","els":[],"texts":[]})) })
        }
        fn goto<'a>(&'a mut self, _: &'a str) -> DriverFuture<'a, Result<()>> {
            Box::pin(async { anyhow::bail!("unexpected navigation") })
        }
        fn close(&mut self) -> DriverFuture<'_, ()> { Box::pin(async {}) }
    }
    fn session() -> Session {
        let mut knobs = Knobs::default();
        knobs.shape = "off".into();
        Session::from_browser(Browser::from_driver(Fake), knobs, Decider(Jev::without_key())).unwrap()
    }
    struct Database(std::path::PathBuf);
    impl Database {
        fn new() -> Self { Self(std::env::temp_dir().join(format!("fab-program-{}", TaskId::new().unwrap()))) }
        fn store(&self) -> Store { Store::open(self.0.join("tasks.sqlite3")).unwrap() }
    }
    impl Drop for Database { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
    fn state(machine: ProgramMachine) -> State {
        State { machine, scraper: Default::default(), log: vec![], emitted: 0, turns: 0, cost: 0.0, steps: 0, binding: None, navigation: None, arrived: None, reopen: None }
    }

    #[tokio::test]
    async fn resumed_runner_does_not_reemit_committed_records() -> Result<()> {
        let db = Database::new();
        let store = db.store();
        let task = store.submit("test", &json!({}), None)?;
        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, task.revision)?;
        let journal = Arc::new(Journal::new(db.store(), guard.clone(), false));
        let program = fab_core::script::parse("set n = 1\nemit n\nset n = 2\nemit n\nreturn n")?;
        let mut saved = state(ProgramMachine::new(program.clone())?);
        let Event::Record(record) = saved.machine.advance() else { panic!("expected record") };
        saved.emitted += 1;
        journal.save(&saved, Some(record))?;
        store.interrupt(&guard, journal.task()?.revision, "simulated stopped process")?;
        drop(journal);
        drop(guard);

        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, store.get(&task.id)?.revision)?;
        let journal = Arc::new(Journal::new(db.store(), guard, false));
        let mut ctx = crate::api::Ctx::new(None);
        ctx.program_journal = Some(journal);
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = lines.clone();
        let mut session = session();
        session.live = Some(Arc::new(move |line| capture.lock().unwrap().push(line)));
        let result = crate::api::run_script(&mut session, &mut ctx, &program, None, None).await;
        assert!(result.ok, "{}", result.text);
        assert_eq!(store.output(&task.id, None)?.len(), 2);
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains('2'));
        assert!(!result.text.contains("{\"n\":1}"));
        Ok(())
    }

    #[tokio::test]
    async fn stale_page_checkpoint_pauses_before_dispatch() -> Result<()> {
        let db = Database::new();
        let store = db.store();
        let task = store.submit("test", &json!({}), None)?;
        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, task.revision)?;
        let journal = Arc::new(Journal::new(db.store(), guard, false));
        let program = fab_core::script::parse("do \"submit\"")?;
        let mut saved = state(ProgramMachine::new(program.clone())?);
        saved.binding = Some(PageBinding { page: "other-page".into(), document: "document-a".into(), revision: 1, url: None });
        journal.save(&saved, None)?;
        let mut ctx = crate::api::Ctx::new(None);
        ctx.program_journal = Some(journal);
        let result = crate::api::run_script(&mut session(), &mut ctx, &program, None, None).await;
        assert!(!result.ok);
        assert!(matches!(store.get(&task.id)?.state, TaskState::Paused { .. }));
        assert!(store.effects(&task.id)?.is_empty());
        Ok(())
    }

    #[test]
    fn asserted_resolution_requires_the_promised_shape() -> Result<()> {
        use fab_core::program_machine::RequestKind;
        let db = Database::new();
        let store = db.store();
        let task = store.submit("test", &json!({}), None)?;
        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, task.revision)?;
        let journal = Journal::new(db.store(), guard.clone(), false);
        for (source, wrong, right) in [
            ("test \"ready\" -> ok\nreturn ok", json!("yes"), json!(true)),
            ("read \"price\" -> p\nreturn p", json!(null), json!("$3")),
        ] {
            let mut saved = state(ProgramMachine::new(fab_core::script::parse(source)?)?);
            let Event::Request(asked) = saved.machine.advance() else { panic!("expected request") };
            assert!(matches!(asked.kind, RequestKind::Leaf { .. }));
            let pending = journal.begin(&saved, EffectKind::ProgramRequest, &serde_json::to_value(&asked)?)?;
            let current = journal.task()?;
            if source.starts_with("test") {
                assert!(resolve_value(&current, &wrong).is_err(), "non-boolean answer to a yes/no question");
            }
            let (checkpoint, _) = resolve_value(&current, &right)?;
            let resumed: State = serde_json::from_value(checkpoint.payload()["program"].clone())?;
            assert!(resumed.machine.pending().is_none());
            journal.complete(&pending, &json!({}), &saved)?;
        }
        let mut saved = state(ProgramMachine::new(fab_core::script::parse("for x in items \"rows\"\n  emit x\nend")?)?);
        let Event::Request(asked) = saved.machine.advance() else { panic!("expected request") };
        journal.begin(&saved, EffectKind::ProgramRequest, &serde_json::to_value(&asked)?)?;
        assert!(resolve_value(&journal.task()?, &json!([])).is_err(), "scraper requests need full evidence");
        Ok(())
    }

    #[test]
    fn asserted_navigation_clears_the_pending_url_only() -> Result<()> {
        let db = Database::new();
        let store = db.store();
        let task = store.submit("test", &json!({}), None)?;
        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, task.revision)?;
        let journal = Journal::new(db.store(), guard.clone(), false);
        let mut saved = state(ProgramMachine::new(fab_core::script::parse("return 1")?)?);
        saved.navigation = Some("https://test.invalid/".into());
        saved.binding = Some(PageBinding { page: "p".into(), document: "d".into(), revision: 1, url: None });
        journal.begin(&saved, EffectKind::ProgramNavigation, &json!({"url": "https://test.invalid/"}))?;
        let current = journal.task()?;
        assert!(resolve_navigation(&current, &json!("yes")).is_err());
        assert!(resolve_value(&current, &json!(true)).is_err(), "not a program request");
        let (checkpoint, _) = resolve_navigation(&current, &json!(true))?;
        let resumed: State = serde_json::from_value(checkpoint.payload()["program"].clone())?;
        assert!(resumed.navigation.is_none() && resumed.binding.is_none());
        assert_eq!(resumed.arrived.as_deref(), Some("https://test.invalid/"));
        assert!(resumed.machine == saved.machine);
        Ok(())
    }

    #[test]
    fn checkpoint_item_handles_must_exist_in_the_saved_scraper() -> Result<()> {
        use fab_core::program_machine::Response;
        let db = Database::new();
        let store = db.store();
        let task = store.submit("test", &json!({}), None)?;
        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, task.revision)?;
        let journal = Journal::new(db.store(), guard.clone(), false);
        let mut saved = state(ProgramMachine::new(fab_core::script::parse("for x in items \"rows\"\n  emit x\nend")?)?);
        let Event::Request(asked) = saved.machine.advance() else { panic!("expected request") };
        saved.machine.complete(&asked.token, Ok(Response::Value(fab_core::script::Value::List(vec![fab_core::script::Value::Item(3)]))))?;
        assert_eq!(saved.machine.item_handles(), [3]);
        journal.save(&saved, None)?;
        assert!(journal.load().is_err(), "handle 3 names no saved item");
        Ok(())
    }

    #[tokio::test]
    async fn declared_records_leave_in_the_declared_shape() -> Result<()> {
        use crate::events::ErrorCode;
        let shape = crate::api::shape_of(&json!({
            "records": {"properties": {"name": {"type": "string"}, "price": {"type": "number"}}, "required": ["name", "price"]},
            "returns": {"type": "integer"}}))?;
        let run = |source: &'static str| {
            let shape = shape.clone();
            async move {
                let mut ctx = crate::api::Ctx::new(None);
                ctx.shape = Arc::new(shape);
                let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
                let capture = lines.clone();
                let mut session = session();
                session.live = Some(Arc::new(move |line| capture.lock().unwrap().push(line)));
                let reply = crate::api::run_script(&mut session, &mut ctx, &fab_core::script::parse(source).unwrap(), None, None).await;
                let records: Vec<Value> = lines.lock().unwrap().iter().filter_map(|l| l.strip_prefix(crate::api::RECORD)).map(|r| serde_json::from_str::<Value>(r).unwrap()["data"].clone()).collect();
                (reply, records)
            }
        };
        let (reply, records) = run("emit price: \"$1,299\", name: 7, extra: 1\nreturn \"3 items\"").await;
        assert!(reply.ok, "{}", reply.text);
        assert_eq!(serde_json::to_string(&records[0])?, r#"{"name":"7","price":1299}"#);
        assert_eq!(reply.value, json!(3));
        let (reply, records) = run("emit name: \"a\", price: 1\nemit name: \"b\", price: \"n/a\"").await;
        assert_eq!(records.len(), 1, "the record that does not fit is not emitted");
        assert_eq!(reply.error.unwrap().code, ErrorCode::SchemaMismatch);
        let (reply, _) = run("return 1.5").await;
        assert_eq!(reply.error.unwrap().code, ErrorCode::SchemaMismatch);
        Ok(())
    }

    #[tokio::test]
    async fn asserted_arrival_is_checked_before_the_page_is_bound() -> Result<()> {
        for (arrived, proceeds) in [("https://elsewhere.invalid/", false), ("https://test.invalid/", true)] {
            let db = Database::new();
            let store = db.store();
            let task = store.submit("test", &json!({}), None)?;
            let guard = Arc::new(store.acquire(&task.id)?);
            store.resume(&guard, task.revision)?;
            let journal = Arc::new(Journal::new(db.store(), guard, false));
            let program = fab_core::script::parse("return 1")?;
            let mut saved = state(ProgramMachine::new(program.clone())?);
            saved.arrived = Some(arrived.into());
            journal.save(&saved, None)?;
            let mut ctx = crate::api::Ctx::new(None);
            ctx.program_journal = Some(journal);
            let mut session = session();
            session.snapshot().await?;
            let result = crate::api::run_script(&mut session, &mut ctx, &program, None, None).await;
            assert_eq!(result.ok, proceeds, "{arrived}: {}", result.text);
            assert_eq!(matches!(store.get(&task.id)?.state, TaskState::Paused { .. }), !proceeds);
        }
        Ok(())
    }

    /// A page for scraper recovery: an address, a document per load, and a
    /// list whose item keys the test sets. A hanging page never finishes loading.
    #[derive(Default)]
    struct Page { url: String, doc: u32, gotos: Vec<String>, keys: Vec<&'static str>, hang: bool }
    struct Scripted(Arc<std::sync::Mutex<Page>>);
    impl PageDriver for Scripted {
        fn name(&self) -> &'static str { "scripted" }
        fn describe(&self) -> String { "scripted page".into() }
        fn page_id(&self) -> String { "page-a".into() }
        fn eval<'a>(&'a mut self, e: &'a str) -> DriverFuture<'a, Result<Value>> {
            let p = self.0.lock().unwrap();
            let e = e.rsplit_once("return await (").map_or(e, |x| x.1);
            let doc = format!("doc-{}", p.doc);
            let v = if e.starts_with("location.href") { json!(p.url) }
                else if e.starts_with("__fs.status()") { json!(200) }
                else if e.starts_with("__fs.items(") { json!(p.keys.iter().enumerate().map(|(i, k)| json!({"i": i, "key": k, "level": 0, "fields": {"title": k.to_uppercase()}})).collect::<Vec<_>>()) }
                else if e.starts_with("__fs.count(") { json!(p.keys.len()) }
                else if e.starts_with("__ub.docId") { json!(doc) }
                else if e.starts_with("__ub.snapshot") { json!({"docId": doc, "version": 1, "url": p.url, "title": "t", "els": [], "texts": []}) }
                else { json!({}) };
            Box::pin(async move { Ok(v) })
        }
        fn goto<'a>(&'a mut self, u: &'a str) -> DriverFuture<'a, Result<()>> {
            let page = self.0.clone();
            Box::pin(async move {
                if page.lock().unwrap().hang { std::future::pending::<()>().await; }
                let mut p = page.lock().unwrap();
                p.url = u.to_string();
                p.doc += 1;
                p.gotos.push(u.to_string());
                Ok(())
            })
        }
        fn close(&mut self) -> DriverFuture<'_, ()> { Box::pin(async {}) }
    }
    fn scripted(url: &str, keys: Vec<&'static str>) -> (Session, Arc<std::sync::Mutex<Page>>) {
        let page = Arc::new(std::sync::Mutex::new(Page { url: url.into(), keys, ..Default::default() }));
        let mut knobs = Knobs::default();
        knobs.shape = "off".into();
        (Session::from_browser(Browser::from_driver(Scripted(page.clone())), knobs, Decider(Jev::without_key())).unwrap(), page)
    }
    const LIST: &str = "https://test.invalid/list";
    /// A scraper that has read `items` of the list "rows" (`ul>li`, mapped
    /// for `title`, linking each item's page), plus `extra` fields.
    fn scraper(items: &[&str], extra: Value) -> crate::scrape::Scraper {
        let mut v = json!({"request": "", "lists": {"rows": {"sel": "ul>li", "span": 1, "pager": false}},
            "maps": {"ul>li": [["title"], {"title": "h2"}, "a@href"]}, "doc_maps": {}, "only_level": {}, "link_asked": ["ul>li"], "relearned": {},
            "seen": {"ul>li@": items},
            "items": items.iter().map(|k| json!({"list": "ul>li", "key": k, "i": 0, "level": 0, "fields": {"title": k}, "link": format!("https://test.invalid/item/{k}")})).collect::<Vec<_>>(),
            "real_layouts": [], "checked_layouts": [], "virt": null, "open_item": null, "fetched": [], "navs": [], "last_list": "ul>li",
            "turns": 0, "cost": 0.0, "llm_ms": 0.0, "log": []});
        for (k, x) in extra.as_object().unwrap() { v[k] = x.clone(); }
        serde_json::from_value(v).unwrap()
    }
    fn started(db: &Database) -> Result<(Store, TaskRecord, Arc<TaskGuard>)> {
        let store = db.store();
        let task = store.submit("test", &json!({}), None)?;
        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, task.revision)?;
        Ok((store, task, guard))
    }
    fn kinds(store: &Store, task: &TaskId) -> Result<Vec<(EffectKind, Value, bool)>> {
        Ok(store.effects(task)?.into_iter().map(|e| (e.kind, e.request, matches!(e.state, EffectState::Confirmed { receipt: crate::task_store::Receipt::Applied { .. } }))).collect())
    }

    /// A run interrupted on an item's page (opened for real from the list):
    /// the program suspended at its `back`, and as it was at the `open`.
    fn opened(emitted: usize) -> Result<(fab_core::script::Program, State)> {
        use fab_core::program_machine::Response;
        use fab_core::script::Value as V;
        let program = fab_core::script::parse("for x in items \"rows\"\n  open x\n  back\nend\nreturn \"done\"")?;
        let list = scraper(&["a"], json!({}));
        let mut machine = ProgramMachine::new(program.clone())?;
        let Event::Request(asked) = machine.advance() else { panic!("expected items") };
        machine.complete(&asked.token, Ok(Response::Value(V::List(vec![V::Item(0)]))))?;
        let Event::Request(asked) = machine.advance() else { panic!("expected the item") };
        machine.complete(&asked.token, Ok(Response::Value(list.record(0))))?;
        let Event::Request(asked) = machine.advance() else { panic!("expected open") };
        let at_open = machine.clone();
        machine.complete(&asked.token, Ok(Response::Done))?;
        let mut saved = state(machine);
        saved.scraper = scraper(&["a"], json!({"open_item": 0, "navs": [{"Url": LIST}]}));
        saved.emitted = emitted;
        saved.binding = Some(PageBinding { page: "gone".into(), document: "gone".into(), revision: 1, url: None });
        saved.reopen = Some(Reopen { machine: at_open, scraper: list, emitted: 0, acted: false });
        Ok((program, saved))
    }
    async fn run(journal: Arc<Journal>, session: &mut Session, program: &fab_core::script::Program) -> crate::api::Reply {
        let mut ctx = crate::api::Ctx::new(None);
        ctx.program_journal = Some(journal);
        crate::api::run_script(session, &mut ctx, program, None, None).await
    }

    #[tokio::test]
    async fn adopting_with_an_open_detail_page_returns_to_the_list_and_opens_the_item_again() -> Result<()> {
        let db = Database::new();
        let (store, task, guard) = started(&db)?;
        let journal = Arc::new(Journal::new(db.store(), guard, true));
        let (program, saved) = opened(0)?;
        journal.save(&saved, None)?;
        let (mut session, page) = scripted("https://elsewhere.invalid/", vec![]);
        let reply = run(journal, &mut session, &program).await;
        assert!(reply.ok, "{}", reply.text);
        assert_eq!(reply.value, json!("done"));
        let item = "https://test.invalid/item/a";
        assert_eq!(page.lock().unwrap().gotos, [LIST, item, LIST], "back to the list, the item opened again, and back");
        let effects = kinds(&store, &task.id)?;
        assert_eq!((effects[0].0, &effects[0].1, effects[0].2), (EffectKind::ProgramReturn, &json!({"url": LIST}), true));
        assert!(effects[1].0 == EffectKind::ProgramRequest && effects[1].1["kind"].get("Open").is_some(), "the program opens the item again: {:?}", effects[1].1);
        Ok(())
    }

    #[tokio::test]
    async fn opening_again_is_refused_when_it_would_repeat_work() -> Result<()> {
        for acted in [false, true] {
            let db = Database::new();
            let (store, task, guard) = started(&db)?;
            let journal = Arc::new(Journal::new(db.store(), guard, true));
            // A record came from the item's page, or a step that may act ran there.
            let (program, mut saved) = opened(if acted { 0 } else { 1 })?;
            saved.reopen.as_mut().unwrap().acted = acted;
            journal.save(&saved, None)?;
            let (mut session, page) = scripted("https://elsewhere.invalid/", vec![]);
            let reply = run(journal, &mut session, &program).await;
            assert_eq!(reply.error.map(|e| e.code), Some(crate::events::ErrorCode::Paused));
            assert!(matches!(store.get(&task.id)?.state, TaskState::Paused { .. }));
            assert!(page.lock().unwrap().gotos.is_empty() && store.effects(&task.id)?.is_empty());
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_return_interrupted_mid_navigation_is_recoverable() -> Result<()> {
        use crate::task_commands::{ResolutionCommand, TaskCommand, execute_in_session};
        let db = Database::new();
        let (store, task, guard) = started(&db)?;
        let journal = Arc::new(Journal::new(db.store(), guard.clone(), true));
        let (program, saved) = opened(0)?;
        journal.save(&saved, None)?;
        let (mut session, page) = scripted("https://elsewhere.invalid/", vec![]);
        page.lock().unwrap().hang = true;
        // The process stops while the list page loads.
        assert!(tokio::time::timeout(std::time::Duration::from_millis(300), run(journal, &mut session, &program)).await.is_err());
        drop(guard);
        let effect = store.get(&task.id)?.pending_effect.expect("the return is in flight");
        assert_eq!((effect.kind, effect.state.clone()), (EffectKind::ProgramReturn, crate::task_store::EffectState::Dispatched));
        let interrupted = store.recover_interrupted(&store.acquire(&task.id)?)?;
        // Asserting the return: the program continues from before the open, on the list page.
        let (checkpoint, _) = resolve_return(&interrupted, &json!(true))?;
        let asserted = parse(&checkpoint.payload()["program"])?;
        assert!(matches!(asserted.machine.pending().map(|p| &p.kind), Some(fab_core::program_machine::RequestKind::Open { .. })));
        assert_eq!((asserted.arrived.as_deref(), asserted.binding.is_none(), asserted.reopen.is_none()), (Some(LIST), true, true));
        // Or it did not happen: resuming returns again, and finishes.
        execute_in_session(&store, "test", TaskCommand::Resolve { task: task.id.clone(), effect: effect.id.clone(), resolution: ResolutionCommand::NotApplied { reason: "the list never loaded".into() } })?;
        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, store.get(&task.id)?.revision)?;
        page.lock().unwrap().hang = false;
        let reply = run(Arc::new(Journal::new(db.store(), guard, true)), &mut session, &program).await;
        assert!(reply.ok, "{}", reply.text);
        assert_eq!(page.lock().unwrap().gotos, [LIST, "https://test.invalid/item/a", LIST]);
        let effects = kinds(&store, &task.id)?;
        assert_eq!(effects.iter().filter(|e| e.0 == EffectKind::ProgramReturn).map(|e| e.2).collect::<Vec<_>>(), [false, true]);
        Ok(())
    }

    /// A run interrupted with `source`'s first request uncertain, on page-a
    /// (showing document `doc-0`), after reading `read` of the list "rows".
    fn interrupted(db: &Database, source: &str, read: &[&str]) -> Result<(Store, TaskRecord, EffectId, fab_core::script::Program)> {
        interrupted_on(db, source, read, "page-a")
    }
    fn interrupted_on(db: &Database, source: &str, read: &[&str], bound: &str) -> Result<(Store, TaskRecord, EffectId, fab_core::script::Program)> {
        let (store, task, guard) = started(db)?;
        let journal = Journal::new(db.store(), guard.clone(), false);
        let program = fab_core::script::parse(source)?;
        let mut saved = state(ProgramMachine::new(program.clone())?);
        saved.scraper = scraper(read, json!({}));
        saved.binding = Some(PageBinding { page: bound.into(), document: "doc-0".into(), revision: 1, url: Some(LIST.into()) });
        let Event::Request(asked) = saved.machine.advance() else { panic!("expected a request") };
        let pending = journal.begin(&saved, EffectKind::ProgramRequest, &serde_json::to_value(&asked)?)?;
        store.interrupt(&guard, journal.task()?.revision, "stopped mid-request")?;
        Ok((store, task, pending.id().clone(), program))
    }
    /// A run interrupted with its first `open` (or `back`) request
    /// uncertain, on page-a showing document `doc-0`, with the saved
    /// continuation carrying `extra` scraper state.
    fn interrupted_at(db: &Database, source: &str, open: bool, extra: Value) -> Result<(Store, TaskRecord, EffectId, fab_core::script::Program)> {
        use fab_core::program_machine::{RequestKind, Response};
        use fab_core::script::{Eff, Value as V};
        let (store, task, guard) = started(db)?;
        let journal = Journal::new(db.store(), guard.clone(), false);
        let program = fab_core::script::parse(source)?;
        let mut saved = state(ProgramMachine::new(program.clone())?);
        saved.scraper = scraper(&["a"], extra);
        saved.binding = Some(PageBinding { page: "page-a".into(), document: "doc-0".into(), revision: 1, url: Some(LIST.into()) });
        // Advance to the request we want to interrupt, answering what comes
        // before it the way the runtime would.
        let asked = loop {
            let Event::Request(r) = saved.machine.advance() else { panic!("expected a request") };
            let wanted = match (&r.kind, open) {
                (RequestKind::Open { .. }, true) | (RequestKind::Back, false) => true,
                _ => false,
            };
            if wanted {
                break r;
            }
            let response = match &r.kind {
                RequestKind::Effect { kind: Eff::Items, .. } => Response::Value(V::List(vec![V::Item(0)])),
                // The handle becomes the item's own value before the body runs.
                RequestKind::ResolveItem { .. } => Response::Value(V::Obj([("title".into(), V::Str("a".into())), ("__item".into(), V::Item(0))].into())),
                RequestKind::ForNextExhausted { .. } => Response::IteratorItems(vec![]),
                _ => Response::Done,
            };
            saved.machine.complete(&r.token, Ok(response))?;
        };
        let pending = journal.begin(&saved, EffectKind::ProgramRequest, &serde_json::to_value(&asked)?)?;
        store.interrupt(&guard, journal.task()?.revision, "stopped mid-request")?;
        Ok((store, task, pending.id().clone(), program))
    }
    /// The same, for a run whose first `open` is the uncertain request.
    fn interrupted_at_open(db: &Database, source: &str) -> Result<(Store, TaskRecord, EffectId, fab_core::script::Program)> {
        interrupted_at(db, source, true, json!({"navs": [{"Url": LIST}]}))
    }
    fn code(e: &anyhow::Error) -> crate::events::ErrorCode { Failure::of(e).code }

    #[tokio::test]
    async fn an_uncertain_next_page_is_resolved_from_what_the_page_shows() -> Result<()> {
        let source = "if next page\n  return \"more\"\nend\nreturn \"end\"";
        // Unread items: the page moved on, and they are read next.
        let db = Database::new();
        let (store, task, effect, program) = interrupted(&db, source, &["a", "b"])?;
        let (mut session, _) = scripted(LIST, vec!["c", "d"]);
        let resolved = resolve_observed(&store, &task.id, &effect, "checked the page", &mut session).await?;
        assert!(matches!(resolved.state, TaskState::Paused { .. }) && resolved.pending_effect.is_none());
        let receipt = store.effects(&task.id)?.pop().unwrap().state;
        let crate::task_store::EffectState::Confirmed { receipt: crate::task_store::Receipt::Applied { value, .. } } = receipt else { panic!("applied") };
        assert_eq!((&value["response"], &value["evidence"]["unseen"]), (&json!({"Ok": {"Value": {"Bool": true}}}), &json!(["c", "d"])));
        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, resolved.revision)?;
        let reply = run(Arc::new(Journal::new(db.store(), guard, false)), &mut session, &program).await;
        assert_eq!(reply.value, json!("more"), "{}", reply.text);

        // Only items already read: it did not move on, and runs again.
        let db = Database::new();
        let (store, task, effect, _) = interrupted(&db, source, &["a", "b"])?;
        let before = store.get(&task.id)?.checkpoint;
        let (mut session, _) = scripted(LIST, vec!["a", "b"]);
        let resolved = resolve_observed(&store, &task.id, &effect, "checked the page", &mut session).await?;
        assert!(resolved.pending_effect.is_none());
        assert_eq!(resolved.checkpoint.map(|c| c.payload().clone()), before.map(|c| c.payload().clone()));
        assert!(matches!(store.effects(&task.id)?.pop().unwrap().state, crate::task_store::EffectState::Confirmed { receipt: crate::task_store::Receipt::NotApplied { .. } }));

        // None of the list, or another browser page: nothing can be told.
        for (keys, bound) in [(vec![], "page-a"), (vec!["c"], "page-b")] {
            let db = Database::new();
            let (store, task, effect, _) = interrupted_on(&db, source, &["a", "b"], bound)?;
            let (mut session, _) = scripted(LIST, keys);
            let err = resolve_observed(&store, &task.id, &effect, "checked the page", &mut session).await.unwrap_err();
            assert_eq!(code(&err), crate::events::ErrorCode::InvalidArgs, "{err:#}");
            assert!(store.get(&task.id)?.pending_effect.is_some(), "still uncertain");
        }
        Ok(())
    }

    /// A navigation the page can confirm: `open` and `back` only move the
    /// browser, so the address on screen says whether the request happened.
    /// The other answer is never proof that it did not (the tab may have
    /// moved on), so it stays `not_applied` and repeats a GET.
    #[tokio::test]
    async fn an_uncertain_navigation_is_resolved_by_the_address_on_screen() -> Result<()> {
        let open = "for x in items \"rows\"\n  open x\n  return \"done\"\nend";
        // The page on the item's address settles `open` applied.
        let db = Database::new();
        let (store, task, effect, program) = interrupted_at_open(&db, open)?;
        let (mut session, _) = scripted("https://test.invalid/item/a", vec![]);
        let resolved = resolve_observed(&store, &task.id, &effect, "checked the page", &mut session).await?;
        assert!(resolved.pending_effect.is_none());
        let crate::task_store::EffectState::Confirmed { receipt: crate::task_store::Receipt::Applied { value, .. } } = store.effects(&task.id)?.pop().unwrap().state else { panic!("the page was on the address `open` was going to, so it was applied") };
        assert_eq!(value["response"], json!({"Ok": "Done"}));
        // The run continues from there instead of opening the page again.
        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, resolved.revision)?;
        let reply = run(Arc::new(Journal::new(db.store(), guard, false)), &mut session, &program).await;
        assert_eq!(reply.value, json!("done"), "{}", reply.text);

        // Somewhere else: not proof either way, so it runs again.
        let db = Database::new();
        let (store, task, effect, _) = interrupted_at_open(&db, open)?;
        let (mut session, _) = scripted(LIST, vec![]);
        let resolved = resolve_observed(&store, &task.id, &effect, "checked the page", &mut session).await?;
        assert!(resolved.pending_effect.is_none());
        assert!(matches!(store.effects(&task.id)?.pop().unwrap().state, crate::task_store::EffectState::Confirmed { receipt: crate::task_store::Receipt::NotApplied { .. } }));

        // `back` to the list page: the page on it settles it applied.
        let db = Database::new();
        let (store, task, effect, _) = interrupted_at(&db, "back\nreturn \"done\"", false, json!({"navs": [{"Url": LIST}]}))?;
        let (mut session, _) = scripted(LIST, vec![]);
        let resolved = resolve_observed(&store, &task.id, &effect, "checked the page", &mut session).await?;
        assert!(resolved.pending_effect.is_none());
        assert!(matches!(store.effects(&task.id)?.pop().unwrap().state, crate::task_store::EffectState::Confirmed { receipt: crate::task_store::Receipt::Applied { .. } }));

        // A `back` with no list page to return to cannot be told from the page.
        let db = Database::new();
        let (store, task, effect, _) = interrupted_at(&db, "back\nreturn \"done\"", false, json!({}))?;
        let err = resolve_observed(&store, &task.id, &effect, "checked the page", &mut scripted(LIST, vec![]).0).await.unwrap_err();
        assert_eq!(code(&err), crate::events::ErrorCode::InvalidArgs, "{err:#}");
        Ok(())
    }

    #[tokio::test]
    async fn an_uncertain_items_request_is_answered_with_the_items_the_page_shows() -> Result<()> {
        let source = "for x in items \"rows\"\n  emit x.title\nend";
        let db = Database::new();
        let (store, task, effect, program) = interrupted(&db, source, &["a"])?;
        let (mut session, _) = scripted(LIST, vec!["c"]);
        let resolved = resolve_observed(&store, &task.id, &effect, "checked the page", &mut session).await?;
        let guard = Arc::new(store.acquire(&task.id)?);
        store.resume(&guard, resolved.revision)?;
        let reply = run(Arc::new(Journal::new(db.store(), guard, false)), &mut session, &program).await;
        assert!(reply.ok, "{}", reply.text);
        let records: Vec<Value> = store.output(&task.id, None)?.into_iter().map(|o| o.value["data"].clone()).collect();
        assert_eq!(records, [json!({"title": "C"})]);

        // A list the run never identified: answering would need the model.
        let db = Database::new();
        let (store, task, effect, _) = interrupted(&db, "for x in items \"other\"\n  emit x\nend", &["a"])?;
        let (mut session, _) = scripted(LIST, vec!["c"]);
        let err = resolve_observed(&store, &task.id, &effect, "checked the page", &mut session).await.unwrap_err();
        assert_eq!(code(&err), crate::events::ErrorCode::InvalidArgs, "{err:#}");
        // The page moved to another document since: refused too.
        let db = Database::new();
        let (store, task, effect, _) = interrupted(&db, source, &["a"])?;
        let (mut session, page) = scripted(LIST, vec!["c"]);
        page.lock().unwrap().doc = 7;
        let err = resolve_observed(&store, &task.id, &effect, "checked the page", &mut session).await.unwrap_err();
        assert_eq!(code(&err), crate::events::ErrorCode::InvalidArgs, "{err:#}");
        // Other requests, and scalar evidence for these, are refused.
        let db = Database::new();
        let (store, task, effect, _) = interrupted(&db, "read \"price\" -> p\nreturn p", &[])?;
        assert!(resolve_observed(&store, &task.id, &effect, "checked", &mut scripted(LIST, vec![]).0).await.is_err());
        let db = Database::new();
        let (store, task, _, _) = interrupted(&db, "if next page\n  return 1\nend", &["a"])?;
        assert!(resolve_value(&store.get(&task.id)?, &json!(true)).is_err(), "scalar evidence for `next page`");
        Ok(())
    }
}
