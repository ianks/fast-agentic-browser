//! Scenario suite runner: scripted mode (fixed instructions → act/extract) and
//! planner mode (a cheap LLM drives the tool from a goal). Writes JSON results
//! and prints a per-scenario table.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Instant;
use fab_core::{Knobs, Session, Timings};

use crate::planner::{self, PlannerStats};
use crate::mcp_client::McpClient;
use crate::server::FixtureServer;

#[derive(Debug, Clone, Deserialize)]
pub struct Suite {
    pub scenario: Vec<Scenario>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub fixture: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Natural-language goal for planner mode.
    pub goal: String,
    /// Scripted steps.
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Each must subset-match some record posted by the page.
    #[serde(default)]
    pub expect_records: Vec<Value>,
    /// None of these may match any record.
    #[serde(default)]
    pub forbid_records: Vec<Value>,
    /// Benign events that may appear. In strict mode every record must match
    /// an `expect_records` or `allow_records` pattern, otherwise the run fails.
    #[serde(default)]
    pub allow_records: Vec<Value>,
    /// Planner mode: the final answer must contain this.
    #[serde(default)]
    pub expect_answer: Option<String>,
    /// Alternative wordings of `goal` (same literal values), for agent modes.
    #[serde(default)]
    pub paraphrases: Vec<String>,
}

impl Scenario {
    /// Goal wording `p` (0 = the canonical goal).
    pub fn goal_variant(&self, p: usize) -> &str {
        if p == 0 { &self.goal } else { self.paraphrases.get(p - 1).map(String::as_str).unwrap_or(&self.goal) }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    pub act: Option<String>,
    /// Several instructions executed as one `run` call.
    pub run: Option<Vec<String>>,
    pub extract: Option<String>,
    /// For extract steps: the answer must contain this.
    pub expect: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepLog {
    pub kind: String,
    pub input: String,
    pub ok: bool,
    pub detail: Value,
    pub ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub scenario: String,
    /// Planner model (agent modes).
    #[serde(default)]
    pub model: String,
    /// Goal wording index (0 = canonical goal).
    #[serde(default)]
    pub para: usize,
    pub rep: usize,
    pub ok: bool,
    pub fail: Option<String>,
    /// Wall time for the task, excluding the initial page load.
    pub wall_ms: f64,
    pub goto_ms: f64,
    pub timings: Timings,
    pub steps: Vec<StepLog>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planner: Option<PlannerStats>,
    /// Every event the page recorded, so runs can be re-scored when checks change.
    #[serde(default)]
    pub records: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub label: String,
    pub mode: String,
    pub knobs: Value,
    pub started: String,
    pub runs: Vec<Value>,
}

pub fn load_suite(path: &Path) -> Result<Suite> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(toml::from_str(&s)?)
}

fn norm(s: &str) -> String {
    s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn val_eq(a: &Value, b: &Value) -> bool {
    let s = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    s(a) == s(b)
}

fn subset(expect: &Value, rec: &Value) -> bool {
    match expect.as_object() {
        Some(m) => m.iter().all(|(k, v)| rec.get(k).is_some_and(|r| val_eq(v, r))),
        None => false,
    }
}

pub fn check_records(sc: &Scenario, records: &[Value], strict: bool) -> Option<String> {
    for e in &sc.expect_records {
        if !records.iter().any(|r| subset(e, r)) {
            return Some(format!("missing record {e} (got {})", Value::Array(records.to_vec())));
        }
    }
    for f in &sc.forbid_records {
        if let Some(r) = records.iter().find(|r| subset(f, r)) {
            return Some(format!("forbidden record {r}"));
        }
    }
    if strict {
        if let Some(r) = records.iter().find(|r| !sc.expect_records.iter().chain(&sc.allow_records).any(|p| subset(p, r))) {
            return Some(format!("unexpected mutation {r}"));
        }
    }
    None
}

pub async fn run_scripted(sess: &mut Session, srv: &FixtureServer, sc: &Scenario, rep: usize, strict: bool) -> Run {
    srv.records.clear();
    let tg = Instant::now();
    let goto = sess.goto(&srv.url(&sc.fixture)).await;
    let goto_ms = tg.elapsed().as_secs_f64() * 1e3;
    let mut run = Run {
        scenario: sc.name.clone(),
        model: String::new(),
        para: 0,
        rep,
        ok: false,
        fail: None,
        wall_ms: 0.0,
        goto_ms,
        timings: Timings::default(),
        steps: vec![],
        planner: None,
        records: vec![],
    };
    if let Err(e) = goto {
        run.fail = Some(format!("goto: {e:#}"));
        return run;
    }
    let t0 = Instant::now();
    for st in &sc.steps {
        let ts = Instant::now();
        if let Some(a) = &st.act {
            let r = sess.act(a).await;
            run.timings.merge(&r.timings);
            let ok = r.ok;
            let detail = serde_json::to_value(&r).unwrap_or_default();
            run.steps.push(StepLog { kind: "act".into(), input: a.clone(), ok, detail, ms: ts.elapsed().as_secs_f64() * 1e3 });
            if !ok {
                run.fail = Some(format!("act failed: {a}: {}", r.error.unwrap_or_default()));
                break;
            }
        } else if let Some(list) = &st.run {
            let rs = sess.run(list).await;
            let ok = rs.len() == list.len() && rs.iter().all(|r| r.ok);
            for r in &rs {
                run.timings.merge(&r.timings);
            }
            let err = rs.iter().find(|r| !r.ok).and_then(|r| r.error.clone());
            run.steps.push(StepLog {
                kind: "run".into(),
                input: list.join(" | "),
                ok,
                detail: serde_json::to_value(&rs).unwrap_or_default(),
                ms: ts.elapsed().as_secs_f64() * 1e3,
            });
            if !ok {
                run.fail = Some(format!("run failed: {}", err.unwrap_or_default()));
                break;
            }
        } else if let Some(q) = &st.extract {
            match sess.extract(q).await {
                Ok(r) => {
                    run.timings.merge(&r.timings);
                    let ans = r.answer.clone().unwrap_or_default();
                    let ok = st.expect.as_ref().is_none_or(|e| norm(&ans).contains(&norm(e)));
                    run.steps.push(StepLog {
                        kind: "extract".into(),
                        input: q.clone(),
                        ok,
                        detail: serde_json::to_value(&r).unwrap_or_default(),
                        ms: ts.elapsed().as_secs_f64() * 1e3,
                    });
                    if !ok {
                        run.fail = Some(format!("extract {q:?}: got {ans:?}, want {:?}", st.expect.clone().unwrap_or_default()));
                        break;
                    }
                }
                Err(e) => {
                    run.fail = Some(format!("extract error: {e:#}"));
                    break;
                }
            }
        }
    }
    run.wall_ms = t0.elapsed().as_secs_f64() * 1e3;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    run.records = srv.records.take();
    if run.fail.is_none() {
        run.fail = check_records(sc, &run.records, strict);
    }
    run.ok = run.fail.is_none();
    run
}

/// Agent-benchmark run. The initial page load and first observation happen
/// before the timer starts, identically for both arms.
pub async fn run_agent(
    arm: &mut Arm<'_>,
    srv: &FixtureServer,
    sc: &Scenario,
    para: usize,
    rep: usize,
    model: &str,
    strict: bool,
) -> Run {
    run_agent_with(arm, srv, sc, para, rep, model, strict, None, None).await
}

/// `start`: all arms wait here after loading the page, so they begin the goal
/// together. `ev`: arm index + channel for live events.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_with(
    arm: &mut Arm<'_>,
    srv: &FixtureServer,
    sc: &Scenario,
    para: usize,
    rep: usize,
    model: &str,
    strict: bool,
    start: Option<(&tokio::sync::Barrier, std::time::Duration)>,
    ev: Option<(usize, tokio::sync::mpsc::UnboundedSender<planner::Event>)>,
) -> Run {
    srv.records.clear();
    let url = srv.url(&sc.fixture);
    let tg = Instant::now();
    let page = match arm {
        Arm::Experiment(sess) | Arm::Goal(sess) | Arm::Agent(sess) | Arm::Vm(sess) | Arm::Script(sess) | Arm::Do(sess) => sess.goto(&url).await,
        Arm::Control(c) => cdt_open(c, &url).await,
    };
    let goto_ms = tg.elapsed().as_secs_f64() * 1e3;
    let mut run = Run {
        scenario: sc.name.clone(),
        model: model.to_string(),
        para,
        rep,
        ok: false,
        fail: None,
        wall_ms: 0.0,
        goto_ms,
        timings: Timings::default(),
        steps: vec![],
        planner: None,
        records: vec![],
    };
    let page = match page {
        Ok(p) => p,
        Err(e) => {
            run.fail = Some(format!("goto: {e:#}"));
            return run;
        }
    };
    // Every arm starts with a warm LLM connection (no per-task TLS handshake).
    if !matches!(arm, Arm::Vm(_)) {
        if let Ok(l) = planner::llm_for(model) {
            l.warm().await;
        }
    }
    // Both arms have their page open: hold (so watchers see both ready), then start together.
    if let Some((b, lead)) = start {
        b.wait().await;
        tokio::time::sleep(lead).await;
    }
    let t0 = Instant::now();
    let emitter = ev.map(|(arm, tx)| planner::Emitter { arm, tx, t0 });
    // Watchers also see fab's own decisions and actions inside each tool call.
    if let (Arm::Experiment(sess) | Arm::Goal(sess) | Arm::Agent(sess) | Arm::Vm(sess) | Arm::Script(sess) | Arm::Do(sess), Some(e)) = (&mut *arm, &emitter) {
        let e = e.clone();
        sess.live = Some(std::sync::Arc::new(move |line| e.emit("jev", line)));
    }
    let goal = sc.goal_variant(para);
    let out = match arm {
        Arm::Experiment(sess) => planner::drive(planner::Toolset::Usebrowser(sess), goal, &url, &page, model, emitter.as_ref()).await,
        Arm::Goal(sess) => planner::drive(planner::Toolset::Goal(sess), goal, &url, &page, model, emitter.as_ref()).await,
        Arm::Control(c) => planner::drive(planner::Toolset::Cdt(c), goal, &url, &page, model, emitter.as_ref()).await,
        Arm::Agent(sess) => crate::autopilot::run(sess, goal, &url, &page, Some(model), emitter.as_ref(), None).await,
        Arm::Vm(sess) => crate::autopilot::run(sess, goal, &url, &page, None, emitter.as_ref(), None).await,
        Arm::Script(sess) => run_script_arm(sess, goal, model, true).await,
        Arm::Do(sess) => run_script_arm(sess, goal, model, false).await,
    };
    run.wall_ms = t0.elapsed().as_secs_f64() * 1e3;
    if let Arm::Experiment(sess) | Arm::Goal(sess) | Arm::Agent(sess) | Arm::Vm(sess) | Arm::Script(sess) | Arm::Do(sess) = arm {
        sess.live = None;
    }
    // Give in-flight record posts a moment to land.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    match out {
        Ok((stats, logs, timings)) => {
            run.timings = timings;
            run.steps = logs;
            let answer = stats.answer.clone().unwrap_or_default();
            run.planner = Some(stats);
            run.records = srv.records.take();
            run.fail = check_records(sc, &run.records, strict);
            if run.fail.is_none() {
                if let Some(want) = &sc.expect_answer {
                    if !norm(&answer).contains(&norm(want)) {
                        run.fail = Some(format!("answer {answer:?} lacks {want:?}"));
                    }
                }
            }
        }
        Err(e) => {
            run.records = srv.records.take();
            if let Some(ae) = e.downcast_ref::<planner::AgentError>() {
                run.steps = ae.logs.clone();
                run.timings = ae.timings.clone();
                run.planner = Some(ae.stats.clone());
            }
            run.fail = Some(format!("agent: {e:#}"));
        }
    }
    run.ok = run.fail.is_none();
    run
}

