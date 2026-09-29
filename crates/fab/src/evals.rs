//! Evals that drive the `fab` CLI the way an agent does and read its event
//! stream: the scraping eval (cases in bench/scrape/cases.toml, gold records
//! from `scrape_gen`), the compile pilot, the page-pool stress test, and an
//! independent Hacker News check. Results go under bench/results/ (ignored).

use crate::events::Event;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The repo root (bench/, fixtures/).
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The `fab` next to this binary (or `$FAB`).
pub fn fab_exe() -> PathBuf {
    std::env::var_os("FAB").map(PathBuf::from).unwrap_or_else(|| std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("fab"))).unwrap_or_else(|| "fab".into()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub name: String,
    pub url: String,
    pub prompt: String,
    /// Gold fields that identify a record, joined by "+".
    pub key: String,
}

pub fn cases() -> Result<Vec<Case>> {
    #[derive(Deserialize)]
    struct File {
        case: Vec<Case>,
    }
    let text = std::fs::read_to_string(root().join("bench/scrape/cases.toml"))?;
    Ok(toml::from_str::<File>(&text)?.case)
}

pub fn gold(case: &str) -> Result<Vec<Map<String, Value>>> {
    let text = std::fs::read_to_string(root().join(format!("bench/scrape/gold/{case}.jsonl"))).with_context(|| format!("no gold for {case}: run `fab-bench scrape gen`"))?;
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| Ok(serde_json::from_str(l)?)).collect()
}

// ---------------------------------------------------------------- scoring

fn norm_key(k: &str) -> String {
    let mut out = String::new();
    let mut sep = false;
    for c in k.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if sep && !out.is_empty() {
                out.push('_');
            }
            sep = false;
            out.push(c);
        } else {
            sep = true;
        }
    }
    out
}

/// Python's `f"{v:g}"`: six significant digits, trailing zeros dropped.
fn fmt_g(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return if v == 0.0 { "0".into() } else { v.to_string() };
    }
    let sci = format!("{v:.5e}");
    let (mant, exp) = sci.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("exponent");
    let trim = |s: &str| if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s.to_string() };
    if !(-4..6).contains(&exp) {
        format!("{}e{}{:02}", trim(mant), if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        trim(&format!("{v:.*}", (5 - exp).max(0) as usize))
    }
}

/// A value as compared: whitespace-normalized text; null-ish is None.
fn norm_val(v: Option<&Value>) -> Option<String> {
    let s = match v? {
        Value::Null => return None,
        Value::Number(n) => match n.as_i64() {
            Some(i) if !n.is_f64() => i.to_string(),
            _ => fmt_g(n.as_f64().unwrap_or(0.0)),
        },
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let s = s.replace('\u{a0}', " ").split_whitespace().collect::<Vec<_>>().join(" ");
    (!["", "null", "none", "n/a"].contains(&s.to_lowercase().as_str())).then_some(s)
}

/// The output's value for gold field `k`: the same normalized name, else a
/// name containing it (or contained in it).
fn field<'a>(out: &'a Map<String, Value>, k: &str) -> Option<&'a Value> {
    let o: Vec<(String, &Value)> = out.iter().map(|(a, b)| (norm_key(a), b)).collect();
    o.iter().find(|(a, _)| a == k).or_else(|| o.iter().find(|(a, _)| a.contains(k) || k.contains(a.as_str()))).map(|(_, b)| *b)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    pub case: String,
    pub gold: usize,
    pub out: usize,
    pub precision: f64,
    pub recall: f64,
    pub fields: f64,
    pub order_inversions: usize,
    pub exact: bool,
    pub missing: Vec<Value>,
    pub wrong: Vec<Value>,
    #[serde(default)]
    pub secs: f64,
}

