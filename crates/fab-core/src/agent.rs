//! The public API an LLM drives: goto / act / run / extract / observe.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::json;
use std::time::Instant;

use crate::backend::Browser;
use crate::config::Knobs;
use crate::dataset::{DecisionCase, Gold};
use crate::decide::{self, Decider, FillValue, Plan};
use crate::direct::{self, Cmd};
use crate::license;
use crate::snapshot::Kind;
use crate::jev::{Answers, Jev, Questions};
use crate::snapshot::Snapshot;

#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct Timings {
    pub total_ms: f64,
    pub snapshot_ms: f64,
    pub decide_ms: f64,
    pub exec_ms: f64,
    pub settle_ms: f64,
    pub decide_calls: u32,
    pub input_tokens: u64,
    pub cost: f64,
    /// Small-LLM clarification calls made inside the engine.
    #[serde(default)]
    pub llm_calls: u32,
    #[serde(default)]
    pub llm_ms: f64,
    #[serde(default)]
    pub llm_cost: f64,
}

impl Timings {
    fn add_answers(&mut self, a: &Answers) {
        self.decide_ms += a.latency.as_secs_f64() * 1e3;
        self.decide_calls += 1;
        self.input_tokens += a.usage.input_tokens;
        self.cost += a.usage.cost.unwrap_or(0.0);
    }