/// Re-applies the suite's current checks to stored runs (records + answers).
pub fn rescore(file: &Path, suite: &Path, strict: bool) -> Result<()> {
    let suite = load_suite(suite)?;
    let mut report: Report = serde_json::from_str(&std::fs::read_to_string(file)?)?;
    let (mut flipped_fail, mut flipped_pass) = (0, 0);
    for run in report.runs.iter_mut() {
        let name = run["scenario"].as_str().unwrap_or("").to_string();
        let Some(sc) = suite.scenario.iter().find(|s| s.name == name) else { continue };
        let prev = run["fail"].as_str().map(str::to_string);
        // Harness/agent errors stand; only outcome checks are recomputed.
        if prev.as_deref().is_some_and(|f| f.starts_with("agent:") || f.starts_with("goto:") || f.starts_with("act failed") || f.starts_with("run failed") || f.starts_with("extract")) {
            continue;
        }
        let records: Vec<Value> = run["records"].as_array().cloned().unwrap_or_default();
        let mut fail = check_records(sc, &records, strict);
        if fail.is_none() {
            if let (Some(want), Some(p)) = (&sc.expect_answer, run.get("planner")) {
                let answer = p["answer"].as_str().unwrap_or("");
                if !norm(answer).contains(&norm(want)) {
                    fail = Some(format!("answer {answer:?} lacks {want:?}"));
                }
            }
        }
        let was_ok = run["ok"].as_bool() == Some(true);
        match (was_ok, fail.is_none()) {
            (true, false) => flipped_fail += 1,
            (false, true) => flipped_pass += 1,
            _ => {}
        }
        run["ok"] = Value::Bool(fail.is_none());
        run["fail"] = fail.map(Value::String).unwrap_or(Value::Null);
    }
    std::fs::write(file, serde_json::to_string_pretty(&report)?)?;
    eprintln!("rescored {} (strict={strict}): {flipped_fail} PASS→FAIL, {flipped_pass} FAIL→PASS", file.display());
    print_table(&report);
    Ok(())
}