pub fn score(case: &Case, gold: &[Map<String, Value>], outs: &[Map<String, Value>]) -> Score {
    let keys: Vec<&str> = case.key.split('+').collect();
    let key_of = |get: &dyn Fn(&str) -> Option<String>| keys.iter().map(|k| get(k).unwrap_or_default().to_lowercase()).collect::<Vec<_>>();
    let mut by_key: std::collections::HashMap<Vec<String>, Vec<usize>> = Default::default();
    for (i, g) in gold.iter().enumerate() {
        by_key.entry(key_of(&|k| norm_val(g.get(k)))).or_default().push(i);
    }
    let (mut matched, mut order, mut fields_ok, mut fields_n, mut wrong) = (0usize, vec![], 0usize, 0usize, vec![]);
    let mut used = std::collections::HashSet::new();
    for o in outs {
        let k = key_of(&|k| norm_val(field(o, k)));
        let Some(&i) = by_key.get(&k).into_iter().flatten().find(|i| !used.contains(*i)) else {
            wrong.push(json!({"extra": o}));
            continue;
        };
        used.insert(i);
        matched += 1;
        order.push(i);
        for (gk, gv) in &gold[i] {
            fields_n += 1;
            let ov = field(o, gk);
            if norm_val(ov) == norm_val(Some(gv)) {
                fields_ok += 1;
            } else if wrong.len() < 8 {
                wrong.push(json!({"field": gk, "want": gv, "got": ov, "rec": i}));
            }
        }
    }
    let missing: Vec<Value> = (0..gold.len()).filter(|i| !used.contains(i)).take(3).map(|i| Value::Object(gold[i].clone())).collect();
    let inversions = order.windows(2).filter(|w| w[1] < w[0]).count();
    let p = if outs.is_empty() { 0.0 } else { matched as f64 / outs.len() as f64 };
    let r = if gold.is_empty() { 1.0 } else { matched as f64 / gold.len() as f64 };
    let fa = if fields_n == 0 { 0.0 } else { fields_ok as f64 / fields_n as f64 };
    let round = |x: f64| (x * 10_000.0).round() / 10_000.0;
    Score {
        case: case.name.clone(),
        gold: gold.len(),
        out: outs.len(),
        precision: round(p),
        recall: round(r),
        fields: round(fa),
        order_inversions: inversions,
        exact: p == 1.0 && r == 1.0 && fa == 1.0 && inversions == 0,
        missing,
        wrong: wrong.into_iter().take(5).collect(),
        secs: 0.0,
    }
}

/// The records in a `fab` event stream.
pub fn records(stdout: &str) -> Vec<Map<String, Value>> {
    stdout.lines().filter_map(|l| serde_json::from_str::<Event>(l).ok()).filter_map(|e| if let Event::Record { record, .. } = e { Some(record.data) } else { None }).collect()
}

/// The `end` of a `fab` event stream.
pub fn end(stdout: &str) -> Option<crate::events::End> {
    stdout.lines().rev().find_map(|l| match serde_json::from_str::<Event>(l) {
        Ok(Event::End(e)) => Some(e),
        _ => None,
    })
}

// ---------------------------------------------------------------- scrape run

/// How a case is asked: in words, or as a structured request whose
/// `records` schema is derived from the gold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Arm {
    Words,
    Schema,
}

/// A records schema with the gold's fields, in order: strings (nullable
/// where the gold has a null), numbers where the gold has only numbers.
pub fn schema_of(gold: &[Map<String, Value>]) -> Value {
    let mut props = Map::new();
    let mut required = vec![];
    for k in gold.first().map(|g| g.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
        let vals: Vec<&Value> = gold.iter().filter_map(|g| g.get(&k)).collect();
        let nullable = vals.iter().any(|v| v.is_null()) || vals.len() < gold.len();
        let kind = if vals.iter().filter(|v| !v.is_null()).all(|v| v.is_number()) && vals.iter().any(|v| v.is_number()) { "number" } else { "string" };
        props.insert(k.clone(), if nullable { json!({"type": [kind, "null"]}) } else { json!({"type": kind}) });
        if !nullable {
            required.push(k);
        }
    }
    json!({"type": "object", "properties": props, "required": required})
}