    pub fn merge(&mut self, o: &Timings) {
        self.total_ms += o.total_ms;
        self.snapshot_ms += o.snapshot_ms;
        self.decide_ms += o.decide_ms;
        self.exec_ms += o.exec_ms;
        self.settle_ms += o.settle_ms;
        self.decide_calls += o.decide_calls;
        self.input_tokens += o.input_tokens;
        self.cost += o.cost;
        self.llm_calls += o.llm_calls;
        self.llm_ms += o.llm_ms;
        self.llm_cost += o.llm_cost;
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ActResult {
    pub ok: bool,
    pub instruction: String,
    pub actions: Vec<String>,
    pub steps: usize,
    /// Decider's final estimate that the instruction is done.
    pub done: f64,
    /// Lowest click confidence across steps.
    pub confidence: f64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<(String, f64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The failure came after input that may have reached the page: the
    /// outcome is unknown, so the action must not be repeated automatically.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub uncertain: bool,
    pub url: String,
    pub title: String,
    pub timings: Timings,
    /// Pre-commit form audits (see `Knobs::audit`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audits: Vec<serde_json::Value>,
    /// Ended on a commit the audit passed with this P(match), and the page
    /// changed after it with no error shown.
    #[serde(skip)]
    pub verified_commit: Option<f64>,
}

/// Whether an action error came after input that may have reached the page.
pub fn may_have_executed(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| matches!(cause.downcast_ref::<crate::backend::driver::InputError>(), Some(crate::backend::driver::InputError::MayHaveExecuted(_))))
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExtractResult {
    pub answer: Option<String>,
    pub p: f64,
    /// Probability the page contains an answer at all.
    pub exists: f64,
    pub alternatives: Vec<(String, f64)>,
    pub timings: Timings,
}

pub struct Session {
    identity: crate::domain::PageId,
    pub browser: Browser,
    pub decider: Decider,
    pub k: Knobs,
    last: Option<Snapshot>,
    /// Raw decision trace of the last act, for debugging.
    pub trace: Vec<serde_json::Value>,
    pub keep_trace: bool,
    /// When set, every Jev decision is captured as a labelled case (the label is
    /// what was executed; callers keep cases only from passing runs).
    pub recorder: Option<Vec<DecisionCase>>,
    /// The listing the caller last saw (ids in instructions refer to it).
    pub seen: Option<Snapshot>,
    /// The goal being carried out by `do_goal`, as context for its steps.
    pub goal_ctx: Option<String>,
    /// Running one step of a `do` program: the goal grounds referents only.
    step_scoped: bool,
    /// Live activity feed (decisions and actions as they happen), for watchers.
    pub live: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
    /// Speculative runs: actions wait until the gate allows them (see [`Gate`]).
    pub gate: Option<tokio::sync::watch::Receiver<Gate>>,
    /// Form audits of the current `do_goal` run, reported with its result.
    audit_log: Vec<serde_json::Value>,
    /// Commit clicks (R2/R3) executed so far in this session.
    pub commits: usize,
    /// The user's own task, when known (agent mode). Checks of what is about
    /// to be submitted use it rather than a planner's rewritten sub-goal.
    pub task: Option<String>,
    /// (combobox key, value) suggestions picked in the current act: a picked
    /// value becomes a chip and the input empties, which looks unfilled.
    picked: std::collections::HashSet<(usize, String)>,
    /// Client for `Knobs::clarify`, created on first use.
    clarifier: Option<crate::llm::Llm>,
    /// Site shapes (see `shape.rs`), when `Knobs::shape` isn't "off".
    shapes: Option<std::sync::Arc<std::sync::Mutex<crate::shape::ShapeStore>>>,
    /// A navigation click awaiting its outcome: (site, from template, control).
    pending_nav: Option<(String, u32, u64)>,
}

/// Makes pages in one browser: each a [`Session`] of its own (snapshot,
/// decisions, live feed) on its own tab, sharing the browser, its cookies and
/// logins, the decision client and site shapes.
#[derive(Clone)]
pub struct Pages {
    handle: crate::backend::Handle,
    k: Knobs,
    jev: Jev,
    shapes: Option<std::sync::Arc<std::sync::Mutex<crate::shape::ShapeStore>>>,
}

impl Pages {
    /// Whether the browser can hold more than one page.
    pub fn multi(&self) -> bool {
        self.handle.multi()
    }

    pub async fn open(&self) -> Result<Session> {
        let browser = self.handle.page().await?;
        Ok(Session { identity: crate::domain::PageId::new()?, browser, decider: Decider(self.jev.clone()), k: self.k.clone(), last: None, trace: vec![], keep_trace: false, recorder: None, seen: None, goal_ctx: None, step_scoped: false, live: None, gate: None, audit_log: vec![], commits: 0, task: None, picked: Default::default(), clarifier: None, shapes: self.shapes.clone(), pending_nav: None })
    }

    /// Closes the whole browser.
    pub async fn shutdown(&self) {
        self.handle.shutdown().await;
    }
}

/// What a clarification decided.
enum Clarified {
    Pick(usize),
    Done,
}

const CLARIFY_SYSTEM: &str = "You are the judgment step of a fast browser automation engine. It is carrying out a task on a web page and is unsure what to do next. Choose exactly one option: the element to click next, `done` if the task is already fully complete, or `none` if no option is right. Reply with JSON only: {\"pick\": \"<option id>\"}.";

/// Lets a run start deciding before it is known what the task needs. Nothing
/// executes while `Hold`; `ReadOnly` allows everything short of a commit
/// (R2/R3 clicks wait); `Release` allows all; `Abort` stops the run before its
/// next action, leaving the page as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Hold,
    ReadOnly,
    Release,
    Abort,
}

impl Session {
    pub async fn new(k: Knobs) -> Result<Self> {
        // Without a key the session still runs precise commands (see `jev::NO_KEY`).
        let mut j = Jev::from_env().unwrap_or_else(|_| Jev::without_key());
        j.hedge = k.hedge;
        j.hedge_q = k.hedge_q;
        let decider = Decider(j);
        // Launch the browser and warm the decider connection concurrently.
        let warm = async {
            if decider.0.has_key() { decider.0.warm().await.map(|_| ()) } else { Ok(()) }
        };
        let (browser, w) = tokio::join!(Browser::launch(&k), warm);
        if let Err(e) = w { tracing::warn!("decider warmup failed: {e}"); }
        Self::from_browser(browser?, k, decider)
    }

    /// Attach an owned page driver without launching a browser or consulting
    /// environment credentials. This is also the driver conformance seam.
    pub fn from_browser(browser: Browser, k: Knobs, decider: Decider) -> Result<Self> {
        let shapes = (k.shape != "off").then(|| {
            let dir = if k.shape_dir.is_empty() { crate::shape::ShapeStore::default_dir() } else { k.shape_dir.clone().into() };
            crate::shape::shared(dir)
        });
        Ok(Self { identity: crate::domain::PageId::new()?, browser, decider, k, last: None, trace: vec![], keep_trace: false, recorder: None, seen: None, goal_ctx: None, step_scoped: false, live: None, gate: None, audit_log: vec![], commits: 0, task: None, picked: Default::default(), clarifier: None, shapes, pending_nav: None })
    }

    pub async fn close(&self) {
        self.browser.close().await;
    }

    /// Opens more pages in this session's browser (see [`Pages`]).
    pub fn pages(&self) -> Pages {
        Pages { handle: self.browser.handle(), k: self.k.clone(), jev: self.decider.0.clone(), shapes: self.shapes.clone() }
    }

    /// Starts over on a fresh blank tab (the current one crashed or hung).
    pub async fn renew(&mut self) -> Result<()> {
        self.browser.renew().await?;
        self.invalidate();
        self.pending_nav = None;
        Ok(())
    }

    fn live(&self, line: impl FnOnce() -> String) {
        if let Some(f) = &self.live {
            f(line());
        }
    }

    /// Waits until the gate allows an action of this kind; false = aborted.
    async fn gate_allows(&self, commit: bool) -> bool {
        let Some(g) = &self.gate else { return true };
        let mut g = g.clone();
        let ok = |v: Gate| match v {
            Gate::Abort | Gate::Release => true,
            Gate::ReadOnly => !commit,
            Gate::Hold => false,
        };
        let v = match g.wait_for(|v| ok(*v)).await {
            Ok(v) => *v,
            Err(_) => return true,
        };
        v != Gate::Abort
    }

    fn live_actions(&self, actions: &[String]) {
        for a in actions {
            self.live(|| format!("  → {a}"));
        }
    }

    pub async fn snapshot(&mut self) -> Result<&Snapshot> {
        let arg = match (&self.last, self.k.snapshot_cache) {
            (Some(s), true) => json!({"since": s.version, "doc": s.doc_id}),
            _ => json!({}),
        };
        let mut v = self.browser.eval(&format!("__ub.snapshot({arg})")).await?;
        if v.get("same").is_none() {
            // A page that echoes a typed secret back never shows it to anyone.
            crate::secrets::redact::value(&mut v);
            self.last = Some(serde_json::from_value(v).context("bad snapshot")?);
            self.observe_shape();
        }
        Ok(self.last.as_ref().unwrap())
    }

    /// The task's literals, masked out of everything the shape cache keeps.
    fn shape_literals(&self) -> Vec<String> {
        let t = self.task.clone().or_else(|| self.goal_ctx.clone()).unwrap_or_default();
        crate::shape::literals_of(&t)
    }

    /// Learns the current page's template, and the edge a pending navigation
    /// click just took.
    fn observe_shape(&mut self) {
        if !matches!(self.k.shape.as_str(), "on" | "record") {
            return;
        }
        let lits = self.shape_literals();
        let (Some(store), Some(snap)) = (self.shapes.as_ref(), self.last.as_ref()) else { return };
        let Some(site) = crate::shape::site_key(&snap.url) else { return };
        let key = crate::shape::template_key(snap, &lits);
        let desc = crate::shape::template_desc(snap, &lits);
        let mut store = store.lock().unwrap();
        let to = store.note_template(&site, &key, &desc);
        if let Some((s, from, ctl)) = self.pending_nav.take() {
            if s == site && from != to {
                store.note_edge(&site, from, ctl, to);
            }
        }
    }

    /// Before clicking `i` on `snap`: remember it, if it is a navigation
    /// control, so the page it leads to becomes a learned edge.
    fn note_nav(&mut self, i: usize, snap: &Snapshot) {
        if !matches!(self.k.shape.as_str(), "on" | "record") {
            return;
        }
        let lits = self.shape_literals();
        let Some(store) = self.shapes.as_ref() else { return };
        let Some(e) = snap.els.iter().find(|e| e.i == i) else { return };
        let (Some(site), Some(ctl)) = (crate::shape::site_key(&snap.url), crate::shape::control_hash(e, &lits)) else { return };
        let key = crate::shape::template_key(snap, &lits);
        let desc = crate::shape::template_desc(snap, &lits);
        let from = store.lock().unwrap().note_template(&site, &key, &desc);
        self.pending_nav = Some((site, from, ctl));
    }

    /// Writes what was learned about site shapes.
    pub fn flush_shapes(&mut self) {
        if let Some(s) = self.shapes.as_ref() {
            s.lock().unwrap().flush();
        }
    }

    /// Goes straight to the known page where `task` belongs: one Jev choice
    /// over the pages reachable from here, then the learned navigation path,
    /// checking the template at every hop. Commits nothing (edges are R0/R1
    /// controls only). Returns whether it moved.
    pub async fn shape_navigate(&mut self, task: &str, tm: &mut Timings) -> Result<bool> {
        if !matches!(self.k.shape.as_str(), "on" | "use") || self.shapes.is_none() {
            return Ok(false);
        }
        let snap = self.snapshot().await?.clone();
        let lits = crate::shape::literals_of(task);
        let Some(site) = crate::shape::site_key(&snap.url) else { return Ok(false) };
        let key = crate::shape::template_key(&snap, &lits);
        let (cur, cur_desc, reach) = {
            let mut store = self.shapes.as_ref().unwrap().lock().unwrap();
            let s = store.site(&site);
            let Some(cur) = s.find(&key) else { return Ok(false) };
            let reach: Vec<(u32, String)> = s.reachable(cur, 60).into_iter().map(|(t, _)| (t, s.templates[t as usize].desc.clone())).collect();
            (cur, s.templates[cur as usize].desc.clone(), reach)
        };
        // Worth a question only when something is at least two clicks away.
        let far = {
            let mut store = self.shapes.as_ref().unwrap().lock().unwrap();
            let s = store.site(&site);
            s.reachable(cur, 60).iter().any(|(_, d)| *d >= 2)
        };
        if reach.is_empty() || !far {
            return Ok(false);
        }
        let mut q = Questions::default();
        let opts = std::iter::once(("here".to_string(), Some(format!("The current page: {cur_desc}"))))
            .chain(reach.iter().map(|(t, d)| (format!("t{t}"), Some(d.clone()))))
            .chain(std::iter::once(("none".to_string(), Some("None of these pages; it is somewhere else.".to_string()))));
        q.choice(
            "page",
            "`pages` are pages of this website seen before. On which page is `task` carried out, or does it show what `task` asks about? Pick `here` if the current page is the place to start.",
            opts,
        );
        let state = json!({"task": task, "current_page": cur_desc});
        let t0 = Instant::now();
        let a = self.decider.ask(&state, &q).await?;
        tm.add_answers(&a);
        let Some((pick, p, _)) = a.choice("page") else { return Ok(false) };
        let Some(target) = pick.strip_prefix('t').and_then(|n| n.parse::<u32>().ok()) else {
            self.live(|| format!("site map: stay ({pick}, {:.0} ms)", t0.elapsed().as_secs_f64() * 1e3));
            return Ok(false);
        };
        if p < self.k.shape_tau {
            self.live(|| format!("site map: unsure (p={p:.2}), not jumping"));
            return Ok(false);
        }
        let path = self.shapes.as_ref().unwrap().lock().unwrap().site(&site).path(cur, target);
        let Some(path) = path else { return Ok(false) };
        // One hop is what the engine's own first decision costs anyway.
        if path.len() < 2 {
            self.live(|| "site map: target is one click away; leaving it to the engine".to_string());
            return Ok(false);
        }
        if !self.gate_allows(false).await {
            return Ok(false);
        }
        let desc = reach.iter().find(|(t, _)| *t == target).map(|(_, d)| d.clone()).unwrap_or_default();
        self.live(|| format!("site map: going to {} ({} hop{}, p={p:.2})", crate::snapshot::truncate(&desc, 60), path.len(), if path.len() == 1 { "" } else { "s" }));
        let mut moved = false;
        for e in path {
            let snap = self.snapshot().await?.clone();
            let lits = crate::shape::literals_of(task);
            let live: Vec<usize> = snap
                .els
                .iter()
                .filter(|x| !x.has_flag("disabled") && !x.has_flag("covered") && !x.latent())
                .filter(|x| crate::shape::control_hash(x, &lits) == Some(e.ctl))
                .map(|x| x.i)
                .collect();
            let [i] = live.as_slice() else { break };
            let doc = snap.doc_id.clone();
            self.note_nav(*i, &snap);
            self.browser.click(*i, self.k.exec).await?;
            self.browser.settle(&self.k, &doc).await?;
            moved = true;
            let now = self.snapshot().await?.clone();
            let want = self.shapes.as_ref().unwrap().lock().unwrap().site(&site).templates.get(e.to as usize).map(|t| t.key.clone());
            if want.as_deref() != Some(crate::shape::template_key(&now, &lits).as_str()) {
                self.live(|| "site map: the page differs from what was learned; continuing from here".to_string());
                break;
            }
        }
        Ok(moved)
    }

    /// Forgets the cached snapshot, after the page changed behind the
    /// session's back (tab switch, history, reload).
    pub fn invalidate(&mut self) {
        self.last = None;
        self.seen = None;
    }

    /// Navigate and return a compact page summary.
    pub async fn goto(&mut self, url: &str) -> Result<String> {
        self.last = None;
        self.browser.goto(url, &self.k).await?;
        self.page_summary().await
    }

    /// Page summary for the caller; its element ids become the ones the next
    /// instructions are resolved against.
    pub async fn page_summary(&mut self) -> Result<String> {
        let out = self.snapshot().await?.summary(60, 1500);
        self.seen = self.last.clone();
        Ok(out)
    }

    /// The last snapshot taken (what a caller most recently saw).
    pub fn last_snapshot(&self) -> Option<&Snapshot> {
        self.last.as_ref()
    }

    /// Page view after an action: what changed since `before`, then the page summary.
    pub async fn page_after(&mut self, before: Option<&Snapshot>) -> Result<String> {
        let snap = self.snapshot().await?;
        let mut out = String::new();
        if let Some(b) = before {
            let same_doc = b.doc_id == snap.doc_id;
            if same_doc {
                let ch = snap.changes_since(b, 3000);
                if b.url != snap.url {
                    out.push_str(&format!("(address changed to {} on the same page)\n", snap.url));
                }
                if !ch.is_empty() {
                    out.push_str("--- new on page ---\n");
                    out.push_str(&ch);
                } else {
                    out.push_str("(no visible change on the page)\n");
                }
            } else {
                out.push_str("(navigated to a new page)\n");
            }
        }
        out.push_str("--- page ---\n");
        // Controls the action revealed come first in the listing. "New" by
        // content (role, name, context), not node key: apps that re-render the
        // whole view give every element a new node.
        let fresh: std::collections::HashSet<usize> = match before {
            Some(b) if b.doc_id == snap.doc_id => {
                let fp = |e: &crate::snapshot::El| (e.r.clone(), e.n.clone(), e.c.clone());
                let old: std::collections::HashSet<_> = b.els.iter().map(fp).collect();
                snap.els.iter().filter(|e| !old.contains(&fp(e))).map(|e| e.i).collect()
            }
            _ => Default::default(),
        };
        out.push_str(&snap.summary_first(60, if out.len() > 200 { 600 } else { 1500 }, &fresh));
        self.seen = self.last.clone();
        Ok(out)
    }

    /// Carry out one high-level instruction, looping snapshot → decide → execute
    /// until the decider says it's done (or nothing is left to do).
    pub async fn act(&mut self, instr: &str) -> ActResult {
        let orig = instr;
        let t0 = Instant::now();
        let mut r = ActResult { instruction: instr.to_string(), confidence: 1.0, ..Default::default() };
        self.trace.clear();
        self.picked.clear();
        // Ids in the instruction refer to the listing the caller saw.
        let seen = self.seen.clone().or_else(|| self.last.clone());
        let res = match direct::parse(instr) {
            // A precise command executes exactly what it says, or fails with the
            // reason; it is never rewritten into an open-ended decision loop.
            Some(cmds) => {
                let res = self.act_direct(&cmds, seen.as_ref(), &mut r).await;
                self.live_actions(&r.actions);
                res
            }
            None => {
                let instr = direct::describe_ids(&direct::sanitize(instr), seen.as_ref());
                let pre = self.last.clone();
                let res = if self.k.engine == "dvm" {
                    self.act_dvm(&instr, &mut r).await
                } else {
                    self.act_inner(&instr, &mut r).await
                };
                if res.is_ok() && r.ok && !conditional(orig) {
                    // Literals the caller wrote, not ones introduced by rewriting ids.
                    if let Some(l) = unused_literal(&direct::sanitize(orig), &r.actions, self.last.as_ref()) {
                        // A field that held it before acting counts: the browser
                        // restored the form (back/forward cache), nothing to type.
                        let held = pre.as_ref().is_some_and(|s| s.els.iter().any(|e| e.v.as_deref().is_some_and(|v| norm_lc(v) == norm_lc(&l))));
                        if !held {
                            r.ok = false;
                            r.error = Some(format!("\"{l}\" was never used: no field took it and nothing acted on matches it"));
                        }
                    }
                }
                res
            }
        };
        if let Err(e) = res {
            r.ok = false;
            r.uncertain = may_have_executed(&e);
            r.error = Some(format!("{e:#}"));
        }
        if let Some(s) = &self.last {
            r.url = s.url.clone();
            r.title = s.title.clone();
        }
        r.timings.total_ms = t0.elapsed().as_secs_f64() * 1e3;
        self.audit_log.extend(r.audits.iter().cloned());
        self.flush_shapes();
        r
    }

    /// Decision-VM loop: one decision per observation (with refine/escalate
    /// inside `dvm::decide`), reveal macros for latent targets, then execute.
    async fn act_dvm(&mut self, instr: &str, r: &mut ActResult) -> Result<()> {
        let mut last_click: Option<(String, u64)> = None;
        // Feedback for the next decision (e.g. why a commit was held back).
        let mut notes: Vec<String> = Vec::new();
        let mut blocked = 0u32;
        // Fields changed since the last commit click (a form not yet submitted).
        let mut unsubmitted = false;
        let mut guarded = 0u32;
        let mut dialogs = 0u32;
        let mut clarified = 0u32;
        let mut filled_first = false;
        // The last commit clicked, to tell whether it landed (see answer_dialog).
        let mut last_commit: Option<(crate::snapshot::El, Snapshot)> = None;
        // Progress for the loop detector: rows and addresses not seen before.
        let mut seen_rows: std::collections::HashSet<String> = Default::default();
        let mut seen_urls: std::collections::HashSet<String> = Default::default();
        let mut progress_at = 0usize;
        for step in 0..self.k.max_steps {
            r.steps = step + 1;
            let ts = Instant::now();
            self.snapshot().await?;
            r.timings.snapshot_ms += ts.elapsed().as_secs_f64() * 1e3;
            let snap = self.last.clone().unwrap();

            let goal = self.goal_ctx.clone();
            let shown = if self.step_scoped { None } else { goal.as_deref() };
            // What grounds and licenses the decision: this call's goal plus, in
            // agent mode, the user's own task (a planner's sub-goal may not
            // repeat the verb the user used).
            let ground = match (&goal, &self.task) {
                (Some(g), Some(t)) if g != t => Some(format!("{g} {t}")),
                (None, Some(t)) => Some(t.clone()),
                _ => goal.clone(),
            };
            let tj = Instant::now();
            let hist: Vec<String> = r.actions.iter().chain(notes.iter()).cloned().collect();
            let mut d = crate::dvm::decide_with(&snap, instr, shown, ground.as_deref(), &hist, &self.k, &self.decider.0).await?;
            for a in &d.answers {
                r.timings.add_answers(a);
            }
            self.live(|| {
                let what = match &d.outcome {
                    crate::dvm::Outcome::Done => "done".to_string(),
                    crate::dvm::Outcome::Escalate(why) => format!("escalate: {}", crate::snapshot::truncate(why, 90)),
                    crate::dvm::Outcome::Commit => format!("act (p={:.2})", if d.plan.click.is_some() { d.plan.click_conf } else { 1.0 }),
                };
                format!("Jev {:.0} ms · {} round{} → {what}", tj.elapsed().as_secs_f64() * 1e3, d.rounds, if d.rounds == 1 { "" } else { "s" })
            });
            if self.keep_trace {
                self.trace.push(json!({"plan": d.plan, "outcome": d.outcome, "rounds": d.rounds, "trace": d.trace}));
            }
            if let Some(rec) = self.recorder.as_mut() {
                let gold = match d.outcome {
                    crate::dvm::Outcome::Done => Some(Gold::done()),
                    crate::dvm::Outcome::Commit => Some(Gold::from_plan(&d.plan, &snap)),
                    crate::dvm::Outcome::Escalate(_) => None,
                };
                if let Some(gold) = gold {
                    rec.push(DecisionCase {
                        id: String::new(),
                        scenario: String::new(),
                        source: "scripted".into(),
                        instr: instr.to_string(),
                        history: r.actions.clone(),
                        snap: snap.clone(),
                        gold,
                        tags: vec![],
                    });
                }
            }
            r.done = d.plan.done;
            // Uncertain about the click but sure about the fields: fill first
            // (typing commits nothing) and decide again; a filled field often
            // enables or explains the button ("type UNLINK to confirm").
            if let crate::dvm::Outcome::Escalate(why) = &d.outcome {
                if !filled_first && !d.plan.fills.is_empty() && clarifiable(why) && gate_ok(self).await {
                    filled_first = true;
                    let fills = Plan { click: None, enter: false, ..d.plan.clone() };
                    let n0 = r.actions.len();
                    let res = self.execute(&fills, &snap, &mut r.actions).await;
                    self.live_actions(&r.actions[n0..]);
                    res?;
                    let mut light = self.k.clone();
                    light.settle_cap_ms = light.settle_cap_ms.min(500);
                    self.browser.settle(&light, &snap.doc_id).await?;
                    unsubmitted = true;
                    notes.push("(filled the fields first; deciding the next click again)".into());
                    continue;
                }
            }
            // Too uncertain to act: one small-LLM clarification picks among the
            // candidates, instead of handing the whole task back.
            if let crate::dvm::Outcome::Escalate(why) = &d.outcome {
                if clarified < self.k.clarify_max && !self.k.clarify.is_empty() && clarifiable(why) {
                    clarified += 1;
                    let why = why.clone();
                    match self.clarify(&snap, instr, &hist, &d, &why, r).await {
                        Some(Clarified::Pick(i)) => {
                            d.plan.click = Some(i);
                            d.plan.enter = false;
                            d.plan.click_conf = d.plan.click_conf.max(0.5);
                            // Re-decide after the click rather than trusting a finish.
                            d.plan.final_p = 0.0;
                            d.outcome = crate::dvm::Outcome::Commit;
                        }
                        Some(Clarified::Done) => d.outcome = crate::dvm::Outcome::Done,
                        None => {}
                    }
                }
            }
            match &d.outcome {
                crate::dvm::Outcome::Done => {
                    // An open dialog is a question the run hasn't answered yet.
                    if !self.step_scoped && snap.modal && dialogs < 2 {
                        dialogs += 1;
                        if self.answer_dialog(&snap, instr, r).await? {
                            unsubmitted = self.commit_swallowed(&last_commit).await?;
                            continue;
                        }
                    }
                    // Done is a judgement about the page; an open confirmation or
                    // a filled form whose licensed submit was never clicked says
                    // otherwise, whatever the page text suggests.
                    // Fields that appeared after the last commit click (a follow-up
                    // question: "your reading is lower than last time — did the
                    // meter roll over?") mean the submission isn't through yet.
                    let asks_more = last_commit.as_ref().is_some_and(|(_, before)| new_fields(&snap, before));
                    if !self.step_scoped && guarded < 2 {
                        if let Some(why) = pending_commit(&snap, &format!("{instr} {}", goal.as_deref().unwrap_or("")), unsubmitted || asks_more, last_commit.as_ref()) {
                            guarded += 1;
                            self.live(|| format!("guard: not done yet — {why}"));
                            notes.push(format!("(not finished yet: {why})"));
                            continue;
                        }
                    }
                    r.ok = true;
                    // Commit evidence: a commit ran in this act and the page moved
                    // on from it, with no error, nothing pending and nothing busy.
                    if let Some((_, before)) = &last_commit {
                        let moved = snap.version != before.version || snap.doc_id != before.doc_id || snap.url != before.url;
                        if moved && !error_on_page(&snap) && !asks_more && !unsubmitted {
                            let busy = self.browser.eval("__ub.busy()").await.ok().and_then(|v| v.as_bool()).unwrap_or(false);
                            if !busy {
                                r.verified_commit = Some(r.audits.last().and_then(|a| a["p"].as_f64()).unwrap_or(1.0));
                            }
                        }
                    }
                    return Ok(());
                }
                crate::dvm::Outcome::Escalate(why) => {
                    r.ok = false;
                    r.error = Some(why.clone());
                    r.alternatives = d.alternatives.clone();
                    return Ok(());
                }
                crate::dvm::Outcome::Commit => {}
            }
            let mut plan = d.plan;
            // A click on the element that reveals the plan's latent fields is the
            // reveal itself; don't click it a second time (it would close again).
            let reveals = crate::dvm::reveals(&plan, &snap);
            if plan.click.is_some_and(|c| reveals.iter().any(|(_, t)| *t == c)) {
                plan.click = None;
            }
            if plan.click.is_some() || plan.enter {
                r.confidence = r.confidence.min(plan.click_conf);
            }
            if let Some(c) = plan.click {
                let key = (decide::describe_click(c, &snap), snap.version);
                if last_click.as_ref() == Some(&key) {
                    r.error = Some(format!("repeated {} with no effect", key.0));
                    return Ok(());
                }
                last_click = Some(key);
            }

            let commit = plan
                .click
                .and_then(|c| snap.els.iter().find(|e| e.i == c))
                .is_some_and(|e| license::risk(e).0 >= license::Risk::R2);
            if !self.gate_allows(commit).await {
                r.error = Some("aborted: the task needs a different plan".into());
                return Ok(());
            }
            let te = Instant::now();
            let doc = snap.doc_id.clone();
            let n0 = r.actions.len();
            // Reveal macros: open whatever hides the plan's latent targets first.
            // A text input reveals by typing (typeahead suggestions render only
            // after input), anything else by clicking.
            let task_text = goal.clone().unwrap_or_else(|| instr.to_string());
            for (t, trig) in &reveals {
                self.note_nav(*trig, &snap);
                let typeahead = snap.els.iter().find(|e| e.i == *trig).is_some_and(|e| e.kind() == Kind::Text);
                let target_name = snap.els.iter().find(|e| e.i == *t).map(|e| e.n.clone()).unwrap_or_default();
                if typeahead && !target_name.trim().is_empty() {
                    let q = typeahead_query(&target_name, &task_text);
                    self.browser.fill(*trig, &q, self.k.exec).await?;
                    r.actions.push(format!("typed \"{q}\" into {} to show suggestions", decide::describe_click(*trig, &snap).trim_start_matches("clicked ")));
                } else {
                    self.browser.click(*trig, self.k.exec).await?;
                    r.actions.push(format!("revealed via {}", decide::describe_click(*trig, &snap).trim_start_matches("clicked ")));
                }
                self.browser.settle(&self.k, &doc).await?;
            }
            let snap = if reveals.is_empty() {
                snap
            } else {
                let old = snap;
                self.snapshot().await?;
                let s = self.last.clone().unwrap();
                // Suggestion lists re-render: follow each target to its new node.
                for (t, _) in &reveals {
                    if s.els.iter().any(|e| e.i == *t && !e.latent()) {
                        continue;
                    }
                    let moved = old.els.iter().find(|e| e.i == *t).and_then(|o| {
                        let mut o = o.clone();
                        o.rv = None;
                        o.f = o.f.map(|f| f.split(' ').filter(|x| *x != "latent").collect::<Vec<_>>().join(" "));
                        s.refind(&o, &old)
                    });
                    let Some(n) = moved else { anyhow::bail!("revealing did not expose e{t}") };
                    if plan.click == Some(*t) {
                        plan.click = Some(n);
                    }
                    for f in plan.fills.iter_mut().filter(|f| f.el == *t) {
                        f.el = n;
                    }
                }
                s
            };
            // Pre-commit audit: fill first, then check the form as it would be
            // submitted against the task before the irreversible click.
            let mut shown_upto = n0;
            // A field-less confirmation dialog has nothing to contradict the
            // task; what it confirms was on the page before it.
            let bare_dialog = snap.modal
                && plan.fills.is_empty()
                && !snap.els.iter().any(|e| {
                    matches!(e.kind(), Kind::Text | Kind::Select | Kind::Check | Kind::Radio) && !e.has_flag("covered") && !e.latent()
                });
            // A per-row action ("Mark paid", "Message") on a row with no fields
            // has no form to contradict; which row is right is enforced by row
            // grounding (the row that best matches the task).
            let row_action = plan.click.and_then(|c| snap.els.iter().find(|e| e.i == c)).is_some_and(|t| {
                t.rc.is_some()
                    && !snap.els.iter().any(|e| e.rc == t.rc && matches!(e.kind(), Kind::Text | Kind::Select | Kind::Check | Kind::Radio))
            }) && plan.fills.is_empty();
            let (plan, snap) = if commit && self.k.audit != "off" && !bare_dialog && !row_action {
                let c = plan.click.unwrap();
                let fills = Plan { click: None, enter: false, ..plan.clone() };
                let had_fills = !fills.fills.is_empty();
                let res = self.execute(&fills, &snap, &mut r.actions).await;
                self.live_actions(&r.actions[n0..]);
                shown_upto = r.actions.len();
                res?;
                // Field values are in the DOM as soon as the fills run; only let
                // dependent UI catch up briefly (no network/timer wait).
                if had_fills {
                    let mut light = self.k.clone();
                    light.settle_cap_ms = light.settle_cap_ms.min(200);
                    light.timer_max_ms = 0;
                    self.browser.settle(&light, &doc).await?;
                }
                self.snapshot().await?;
                let now = self.last.clone().unwrap();
                let target = if now.els.iter().any(|e| e.i == c) {
                    Some(c)
                } else {
                    snap.els.iter().find(|e| e.i == c).and_then(|old| now.refind(old, &snap))
                };
                let Some(tc) = target else { anyhow::bail!("the button to submit ({}) disappeared after filling the form", decide::describe_click(c, &snap)) };
                // The goal of this `do` call (a planner's sub-goal inside a
                // program), not the user's whole task: checked against the whole
                // task, intermediate commits looked contradictory (audit blocks
                // tripled on held-out, 93 → 280, and runs hit the turn budget).
                let task = goal.clone().unwrap_or_else(|| instr.to_string());
                let ta = Instant::now();
                if let Some(au) = crate::dvm::audit(&now, &task, tc, &self.decider.0).await? {
                    if let Some(a) = &au.answers {
                        r.timings.add_answers(a);
                    }
                    let wrong = au.wrong.and_then(|(i, _)| now.els.iter().find(|e| e.i == i)).map(|e| e.desc(true)).or_else(|| au.line.clone());
                    let block = self.k.audit == "on" && au.p < self.k.audit_tau;
                    self.live(|| {
                        format!(
                            "Jev audit {:.0} ms · form matches task p={:.2}{}{}",
                            ta.elapsed().as_secs_f64() * 1e3,
                            au.p,
                            wrong.as_deref().map(|w| format!(" · suspect: {}", crate::snapshot::truncate(w, 70))).unwrap_or_default(),
                            if block { " → not submitting" } else { "" }
                        )
                    });
                    r.audits.push(json!({"p": au.p, "suspect": wrong, "suspect_p": au.wrong.map(|w| w.1), "blocked": block, "commit": decide::describe_click(tc, &now)}));
                    if block && blocked >= 2 {
                        // Re-deciding hasn't fixed it (the wrong value may sit on an
                        // earlier page): hand back rather than submit it.
                        r.ok = false;
                        r.error = Some(format!(
                            "not submitted: before {}, the form still disagrees with the task{}",
                            decide::describe_click(tc, &now),
                            wrong.map(|w| format!(" ({w})")).unwrap_or_default()
                        ));
                        return Ok(());
                    }
                    if block {
                        blocked += 1;
                        // The click never happened, so it can't count as a repeat.
                        last_click = None;
                        notes.push(format!(
                            "(not submitted yet: before {}, the form disagrees with the task{})",
                            decide::describe_click(tc, &now),
                            wrong.map(|w| format!(": check {w}")).unwrap_or_default()
                        ));
                        r.timings.exec_ms += te.elapsed().as_secs_f64() * 1e3;
                        continue;
                    }
                }
                (Plan { fills: vec![], click: Some(tc), ..plan }, now)
            } else {
                (plan, snap)
            };
            let res = self.execute(&plan, &snap, &mut r.actions).await;
            self.live_actions(&r.actions[shown_upto..]);
            res?;
            r.timings.exec_ms += te.elapsed().as_secs_f64() * 1e3;
            if plan.last {
                self.browser.settle(&self.k, &snap.doc_id).await?;
                r.ok = true;
                return Ok(());
            }
            if commit {
                unsubmitted = false;
                self.commits += 1;
                last_commit = plan.click.and_then(|c| snap.els.iter().find(|e| e.i == c)).map(|e| (e.clone(), snap.clone()));
            } else if !plan.fills.is_empty() {
                unsubmitted = true;
            }

            let tw = Instant::now();
            self.browser.settle(&self.k, &doc).await?;
            r.timings.settle_ms += tw.elapsed().as_secs_f64() * 1e3;

            // Stuck: the same action again and again, or two alternating, with
            // no progress (no new rows, no address not visited before). Each
            // click changes something small (a message, a timestamp), so the
            // page-version guard above doesn't see it; "Load more" and "Next"
            // bring new rows, so they never count.
            self.snapshot().await?;
            if let Some(now) = &self.last {
                let fresh_rows = now.records.iter().filter_map(|x| x.label.clone()).filter(|l| seen_rows.insert(l.clone())).count();
                let fresh_url = seen_urls.insert(now.url.clone());
                if (fresh_rows > 0 && step > 0) || (fresh_url && step > 0) {
                    progress_at = r.actions.len();
                }
            }
            if let Some(what) = looping(&r.actions[progress_at.min(r.actions.len())..]) {
                let alerts: Vec<String> = self.last.as_ref().map(|s| s.texts.iter().filter(|t| t.a.is_some()).map(|t| t.x.clone()).take(3).collect()).unwrap_or_default();
                let said = if alerts.is_empty() { String::new() } else { format!("; the page says: {}", alerts.join(" | ")) };
                // Hand back at once: re-deciding with the page's message was
                // measured slower (21-29 s vs 17 s) and didn't fix the cases
                // seen, which need a value reformatted (Jev picks, never writes).
                r.error = Some(format!("stuck: {what} keeps repeating without progress{said}"));
                return Ok(());
            }

            if plan.final_p >= self.k.trust_final && (plan.click.is_some() || plan.enter) {
                // A commit that opened a confirmation dialog is not the end.
                if !self.step_scoped && guarded < 2 {
                    self.snapshot().await?;
                    let now = self.last.clone().unwrap();
                    if now.modal && dialogs < 2 {
                        dialogs += 1;
                        if self.answer_dialog(&now, instr, r).await? {
                            unsubmitted = self.commit_swallowed(&last_commit).await?;
                            continue;
                        }
                    }
                    // A click that opened a form (new fields, a licensed submit
                    // waiting) is a step into the flow, not its end.
                    let opened_form = now.els.iter().any(|e| {
                        matches!(e.kind(), Kind::Text | Kind::Select | Kind::Check | Kind::Radio)
                            && !e.latent()
                            && !e.has_flag("covered")
                            && !snap.els.iter().any(|o| o.i == e.i)
                    });
                    if let Some(why) = pending_commit(&now, &format!("{instr} {}", goal.as_deref().unwrap_or("")), unsubmitted || opened_form, last_commit.as_ref()) {
                        guarded += 1;
                        self.live(|| format!("guard: not done yet — {why}"));
                        notes.push(format!("(not finished yet: {why})"));
                        continue;
                    }
                }
                r.ok = true;
                r.done = plan.final_p;
                let now = self.last.clone();
                let error = now.as_ref().is_some_and(error_on_page);
                if commit {
                    // A commit with nothing to audit (a confirmation dialog, a
                    // per-row action) is evidence by itself once the page moves on.
                    let audited = if row_action || bare_dialog { Some(1.0) } else { r.audits.last().and_then(|a| a["p"].as_f64()) };
                    let changed = now.as_ref().is_some_and(|n| n.version != snap.version || n.doc_id != snap.doc_id || n.url != snap.url);
                    if changed && !error {
                        r.verified_commit = audited;
                    }
                } else if plan.final_p >= 0.8 && !error && now.as_ref().is_some_and(|n| n.version != snap.version || n.doc_id != snap.doc_id || n.url != snap.url) {
                    // A confident last step ("Mark paid", "Sign in") that visibly
                    // changed the page, with no error and nothing in progress.
                    let busy = self.browser.eval("__ub.busy()").await.ok().and_then(|v| v.as_bool()).unwrap_or(false);
                    if !busy {
                        r.verified_commit = Some(plan.final_p);
                    }
                }
                return Ok(());
            }
        }
        r.error = Some(format!("step budget ({}) exhausted", self.k.max_steps));
        Ok(())
    }

    async fn act_inner(&mut self, instr: &str, r: &mut ActResult) -> Result<()> {
        let mut last_click: Option<(String, u64)> = None;
        for step in 0..self.k.max_steps {
            r.steps = step + 1;
            let ts = Instant::now();
            self.snapshot().await?;
            r.timings.snapshot_ms += ts.elapsed().as_secs_f64() * 1e3;
            let snap = self.last.clone().unwrap();

            let (state, q, map) = decide::build(&snap, instr, &r.actions, &self.k);
            let ans = self.decider.ask(&state, &q).await?;
            r.timings.add_answers(&ans);
            let mut plan = decide::decode(&ans, &map, &snap);
            decide::dedupe_values(&mut plan, instr);
            self.live(|| format!("Jev {:.0} ms → done p={:.2}", ans.latency.as_secs_f64() * 1e3, plan.done));
            if self.keep_trace {
                self.trace.push(json!({"state": state, "questions": q.0, "plan": plan}));
            }
            r.done = plan.done;

            let done = plan.done >= self.k.done_threshold && (step > 0 || plan.is_noop() || plan.done >= 0.9);
            if let Some(rec) = self.recorder.as_mut() {
                let gold = if done { Some(Gold::done()) } else { (!plan.is_noop()).then(|| Gold::from_plan(&plan, &snap)) };
                if let Some(gold) = gold {
                    rec.push(DecisionCase {
                        id: String::new(),
                        scenario: String::new(),
                        source: "scripted".into(),
                        instr: instr.to_string(),
                        history: r.actions.clone(),
                        snap: snap.clone(),
                        gold,
                        tags: vec![],
                    });
                }
            }
            if done || plan.is_noop() {
                r.ok = done;
                if !r.ok {
                    r.error = Some(if step == 0 {
                        "no applicable action found on this page".into()
                    } else {
                        format!("nothing further to do, but the instruction does not look done (p={:.2})", plan.done)
                    });
                    r.alternatives = plan.alternatives.clone();
                }
                return Ok(());
            }
            if plan.click.is_some() || plan.enter {
                r.confidence = r.confidence.min(plan.click_conf);
                if plan.click_conf < self.k.min_conf {
                    r.ok = false;
                    r.alternatives = plan.alternatives.clone();
                    r.error = Some(format!("ambiguous click (confidence {:.2})", plan.click_conf));
                    return Ok(());
                }
            }

            // License: commit/destructive clicks need the instruction to ask for them.
            if let Some(e) = plan.click.and_then(|c| snap.els.iter().find(|e| e.i == c)) {
                let context = format!("{instr} {}", self.goal_ctx.as_deref().unwrap_or(""));
                if let Err(why) = license::grounded(&context, e, &snap) {
                    r.alternatives = plan.alternatives.clone();
                    r.error = Some(why);
                    return Ok(());
                }
                if let Err(why) = license::licensed(instr, e) {
                    r.alternatives = plan.alternatives.clone();
                    r.error = Some(format!("not permitted: {why}. Ask for it explicitly if that's intended"));
                    return Ok(());
                }
            }

            // Loop guard: the same click on an unchanged page twice means we're stuck.
            if let Some(c) = plan.click {
                let key = (decide::describe_click(c, &snap), snap.version);
                if last_click.as_ref() == Some(&key) {
                    r.error = Some(format!("repeated {} with no effect", key.0));
                    return Ok(());
                }
                last_click = Some(key);
            }

            let te = Instant::now();
            let doc = snap.doc_id.clone();
            let n0 = r.actions.len();
            let res = self.execute(&plan, &snap, &mut r.actions).await;
            self.live_actions(&r.actions[n0..]);
            res?;
            r.timings.exec_ms += te.elapsed().as_secs_f64() * 1e3;

            let tw = Instant::now();
            self.browser.settle(&self.k, &doc).await?;
            r.timings.settle_ms += tw.elapsed().as_secs_f64() * 1e3;

            if plan.final_p >= self.k.trust_final && (plan.click.is_some() || plan.enter) {
                r.ok = true;
                r.done = plan.final_p;
                return Ok(());
            }
        }
        r.error = Some(format!("step budget ({}) exhausted", self.k.max_steps));
        Ok(())
    }

    /// Runs precise commands. Targets resolve by stable key or exact name; an
    /// ambiguous name gets one narrow decision among its candidates (which can
    /// only map the target, never add actions). Anything else fails.
    async fn act_direct(&mut self, cmds: &[Cmd], seen: Option<&Snapshot>, r: &mut ActResult) -> Result<()> {
        let mode = self.k.exec;
        for (n, c) in cmds.iter().enumerate() {
            let ts = Instant::now();
            self.snapshot().await?;
            r.timings.snapshot_ms += ts.elapsed().as_secs_f64() * 1e3;
            let snap = self.last.clone().unwrap();
            let need = direct::Need::of(c);
            let target = match c {
                Cmd::Click(t) | Cmd::Type(t, _) | Cmd::Select(t, _) | Cmd::Check(t, _) => Some(t),
                Cmd::Enter(t) => t.as_ref(),
            };
            let el = match target {
                None => None,
                Some(t) => match direct::resolve(t, need, &snap, seen) {
                    direct::Resolved::Found(i) => Some(i),
                    direct::Resolved::Ambiguous(cands) => match self.narrow(&direct::describe(c), &cands, &snap, r).await? {
                        Some(i) => Some(i),
                        None => anyhow::bail!(
                            "command {} ({}) is ambiguous; candidates: {}",
                            n + 1,
                            direct::describe(c),
                            candidates(&snap, &cands)
                        ),
                    },
                    direct::Resolved::Missing(why) => {
                        let near: Vec<usize> = snap
                            .prune_els(&direct::describe(c), 5, |e| need.accepts(e))
                            .into_iter()
                            .map(|e| e.i)
                            .collect();
                        // A name that is close but not exact ("Ines Carvalho" vs
                        // "Ines Carvalho (Billing)"): one narrow round may map it.
                        // It only picks the command's target, never adds actions.
                        match self.narrow(&direct::describe(c), &near, &snap, r).await? {
                            Some(i) => Some(i),
                            None => anyhow::bail!("command {} ({}): {why}; closest: {}", n + 1, direct::describe(c), candidates(&snap, &near)),
                        }
                    }
                },
            };
            let doc = snap.doc_id.clone();
            let te = Instant::now();
            let e = el.and_then(|i| snap.els.iter().find(|e| e.i == i));
            let mut settle = false;
            match c {
                Cmd::Type(_, v) | Cmd::Select(_, v) => {
                    let i = el.unwrap();
                    let fill = if e.is_some_and(|e| e.kind() == Kind::Select) {
                        self.browser.select(i, v).await?;
                        decide::Fill { el: i, value: FillValue::Select(v.clone()), p: 1.0 }
                    } else {
                        self.browser.fill(i, v, mode).await?;
                        decide::Fill { el: i, value: FillValue::Text(v.clone()), p: 1.0 }
                    };
                    r.actions.push(decide::describe_fill(&fill, &snap));
                }
                Cmd::Check(_, want) => {
                    let i = el.unwrap();
                    if e.is_some_and(|e| e.has_flag("checked")) != *want {
                        self.browser.click(i, mode).await?;
                        settle = true;
                    }
                    r.actions.push(decide::describe_fill(&decide::Fill { el: i, value: FillValue::Check(*want), p: 1.0 }, &snap));
                }
                Cmd::Click(_) => {
                    let i = el.unwrap();
                    if e.is_some_and(|e| license::risk(e).0 >= license::Risk::R2) {
                        self.commits += 1;
                    }
                    self.note_nav(i, &snap);
                    self.browser.click(i, mode).await?;
                    r.actions.push(decide::describe_click(i, &snap));
                    settle = true;
                }
                Cmd::Enter(_) => {
                    self.browser.enter(el, mode).await?;
                    r.actions.push("pressed Enter".into());
                    settle = true;
                }
            }
            r.timings.exec_ms += te.elapsed().as_secs_f64() * 1e3;
            if settle || n + 1 == cmds.len() {
                let tw = Instant::now();
                self.browser.settle(&self.k, &doc).await?;
                r.timings.settle_ms += tw.elapsed().as_secs_f64() * 1e3;
            }
            r.steps = n + 1;
        }
        r.ok = true;
        r.done = 1.0;
        Ok(())
    }

    /// One narrow decision: which of `cands` does the command mean? Returns None
    /// unless Jev is confident (higher bar for commit/destructive targets).
    async fn narrow(&mut self, cmd: &str, cands: &[usize], snap: &Snapshot, r: &mut ActResult) -> Result<Option<usize>> {
        let els: Vec<&crate::snapshot::El> = cands.iter().filter_map(|i| snap.els.iter().find(|e| e.i == *i)).take(16).collect();
        if els.is_empty() {
            return Ok(None);
        }
        let mut q = Questions::default();
        let opts = els
            .iter()
            .map(|e| (e.id(), Some(e.desc(true))))
            .chain(std::iter::once(("none".to_string(), Some("None of these is what the command refers to.".to_string()))));
        q.choice("target", "Which element in `elements` does `command` refer to?", opts);
        let state = json!({
            "command": cmd,
            "page": format!("{} — {}", snap.title, snap.url),
            "elements": els.iter().map(|e| e.line(true)).collect::<Vec<_>>(),
        });
        let ans = self.decider.ask(&state, &q).await?;
        r.timings.add_answers(&ans);
        let Some((choice, p, _)) = ans.choice("target") else { return Ok(None) };
        let Some(i) = choice.strip_prefix('e').and_then(|s| s.parse::<usize>().ok()) else { return Ok(None) };
        let risky = els.iter().find(|e| e.i == i).is_some_and(|e| license::risk(e).0 >= license::Risk::R2);
        Ok((p >= if risky { 0.9 } else { 0.7 }).then_some(i))
    }

    async fn execute(&mut self, plan: &Plan, snap: &Snapshot, log: &mut Vec<String>) -> Result<()> {
        use crate::domain::{ActionBatch, Activation, FieldEdit, ObservationStamp};
        let stamp = ObservationStamp::capture(self.identity.clone(), 0, snap)?;
        let batch = ActionBatch::resolve(plan, snap, &stamp)?;
        let one = !self.k.fanout;
        let mode = self.k.exec;
        let mut last_text = None;
        for (f, edit) in plan.fills.iter().zip(batch.edits()) {
            // The page may have re-rendered since the snapshot (an earlier
            // action, or async rendering): follow the element to its new node.
            let el = self.live_key(f.el, snap).await?;
            let combo = snap.els.iter().any(|e| e.i == f.el && e.r == "combobox");
            if let FillValue::Text(v) = &f.value {
                if combo && self.picked.contains(&(f.el, v.to_lowercase())) {
                    continue; // already chosen: the chip holds it
                }
            }
            match edit {
                FieldEdit::Text { value, .. } => {
                    let v = value.as_str();
                    self.browser.fill(el, v, mode).await?;
                    last_text = Some(el);
                    // A search-as-you-type combobox takes a value only when a
                    // suggestion is picked: pick the one that is exactly it.
                    if combo {
                        if let Some(o) = self.pick_suggestion(v, &snap.doc_id).await? {
                            self.picked.insert((f.el, v.to_lowercase()));
                            log.push(format!("selected suggestion \"{o}\""));
                        }
                    }
                }
                FieldEdit::Select { option, .. } => self.browser.select(el, option).await?,
                FieldEdit::Check { .. } | FieldEdit::Radio { .. } => self.browser.click(el, mode).await?,
            }
            log.push(decide::describe_fill(f, snap));
            if one {
                return Ok(());
            }
        }
        if let Some(Activation::Click(target)) = batch.activation() {
            let c = target.key();
            // Filling can re-render what the click targets (a search box
            // filtering the list below it): follow the element, don't fail.
            let el = self.live_key(c, snap).await?;
            self.note_nav(c, snap);
            self.browser.click(el, mode).await?;
            log.push(decide::describe_click(c, snap));
        } else if matches!(batch.activation(), Some(Activation::Enter(_))) {
            self.browser.enter(last_text, mode).await?;
            log.push("pressed Enter".into());
        }
        Ok(())
    }

    /// After typing `typed` into a combobox: click the one visible suggestion
    /// whose name is the typed value (optionally followed by details, e.g.
    /// "ci/build · last run 2h ago"). Nothing when there is no such suggestion
    /// or several.
    async fn pick_suggestion(&mut self, typed: &str, doc: &str) -> Result<Option<String>> {
        let mut light = self.k.clone();
        light.settle_cap_ms = light.settle_cap_ms.min(1500);
        self.browser.settle(&light, doc).await?;
        self.snapshot().await?;
        let now = self.last.clone().unwrap();
        let t = typed.trim().to_lowercase();
        if t.is_empty() {
            return Ok(None);
        }
        let opts: Vec<&crate::snapshot::El> = now
            .els
            .iter()
            .filter(|e| matches!(e.r.as_str(), "option" | "menuitem" | "menuitemradio" | "menuitemcheckbox"))
            .filter(|e| !e.latent() && !e.has_flag("disabled") && !e.has_flag("covered"))
            .collect();
        // The option's own label: its first line, or the text before a
        // separator ("ci/build · Stackhaven CI" → "ci/build").
        let head = |n: &str| -> String {
            let n = n.trim().to_lowercase();
            let cut = n.find([' ', '\u{b7}', '\u{2014}', '\u{2013}', '(', '\n']).map(|i| n[..i].to_string()).unwrap_or(n.clone());
            if cut.is_empty() { n } else { cut }
        };
        // The exact name, else the option whose own label is the value
        // ("ci/build · last run 2h" for "ci/build"). Never a longer name that
        // merely starts with it: "ci/build-docs" is not "ci/build".
        let exact: Vec<_> = opts.iter().filter(|e| e.n.trim().to_lowercase() == t).collect();
        let label: Vec<_> = opts.iter().filter(|e| head(&e.n) == t || e.n.trim().to_lowercase().starts_with(&format!("{t} "))).collect();
        let o = match (exact.as_slice(), label.as_slice()) {
            ([o], _) => **o,
            ([], [o]) => **o,
            _ => return Ok(None),
        };
        let name = o.n.clone();
        self.browser.click(o.i, self.k.exec).await?;
        self.browser.settle(&light, doc).await?;
        Ok(Some(name))
    }

    /// One small-LLM call on an uncertain decision. The options are Jev's ranked
    /// candidates plus the best lexical matches; a pick must still pass the
    /// license and row grounding (R3 needs the task's own verb; R2 needs it, or
    /// no other commit control the task's verbs point to).
    async fn clarify(&mut self, snap: &Snapshot, instr: &str, hist: &[String], d: &crate::dvm::Decision, why: &str, r: &mut ActResult) -> Option<Clarified> {
        let task = self.task.clone().or_else(|| self.goal_ctx.clone()).unwrap_or_else(|| instr.to_string());
        let usable = |e: &crate::snapshot::El| !e.latent() && !e.has_flag("disabled") && !e.has_flag("covered");
        let mut ids: Vec<usize> = d.alternatives.iter().filter_map(|(k, _)| k.strip_prefix('e').and_then(|s| s.parse().ok())).collect();
        for e in snap.prune_els(&format!("{instr} {task}"), 14, usable) {
            if !ids.contains(&e.i) {
                ids.push(e.i);
            }
        }
        let cands: Vec<&crate::snapshot::El> = ids.iter().filter_map(|i| snap.els.iter().find(|e| e.i == *i)).filter(|e| usable(e)).take(16).collect();
        if cands.is_empty() {
            return None;
        }
        let mut text = String::new();
        for l in snap.text_lines(|_| true) {
            if text.len() + l.len() > 1800 {
                break;
            }
            text.push_str(&l);
            text.push('\n');
        }
        let opts: String = cands.iter().map(|e| format!("- {}\n", e.line(true))).collect();
        let step = if instr != task { format!("Current step: {instr}\n") } else { String::new() };
        let so_far = if hist.is_empty() { "nothing yet".to_string() } else { hist.join("; ") };
        let user = format!(
            "Task: {task}\n{step}Done so far: {so_far}\nThe engine is unsure: {why}\n\nPage: {} — {}\n{text}\nOptions:\n{opts}- done: the task is already fully complete\n- none: none of these is the next step\n\nWhich option is the next step?",
            snap.title, snap.url
        );
        if self.clarifier.is_none() {
            self.clarifier = crate::llm::Llm::from_env(&self.k.clarify).ok();
        }
        let llm = self.clarifier.clone()?;
        let body = json!({
            "messages": [{"role": "system", "content": CLARIFY_SYSTEM}, {"role": "user", "content": user}],
            "temperature": 0,
            "reasoning": {"effort": "low"},
        });
        let t = Instant::now();
        let res = llm.chat(body).await;
        let ms = t.elapsed().as_secs_f64() * 1e3;
        r.timings.llm_calls += 1;
        r.timings.llm_ms += ms;
        let (msg, usage, _) = res.ok()?;
        r.timings.llm_cost += usage.cost;
        let content = msg["content"].as_str().unwrap_or("");
        let pick = serde_json::from_str::<serde_json::Value>(content.trim().trim_start_matches("```json").trim_matches('`').trim())
            .ok()
            .and_then(|v| v["pick"].as_str().map(str::to_string))
            .or_else(|| {
                let l = content.to_lowercase();
                l.split(|c: char| !c.is_alphanumeric()).find(|w| (w.starts_with('e') && w[1..].chars().all(|c| c.is_ascii_digit()) && w.len() > 1) || *w == "done" || *w == "none").map(str::to_string)
            })?;
        let pick = pick.trim().to_lowercase();
        self.live(|| format!("clarify ({:.0} ms) → {pick}", ms));
        if pick == "done" {
            return Some(Clarified::Done);
        }
        let i: usize = pick.strip_prefix('e')?.parse().ok()?;
        let e = cands.iter().find(|e| e.i == i)?;
        let licence = format!("{instr} {task}");
        let risk = license::risk(e).0;
        let lexical = license::licensed(&licence, e).is_ok();
        let elsewhere = snap.els.iter().any(|x| {
            x.i != i && usable(x) && license::risk(x).0 >= license::Risk::R2 && license::licensed(&licence, x).is_ok()
        });
        let allowed = match risk {
            license::Risk::R0 | license::Risk::R1 => true,
            license::Risk::R2 => lexical || !elsewhere,
            license::Risk::R3 => lexical,
        };
        if !allowed || license::grounded(&licence, e, snap).is_err() {
            self.live(|| format!("clarify pick e{i} not allowed ({:?})", risk));
            return None;
        }
        Some(Clarified::Pick(i))
    }

    /// A dialog is open where the run would stop: Jev picks the button the task
    /// calls for (if any) and it is clicked. True if something was clicked.
    async fn answer_dialog(&mut self, snap: &Snapshot, instr: &str, r: &mut ActResult) -> Result<bool> {
        let task = self.task.clone().or_else(|| self.goal_ctx.clone()).unwrap_or_else(|| instr.to_string());
        let Some((i, p, a)) = crate::dvm::dialog_choice(snap, &task, &self.decider.0).await? else { return Ok(false) };
        r.timings.add_answers(&a);
        let Some(e) = snap.els.iter().find(|e| e.i == i) else { return Ok(false) };
        if p < 0.5 || license::licensed(&format!("{instr} {task}"), e).is_err() {
            return Ok(false);
        }
        let doc = snap.doc_id.clone();
        self.note_nav(i, snap);
        self.browser.click(i, self.k.exec).await?;
        if license::risk(e).0 >= license::Risk::R2 {
            self.commits += 1;
        }
        let desc = decide::describe_click(i, snap);
        self.live(|| format!("dialog → {desc} (p={p:.2})"));
        r.actions.push(desc);
        self.browser.settle(&self.k, &doc).await?;
        Ok(true)
    }

    /// After a dialog was answered: is the last commit button still on the page
    /// and enabled? Then its click was swallowed (a popup that opened on a timer
    /// made the page ignore clicks) and the form still needs submitting.
    async fn commit_swallowed(&mut self, last: &Option<(crate::snapshot::El, Snapshot)>) -> Result<bool> {
        let Some((el, before)) = last else { return Ok(false) };
        self.snapshot().await?;
        let now = self.last.clone().unwrap();
        if now.modal {
            return Ok(false);
        }
        let again = now.refind(el, before).and_then(|i| now.els.iter().find(|e| e.i == i));
        let swallowed = again.is_some_and(|e| !e.has_flag("disabled") && !e.has_flag("covered"));
        if swallowed {
            self.live(|| format!("guard: {} \"{}\" is still there after the dialog; its click did not land", el.r, el.n));
        }
        Ok(swallowed)
    }

    /// `i` if its node is still in the page; otherwise the same control in a
    /// fresh snapshot (unique fingerprint match), or an error.
    async fn live_key(&mut self, i: usize, snap: &Snapshot) -> Result<usize> {
        let document = self.browser.eval("__ub.docId").await?;
        anyhow::ensure!(document.as_str() == Some(snap.doc_id.as_str()), "stale element e{i}: the observed document was replaced");
        let alive = self.browser.eval(&format!("(()=>{{try{{__ub.el({i});return true}}catch(e){{return false}}}})()")).await?;
        if alive.as_bool() != Some(false) {
            return Ok(i);
        }
        let Some(old) = snap.els.iter().find(|e| e.i == i).cloned() else { anyhow::bail!("stale element e{i}") };
        self.snapshot().await?;
        let now = self.last.clone().unwrap();
        now.refind(&old, snap).ok_or_else(|| anyhow::anyhow!("stale element e{i}: {} {:?} is no longer on the page", old.r, old.n))
    }

    /// Run several instructions back to back with no LLM round-trips between them.
    /// Stops at the first failure.
    pub async fn run(&mut self, steps: &[String]) -> Vec<ActResult> {
        let mut out = Vec::new();
        for s in steps {
            let r = self.act(s).await;
            let ok = r.ok;
            out.push(r);
            if !ok {
                break;
            }
        }
        out
    }

    /// Extractive question answering over the page's text blocks.
    pub async fn extract(&mut self, question: &str) -> Result<ExtractResult> {
        let t0 = Instant::now();
        let mut tm = Timings::default();
        self.snapshot().await?;
        tm.snapshot_ms = t0.elapsed().as_secs_f64() * 1e3;
        let snap = self.last.clone().unwrap();
        let k = (self.k.prune_k * 2).clamp(2, 250);
        let texts = snap.prune_texts(question, k);
        // Include the current values of form fields too, as pseudo-blocks.
        let mut opts: Vec<(String, Option<String>)> = texts.iter().map(|t| (t.id(), Some(t.desc()))).collect();
        let mut by_id: std::collections::HashMap<String, String> = texts.iter().map(|t| (t.id(), t.x.clone())).collect();
        for e in snap.els.iter().filter(|e| e.v.as_deref().is_some_and(|v| !v.is_empty())).take(250 - opts.len().min(250)) {
            if opts.len() >= 254 {
                break;
            }
            opts.push((e.id(), Some(e.desc(true))));
            by_id.insert(e.id(), e.v.clone().unwrap_or_default());
        }
        if opts.len() < 2 {
            opts.push(("none".into(), Some("No text on the page answers the question.".into())));
        }
        let lines: Vec<String> = opts.iter().map(|(id, d)| format!("{id} {}", d.as_deref().unwrap_or(""))).collect();
        let state = json!({"question": question, "page": format!("{} — {}", snap.title, snap.url), "blocks": lines});
        let mut q = Questions::default();
        let opts = if self.k.opt_desc { opts } else { opts.into_iter().map(|(k, _)| (k, None)).collect() };
        q.choice("where", "Which block in `blocks` contains the answer to `question`?", opts);
        q.noul("exists", "Does some block in `blocks` answer `question`?");
        let a = self.decider.ask(&state, &q).await?;
        tm.add_answers(&a);
        let (id, p, _) = a.choice("where").unwrap_or(("none", 0.0, 0.0));
        let alternatives = a
            .ranked("where")
            .into_iter()
            .take(3)
            .map(|(id, p)| (by_id.get(id).cloned().unwrap_or_default(), p))
            .collect();
        tm.total_ms = t0.elapsed().as_secs_f64() * 1e3;
        Ok(ExtractResult { answer: by_id.get(id).cloned(), p, exists: a.noul("exists").unwrap_or(0.0), alternatives, timings: tm })
    }

    /// Page text for reading data (tables, lists). When the page is larger than
    /// `max_chars`, keeps the blocks most relevant to `query`, in page order.
    pub async fn read(&mut self, query: &str, max_chars: usize) -> Result<String> {
        let snap = self.snapshot().await?;
        let mut out = format!("# {}\n{}\n", snap.title, snap.url);
        let total: usize = snap.texts.iter().map(|t| t.x.len() + 1).sum();
        let lines = if total <= max_chars || query.trim().is_empty() {
            snap.text_lines(|_| true)
        } else {
            // Keep the most relevant texts, but always whole records.
            let keep: std::collections::HashSet<usize> =
                snap.prune_texts(query, snap.texts.len().min(400)).iter().map(|t| t.i).collect();
            let recs: std::collections::HashSet<u32> =
                snap.texts.iter().filter(|t| keep.contains(&t.i)).filter_map(|t| t.rc).collect();
            snap.text_lines(|t| keep.contains(&t.i) || t.rc.is_some_and(|r| recs.contains(&r)))
        };
        for line in lines {
            if out.len() + line.len() > max_chars {
                out.push_str("…(truncated; pass a query to focus)\n");
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        Ok(out)
    }

    /// Whether `cond` holds on the current page: one Jev check, no action.
    pub async fn expect(&mut self, cond: &str) -> (bool, Timings) {
        self.expect_in(cond, cond, &[]).await
    }

    /// `expect` within a goal, given what has been done so far.
    async fn expect_in(&mut self, goal: &str, cond: &str, log: &[String]) -> (bool, Timings) {
        let mut tm = Timings::default();
        let ok = match self.snapshot().await {
            Ok(snap) => {
                let snap = snap.clone();
                match crate::dvm::goal::check(&self.decider.0, goal, &[cond.to_string()], log, &snap).await {
                    Ok((res, a)) => {
                        tm.add_answers(&a);
                        res.first().is_some_and(|(_, p)| *p >= 0.5)
                    }
                    Err(_) => false,
                }
            }
            Err(_) => false,
        };
        (ok, tm)
    }

    /// Accomplishes a goal end to end. Without `steps`, the decision loop runs on
    /// the whole goal (navigation, dialogs, re-login and retries fall out of the
    /// per-page decisions). With `steps`, each is an instruction string or a
    /// data op: {"collect": what, "where", "op", "of", "by", "as"} or
    /// {"expect": condition}; "{name}" in later steps inserts earlier results.
    /// Before reporting done, every clause of the goal must pass a Jev check.
    pub async fn do_goal(&mut self, goal: &str, steps: &[serde_json::Value]) -> serde_json::Value {
        self.goal_ctx = Some(goal.to_string());
        self.audit_log.clear();
        let mut out = self.do_goal_inner(goal, steps).await;
        self.goal_ctx = None;
        self.flush_shapes();
        if !self.audit_log.is_empty() {
            out["audits"] = json!(std::mem::take(&mut self.audit_log));
        }
        out
    }

    async fn do_goal_inner(&mut self, goal: &str, steps: &[serde_json::Value]) -> serde_json::Value {
        let t0 = Instant::now();
        let mut log: Vec<String> = Vec::new();
        let mut tm = Timings::default();
        let mut vars: std::collections::HashMap<String, serde_json::Value> = Default::default();
        let mut evidence: Vec<String> = Vec::new();
        let subst = |s: &str, vars: &std::collections::HashMap<String, serde_json::Value>| {
            let mut out = s.to_string();
            for (k, v) in vars {
                let val = match v {
                    serde_json::Value::String(x) => x.clone(),
                    serde_json::Value::Object(o) if o.contains_key("top") => o["top"].as_str().unwrap_or("").to_string(),
                    other => other.to_string(),
                };
                out = out.replace(&format!("{{{k}}}"), &val);
            }
            out
        };
        let fail = |status: &str, why: String, log: &[String], tm: &Timings, vars: &std::collections::HashMap<String, serde_json::Value>| {
            json!({"status": status, "error": why, "actions": log, "results": vars, "timings": tm})
        };
        // A step whose input may have executed ends the goal as uncertain.
        let fail_after = |r: &ActResult, status: &str, why: String, log: &[String], tm: &Timings, vars: &std::collections::HashMap<String, serde_json::Value>| {
            let mut v = fail(if r.uncertain { "uncertain" } else { status }, why, log, tm, vars);
            if r.uncertain {
                v["uncertain"] = json!(true);
            }
            v
        };
        let saved = self.k.max_steps;
        let mut verified_commit: Option<f64> = None;
        // A site seen before: start on the page the goal belongs on (agent
        // mode does this itself, before classifying the task).
        if steps.is_empty() && self.task.is_none() {
            if let Ok(true) = self.shape_navigate(goal, &mut tm).await {
                log.push("went to the matching page via the site map".into());
            }
        }
        if steps.is_empty() {
            // Whole goals can span multi-page forms; loops are caught by the
            // repeated-action guard, not by this budget.
            self.k.max_steps = self.k.max_steps.max(24);
            let r = self.act(goal).await;
            verified_commit = r.verified_commit;
            self.k.max_steps = saved;
            tm.merge(&r.timings);
            log.extend(r.actions.clone());
            if !r.ok {
                let err = r.error.clone().unwrap_or_default();
                let status = if err.starts_with("impossible") {
                    "impossible"
                } else if err.starts_with("aborted") {
                    "aborted"
                } else if r.alternatives.is_empty() {
                    "failed"
                } else {
                    "ambiguous"
                };
                return fail_after(&r, status, err, &log, &tm, &vars);
            }
        } else {
            for (n, st) in steps.iter().enumerate() {
                // "verify …", "check that …", "make sure …" never act: run them as
                // expectations; data ops written as strings run as data ops.
                let st = &match st.as_str() {
                    Some(t) => verification(t).map(|cond| json!({"expect": cond})).or_else(|| data_step(t)).unwrap_or_else(|| st.clone()),
                    None => st.clone(),
                };
                if st.get("read").is_some() {
                    // Reading the page needs no step: `do` returns the page.
                    log.push("read the page".into());
                    continue;
                }
                if let Some(instr) = st.as_str() {
                    let instr = subst(instr, &vars);
                    self.live(|| format!("step {}/{}: {instr}", n + 1, steps.len()));
                    // A step does only its own part of the goal: the goal still
                    // grounds referents ("click Send" → whose row?) but isn't
                    // shown to the decision, so step 1 "type bob" can't also
                    // click Sign in before the password is typed.
                    self.step_scoped = true;
                    let saved_steps = self.k.max_steps;
                    self.k.max_steps = self.k.max_steps.min(4);
                    let r = self.act(&instr).await;
                    self.k.max_steps = saved_steps;
                    self.step_scoped = false;
                    tm.merge(&r.timings);
                    log.extend(r.actions.clone());
                    if !r.ok {
                        let why = format!("step {} ({instr}): {}", n + 1, r.error.clone().unwrap_or_default());
                        return fail_after(&r, "failed", why, &log, &tm, &vars);
                    }
                } else if let Some(what) = st["collect"].as_str() {
                    let g = |k: &str| st[k].as_str().map(|v| subst(v, &vars));
                    let op = g("op").unwrap_or_else(|| "list".into());
                    match self.collect(what, &g("where").unwrap_or_default(), st["pages"].as_str() != Some("current"), &op, g("of").as_deref(), g("by").as_deref()).await {
                        Ok(v) => {
                            evidence.extend(v["evidence"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).take(20));
                            log.push(format!("collected {what}: {} rows, {} matched → {}", v["rows"], v["matched"], v["result"]));
                            self.live(|| format!("step {}/{}: {}", n + 1, steps.len(), log.last().unwrap()));
                            let name = st["as"].as_str().unwrap_or(what).to_string();
                            vars.insert(name, v["result"].clone());
                        }
                        Err(e) => return fail("failed", format!("step {} (collect {what}): {e:#}", n + 1), &log, &tm, &vars),
                    }
                } else if let Some(q) = st["extract"].as_str() {
                    // Read one value off the current page into a variable.
                    let q = subst(q, &vars);
                    match self.extract(&q).await {
                        Ok(x) => {
                            tm.merge(&x.timings);
                            let Some(a) = x.answer.clone().filter(|_| x.exists >= 0.5) else {
                                return fail("failed", format!("step {} (extract {q}): not found on the page", n + 1), &log, &tm, &vars);
                            };
                            let name = st["as"].as_str().unwrap_or("answer").to_string();
                            self.live(|| format!("step {}/{}: read {name} = {a}", n + 1, steps.len()));
                            log.push(format!("read {name}: {a}"));
                            evidence.push(a.clone());
                            vars.insert(name, json!(a));
                        }
                        Err(e) => return fail("failed", format!("step {} (extract {q}): {e:#}", n + 1), &log, &tm, &vars),
                    }
                } else if let Some(cond) = st["expect"].as_str() {
                    let cond = subst(cond, &vars);
                    let (ok, t) = self.expect_in(goal, &cond, &log).await;
                    tm.merge(&t);
                    self.live(|| format!("step {}/{}: {} {cond}", n + 1, steps.len(), if ok { "verified" } else { "NOT verified" }));
                    if !ok {
                        return fail("failed", format!("expected: {cond}"), &log, &tm, &vars);
                    }
                    log.push(format!("verified: {cond}"));
                }
            }
        }
        // Clause gate: don't report done while part of the goal is unmet. A
        // single-clause goal that ended on an audited commit (form matched the
        // task, the page changed, no error) has already been checked.
        let mut unmet = Vec::new();
        let mut unmet_p: Vec<f64> = Vec::new();
        let skip_gate = self.k.clause_skip
            && steps.is_empty()
            && verified_commit.is_some_and(|p| p >= 0.8)
            && crate::dvm::goal::clauses(goal).len() <= 1;
        if skip_gate {
            self.live(|| "clause check skipped: single-part goal ended on an audited commit".to_string());
        } else if let Ok(snap) = self.snapshot().await {
            let snap = snap.clone();
            let cl = crate::dvm::goal::clauses(goal);
            if let Ok((res, a)) = crate::dvm::goal::check(&self.decider.0, goal, &cl, &log, &snap).await {
                tm.add_answers(&a);
                let low: Vec<(String, f64)> = res.into_iter().filter(|(_, p)| *p < 0.5).collect();
                unmet_p = low.iter().map(|(_, p)| *p).collect();
                unmet = low.into_iter().map(|(c, p)| format!("{c} (p={p:.2})")).collect();
                self.live(|| {
                    let n = cl.len();
                    if unmet.is_empty() { format!("Jev clause check: {n}/{n} met") } else { format!("Jev clause check: unmet {}", unmet.join("; ")) }
                });
            }
        }
        tm.total_ms = t0.elapsed().as_secs_f64() * 1e3;
        self.seen = self.last.clone();
        // The actions ran and nothing contradicts them, but the page shows no
        // confirmation: say so, rather than inviting a retry that repeats them.
        // Fixtures and real sites often show nothing after a successful click
        // (a hash route, an item marked internally), so any executed action
        // with no error on the page counts.
        let error_shown = self.last.as_ref().is_some_and(error_on_page);
        let unverified = !unmet.is_empty() && !log.is_empty() && !error_shown;
        let _ = &unmet_p;
        // A program's steps all ran: parts of the goal it didn't cover are still
        // open, but that is what remains, not a failure of these steps.
        let status = if unmet.is_empty() {
            "done"
        } else if !steps.is_empty() {
            "steps_done"
        } else if unverified {
            "done_unverified"
        } else {
            "incomplete"
        };
        json!({
            "status": status,
            "note": if unverified { "the actions were carried out but the page shows no confirmation; don't repeat them" } else { "" },
            "unmet": unmet,
            "results": vars,
            "evidence": evidence,
            "actions": log,
            "timings": tm,
        })
    }

    /// Reads a collection (table/list/feed) across its pages and computes over
    /// it: Jev binds the condition to columns and values in one round, code
    /// filters exactly and does the arithmetic. Returns verbatim evidence.
    pub async fn collect(
        &mut self,
        what: &str,
        where_: &str,
        all_pages: bool,
        op: &str,
        of: Option<&str>,
        by: Option<&str>,
    ) -> Result<serde_json::Value> {
        use crate::dvm::data;
        let t0 = Instant::now();
        let snap = self.snapshot().await?.clone();
        let Some(first) = data::page_rows(&snap, None) else { anyhow::bail!("no table or list found on this page") };
        let (cols, coll) = (first.cols.clone(), first.coll);
        let mut rows = first.rows;
        let mut next = first.next;
        let mut pages = 1;
        while all_pages && pages < 50 {
            let Some(n) = next else { break };
            let doc = self.last.as_ref().map(|s| s.doc_id.clone()).unwrap_or_default();
            self.browser.click(n, self.k.exec).await?;
            self.browser.settle(&self.k, &doc).await?;
            let snap = self.snapshot().await?.clone();
            let Some(p) = data::page_rows(&snap, Some(coll)).or_else(|| data::page_rows(&snap, None)) else { break };
            let before = rows.len();
            for r in p.rows {
                if !rows.iter().any(|x| x.line == r.line) {
                    rows.push(r);
                }
            }
            pages += 1;
            next = p.next;
            if rows.len() == before {
                break; // the pager did nothing new
            }
        }
        let conds = data::conjuncts(where_);
        let mut decide_ms = 0.0;
        let (filters, of_col, by_col) = if conds.is_empty() && of.is_none() && by.is_none() {
            (vec![], None, None)
        } else {
            let (f, o, b, answers) = data::bind(&self.decider.0, what, &cols, &rows, &conds, of, by).await?;
            decide_ms = answers.iter().map(|a| a.latency.as_secs_f64() * 1e3).sum();
            (f, o, b)
        };
        let matched: Vec<&data::Row> = rows.iter().filter(|r| filters.iter().all(|f| data::matches(r, f))).collect();
        let result = data::compute(op, &matched, of_col.as_deref(), by_col.as_deref());
        self.seen = self.last.clone();
        Ok(json!({
            "pages": pages,
            "rows": rows.len(),
            "columns": cols,
            "filters": filters,
            "of": of_col,
            "by": by_col,
            "matched": matched.len(),
            "result": result.value,
            "evidence": result.evidence.iter().take(40).collect::<Vec<_>>(),
            "ms": t0.elapsed().as_secs_f64() * 1e3,
            "decide_ms": decide_ms,
        }))
    }

    /// Relevant interactive elements for `query` (or the first N when empty).
    pub async fn observe(&mut self, query: &str) -> Result<String> {
        let k = self.k.prune_k;
        let snap = self.snapshot().await?;
        let visible = |e: &crate::snapshot::El| !e.latent();
        let out = if query.trim().is_empty() {
            snap.els.iter().filter(|e| visible(e)).take(k).map(|e| e.line(true)).collect::<Vec<_>>().join("\n")
        } else {
            // Only real matches, best first; padding with unrelated elements
            // buried the target (4 matches listed after 60 non-matches).
            let hits = snap.search_els(query, 20, visible);
            if hits.is_empty() {
                format!(
                    "no element matches \"{query}\" ({} elements on the page). Try the name of a person/item/button as written on the page, or `read` to see the text.",
                    snap.els.len()
                )
            } else {
                hits.iter().map(|e| e.line(true)).collect::<Vec<_>>().join("\n")
            }
        };
        self.seen = self.last.clone();
        Ok(out)
    }
}

/// "e3 button \"Save\" — in: Profile, e9 …" for error messages.
fn candidates(snap: &Snapshot, ids: &[usize]) -> String {
    let v: Vec<String> = ids.iter().filter_map(|i| snap.els.iter().find(|e| e.i == *i)).take(5).map(|e| e.line(true)).collect();
    if v.is_empty() { "none".into() } else { v.join("; ") }
}

fn norm_lc(s: &str) -> String {
    s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A quoted literal of the instruction that no action consumed (typed, selected,
/// or named by an element acted on) and that isn't already a field's value.
/// Whether the speculative-run gate allows a non-commit action now.
async fn gate_ok(s: &Session) -> bool {
    s.gate_allows(false).await
}

/// Escalations that are uncertainty about the next step (not refusals of a
/// destructive action or "impossible"), which a clarification can resolve.
fn clarifiable(why: &str) -> bool {
    let w = why.to_lowercase();
    ["ambiguous", "no candidate", "not confident", "nothing further", "no applicable action", "not permitted"]
        .iter()
        .any(|p| w.starts_with(p))
}

/// A repeating tail of the action log: the same action three times running, or
/// two actions alternating (A, B, A, B).
fn looping(actions: &[String]) -> Option<String> {
    let n = actions.len();
    if n >= 3 && actions[n - 1] == actions[n - 2] && actions[n - 2] == actions[n - 3] {
        return Some(actions[n - 1].clone());
    }
    if n >= 4 && actions[n - 1] == actions[n - 3] && actions[n - 2] == actions[n - 4] && actions[n - 1] != actions[n - 2] {
        return Some(format!("{} / {}", actions[n - 2], actions[n - 1]));
    }
    None
}

/// What to type into a typeahead to bring up `option`: the longest run of its
/// words that the task also contains ("Rosalind Achebe" from "Rosalind Achebe
/// rosalind.achebe@x.coop"), else its first two words.
fn typeahead_query(option: &str, task: &str) -> String {
    let ow: Vec<&str> = option.split_whitespace().collect();
    let tl = task.to_lowercase();
    let mut best: &[&str] = &[];
    for i in 0..ow.len() {
        for j in (i + 1..=ow.len()).rev() {
            if j - i <= best.len() {
                break;
            }
            let phrase = ow[i..j].join(" ").to_lowercase();
            if phrase.chars().count() >= 3 && tl.contains(&phrase) {
                best = &ow[i..j];
                break;
            }
        }
    }
    if best.is_empty() {
        best = &ow[..ow.len().min(2)];
    }
    best.join(" ")
}

/// Form fields in `now` that `before` didn't have (by role, name, context).
fn new_fields(now: &Snapshot, before: &Snapshot) -> bool {
    let fp = |e: &crate::snapshot::El| (e.r.clone(), e.n.clone(), e.c.clone());
    let old: std::collections::HashSet<_> = before.els.iter().map(fp).collect();
    now.els.iter().any(|e| {
        matches!(e.kind(), Kind::Text | Kind::Select | Kind::Check | Kind::Radio)
            && !e.latent()
            && !e.has_flag("covered")
            && !e.has_flag("disabled")
            && !old.contains(&fp(e))
    })
}

/// An alert on the page reads like an error.
fn error_on_page(s: &Snapshot) -> bool {
    s.texts.iter().any(|t| {
        t.a.is_some() && {
            let l = t.x.to_lowercase();
            [
                "error", "failed", "invalid", "required", "please ", "not ", "couldn't", "unable", "try again", "again", "check the", "must ",
                "cannot", "can't", "incorrect", "does not match", "doesn't match", "not valid", "lower than", "higher than", "exceeds",
                "missing",
            ]
            .iter()
            .any(|w| l.contains(w))
        }
    })
}

/// Why a run that looks done isn't: an open dialog offering a commit the task
/// licenses (a confirmation), or (`unsubmitted`) fields changed since the last
/// commit while a licensed commit control is still on the page.
fn pending_commit(snap: &Snapshot, licence: &str, unsubmitted: bool, prefer: Option<&(crate::snapshot::El, Snapshot)>) -> Option<String> {
    if !snap.modal && !unsubmitted {
        return None;
    }
    let ok = |e: &&crate::snapshot::El| {
        !e.latent()
            // A submit that is disabled until a new field is filled ("type
            // UNLINK to confirm") is still pending.
            && (unsubmitted || !e.has_flag("disabled"))
            && !e.has_flag("covered")
            && e.kind() == Kind::Click
            && license::risk(e).0 >= license::Risk::R2
            && license::licensed(licence, e).is_ok()
    };
    // The commit whose click didn't land, else the last one in page order
    // (a form's submit follows its fields; the button that opened it doesn't).
    let preferred = prefer.and_then(|(el, before)| snap.refind(el, before)).and_then(|i| snap.els.iter().find(|e| e.i == i)).filter(|e| ok(e));
    let e = preferred.or_else(|| snap.els.iter().filter(ok).last())?;
    Some(if snap.modal {
        format!("the open dialog still asks to confirm with {} \"{}\"", e.r, e.n)
    } else {
        format!("the form was changed but {} \"{}\" has not been clicked", e.r, e.n)
    })
}

fn unused_literal(instr: &str, actions: &[String], snap: Option<&Snapshot>) -> Option<String> {
    let acts: Vec<String> = actions.iter().map(|a| norm_lc(a)).collect();
    crate::spans::quoted(instr).into_iter().find(|l| {
        let ln = norm_lc(l);
        if ln.is_empty() || acts.iter().any(|a| a.contains(&ln)) {
            return false;
        }
        let Some(s) = snap else { return true };
        if s.els.iter().any(|e| e.v.as_deref().is_some_and(|v| norm_lc(v) == ln)) {
            return false;
        }
        // Nothing was done because the page already satisfied the instruction.
        !(actions.is_empty() && s.texts.iter().any(|t| norm_lc(&t.x).contains(&ln)))
    })
}

/// Conditional steps ("If X is not found on page 2, click on page 3") are
/// judgements for a later decision, not literal-carrying commands: I3 (every
/// quoted literal is used) applies only to unconditional steps.
fn conditional(step: &str) -> bool {
    let l = step.trim().to_lowercase();
    l.starts_with("if ") || l.starts_with("when ") || l.starts_with("unless ") || l.contains(", if ") || l.contains(" if not ")
}

/// Data ops that planners write as plain strings: "collect orders where
/// status is Refunded", "count invoices where status is Overdue",
/// "extract the tracking number", "read" / "read the page".
fn data_step(step: &str) -> Option<serde_json::Value> {
    let t = step.trim().trim_end_matches('.');
    let l = t.to_lowercase();
    if l == "read" || l == "read page" || l.starts_with("read the page") {
        return Some(json!({"read": true}));
    }
    // "read the project names under Data platform": a question about the page.
    if l.starts_with("read ") {
        return Some(json!({"extract": t[5..].trim()}));
    }
    if l.starts_with("extract ") {
        return Some(json!({"extract": t[8..].trim()}));
    }
    let (op, rest) = if l.starts_with("collect ") {
        ("list", &t[8..])
    } else if l.starts_with("count ") {
        ("count", &t[6..])
    } else {
        return None;
    };
    let (what, cond) = match rest.find(" where ").or_else(|| rest.find(" Where ")).or_else(|| rest.find(" WHERE ")) {
        Some(i) => (rest[..i].trim(), rest[i + 7..].trim()),
        None => (rest.trim(), ""),
    };
    Some(json!({"collect": what, "where": cond, "op": op}))
}

/// "verify the message was sent" → Some("the message was sent").
fn verification(step: &str) -> Option<String> {
    let l = step.trim().to_lowercase();
    for p in ["verify that ", "verify ", "check that ", "make sure that ", "make sure ", "ensure that ", "ensure ", "confirm that "] {
        if l.starts_with(p) {
            return Some(step.trim()[p.len()..].trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_possibly_executed_input_is_uncertain() {
        use crate::backend::driver::{InputError, PageLost};
        let sent: anyhow::Error = InputError::MayHaveExecuted(anyhow::anyhow!("reply lost")).into();
        assert!(may_have_executed(&sent.context("clicking Pay")));
        let conservative: anyhow::Error = InputError::conservative(anyhow::anyhow!("socket closed")).into();
        assert!(may_have_executed(&conservative));
        let unsent: anyhow::Error = InputError::NotSent(anyhow::anyhow!("no pointer")).into();
        assert!(!may_have_executed(&unsent));
        let lost: anyhow::Error = InputError::conservative(PageLost("gone".into()).into()).into();
        assert!(!may_have_executed(&lost));
        assert!(!may_have_executed(&anyhow::anyhow!("stale element e3")));
    }

    #[test]
    fn detects_loops() {
        let a = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(looping(&a(&["fill x", "click Find", "click Find", "click Find"])).is_some());
        assert!(looping(&a(&["click Back", "click Review", "click Back", "click Review"])).is_some());
        // A wizard (fill, Continue, fill, Continue) is progress.
        assert!(looping(&a(&["fill a", "click Continue", "fill b", "click Continue"])).is_none());
    }

    #[test]
    fn typeahead_query_prefers_task_words() {
        assert_eq!(typeahead_query("Rosalind Achebe rosalind.achebe@x.coop", "share it with Rosalind Achebe as Commenter"), "Rosalind Achebe");
        assert_eq!(typeahead_query("Porto, Portugal (Hybrid)", "location Lisbon"), "Porto, Portugal");
    }
}