pub enum Arm<'a> {
    Experiment(&'a mut Session),
    Goal(&'a mut Session),
    /// Agent mode: fab gets the task; the LLM is a coprocessor.
    Agent(&'a mut Session),
    /// Agent mode with no LLM at all (engine only).
    Vm(&'a mut Session),
    /// `fab do --program`: the goal's logic compiled to a program, then run.
    Script(&'a mut Session),
    /// `fab do` exactly as the CLI runs it (routing included).
    Do(&'a mut Session),
    Control(&'a McpClient),
}

/// The `script` arm: `fab do` as the CLI runs it. The reply (script, one
/// line per step, results) is the answer.
async fn run_script_arm(sess: &mut Session, goal: &str, model: &str, program: bool) -> anyhow::Result<(PlannerStats, Vec<StepLog>, Timings)> {
    let mut ctx = crate::api::Ctx::new(Some(model.to_string()));
    let r = crate::api::call(sess, &mut ctx, "do", &serde_json::json!({"step": goal, "program": program}), sess.live.clone()).await;
    let stats = PlannerStats { arm: "script".into(), model: model.to_string(), turns: r.turns, cost: r.cost, answer: Some(r.text.clone()), ..Default::default() };
    let log = StepLog { kind: "do".into(), input: goal.into(), ok: r.ok, detail: serde_json::Value::String(crate::trunc(&r.text, 4000)), ms: 0.0 };
    Ok((stats, vec![log], Timings::default()))
}

/// Navigates the chrome-devtools-mcp page and returns its first snapshot.
pub async fn cdt_open(c: &McpClient, url: &str) -> Result<String> {
    let (pages, _) = c.call("list_pages", &json!({})).await;
    let page_id = first_page_id(&pages).unwrap_or(1);
    let (nav, err) = c.call("navigate_page", &json!({"pageId": page_id, "type": "url", "url": url})).await;
    if err {
        anyhow::bail!("navigate_page: {nav}");
    }
    let (snap, _) = c.call("take_snapshot", &json!({"pageId": page_id})).await;
    Ok(format!("(pageId {page_id})\n{snap}"))
}

fn first_page_id(s: &str) -> Option<u64> {
    s.lines().find_map(|l| {
        let l = l.trim();
        let digits: String = l.chars().take_while(|c| c.is_ascii_digit()).collect();
        (!digits.is_empty() && l[digits.len()..].starts_with(':')).then(|| digits.parse().ok()).flatten()
    })
}

pub struct BenchOpts {
    pub suite: PathBuf,
    pub only: Vec<String>,
    pub tags: Vec<String>,
    pub mode: String,
    pub repeat: usize,
    pub label: String,
    pub planner_models: Vec<String>,
    /// Goal wordings per scenario (1 = canonical only).
    pub paraphrases: usize,
    /// Fail on any recorded event not matched by expect/allow patterns.
    pub strict: bool,
    /// Scripted mode: append decision cases from passing runs to this JSONL file.
    pub record: Option<PathBuf>,
    pub out_dir: PathBuf,
    pub verbose: bool,
    /// Scenarios run concurrently, each worker with its own browser and fixture
    /// server. Use 1 for timing measurements.
    pub jobs: usize,
}

pub async fn bench(k: Knobs, o: BenchOpts) -> Result<PathBuf> {
    let suite = load_suite(&o.suite)?;
    let scenarios: Vec<&Scenario> = suite
        .scenario
        .iter()
        .filter(|s| o.only.is_empty() || o.only.iter().any(|n| s.name == *n))
        .filter(|s| o.tags.is_empty() || o.tags.iter().any(|t| s.tags.contains(t)))
        .collect();
    let control = o.mode == "control";
    let scripted = o.mode == "scripted";
    let models: Vec<String> = if scripted { vec![String::new()] } else { o.planner_models.clone() };
    // The work list: every (scenario, model, wording, rep) run.
    let mut jobs = Vec::new();
    let mut seen_goals = std::collections::HashSet::new();
    for sc in &scenarios {
        if scripted && sc.steps.is_empty() {
            continue;
        }
        // Agent modes only see the goal, so scenarios sharing fixture+goal are one task.
        if !scripted && !seen_goals.insert((sc.fixture.clone(), sc.goal.clone())) {
            continue;
        }
        let paras = if scripted { 1 } else { o.paraphrases.clamp(1, 1 + sc.paraphrases.len()) };
        for model in &models {
            for para in 0..paras {
                for rep in 0..o.repeat {
                    jobs.push((*sc, model.clone(), para, rep));
                }
            }
        }
    }
    let workers = o.jobs.clamp(1, jobs.len().max(1));
    eprintln!(
        "bench {} · mode={} backend={} · {} scenarios × {} models × {} wordings × {} reps · strict={}{}",
        o.label,
        o.mode,
        if control { "chrome-devtools-mcp" } else { k.backend.as_str() },
        scenarios.len(),
        models.len(),
        if scripted { 1 } else { o.paraphrases },
        o.repeat,
        o.strict,
        if workers > 1 { format!(" · {workers} parallel workers") } else { String::new() }
    );
    let queue = std::sync::Mutex::new(jobs.into_iter().enumerate().collect::<std::collections::VecDeque<_>>());
    let done: std::sync::Mutex<Vec<(usize, Run)>> = Default::default();
    let (queue, done_ref, o_ref, k_ref) = (&queue, &done, &o, &k);
    // Each worker owns a browser (or chrome-devtools-mcp) and a fixture server,
    // so concurrent runs never share page state or outcome records.
    let worker = move |w: usize| async move {
        let srv = crate::server::start(0).await?;
        let mut sess = if control { None } else { Some(Session::new(k_ref.clone()).await?) };
        let cdt = if control {
            Some(McpClient::spawn("npx", &["-y", "chrome-devtools-mcp@latest", "--headless", "--isolated", "--no-usage-statistics"]).await?)
        } else {
            None
        };
        loop {
            let Some((idx, (sc, model, para, rep))) = queue.lock().unwrap().pop_front() else { break };
            if let (true, Some(s)) = (o_ref.record.is_some(), sess.as_mut()) {
                s.recorder = Some(Vec::new());
            }
            let r = match o_ref.mode.as_str() {
                "scripted" => run_scripted(sess.as_mut().unwrap(), &srv, sc, rep, o_ref.strict).await,
                "control" => run_agent(&mut Arm::Control(cdt.as_ref().unwrap()), &srv, sc, para, rep, &model, o_ref.strict).await,
                "goal" => run_agent(&mut Arm::Goal(sess.as_mut().unwrap()), &srv, sc, para, rep, &model, o_ref.strict).await,
                "agent" => run_agent(&mut Arm::Agent(sess.as_mut().unwrap()), &srv, sc, para, rep, &model, o_ref.strict).await,
                "vm" => run_agent(&mut Arm::Vm(sess.as_mut().unwrap()), &srv, sc, para, rep, &model, o_ref.strict).await,
                "script" => run_agent(&mut Arm::Script(sess.as_mut().unwrap()), &srv, sc, para, rep, &model, o_ref.strict).await,
                "do" => run_agent(&mut Arm::Do(sess.as_mut().unwrap()), &srv, sc, para, rep, &model, o_ref.strict).await,
                _ => run_agent(&mut Arm::Experiment(sess.as_mut().unwrap()), &srv, sc, para, rep, &model, o_ref.strict).await,
            };
            let cases = sess.as_mut().and_then(|s| s.recorder.take()).unwrap_or_default();
            // A browser that died (crash, auto-update) fails every later run in
            // this worker: start a fresh one.
            if r.fail.as_deref().is_some_and(|f| f.contains("cdp") || f.contains("browser gone")) && sess.is_some() {
                eprintln!("  (worker {w}: browser lost, relaunching)");
                if let Some(s) = sess.take() {
                    s.close().await;
                }
                sess = Some(Session::new(k_ref.clone()).await?);
            }
            let turns = r
                .planner
                .as_ref()
                .map(|p| format!(" turns {:>2} llm {:>5.0}ms tools {:>5.0}ms", p.turns, p.llm_ms, p.tool_ms))
                .unwrap_or_default();
            let tag = if scripted { String::new() } else { format!(" [{} p{}]", short_model(&model), para) };
            eprintln!(
                "  {:<26}{} #{} {} {:>7.0}ms{}  decide {:>2}×{:>4.0}ms{}",
                sc.name,
                tag,
                rep,
                if r.ok { "PASS" } else { "FAIL" },
                r.wall_ms,
                turns,
                r.timings.decide_calls,
                r.timings.decide_ms / r.timings.decide_calls.max(1) as f64,
                r.fail.as_ref().map(|f| format!("\n      ↳ {}", crate::trunc(f, 300))).unwrap_or_default()
            );
            if o_ref.verbose && !r.ok {
                eprintln!("{}", serde_json::to_string_pretty(&r.steps).unwrap_or_default());
            }
            let mut d = done_ref.lock().unwrap();
            if let Some(path) = &o_ref.record {
                if r.ok {
                    crate::decisions::append(path, &sc.name, cases)?;
                }
            }
            d.push((idx, r));
            // Keep partial results if the run is interrupted.
            let mut runs: Vec<(usize, Run)> = d.clone();
            runs.sort_by_key(|(i, _)| *i);
            write_report(o_ref, k_ref, &runs.into_iter().map(|(_, r)| r).collect::<Vec<_>>())?;
        }
        if let Some(s) = &sess {
            s.close().await;
        }
        anyhow::Ok(())
    };
    let mut set = Vec::new();
    for w in 0..workers {
        set.push(worker(w));
    }
    for r in join_all(set).await {
        r?;
    }
    let mut runs = done.into_inner().unwrap();
    runs.sort_by_key(|(i, _)| *i);
    let runs: Vec<Run> = runs.into_iter().map(|(_, r)| r).collect();
    let path = write_report(&o, &k, &runs)?;
    let report: Report = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    print_table(&report);
    eprintln!("wrote {}", path.display());
    Ok(path)
}

/// Polls every future to completion concurrently on the current task.
async fn join_all<F: std::future::Future>(futs: Vec<F>) -> Vec<F::Output> {
    let mut futs: Vec<std::pin::Pin<Box<F>>> = futs.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = futs.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in futs.iter_mut().enumerate() {
            if out[i].is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => out[i] = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending { std::task::Poll::Pending } else { std::task::Poll::Ready(()) }
    })
    .await;
    out.into_iter().map(|o| o.unwrap()).collect()
}

fn write_report(o: &BenchOpts, k: &Knobs, runs: &[Run]) -> Result<PathBuf> {
    let report = Report {
        label: o.label.clone(),
        mode: o.mode.clone(),
        knobs: serde_json::to_value(k)?,
        started: chrono_now(),
        runs: runs.iter().map(|r| serde_json::to_value(r).unwrap()).collect(),
    };
    std::fs::create_dir_all(&o.out_dir)?;
    let path = o.out_dir.join(format!("{}.json", o.label));
    std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
    Ok(path)
}

fn short_model(m: &str) -> &str {
    m.rsplit('/').next().unwrap_or(m)
}

fn chrono_now() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}", d.as_secs())
}

#[derive(Default)]
struct Agg {
    n: usize,
    pass: usize,
    wall: Vec<f64>,
    decide_calls: f64,
    decide_ms: f64,
    llm_ms: f64,
    llm_turns: f64,
    cost: f64,
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i]
}

fn aggregate(r: &Report, model: Option<&str>) -> Vec<(String, Agg)> {
    let mut out: Vec<(String, Agg)> = Vec::new();
    for run in &r.runs {
        if model.is_some_and(|m| run["model"].as_str().unwrap_or("") != m) {
            continue;
        }
        let name = run["scenario"].as_str().unwrap_or("?").to_string();
        let idx = match out.iter().position(|(n, _)| *n == name) {
            Some(i) => i,
            None => {
                out.push((name, Agg::default()));
                out.len() - 1
            }
        };
        let a = &mut out[idx].1;
        a.n += 1;
        if run["ok"].as_bool() == Some(true) {
            a.pass += 1;
        }
        a.wall.push(run["wall_ms"].as_f64().unwrap_or(0.0));
        let t = &run["timings"];
        a.decide_calls += t["decide_calls"].as_f64().unwrap_or(0.0);
        a.decide_ms += t["decide_ms"].as_f64().unwrap_or(0.0);
        a.cost += t["cost"].as_f64().unwrap_or(0.0);
        if let Some(p) = run.get("planner") {
            a.llm_ms += p["llm_ms"].as_f64().unwrap_or(0.0);
            a.llm_turns += p["turns"].as_f64().unwrap_or(0.0);
            a.cost += p["cost"].as_f64().unwrap_or(0.0);
        }
    }
    out
}

fn models_of(r: &Report) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for run in &r.runs {
        let m = run["model"].as_str().unwrap_or("").to_string();
        if !v.contains(&m) {
            v.push(m);
        }
    }
    v
}

pub fn print_table(r: &Report) {
    let planner = r.mode != "scripted";
    for model in models_of(r) {
        let aggs = aggregate(r, Some(&model));
        let title = if model.is_empty() { String::new() } else { format!(" · {model}") };
        println!("\n### {} ({}){title}\n", r.label, r.mode);
        if planner {
            println!("| scenario | pass | p50 ms | p95 ms | llm turns | llm ms | jev calls | jev ms |");
            println!("|---|---|---:|---:|---:|---:|---:|---:|");
        } else {
            println!("| scenario | pass | p50 ms | p95 ms | decide calls | ms/decide |");
            println!("|---|---|---:|---:|---:|---:|");
        }
        let (mut tp, mut tn, mut all) = (0, 0, Vec::new());
        let mut cost = 0.0;
        for (name, mut a) in aggs {
            tp += a.pass;
            tn += a.n;
            all.extend(a.wall.iter().copied());
            cost += a.cost;
            let n = a.n as f64;
            let p50 = pct(&mut a.wall, 0.5);
            let p95 = pct(&mut a.wall, 0.95);
            if planner {
                println!(
                    "| {name} | {}/{} | {p50:.0} | {p95:.0} | {:.1} | {:.0} | {:.1} | {:.0} |",
                    a.pass,
                    a.n,
                    a.llm_turns / n,
                    a.llm_ms / n,
                    a.decide_calls / n,
                    a.decide_ms / n
                );
            } else {
                println!(
                    "| {name} | {}/{} | {p50:.0} | {p95:.0} | {:.1} | {:.0} |",
                    a.pass,
                    a.n,
                    a.decide_calls / n,
                    a.decide_ms / a.decide_calls.max(1.0)
                );
            }
        }
        let p50 = pct(&mut all, 0.5);
        let sum: f64 = all.iter().sum();
        println!("\n**total: {tp}/{tn} pass · p50 {p50:.0} ms · sum {sum:.0} ms · cost ${cost:.4}**");
    }
}

/// Exact two-sided McNemar p-value from the discordant counts.
pub fn mcnemar(b: usize, c: usize) -> f64 {
    let n = b + c;
    if n == 0 {
        return 1.0;
    }
    let k = b.min(c);
    // Σ_{i≤k} C(n,i)/2^n, computed in log space.
    let ln_choose = |n: usize, i: usize| -> f64 {
        (1..=i).map(|j| ((n - i + j) as f64).ln() - (j as f64).ln()).sum::<f64>()
    };
    let tail: f64 = (0..=k).map(|i| (ln_choose(n, i) - n as f64 * std::f64::consts::LN_2).exp()).sum();
    (2.0 * tail).min(1.0)
}

type Key = (String, String, u64, u64);

fn keyed_runs(runs: &[Value]) -> std::collections::HashMap<Key, &Value> {
    runs.iter()
        .map(|run| {
            let k = (
                run["scenario"].as_str().unwrap_or("").to_string(),
                run["model"].as_str().unwrap_or("").to_string(),
                run["para"].as_u64().unwrap_or(0),
                run["rep"].as_u64().unwrap_or(0),
            );
            (k, run)
        })
        .collect()
}

fn keyed(r: &Report) -> std::collections::HashMap<Key, &Value> {
    keyed_runs(&r.runs)
}

/// Paired comparison of two result files: per-scenario table plus a paired
/// pass-rate test over matching (scenario, model, wording, rep) instances.
pub fn compare(a: &Path, b: &Path) -> Result<()> {
    let ra: Report = serde_json::from_str(&std::fs::read_to_string(a)?)?;
    let rb: Report = serde_json::from_str(&std::fs::read_to_string(b)?)?;
    let aa = aggregate(&ra, None);
    let ab = aggregate(&rb, None);
    println!("| scenario | {} pass | {} pass | {} p50 | {} p50 | Δ |", ra.label, rb.label, ra.label, rb.label);
    println!("|---|---|---|---:|---:|---:|");
    let (mut sa, mut sb) = (0.0, 0.0);
    let (mut pa, mut pb) = (0, 0);
    for (name, mut x) in aa {
        let Some((_, y)) = ab.iter().find(|(n, _)| *n == name) else { continue };
        let mut yw = y.wall.clone();
        let (px, py) = (pct(&mut x.wall, 0.5), pct(&mut yw, 0.5));
        sa += px;
        sb += py;
        pa += x.pass;
        pb += y.pass;
        println!(
            "| {name} | {}/{} | {}/{} | {px:.0} | {py:.0} | {:+.0}% |",
            x.pass,
            x.n,
            y.pass,
            y.n,
            (py / px.max(1.0) - 1.0) * 100.0
        );
    }
    println!("| **total** | {pa} | {pb} | {sa:.0} | {sb:.0} | {:+.0}% |", (sb / sa.max(1.0) - 1.0) * 100.0);

    // Paired analysis.
    let (ka, kb) = (keyed(&ra), keyed(&rb));
    let (mut both, mut a_only, mut b_only, mut neither) = (0, 0, 0, 0);
    let mut ratios = Vec::new();
    for (k, x) in &ka {
        let Some(y) = kb.get(k) else { continue };
        let (xo, yo) = (x["ok"].as_bool() == Some(true), y["ok"].as_bool() == Some(true));
        match (xo, yo) {
            (true, true) => {
                both += 1;
                let (wx, wy) = (x["wall_ms"].as_f64().unwrap_or(0.0), y["wall_ms"].as_f64().unwrap_or(0.0));
                if wx > 0.0 {
                    ratios.push(wy / wx);
                }
            }
            (true, false) => a_only += 1,
            (false, true) => b_only += 1,
            (false, false) => neither += 1,
        }
    }
    let n = both + a_only + b_only + neither;
    if n > 0 {
        let med = pct(&mut ratios, 0.5);
        println!(
            "\n**paired n={n}: {} {:.1}% vs {} {:.1}% · discordant {}-only {a_only} / {}-only {b_only} · McNemar p={:.3} · median wall ratio ({}/{}) on both-pass {med:.2}×**",
            ra.label,
            100.0 * (both + a_only) as f64 / n as f64,
            rb.label,
            100.0 * (both + b_only) as f64 / n as f64,
            ra.label,
            rb.label,
            mcnemar(a_only, b_only),
            rb.label,
            ra.label
        );
    }
    Ok(())
}

// ── arm-vs-arm reporting over saved result files ───────────────────────────

/// Provider failures — credits, rate limits, outages — say nothing about the
/// arm, so runs that hit one are left out of every average and every pairing.
/// Plain substrings, never patterns.
const PROVIDER_ERRORS: [&str; 8] = [
    "HTTP 402",
    "HTTP 429",
    "HTTP 500",
    "HTTP 502",
    "HTTP 503",
    "HTTP 504",
    "error decoding",
    "no message in llm",
];

fn provider_error(run: &Value) -> bool {
    let fail = run["fail"].as_str().unwrap_or("");
    PROVIDER_ERRORS.iter().any(|e| fail.contains(e))
}

fn run_ok(run: &Value) -> bool {
    run["ok"].as_bool().unwrap_or(false)
}

fn run_wall_ms(run: &Value) -> f64 {
    run["wall_ms"].as_f64().unwrap_or(0.0)
}

fn run_turns(run: &Value) -> f64 {
    run["planner"]["turns"].as_f64().unwrap_or(0.0)
}

/// What a task cost: the planner's LLM calls plus everything the engine called.
fn run_cost(run: &Value) -> f64 {
    run["planner"]["cost"].as_f64().unwrap_or(0.0) + run["timings"]["cost"].as_f64().unwrap_or(0.0)
}

/// Mean over a compensated (Neumaier) sum, so the result is the correctly
/// rounded exact mean rather than whatever the running total drifted to.
fn mean(v: &[f64]) -> f64 {
    let (mut sum, mut c) = (0.0, 0.0);
    for &x in v {
        let t = sum + x;
        if sum.abs() >= x.abs() { c += (sum - t) + x; } else { c += (x - t) + sum; }
        sum = t;
    }
    (sum + c) / v.len() as f64
}

/// Median, averaging the two middle values on an even count.
pub fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 }
}