pub async fn run_case(case: &Case, arm: Arm, out_dir: &Path) -> Result<Score> {
    let fab = fab_exe();
    let session = format!("ev-{}", case.name);
    let _ = tokio::process::Command::new(&fab).args(["close", "-s", &session]).output().await;
    let gold = gold(&case.name)?;
    let body = match arm {
        Arm::Words => case.prompt.clone(),
        Arm::Schema => json!({"do": case.prompt, "records": schema_of(&gold)}).to_string(),
    };
    let t0 = Instant::now();
    let out = tokio::process::Command::new(&fab).args(["-s", &session, "--headless", "-v", "do", &body, "--url", &case.url]).output().await?;
    let secs = t0.elapsed().as_secs_f64();
    let _ = tokio::process::Command::new(&fab).args(["close", "-s", &session]).output().await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    std::fs::write(out_dir.join(format!("{}.jsonl", case.name)), stdout.as_bytes())?;
    std::fs::write(out_dir.join(format!("{}.log", case.name)), &out.stderr)?;
    let mut s = score(case, &gold, &records(&stdout));
    s.secs = (secs * 10.0).round() / 10.0;
    Ok(s)
}

pub fn score_line(s: &Score) -> String {
    format!(
        "{:18} {:5}  out {:3}/{:3}  P {:.3}  R {:.3}  F {:.3}  inv {}  {:5.1}s",
        s.case,
        if s.exact { "EXACT" } else { "wrong" },
        s.out,
        s.gold,
        s.precision,
        s.recall,
        s.fields,
        s.order_inversions,
        s.secs
    )
}

pub fn summary(scores: &[Score]) -> String {
    let n = scores.len().max(1) as f64;
    format!(
        "== {}/{} exact · records {}/{} · mean F {:.3} · total {:.0}s",
        scores.iter().filter(|s| s.exact).count(),
        scores.len(),
        scores.iter().map(|s| s.out.min(s.gold)).sum::<usize>(),
        scores.iter().map(|s| s.gold).sum::<usize>(),
        scores.iter().map(|s| s.fields).sum::<f64>() / n,
        scores.iter().map(|s| s.secs).sum::<f64>()
    )
}

/// Runs the named cases (all when empty) in `arm`, one at a time.
pub async fn run_scrape(names: &[String], arm: Arm, out_dir: &Path) -> Result<Vec<Score>> {
    std::fs::create_dir_all(out_dir)?;
    let all = cases()?;
    let chosen: Vec<&Case> = if names.is_empty() { all.iter().collect() } else { names.iter().map(|n| all.iter().find(|c| &c.name == n).with_context(|| format!("no case {n}"))).collect::<Result<_>>()? };
    let mut scores = vec![];
    for c in chosen {
        let s = run_case(c, arm, out_dir).await?;
        println!("{}", score_line(&s));
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(out_dir.join("scores.jsonl"))?;
        std::io::Write::write_all(&mut f, format!("{}\n", serde_json::to_string(&s)?).as_bytes())?;
        scores.push(s);
    }
    println!("{}", summary(&scores));
    println!("results: {}", out_dir.display());
    Ok(scores)
}

// ---------------------------------------------------------------- compile pilot

const DETAIL: &[&str] = &["jobs_details", "jobs_oslo", "shop_details", "forum_comments", "directory_all", "spa_details"];

