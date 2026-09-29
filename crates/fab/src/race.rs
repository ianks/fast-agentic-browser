//! `fab-bench race`: watch two arms run the same goals live, side by side.
//! Left: the planner LLM driving chrome-devtools-mcp (it decides every click).
//! Right: the same LLM calling fab (`do`: Jev makes the decisions).
//! Both start each scenario together; a dashboard shows timers, tool calls and
//! the scoreboard while the two visible browser windows do the work.

use anyhow::Result;
use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use fab_core::backend::cdp::Chrome;
use fab_core::config::window_args;
use fab_core::{Knobs, Session};

use crate::bench::{self, Arm, Run, Scenario};
use crate::mcp_client::McpClient;
use crate::planner::Event;

pub struct RaceOpts {
    pub suites: Vec<PathBuf>,
    pub only: Vec<String>,
    pub model: String,
    /// "goal" (fab `do`) or "experiment" (fab act/run).
    pub right: String,
    pub para: usize,
    pub pause_ms: u64,
    /// Once both browsers show the start page, wait this long before the clocks start.
    pub lead_ms: u64,
    pub headless: bool,
    pub port: u16,
    pub hold_s: u64,
    pub label: String,
}

#[derive(Default, Serialize, Clone)]
struct ArmView {
    label: String,
    status: String,
    #[serde(skip)]
    started: Option<Instant>,
    final_ms: Option<f64>,
    elapsed_ms: f64,
    turns: u32,
    fail: Option<String>,
    events: Vec<Event>,
}

#[derive(Serialize, Clone)]
struct Cell {
    ok: bool,
    ms: f64,
    turns: u32,
}

#[derive(Serialize, Clone)]
struct Row {
    scenario: String,
    cells: [Cell; 2],
}

#[derive(Default, Serialize, Clone)]
struct RaceState {
    suite: String,
    model: String,
    idx: usize,
    total: usize,
    scenario: String,
    goal: String,
    done: bool,
    arms: [ArmView; 2],
    results: Vec<Row>,
}

type Shared = Arc<Mutex<RaceState>>;

/// Usable area of the main display in points, top-left origin (below the menu
/// bar, above the Dock): (x, y, w, h). macOS via AppKit; a laptop default elsewhere.
fn screen() -> (i32, i32, i32, i32) {
    const JXA: &str = "ObjC.import('AppKit'); var s = $.NSScreen.mainScreen; var f = s.frame, v = s.visibleFrame; \
        [v.origin.x, f.size.height - v.origin.y - v.size.height, v.size.width, v.size.height].map(Math.round).join(',')";
    if let Ok(o) = std::process::Command::new("osascript").args(["-l", "JavaScript", "-e", JXA]).output() {
        let v: Vec<i32> = String::from_utf8_lossy(&o.stdout).split(',').filter_map(|x| x.trim().parse().ok()).collect();
        if v.len() == 4 && v[2] > 800 && v[3] > 600 {
            return (v[0], v[1], v[2], v[3]);
        }
    }
    (0, 38, 1512, 944)
}

/// (left browser, right browser, dashboard) rectangles "x,y,w,h".
fn layout() -> (String, String, String) {
    let (x, top, w, h) = screen();
    let dash_h = 300.min(h / 3);
    let bh = h - dash_h;
    let half = w / 2;
    (
        format!("{x},{top},{half},{bh}"),
        format!("{},{top},{},{bh}", x + half, w - half),
        format!("{x},{},{w},{dash_h}", top + bh),
    )
}

struct Dashboard {
    child: tokio::process::Child,
    profile: PathBuf,
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let p = self.profile.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            let _ = std::fs::remove_dir_all(p);
        });
    }
}

/// Opens the dashboard as a small Chrome app window (own profile, no tabs).
fn open_dashboard(url: &str, rect: &str) -> Result<Dashboard> {
    // A Chrome app window (the flags are Chrome's), whatever the default browser is.
    let d = fab_core::backend::discover::pick;
    let bin = d("chrome").or_else(|_| d("chromium")).or_else(|_| d(""))?.0.path;
    let profile = std::env::temp_dir().join(format!("fab-race-dash-{}", std::process::id()));
    std::fs::create_dir_all(&profile)?;
    let child = tokio::process::Command::new(bin)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--disable-extensions", "--disable-sync"])
        .arg(format!("--app={url}"))
        .args(window_args(rect))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    Ok(Dashboard { child, profile })
}

fn snapshot(state: &Shared) -> Value {
    let mut s = state.lock().unwrap().clone();
    for a in s.arms.iter_mut() {
        a.elapsed_ms = match (a.final_ms, a.started) {
            (Some(ms), _) => ms,
            (None, Some(t)) => t.elapsed().as_secs_f64() * 1e3,
            _ => 0.0,
        };
    }
    serde_json::to_value(&s).unwrap_or(Value::Null)
}