/// `nan` reads like Python's, so old and new reports print the same.
fn f2(x: f64) -> String {
    if x.is_nan() { "nan".into() } else { format!("{x:.2}") }
}

/// The `runs` of one result file. Only `runs` is needed, so a file merged by
/// hand (no knobs, no timestamp) reads as well as one fab-bench wrote.
pub fn load_runs(path: &Path) -> Result<Vec<Value>> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?)?;
    Ok(v["runs"].as_array().cloned().unwrap_or_default())
}

/// Arm-vs-arm summary over paired runs: pass rate, McNemar exact p, wall
/// ratio, LLM turns and cost per task, then every arm pair.
pub fn summary(results: &Path, labels: &[String]) -> Result<()> {
    for line in summary_lines(results, labels)? {
        println!("{line}");
    }
    Ok(())
}

pub fn summary_lines(results: &Path, labels: &[String]) -> Result<Vec<String>> {
    let files: Vec<(String, Vec<Value>)> =
        labels.iter().map(|l| Ok((l.clone(), load_runs(&results.join(format!("{l}.json")))?))).collect::<Result<_>>()?;
    let data: Vec<(String, std::collections::HashMap<Key, &Value>)> =
        files.iter().map(|(l, runs)| (l.clone(), keyed_runs(runs))).collect();
    let mut out = vec![format!("{:<34} {:>4} {:>7} {:>9} {:>6} {:>8}", "arm", "n", "pass", "p50 wall", "turns", "$/task")];
    for (label, runs) in &data {
        let bad = runs.values().filter(|r| provider_error(r)).count();
        if bad > 0 {
            out.push(format!("  ({label}: {bad} runs failed on provider errors; excluded)"));
        }
        let rs: Vec<&&Value> = runs.values().filter(|r| !provider_error(r)).collect();
        if rs.is_empty() {
            anyhow::bail!("{label}: every run failed on provider errors");
        }
        let pass = rs.iter().filter(|r| run_ok(r)).count() as f64 / rs.len() as f64;
        let wall = median(&rs.iter().map(|r| run_wall_ms(r)).collect::<Vec<_>>()) / 1000.0;
        out.push(format!(
            "{:<34} {:>4} {:>6.1}% {:>8.1}s {:>6.1} ${:.4}",
            label,
            rs.len(),
            100.0 * pass,
            wall,
            mean(&rs.iter().map(|r| run_turns(r)).collect::<Vec<_>>()),
            mean(&rs.iter().map(|r| run_cost(r)).collect::<Vec<_>>())
        ));
    }
    out.push(String::new());
    for (ai, (a, da)) in data.iter().enumerate() {
        for (b, db) in &data[ai + 1..] {
            let keys: Vec<&Key> = da.keys().filter(|k| db.contains_key(*k) && !provider_error(da[*k]) && !provider_error(db[*k])).collect();
            if keys.is_empty() {
                continue;
            }
            let (mut bo, mut co) = (0, 0);
            let mut both = Vec::new();
            for k in &keys {
                match (run_ok(da[*k]), run_ok(db[*k])) {
                    (true, false) => bo += 1,
                    (false, true) => co += 1,
                    (true, true) => both.push(k),
                    (false, false) => {}
                }
            }
            let n = keys.len() as f64;
            let ratio = if both.is_empty() {
                f64::NAN
            } else {
                median(&both.iter().map(|k| run_wall_ms(db[*k]) / run_wall_ms(da[*k])).collect::<Vec<_>>())
            };
            // Speedup of B over A as a geometric mean of the paired times.
            let geo = if both.is_empty() {
                f64::NAN
            } else {
                (mean(&both.iter().map(|k| (run_wall_ms(da[*k]) / run_wall_ms(db[*k])).ln()).collect::<Vec<_>>())).exp()
            };
            out.push(format!(
                "{a} vs {b}: paired n={}  {:.1}% vs {:.1}%  discordant {bo}/{co}  McNemar p={:.3}  median wall ratio (B/A) {}x · B speedup geo-mean {}x on {} both-pass",
                keys.len(),
                100.0 * keys.iter().filter(|k| run_ok(da[**k])).count() as f64 / n,
                100.0 * keys.iter().filter(|k| run_ok(db[**k])).count() as f64 / n,
                mcnemar(bo, co),
                f2(ratio),
                f2(geo),
                both.len()
            ));
        }
    }
    Ok(out)
}