/// Structural checks of a compiled program for a case.
fn check(case: &Case, prog: &str) -> Map<String, Value> {
    let low = prog.to_lowercase();
    let mut r = Map::new();
    r.insert("parse".into(), json!(fab_core::script::parse(prog).is_ok()));
    r.insert("paginates".into(), json!(low.split(|c: char| !c.is_alphanumeric()).any(|w| w == "next")));
    r.insert("open".into(), json!(low.contains("open ") == DETAIL.contains(&case.name.as_str())));
    let limit = match case.name.as_str() {
        "jobs_first2" => Some("2"),
        "forum_list" | "forum_comments" => Some("3"),
        _ => None,
    };
    if let Some(n) = limit {
        let hit = ["<", ">", "<=", ">=", "=="].iter().any(|op| low.contains(&format!("{op} {n}")) || low.contains(&format!("{op}{n}")) || low.contains(&format!("{n} {op}")));
        r.insert("limit".into(), json!(hit));
    }
    let filter: Option<&[&str]> = match case.name.as_str() {
        "jobs_oslo" => Some(&["oslo"]),
        "shop_details" => Some(&["sponsor"]),
        "shop_sale" => Some(&["sponsor", "sale", "old"]),
        "invoices_overdue" => Some(&["overdue"]),
        _ => None,
    };
    if let Some(words) = filter {
        let hit = low.lines().any(|l| l.trim_start().starts_with("if ") && words.iter().any(|w| l.contains(w)));
        r.insert("filter".into(), json!(hit));
    }
    if case.name == "directory_all" {
        let (i, j) = (low.find("open "), low.rfind("items \""));
        r.insert("nested".into(), json!(matches!((i, j), (Some(i), Some(j)) if j > i)));
    }
    r.insert("no_do".into(), json!(!prog.lines().any(|l| l.trim_start().starts_with("do \""))));
    r.insert("emits".into(), json!(low.contains("emit")));
    r
}

/// Each model compiles every case `n` times; programs are checked
/// structurally. Returns one row per compile.
pub async fn pilot(n: usize, models: &[String], out: &Path) -> Result<Vec<Value>> {
    let fab = fab_exe();
    let cases = cases()?;
    let mut jobs = vec![];
    for m in models {
        for c in &cases {
            for k in 0..n {
                let (fab, m, c) = (fab.clone(), m.clone(), c.clone());
                jobs.push(async move {
                    let t0 = Instant::now();
                    let url = format!("http://127.0.0.1:8080/{}", c.url.split_once(':').map(|x| x.1).unwrap_or(&c.url));
                    let o = tokio::process::Command::new(&fab).env("FAB_COMPILE_MODEL", &m).args(["do", &c.prompt, "--print", "--url", &url]).output().await;
                    let stdout = o.as_ref().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
                    let prog = end(&stdout).and_then(|e| e.value.as_str().map(str::to_string)).unwrap_or_default();
                    let checks = if prog.trim().is_empty() { json!({"parse": false}) } else { Value::Object(check(&c, &prog)) };
                    let ok = checks.as_object().is_some_and(|m| m.values().all(|v| v == true));
                    json!({"model": m, "case": c.name, "k": k, "secs": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0, "ok": ok, "checks": checks, "prog": prog})
                });
            }
        }
    }
    let rows: Vec<Value> = futures_bounded(jobs, 12).await;
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(out)?;
    for r in &rows {
        std::io::Write::write_all(&mut f, format!("{r}\n").as_bytes())?;
    }
    for m in models {
        let rs: Vec<&Value> = rows.iter().filter(|r| r["model"] == *m).collect();
        let mut fails: std::collections::BTreeMap<String, usize> = Default::default();
        for r in &rs {
            for (k, v) in r["checks"].as_object().into_iter().flatten() {
                if v != true {
                    *fails.entry(k.clone()).or_default() += 1;
                }
            }
        }
        let mut secs: Vec<f64> = rs.iter().filter_map(|r| r["secs"].as_f64()).collect();
        secs.sort_by(f64::total_cmp);
        let p50 = secs.get(secs.len() / 2).copied().unwrap_or(0.0);
        println!("{m:34} ok {:3}/{}  p50 {p50:4.1}s  fails {fails:?}", rs.iter().filter(|r| r["ok"] == true).count(), rs.len());
    }
    Ok(rows)
}

/// Runs futures with at most `limit` in flight, keeping their order.
async fn futures_bounded<F: std::future::Future<Output = Value>>(jobs: Vec<F>, limit: usize) -> Vec<Value> {
    use futures_util::StreamExt;
    futures_util::stream::iter(jobs).buffered(limit).collect().await
}