async fn serve_dashboard(state: Shared, port: u16) -> Result<u16> {
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/state", get(|State(s): State<Shared>| async move { axum::Json(snapshot(&s)).into_response() }))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(port)
}

const LABELS: [&str; 2] = ["LLM", "JEV"];

pub async fn race(k: Knobs, o: RaceOpts) -> Result<()> {
    let mut all = Vec::new();
    for p in &o.suites {
        all.extend(bench::load_suite(p)?.scenario);
    }
    let mut seen = std::collections::HashSet::new();
    let scenarios: Vec<Scenario> = all
        .into_iter()
        .filter(|s| o.only.is_empty() || o.only.iter().any(|n| s.name == *n))
        .filter(|s| seen.insert((s.fixture.clone(), s.goal.clone())))
        .collect();
    anyhow::ensure!(!scenarios.is_empty(), "no scenarios selected");

    let (left_rect, right_rect, dash_rect) = layout();
    let headful = !o.headless;

    // Each arm gets its own fixture server so their outcome records never mix.
    let (srv_l, srv_r) = (crate::server::start(0).await?, crate::server::start(0).await?);

    // Right: fab, visible, positioned.
    let mut kr = k.clone();
    kr.headful = headful;
    kr.window = Some(right_rect.clone());
    // Left: our own visible Chrome (no fab instrumentation), driven by chrome-devtools-mcp.
    let left_extra = if headful { window_args(&left_rect) } else { vec![] };
    let (sess, chrome_l) = tokio::join!(Session::new(kr), Chrome::spawn(!headful, &left_extra));
    let mut sess = sess?;
    let chrome_l = chrome_l?;
    let browser_url = format!("http://127.0.0.1:{}", chrome_l.port);
    let cdt = McpClient::spawn("npx", &["-y", "chrome-devtools-mcp@latest", "--browserUrl", browser_url.as_str(), "--no-usage-statistics"])
        .await?;

    let right_label = match o.right.as_str() {
        "goal" => "JEV · fab do()",
        "agent" => "JEV · fab agent mode",
        "vm" => "JEV · engine only (no LLM)",
        _ => "JEV · fab act/run",
    };
    let state: Shared = Arc::new(Mutex::new(RaceState {
        suite: o.suites.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
        model: o.model.clone(),
        total: scenarios.len(),
        ..Default::default()
    }));
    let port = serve_dashboard(state.clone(), o.port).await?;
    let url = format!("http://127.0.0.1:{port}/");
    let _dash = if headful { Some(open_dashboard(&url, &dash_rect)?) } else { None };
    eprintln!("race: {} scenarios · planner {} · dashboard {url}", scenarios.len(), o.model);
    eprintln!("      left window = LLM + chrome-devtools-mcp · right window = {right_label}");

    // Live events → dashboard + stdout.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let st2 = state.clone();
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            let line = format!("{:>6.1}s {} │ {}", e.ms / 1e3, LABELS[e.arm], e.text);
            let mut s = st2.lock().unwrap();
            let a = &mut s.arms[e.arm];
            match e.kind {
                "start" => {
                    a.status = "running".into();
                    a.started = Some(Instant::now());
                    continue;
                }
                "llm" => a.turns += 1,
                "finish" => a.status = "checking".into(),
                _ => {}
            }
            a.events.push(e);
            if a.events.len() > 300 {
                a.events.remove(0);
            }
            drop(s);
            println!("{line}");
        }
    });

    let mut runs: Vec<[Run; 2]> = Vec::new();
    let work = async {
        for (n, sc) in scenarios.iter().enumerate() {
            {
                let mut s = state.lock().unwrap();
                s.idx = n + 1;
                s.scenario = sc.name.clone();
                s.goal = sc.goal_variant(o.para).to_string();
                s.arms = [
                    ArmView { label: "LLM · chrome-devtools-mcp".into(), status: "loading".into(), ..Default::default() },
                    ArmView { label: right_label.into(), status: "loading".into(), ..Default::default() },
                ];
            }
            println!("\n▶ [{}/{}] {} — {}", n + 1, scenarios.len(), sc.name, sc.goal_variant(o.para));
            let start = tokio::sync::Barrier::new(2);
            let right_arm = match o.right.as_str() {
                "goal" => Arm::Goal(&mut sess),
                "agent" => Arm::Agent(&mut sess),
                "vm" => Arm::Vm(&mut sess),
                _ => Arm::Experiment(&mut sess),
            };
            let mut right_arm = right_arm;
            let mut left_arm = Arm::Control(&cdt);
            let (l, r) = tokio::join!(
                bench::run_agent_with(&mut left_arm, &srv_l, sc, o.para, 0, &o.model, true, Some((&start, Duration::from_millis(o.lead_ms))), Some((0, tx.clone()))),
                bench::run_agent_with(&mut right_arm, &srv_r, sc, o.para, 0, &o.model, true, Some((&start, Duration::from_millis(o.lead_ms))), Some((1, tx.clone()))),
            );
            let cells = [&l, &r].map(|x| Cell {
                ok: x.ok,
                ms: x.wall_ms,
                turns: x.planner.as_ref().map(|p| p.turns).unwrap_or(0),
            });
            {
                let mut s = state.lock().unwrap();
                for (i, x) in [&l, &r].iter().enumerate() {
                    let a = &mut s.arms[i];
                    a.status = if x.ok { "PASS".into() } else { "FAIL".into() };
                    a.final_ms = Some(x.wall_ms);
                    a.fail = x.fail.clone().map(|f| crate::trunc(&f, 200));
                }
                s.results.push(Row { scenario: sc.name.clone(), cells: cells.clone() });
            }
            let speed = if l.ok && r.ok && r.wall_ms > 0.0 { format!(" · JEV {:.1}× faster", l.wall_ms / r.wall_ms) } else { String::new() };
            println!(
                "■ {}  LLM {} {:.1}s ({} turns)  │  JEV {} {:.1}s ({} turns){speed}",
                sc.name,
                if l.ok { "PASS" } else { "FAIL" },
                l.wall_ms / 1e3,
                cells[0].turns,
                if r.ok { "PASS" } else { "FAIL" },
                r.wall_ms / 1e3,
                cells[1].turns
            );
            for (lab, x) in [("LLM", &l), ("JEV", &r)] {
                if let Some(f) = &x.fail {
                    println!("    {lab} ↳ {}", crate::trunc(f, 220));
                }
            }
            runs.push([l, r]);
            tokio::time::sleep(Duration::from_millis(o.pause_ms)).await;
        }
        anyhow::Ok(())
    };

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let stopped = tokio::select! {
        r = work => { r?; false }
        _ = tokio::signal::ctrl_c() => true,
        _ = term.recv() => true,
    };

    // Summary.
    let (mut pass, mut ms) = ([0usize; 2], [0f64; 2]);
    for [l, r] in &runs {
        for (i, x) in [l, r].iter().enumerate() {
            pass[i] += x.ok as usize;
            ms[i] += x.wall_ms;
        }
    }
    println!(
        "\n{}race {}: LLM {}/{} in {:.1}s · JEV {}/{} in {:.1}s · JEV {:.1}× faster overall",
        if stopped { "(stopped) " } else { "" },
        o.label,
        pass[0],
        runs.len(),
        ms[0] / 1e3,
        pass[1],
        runs.len(),
        ms[1] / 1e3,
        if ms[1] > 0.0 { ms[0] / ms[1] } else { 0.0 }
    );
    let report = json!({
        "label": o.label, "model": o.model, "suites": o.suites, "right": o.right,
        "runs": runs.iter().map(|[l, r]| json!({"left": l, "right": r})).collect::<Vec<_>>(),
    });
    std::fs::create_dir_all("bench/results")?;
    std::fs::write(format!("bench/results/{}.json", o.label), serde_json::to_string_pretty(&report)?)?;
    state.lock().unwrap().done = true;
    if !stopped && headful && o.hold_s > 0 {
        eprintln!("holding the windows for {}s (Ctrl-C to close)", o.hold_s);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(o.hold_s)) => {}
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    sess.close().await;
    drop(cdt);
    drop(chrome_l);
    Ok(())
}

const PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>fab race</title>
<style>
:root { --bg:#0f1115; --panel:#171a21; --line:#262b36; --fg:#e6e8ee; --dim:#8a93a6; --llm:#f59e0b; --jev:#22c55e; --bad:#ef4444; }
* { box-sizing:border-box } html,body { margin:0; height:100%; background:var(--bg); color:var(--fg); font:12px/1.35 ui-monospace,SFMono-Regular,Menlo,monospace }
#top { display:flex; gap:12px; align-items:baseline; padding:6px 10px; border-bottom:1px solid var(--line); white-space:nowrap; overflow:hidden }
#top .sc { font-weight:700 } #top .goal { color:var(--dim); overflow:hidden; text-overflow:ellipsis; flex:1 } #top .tot { font-weight:700 }
#main { display:grid; grid-template-columns: 1fr 1fr 250px; height:calc(100% - 29px) }
.arm { display:flex; flex-direction:column; border-right:1px solid var(--line); min-width:0 }
.head { display:flex; align-items:baseline; gap:10px; padding:5px 10px; border-bottom:1px solid var(--line) }
.head .name { font-weight:700 } .llm .name { color:var(--llm) } .jev .name { color:var(--jev) }
.timer { font-size:22px; font-weight:700; margin-left:auto; font-variant-numeric:tabular-nums }
.badge { padding:1px 6px; border-radius:4px; background:var(--line); color:var(--dim) }
.badge.PASS { background:#14532d; color:#bbf7d0 } .badge.FAIL { background:#7f1d1d; color:#fecaca } .badge.running { background:#1e3a8a; color:#bfdbfe }
.ev { flex:1; overflow:auto; padding:4px 10px } .ev div { white-space:nowrap; overflow:hidden; text-overflow:ellipsis }
.ev .t { color:var(--dim); display:inline-block; width:52px } .k-llm { color:#c4b5fd } .k-call { color:var(--fg) } .k-result { color:var(--dim) } .k-finish { color:var(--jev); font-weight:700 } .k-error { color:var(--bad) } .k-jev { color:#86efac }
.fail { color:#fca5a5; padding:2px 10px; white-space:nowrap; overflow:hidden; text-overflow:ellipsis }
#board { overflow:auto; padding:4px 8px } #board table { width:100%; border-collapse:collapse } #board td { padding:1px 3px; white-space:nowrap }
#board td.n { color:var(--dim); max-width:110px; overflow:hidden; text-overflow:ellipsis } .ok { color:var(--jev) } .no { color:var(--bad) }
</style></head><body>
<div id="top"><span class="sc" id="sc">starting…</span><span class="goal" id="goal"></span><span class="tot" id="tot"></span></div>
<div id="main">
 <div class="arm llm"><div class="head"><span class="name">◀ LLM</span><span id="l0" class="dim"></span><span class="badge" id="b0">—</span><span class="timer" id="t0">0.0s</span></div><div class="fail" id="f0"></div><div class="ev" id="e0"></div></div>
 <div class="arm jev"><div class="head"><span class="name">JEV ▶</span><span id="l1" class="dim"></span><span class="badge" id="b1">—</span><span class="timer" id="t1">0.0s</span></div><div class="fail" id="f1"></div><div class="ev" id="e1"></div></div>
 <div id="board"><table id="tb"></table></div>
</div>
<script>
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
let last = null, seen = [0, 0];
function render(s) {
  $("sc").textContent = s.done ? `finished · ${s.total} scenarios` : `${s.idx}/${s.total} ${s.scenario}`;
  $("goal").textContent = s.goal + "  ·  planner " + s.model;
  let p = [0, 0], t = [0, 0], n = s.results.length;
  s.results.forEach((r) => r.cells.forEach((c, i) => { p[i] += c.ok; t[i] += c.ms; }));
  $("tot").innerHTML = n ? `<span style="color:var(--llm)">LLM ${p[0]}/${n} ${(t[0]/1e3).toFixed(1)}s</span> · <span style="color:var(--jev)">JEV ${p[1]}/${n} ${(t[1]/1e3).toFixed(1)}s</span> · ${(t[1] ? t[0]/t[1] : 0).toFixed(1)}× ` : "";
  s.arms.forEach((a, i) => {
    $("l" + i).textContent = a.label.split("·")[1] || "";
    const b = $("b" + i); b.textContent = a.status || "—"; b.className = "badge " + (a.status || "");
    $("t" + i).textContent = (a.elapsed_ms / 1e3).toFixed(1) + "s";
    $("f" + i).textContent = a.fail || "";
    const box = $("e" + i);
    if (a.events.length < seen[i]) { box.innerHTML = ""; seen[i] = 0; }
    for (const e of a.events.slice(seen[i])) {
      const d = document.createElement("div");
      d.innerHTML = `<span class="t">${(e.ms/1e3).toFixed(1)}s</span><span class="k-${e.kind}">${esc(e.text)}</span>`;
      box.appendChild(d);
    }
    if (a.events.length !== seen[i]) box.scrollTop = box.scrollHeight;
    seen[i] = a.events.length;
  });
  $("tb").innerHTML = "<tr><td class=n>scenario</td><td style='color:var(--llm)'>LLM</td><td style='color:var(--jev)'>JEV</td></tr>" +
    s.results.slice().reverse().map((r) => `<tr><td class=n title="${esc(r.scenario)}">${esc(r.scenario)}</td>` +
      r.cells.map((c) => `<td class="${c.ok ? "ok" : "no"}">${c.ok ? "✓" : "✗"} ${(c.ms/1e3).toFixed(1)}s</td>`).join("") + "</tr>").join("");
}
async function tick() {
  try { const r = await fetch("/state", {cache: "no-store"}); last = await r.json(); render(last); }
  catch (_) { if (last) { last.done = true; render(last); } }
  setTimeout(tick, 250);
}
tick();
</script></body></html>"##;