/// Each app's first task (the one that learns its shape) and its later tasks,
/// by scenario name. An app is a fixture; tasks run in suite order.
pub fn first_and_later_tasks(suite: &Suite) -> (Vec<String>, Vec<String>) {
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let (mut first, mut later) = (Vec::new(), Vec::new());
    for sc in &suite.scenario {
        let n = seen.entry(sc.fixture.as_str()).or_insert(0);
        if *n == 0 { first.push(sc.name.clone()) } else { later.push(sc.name.clone()) }
        *n += 1;
    }
    (first, later)
}

pub struct WarmOpts {
    pub suite: PathBuf,
    pub tag: String,
    pub planner_models: Vec<String>,
    pub paraphrases: usize,
    pub jobs: usize,
    pub out_dir: PathBuf,
    /// Extra `--set` overrides passed to every `fab-bench bench` child.
    pub sets: Vec<String>,
}

/// Site-shape transfer: learn each app's shape from its first task, then run
/// its later tasks with the learned shapes and without, at the same time, and
/// compare the paired arms. Each arm is a `fab-bench bench` child whose output
/// goes to `<out>/<tag>-{learn,use,cold}.log`.
pub async fn warm(o: WarmOpts) -> Result<()> {
    let (first, later) = first_and_later_tasks(&load_suite(&o.suite)?);
    if first.is_empty() || later.is_empty() {
        anyhow::bail!("{}: need at least two tasks on one fixture", o.suite.display());
    }
    let dir = std::env::current_dir()?.join("target").join(format!("shapes-{}", o.tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(&o.out_dir)?;
    let exe = std::env::current_exe()?;
    let arm = |only: &[String], suffix: &str, shape: &str, shaped_dir: bool| -> Result<tokio::process::Child> {
        let log = std::fs::File::create(o.out_dir.join(format!("{}-{suffix}.log", o.tag)))?;
        let mut c = tokio::process::Command::new(&exe);
        c.arg("bench")
            .args(["--suite".as_ref(), o.suite.as_os_str()])
            .args(["--mode", "agent", "--planner-model", &o.planner_models.join(",")])
            .args(["--paraphrases", &o.paraphrases.to_string(), "--jobs", &o.jobs.to_string()])
            .args(["--only", &only.join(",")])
            .args(["--out".as_ref(), o.out_dir.as_os_str()])
            .args(["--label", &format!("{}-{suffix}", o.tag)]);
        for s in &o.sets {
            c.args(["--set", s]);
        }
        c.args(["--set", &format!("shape={shape}")]);
        if shaped_dir {
            c.arg("--set").arg(format!("shape_dir={}", dir.display()));
        }
        Ok(c.stdout(log.try_clone()?).stderr(log).spawn()?)
    };
    let status = arm(&first, "learn", "record", true)?.wait().await?;
    if !status.success() {
        anyhow::bail!("learning pass failed ({status}); see {}-learn.log", o.out_dir.join(&o.tag).display());
    }
    let sites = std::fs::read_dir(&dir)?.count();
    println!("learned: {sites} sites");
    let mut used = arm(&later, "use", "use", true)?;
    let mut cold = arm(&later, "cold", "off", false)?;
    let (u, c) = (used.wait().await?, cold.wait().await?);
    if !u.success() || !c.success() {
        anyhow::bail!("paired pass failed (use: {u}, cold: {c}); see {}-{{use,cold}}.log", o.out_dir.join(&o.tag).display());
    }
    summary(&o.out_dir, &[format!("{}-cold", o.tag), format!("{}-use", o.tag)])
}

#[derive(Default)]
struct ArmRuns {
    n: usize,
    pass: usize,
    wall: Vec<f64>,
    turns: Vec<f64>,
    cost: Vec<f64>,
}

impl ArmRuns {
    fn pass_rate(&self) -> f64 {
        // A model with no runs in this arm sorts as 0, not as NaN.
        if self.n == 0 { 0.0 } else { self.pass as f64 / self.n as f64 }
    }
}

/// Planner-selection pilot report: strict pass rate, wall time, turns and cost
/// per model and arm, best experiment pass rate first.
pub fn pilot_report(results: &Path) -> Result<()> {
    for line in pilot_report_lines(results)? {
        println!("{line}");
    }
    Ok(())
}

pub fn pilot_report_lines(results: &Path) -> Result<Vec<String>> {
    let mut rows: Vec<(String, ArmRuns, ArmRuns)> = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(results)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("pilot-") && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    for path in files {
        let Ok(v) = serde_json::from_str::<Value>(&std::fs::read_to_string(&path)?) else { continue };
        let mode = v["mode"].as_str().unwrap_or("").to_string();
        for run in v["runs"].as_array().cloned().unwrap_or_default() {
            let model = run["model"].as_str().unwrap_or("").to_string();
            let control = mode == "control";
            let i = match rows.iter().position(|(m, ..)| *m == model) {
                Some(i) => i,
                None => {
                    rows.push((model, ArmRuns::default(), ArmRuns::default()));
                    rows.len() - 1
                }
            };
            let a = if control { &mut rows[i].2 } else { &mut rows[i].1 };
            a.n += 1;
            a.pass += run_ok(&run) as usize;
            a.wall.push(run_wall_ms(&run) / 1000.0);
            a.turns.push(run_turns(&run));
            a.cost.push(run_cost(&run));
        }
    }
    // Ties on the pass rate are broken by name: the files arrive in any order.
    rows.sort_by(|a, b| {
        b.1.pass_rate().partial_cmp(&a.1.pass_rate()).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(&b.0))
    });
    let mut out = vec![format!(
        "{:<32} | {:<38} | {:<38}",
        "model", "experiment: pass wall turns $/task", "control: pass wall turns $/task"
    )];
    for (model, exp, ctl) in &rows {
        let cell = |a: &ArmRuns| {
            if a.n == 0 {
                "-".to_string()
            } else {
                format!(
                    "{:>4.0}% {:>2}n {:>5.1}s {:>4.1}t ${:.4}",
                    100.0 * a.pass_rate(),
                    a.n,
                    median(&a.wall),
                    mean(&a.turns),
                    mean(&a.cost)
                )
            }
        };
        out.push(format!("{model:<32} | {:<38} | {:<38}", cell(exp), cell(ctl)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_task_per_fixture_learns_the_rest_are_measured() {
        let sc = |name: &str, fixture: &str| format!("[[scenario]]\nname = \"{name}\"\nfixture = \"{fixture}\"\ngoal = \"g\"\n");
        let text = [sc("a1", "a.html"), sc("b1", "b.html"), sc("a2", "a.html"), sc("a3", "a.html"), sc("b2", "b.html")].concat();
        let suite: Suite = toml::from_str(&text).unwrap();
        let (first, later) = first_and_later_tasks(&suite);
        assert_eq!(first, ["a1", "b1"]);
        assert_eq!(later, ["a2", "a3", "b2"]);
        let (f, l) = first_and_later_tasks(&Suite { scenario: vec![] });
        assert!(f.is_empty() && l.is_empty());
    }

    #[test]
    fn mcnemar_exact() {
        assert!((mcnemar(0, 0) - 1.0).abs() < 1e-12);
        // 1 vs 9 discordant: p = 2 * (1 + 10) / 1024
        assert!((mcnemar(1, 9) - 22.0 / 1024.0).abs() < 1e-9);
        assert!(mcnemar(5, 5) > 0.99);
        // 5 vs 3: p = 2 * (1 + 8 + 28 + 56) / 256 = 186/256
        assert!((mcnemar(5, 3) - 186.0 / 256.0).abs() < 1e-9);
        // 5 vs 2: p = 2 * (1 + 7 + 21) / 128 = 58/128
        assert!((mcnemar(5, 2) - 58.0 / 128.0).abs() < 1e-9);
        // One-sided: 0 vs 7 → 2/128.
        assert!((mcnemar(0, 7) - 2.0 / 128.0).abs() < 1e-9);
        // Symmetric in its arguments.
        assert!((mcnemar(2, 5) - mcnemar(5, 2)).abs() < 1e-12);
    }

    #[test]
    fn median_takes_the_middle_two() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert!(median(&[]).is_nan());
    }

    #[test]
    fn provider_errors_are_matched_as_substrings() {
        for e in PROVIDER_ERRORS {
            assert!(provider_error(&json!({"fail": format!("planner: {e}, giving up")})), "{e}");
        }
        // A task failure is not a provider failure, and matching is exact-case.
        assert!(!provider_error(&json!({"fail": "expected record missing: refund"})));
        assert!(!provider_error(&json!({"fail": "http 429 slow down"})));
        assert!(!provider_error(&json!({"ok": false, "fail": null})));
    }

    fn run(scenario: &str, ok: bool, wall_ms: f64, fail: Option<&str>) -> Value {
        json!({"scenario": scenario, "model": "m", "para": 0, "rep": 0, "ok": ok, "fail": fail,
               "wall_ms": wall_ms, "timings": {"cost": 0.001}, "planner": {"turns": 4, "cost": 0.002}})
    }

    fn results_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fab-bench-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Known-answer summary: the provider-error run leaves every average, and
    /// the paired line carries the discordant counts, the exact p and both
    /// speed figures.
    #[test]
    fn summary_excludes_provider_errors_and_pairs_by_key() {
        let dir = results_dir("summary");
        let a = json!({"runs": [
            run("s1", true, 10_000.0, None),
            run("s2", true, 20_000.0, None),
            run("s3", true, 30_000.0, None),
            run("s4", false, 1.0, Some("HTTP 429 rate limited")),
        ]});
        let b = json!({"runs": [
            run("s1", true, 5_000.0, None),
            run("s2", false, 4_000.0, Some("expected record missing")),
            run("s3", true, 30_000.0, None),
            run("s4", true, 1.0, None),
        ]});
        std::fs::write(dir.join("a.json"), a.to_string()).unwrap();
        std::fs::write(dir.join("b.json"), b.to_string()).unwrap();
        let out = summary_lines(&dir, &["a".into(), "b".into()]).unwrap();
        assert_eq!(out[1], "  (a: 1 runs failed on provider errors; excluded)");
        // 3 usable runs, all passing, median 20.0 s, 4.0 turns, $0.0030 a task.
        assert_eq!(out[2], format!("{:<34} {:>4} {:>6.1}% {:>8.1}s {:>6.1} ${:.4}", "a", 3, 100.0, 20.0, 4.0, 0.003));
        // b keeps all four: median of 1, 4000, 5000, 30000 ms is 4.5 s.
        assert_eq!(out[3], format!("{:<34} {:>4} {:>6.1}% {:>8.1}s {:>6.1} ${:.4}", "b", 4, 75.0, 4.5, 4.0, 0.003));
        // s4 is out (a hit a provider error), so n=3: a 3/3, b 2/3, one
        // discordant pair a-only, p = 1, and both-pass s1 (2×) and s3 (1×).
        assert_eq!(
            out[5],
            "a vs b: paired n=3  100.0% vs 66.7%  discordant 1/0  McNemar p=1.000  median wall ratio (B/A) 0.75x · B speedup geo-mean 1.41x on 2 both-pass"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn pilot_run(scenario: &str, model: &str, ok: bool, wall_ms: f64) -> Value {
        json!({"scenario": scenario, "model": model, "para": 0, "rep": 0, "ok": ok, "fail": null,
               "wall_ms": wall_ms, "timings": {"cost": 0.0}, "planner": {"turns": 3, "cost": 0.0}})
    }

    /// The pilot report reads the arm from each file's own `mode`, and orders
    /// models by experiment pass rate with ties broken by name.
    #[test]
    fn pilot_report_orders_by_pass_rate_then_name() {
        let dir = results_dir("pilot");
        let exp = json!({"mode": "experiment", "runs": [
            pilot_run("a", "z-ai/glm", true, 1000.0), pilot_run("b", "z-ai/glm", false, 3000.0),
            pilot_run("a", "inception/mercury", true, 2000.0), pilot_run("b", "inception/mercury", true, 4000.0),
        ]});
        let ctl = json!({"mode": "control", "runs": [pilot_run("a", "stepfun/step", true, 9000.0)]});
        std::fs::write(dir.join("pilot-comp-exp-cheap.json"), exp.to_string()).unwrap();
        std::fs::write(dir.join("pilot-comp-ctl-cheap.json"), ctl.to_string()).unwrap();
        let out = pilot_report_lines(&dir).unwrap();
        assert_eq!(out[0], format!("{:<32} | {:<38} | {:<38}", "model", "experiment: pass wall turns $/task", "control: pass wall turns $/task"));
        let names: Vec<&str> = out[1..].iter().map(|l| l.split('|').next().unwrap().trim()).collect();
        // 2/2 beats 1/2, and stepfun only ran in the control arm.
        assert_eq!(names, ["inception/mercury", "z-ai/glm", "stepfun/step"]);
        assert_eq!(
            out[1],
            format!("{:<32} | {:<38} | {:<38}", "inception/mercury", format!("{:>4.0}% {:>2}n {:>5.1}s {:>4.1}t ${:.4}", 100.0, 2, 3.0, 3.0, 0.0), "-")
        );
        assert!(out[2].contains(" | - "), "no control cell: {}", out[2]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn strict_rejects_unlisted_events() {        let sc: Scenario = toml::from_str(
            r#"
name = "t"
fixture = "f.html"
goal = "g"
expect_records = [{ event = "save", email = "a@b.c" }]
allow_records = [{ event = "view" }]
"#,
        )
        .unwrap();
        let ok = vec![json!({"event": "save", "email": "a@b.c"}), json!({"event": "view", "id": 3})];
        assert!(check_records(&sc, &ok, true).is_none());
        let bad = vec![json!({"event": "save", "email": "a@b.c"}), json!({"event": "wishlist", "id": 3})];
        assert!(check_records(&sc, &bad, true).unwrap().contains("unexpected mutation"));
        assert!(check_records(&sc, &bad, false).is_none());
    }
}