// ---------------------------------------------------------------- pool stress

pub struct Stress {
    pub n: usize,
    pub browser: Option<String>,
    pub max: usize,
    pub idle: u64,
    pub session: String,
    pub rounds: usize,
}

/// N clients drive one session at once: each types its own username into a
/// fixture login form, then submits it with a second step that has no URL.
/// The second step must continue on the page its client's first left, and
/// the submitted address must carry that client's username. The same work
/// then runs one client at a time; finally the pool must scale back to one
/// page. Returns the report; fails if any client or the scale-down failed.
pub async fn pool_stress(a: &Stress) -> Result<Value> {
    let fab = fab_exe();
    let mut start = vec!["--headless".to_string()];
    if let Some(b) = &a.browser {
        start.extend(["--browser".into(), b.clone()]);
    }
    let run = |args: Vec<String>, client: Option<String>| {
        let (fab, start, session) = (fab.clone(), start.clone(), a.session.clone());
        let (max, idle) = (a.max, a.idle);
        async move {
            let mut cmd = tokio::process::Command::new(&fab);
            cmd.env("FAB_MODEL", "none").env("FAB_POOL_MAX", max.to_string()).env("FAB_POOL_IDLE", idle.to_string()).env("FAB_IDLE", "120");
            if let Some(c) = client {
                cmd.env("FAB_CLIENT", c);
            }
            let t0 = Instant::now();
            let out = cmd.arg("-s").arg(&session).args(&start).arg("-v").args(&args).output().await;
            let secs = t0.elapsed().as_secs_f64();
            let (stdout, stderr) = out.map(|o| (String::from_utf8_lossy(&o.stdout).to_string(), String::from_utf8_lossy(&o.stderr).to_string())).unwrap_or_default();
            (end(&stdout), stderr, secs)
        }
    };
    let client = |i: usize, tag: String| {
        let run = &run;
        async move {
            let (c, u) = (format!("c{i}-{tag}"), format!("user-{i}-{tag}"));
            let (e1, log1, t1) = run(vec!["do".into(), "--url".into(), format!("fixture:login.html?w={i}"), format!("type \"{u}\" into the username field")], Some(c.clone())).await;
            let ok1 = e1.as_ref().is_some_and(|e| e.ok && e.url.as_deref().is_some_and(|url| url.contains(&format!("login.html?w={i}")))) && log1.contains(&format!("typed \"{u}\""));
            let (e2, _, t2) = run(vec!["do".into(), "click the \"Sign in\" button".into()], Some(c)).await;
            let want = format!("welcome.html?user={u}");
            let ok2 = e2.as_ref().is_some_and(|e| e.ok && e.url.as_deref().is_some_and(|url| url.contains(&want) && url[url.find(&want).unwrap() + want.len()..].chars().next().is_none_or(|c| c == '&')));
            json!({"i": i, "ok": ok1 && ok2, "t": [(t1 * 100.0).round() / 100.0, (t2 * 100.0).round() / 100.0]})
        }
    };
    let info = || {
        let (fab, session) = (fab.clone(), a.session.clone());
        async move {
            let out = tokio::process::Command::new(&fab).arg("sessions").output().await.ok()?;
            String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| serde_json::from_str::<Event>(l).ok()).find_map(|e| match e {
                Event::Item { data } if data["name"] == session.as_str() => Some(data["pool"].clone()),
                _ => None,
            })
        }
    };
    let median = |res: &[Value], k: usize| {
        let mut v: Vec<f64> = res.iter().filter_map(|x| x["t"][k].as_f64()).collect();
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let _ = tokio::process::Command::new(&fab).args(["close", "-s", &a.session]).output().await;
    let (e, _, t) = run(vec!["do".into(), "--url".into(), "fixture:login.html".into(), "type \"warmup\" into the username field".into()], None).await;
    println!("browser up in {t:.1}s ({})", if e.is_some_and(|e| e.ok) { "ok" } else { "failed" });
    let mut report = json!({"browser": a.browser.clone().unwrap_or_else(|| "default".into()), "n": a.n, "max": a.max, "parallel_s": []});
    let mut bad = 0;
    for r in 0..a.rounds {
        let t0 = Instant::now();
        let res: Vec<Value> = futures_util::future::join_all((0..a.n).map(|i| client(i, format!("p{r}")))).await;
        let par = t0.elapsed().as_secs_f64();
        let peak = info().await.unwrap_or(Value::Null);
        bad += res.iter().filter(|x| x["ok"] != true).count();
        println!("parallel  round {r}: {}/{} ok in {par:.1}s · step median [{}, {}] · pool {peak}", res.iter().filter(|x| x["ok"] == true).count(), a.n, median(&res, 0), median(&res, 1));
        report["parallel_s"].as_array_mut().unwrap().push(json!((par * 10.0).round() / 10.0));
        report["peak"] = peak["peak"].clone();
    }
    let t0 = Instant::now();
    let mut res = vec![];
    for i in 0..a.n {
        res.push(client(i, "s".into()).await);
    }
    let seq = t0.elapsed().as_secs_f64();
    bad += res.iter().filter(|x| x["ok"] != true).count();
    println!("sequential: {}/{} ok in {seq:.1}s · step median [{}, {}]", res.iter().filter(|x| x["ok"] == true).count(), a.n, median(&res, 0), median(&res, 1));
    report["sequential_s"] = json!((seq * 10.0).round() / 10.0);
    // Idle pages close after FAB_POOL_IDLE (checked every idle/2 s).
    let t0 = Instant::now();
    let mut pages = Value::Null;
    while t0.elapsed().as_secs() < a.idle * 4 + 10 {
        pages = info().await.map(|p| p["pages"].clone()).unwrap_or(Value::Null);
        if pages == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    report["scaled_down_to"] = pages.clone();
    report["scale_down_s"] = json!((t0.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    let _ = tokio::process::Command::new(&fab).args(["close", "-s", &a.session]).output().await;
    report["failed"] = json!(bad);
    let best = report["parallel_s"].as_array().unwrap().iter().filter_map(Value::as_f64).fold(f64::MAX, f64::min);
    report["speedup"] = json!((seq / best * 100.0).round() / 100.0);
    println!("{report}");
    if bad > 0 || pages != 1 {
        bail!("{bad} client(s) failed; pool scaled down to {pages} page(s)");
    }
    Ok(report)
}

// ---------------------------------------------------------------- Hacker News check

/// A start tag's name and attributes, from a tiny HTML scanner (enough for
/// HN's regular markup; not a general parser).
#[derive(Debug)]
enum Tok<'a> {
    Open(&'a str, Vec<(&'a str, String)>),
    Close(&'a str),
    Text(&'a str),
}

fn tokens(html: &str) -> Vec<Tok<'_>> {
    let mut out = vec![];
    let mut rest = html;
    while !rest.is_empty() {
        match rest.find('<') {
            Some(0) => {
                let Some(end) = rest.find('>') else { break };
                let tag = &rest[1..end];
                rest = &rest[end + 1..];
                if let Some(name) = tag.strip_prefix('/') {
                    out.push(Tok::Close(name.trim()));
                } else if !tag.starts_with('!') {
                    let name_end = tag.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(tag.len());
                    out.push(Tok::Open(&tag[..name_end], attrs(&tag[name_end..])));
                }
            }
            Some(i) => {
                out.push(Tok::Text(&rest[..i]));
                rest = &rest[i..];
            }
            None => {
                out.push(Tok::Text(rest));
                break;
            }
        }
    }
    out
}

fn attrs(s: &str) -> Vec<(&str, String)> {
    let mut out = vec![];
    let mut rest = s.trim_start();
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq].trim().trim_start_matches('/');
        let after = rest[eq + 1..].trim_start();
        let (val, next) = match after.chars().next() {
            Some(q @ ('"' | '\'')) => match after[1..].find(q) {
                Some(e) => (&after[1..=e], &after[e + 2..]),
                None => (&after[1..], ""),
            },
            _ => {
                let e = after.find(char::is_whitespace).unwrap_or(after.len());
                (&after[..e], &after[e..])
            }
        };
        out.push((name.rsplit(char::is_whitespace).next().unwrap_or(name), unescape(val)));
        rest = next.trim_start();
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e < 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")).and_then(|h| u32::from_str_radix(h, 16).ok()).or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok())).and_then(char::from_u32),
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[derive(Debug, Default, Clone)]
struct Comment {
    indent: u32,
    author: String,
    body: String,
}

/// An item page's comments in page order (author, indent, body text).
fn comments(html: &str) -> Vec<Comment> {
    let mut out: Vec<Comment> = vec![];
    let (mut in_user, mut in_text, mut depth) = (false, false, 0);
    for t in tokens(html) {
        match t {
            Tok::Open(name, a) => {
                let get = |k: &str| a.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
                if name == "td" && get("class") == Some("ind") {
                    out.push(Comment { indent: get("indent").and_then(|i| i.parse().ok()).unwrap_or(0), ..Default::default() });
                }
                let cur = out.last_mut();
                if name == "a" && get("class") == Some("hnuser") && cur.as_ref().is_some_and(|c| c.author.is_empty()) {
                    in_user = true;
                }
                if name == "div" && get("class").is_some_and(|c| c.starts_with("commtext")) && !out.is_empty() {
                    in_text = true;
                    depth = 0;
                } else if name == "div" && in_text {
                    depth += 1;
                }
                if in_text && name == "p" {
                    if let Some(c) = out.last_mut() {
                        c.body.push('\n');
                    }
                }
            }
            Tok::Close(name) => {
                if name == "a" {
                    in_user = false;
                }
                if name == "div" && in_text {
                    if depth == 0 {
                        in_text = false;
                    } else {
                        depth -= 1;
                    }
                }
            }
            Tok::Text(d) => {
                if let Some(c) = out.last_mut() {
                    let d = unescape(d);
                    if in_user {
                        c.author.push_str(&d);
                    }
                    if in_text {
                        c.body.push_str(&d);
                    }
                }
            }
        }
    }
    out
}

fn norm(t: &str) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Checks `fab`'s "top 3 replies of every front-page story" records against
/// Hacker News itself: each story's first three top-level comments, in order.
/// A record that is a real top-level comment of the story at another rank
/// counts as drift (the ranking moved since), not an error.
pub async fn verify_hn(jsonl: &Path) -> Result<Value> {
    let http = reqwest::Client::builder().user_agent("Mozilla/5.0").timeout(std::time::Duration::from_secs(30)).build()?;
    let get = |u: String| {
        let http = http.clone();
        async move { anyhow::Ok(http.get(u).send().await?.text().await?) }
    };
    let front = get("https://news.ycombinator.com/".into()).await?;
    let marker = "<tr class=\"athing submission\" id=\"";
    let ids: Vec<String> = front.match_indices(marker).filter_map(|(i, _)| front[i + marker.len()..].split('"').next().map(str::to_string)).collect();
    let (mut want, mut story) = (vec![], vec![]);
    let mut tops: std::collections::HashMap<String, std::collections::HashSet<(String, String)>> = Default::default();
    for id in &ids {
        let all: Vec<Comment> = comments(&get(format!("https://news.ycombinator.com/item?id={id}")).await?).into_iter().filter(|c| c.indent == 0).collect();
        tops.insert(id.clone(), all.iter().map(|c| (c.author.clone(), norm(&c.body))).collect());
        for c in all.iter().take(3) {
            want.push(c.clone());
            story.push(id.clone());
        }
    }
    let text = std::fs::read_to_string(jsonl)?;
    let got = records(&text);
    let (mut ok, mut drift, mut bad) = (0, 0, vec![]);
    for (k, w) in want.iter().enumerate() {
        let g = got.get(k);
        let (ga, gb) = (g.and_then(|g| g.get("author")).and_then(Value::as_str).unwrap_or_default().to_string(), norm(g.and_then(|g| g.get("body")).and_then(Value::as_str).unwrap_or_default()));
        if ga == w.author && gb == norm(&w.body) {
            ok += 1;
        } else if tops[&story[k]].contains(&(ga.clone(), gb.clone())) {
            drift += 1;
        } else {
            bad.push(json!({"k": k, "want": w.author, "got": ga, "want_body": norm(&w.body).chars().take(80).collect::<String>(), "got_body": gb.chars().take(80).collect::<String>()}));
        }
    }
    let keys: std::collections::BTreeSet<&String> = got.iter().flat_map(|g| g.keys()).collect();
    let report = json!({"stories": ids.len(), "expected": want.len(), "got": got.len(), "keys": keys, "exact": ok, "drift": drift, "wrong": bad.len(), "mismatches": bad.iter().take(8).collect::<Vec<_>>()});
    println!("stories {} · expected {} replies · got {} · keys {:?}", ids.len(), want.len(), got.len(), keys);
    println!("exact in order: {ok}/{} · exact comment, rank moved since: {drift} · wrong: {}", want.len(), bad.len());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g_format_matches_python() {
        for (v, s) in [(1299.5, "1299.5"), (1299.0, "1299"), (0.1, "0.1"), (1234567.0, "1.23457e+06"), (0.00001234, "1.234e-05"), (4.0, "4"), (99.99, "99.99"), (123456.0, "123456")] {
            assert_eq!(fmt_g(v), s, "{v}");
        }
    }

    #[test]
    fn scoring_matches_by_key_and_normalized_fields() {
        let case = Case { name: "t".into(), url: String::new(), prompt: String::new(), key: "title".into() };
        let g = |v: Value| v.as_object().unwrap().clone();
        let gold = vec![g(json!({"title": "A", "price": "1,299"})), g(json!({"title": "B", "price": null}))];
        let outs = vec![g(json!({"Title": "b", "item_price": "n/a"})), g(json!({"title": " A ", "price": "1,299"}))];
        let s = score(&case, &gold, &outs);
        // Keys match case-insensitively; field values don't ("b" ≠ "B").
        assert_eq!((s.out, s.precision, s.recall, s.fields, s.order_inversions, s.exact), (2, 1.0, 1.0, 0.75, 1, false));
        let s = score(&case, &gold, &[outs[1].clone(), g(json!({"title": "B"})), g(json!({"title": "C"}))]);
        assert_eq!((s.precision, s.recall, s.exact), (0.6667, 1.0, false));
    }

    #[test]
    fn hn_comments_are_read_in_page_order() {
        let html = r#"<table><tr class="athing comtr"><td class="ind" indent="0"></td><td><a class="hnuser" href="u">ada</a>
            <div class="commtext c00">First &amp; <i>best</i><p>second para</div></td></tr>
            <tr><td class="ind" indent="1"></td><td><a class="hnuser">bob</a><div class="commtext c5a">reply <div>nested</div> end</div></td></tr></table>"#;
        let c = comments(html);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].indent, c[0].author.as_str(), norm(&c[0].body).as_str()), (0, "ada", "First & best second para"));
        assert_eq!((c[1].indent, c[1].author.as_str(), norm(&c[1].body).as_str()), (1, "bob", "reply nested end"));
    }

    #[test]
    fn schema_follows_the_gold() {
        let g = |v: Value| v.as_object().unwrap().clone();
        let s = schema_of(&[g(json!({"title": "A", "votes": 3, "note": null})), g(json!({"title": "B", "votes": 5, "note": "x"}))]);
        assert_eq!(s, json!({"type": "object", "properties": {"title": {"type": "string"}, "votes": {"type": "number"}, "note": {"type": ["string", "null"]}}, "required": ["title", "votes"]}));
        assert!(crate::request::RecordSchema::parse(&s).is_ok());
    }
}
