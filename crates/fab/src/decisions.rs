//! `fab-bench decisions`: the labelled decision dataset and offline evaluation of the
//! decision engine against live Jev (no browser).

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use fab_core::Knobs;
use fab_core::dataset::{self, DecisionCase, Gold};
use fab_core::decide;
use fab_core::jev::Jev;

#[derive(Subcommand)]
pub enum Cmd {
    /// Replay every case through the decision engine and score it.
    Eval {
        /// JSONL files of cases.
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[arg(long)]
        limit: Option<usize>,
        /// Requests in flight.
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Write a JSON report here.
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, short)]
        verbose: bool,
    },
    /// Summarize a dataset.
    Stats {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Add cases whose instruction is an LLM paraphrase (same observation and
    /// gold). Paraphrases that drop or alter a quoted literal are discarded.
    Paraphrase {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[arg(long, default_value_t = 4)]
        n: usize,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "openai/gpt-6-luna")]
        model: String,
    },
}

/// Appends cases from one passing run, assigning stable ids.
pub fn append(path: &Path, scenario: &str, cases: Vec<DecisionCase>) -> Result<()> {
    if cases.is_empty() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    for (i, mut c) in cases.into_iter().enumerate() {
        c.scenario = scenario.to_string();
        c.id = format!("{scenario}#{i}:{:08x}", fnv(&c.instr) ^ fnv(&c.history.join("|")));
        writeln!(f, "{}", serde_json::to_string(&c)?)?;
    }
    Ok(())
}

fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193))
}

pub fn load(files: &[PathBuf]) -> Result<Vec<DecisionCase>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for f in files {
        let text = std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?;
        for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
            let c: DecisionCase = serde_json::from_str(line).with_context(|| format!("{}:{}", f.display(), n + 1))?;
            // The same observation+instruction recorded twice counts once.
            if seen.insert(c.id.clone()) {
                out.push(c);
            }
        }
    }
    Ok(out)
}

pub async fn run(k: Knobs, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Stats { files } => {
            let cases = load(&files)?;
            let mut by: std::collections::BTreeMap<String, usize> = Default::default();
            for c in &cases {
                *by.entry(c.scenario.clone()).or_default() += 1;
            }
            let clicks = cases.iter().filter(|c| c.gold.click.is_some()).count();
            let writes = cases.iter().filter(|c| !c.gold.writes.is_empty()).count();
            let done = cases.iter().filter(|c| c.gold.done).count();
            println!("{} cases · {clicks} with a click · {writes} with writes · {done} done", cases.len());
            for (s, n) in by {
                println!("  {s:<28} {n}");
            }
            Ok(())
        }
        Cmd::Eval { files, limit, concurrency, out, verbose } => eval(k, files, limit, concurrency, out, verbose).await,
        Cmd::Paraphrase { files, n, out, model } => paraphrase(files, n, out, &model).await,
    }
}

struct Outcome {
    id: String,
    instr: String,
    score: dataset::Score,
    /// The engine handed back instead of committing.
    escalated: bool,
    rounds: u32,
    trace: serde_json::Value,
    gold: Gold,
    pred: Gold,
    ms: f64,
    tokens: u64,
    cost: f64,
    err: Option<String>,
}

/// One decision on a stored observation: (prediction, escalated, rounds, tokens, cost).
async fn decide_case(c: &DecisionCase, k: &Knobs, jev: &Jev) -> Result<(Gold, bool, u32, u64, f64, serde_json::Value)> {
    if k.engine == "dvm" {
        let d = fab_core::dvm::decide(&c.snap, &c.instr, None, &c.history, k, jev).await?;
        let tokens = d.answers.iter().map(|a| a.usage.input_tokens).sum();
        let cost = d.answers.iter().map(|a| a.usage.cost.unwrap_or(0.0)).sum();
        let (pred, esc) = match d.outcome {
            fab_core::dvm::Outcome::Done => (Gold::done(), false),
            fab_core::dvm::Outcome::Commit => (Gold::from_plan(&d.plan, &c.snap), false),
            fab_core::dvm::Outcome::Escalate(_) => (Gold::default(), true),
        };
        return Ok((pred, esc, d.rounds, tokens, cost, json!({"trace": d.trace, "outcome": d.outcome})));
    }
    let (state, q, map) = decide::build(&c.snap, &c.instr, &c.history, k);
    let ans = jev.ask(&state, &q).await?;
    let mut plan = decide::decode(&ans, &map, &c.snap);
    decide::dedupe_values(&mut plan, &c.instr);
    // Same stop rule as the legacy act loop.
    let step0 = c.history.is_empty();
    let done = plan.done >= k.done_threshold && (!step0 || plan.is_noop() || plan.done >= 0.9);
    let pred = if done { Gold::done() } else { Gold::from_plan(&plan, &c.snap) };
    let esc = !done && plan.is_noop();
    let t = json!({"click": ans.ranked("click").into_iter().take(3).collect::<Vec<_>>(), "done": plan.done});
    Ok((pred, esc, 1, ans.usage.input_tokens, ans.usage.cost.unwrap_or(0.0), t))
}

/// The gold of the next recorded step of the same run and instruction, so a
/// macro that correctly does two steps in one round isn't scored as a mistake.
fn next_gold(cases: &[DecisionCase]) -> std::collections::HashMap<String, Gold> {
    let parse = |id: &str| -> Option<(String, usize, String)> {
        let (head, suffix) = match id.find('~') {
            Some(p) => (&id[..p], id[p..].to_string()),
            None => (id, String::new()),
        };
        let (scen, rest) = head.split_once('#')?;
        let idx: usize = rest.split(':').next()?.parse().ok()?;
        Some((scen.to_string(), idx, suffix))
    };
    let mut by: std::collections::HashMap<(String, usize, String, String), &DecisionCase> = Default::default();
    for c in cases {
        if let Some((s, i, suf)) = parse(&c.id) {
            by.insert((s, i, suf, c.instr.clone()), c);
        }
    }
    let mut out = std::collections::HashMap::new();
    for c in cases {
        if let Some((s, i, suf)) = parse(&c.id) {
            if let Some(n) = by.get(&(s, i + 1, suf, c.instr.clone())) {
                out.insert(c.id.clone(), n.gold.clone());
            }
        }
    }
    out
}

/// Accept a prediction that does the gold step plus (part of) the next step.
fn ahead_ok(pred: &Gold, gold: &Gold, next: Option<&Gold>) -> bool {
    let Some(next) = next else { return false };
    let writes_ok = pred.writes.iter().all(|w| gold.writes.contains(w) || next.writes.contains(w))
        && gold.writes.iter().all(|w| pred.writes.contains(w));
    let click_ok = pred.click == gold.click || (pred.click.is_some() && pred.click == next.click);
    writes_ok && click_ok && !pred.done
}

async fn eval(k: Knobs, files: Vec<PathBuf>, limit: Option<usize>, conc: usize, out: Option<PathBuf>, verbose: bool) -> Result<()> {
    let mut cases = load(&files)?;
    if let Some(l) = limit {
        cases.truncate(l);
    }
    let nexts = std::sync::Arc::new(next_gold(&cases));
    let mut jev = Jev::from_env()?;
    jev.hedge = k.hedge;
    jev.hedge_q = k.hedge_q;
    jev.warm().await.ok();
    let jev = Arc::new(jev);
    let k = Arc::new(k);
    let sem = Arc::new(tokio::sync::Semaphore::new(conc.max(1)));
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for c in cases {
        let (jev, k, sem, nexts) = (jev.clone(), k.clone(), sem.clone(), nexts.clone());
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await;
            let t = Instant::now();
            let res = decide_case(&c, &k, &jev).await;
            let ms = t.elapsed().as_secs_f64() * 1e3;
            match res {
                Ok((pred, escalated, rounds, tokens, cost, trace)) => Outcome {
                    trace,
                    score: {
                        let mut sc = dataset::score(&pred, &c.gold);
                        if !sc.all() && ahead_ok(&pred, &c.gold, nexts.get(&c.id)) {
                            sc = dataset::Score { click_ok: true, writes_ok: true, done_ok: true, commission: false };
                        }
                        sc
                    },
                    escalated,
                    rounds,
                    id: c.id,
                    instr: c.instr,
                    gold: c.gold,
                    pred,
                    ms,
                    tokens,
                    cost,
                    err: None,
                },
                Err(e) => Outcome {
                    trace: serde_json::Value::Null,
                    id: c.id,
                    instr: c.instr,
                    escalated: false,
                    rounds: 0,
                    score: Default::default(),
                    gold: c.gold,
                    pred: Gold::default(),
                    ms,
                    tokens: 0,
                    cost: 0.0,
                    err: Some(format!("{e:#}")),
                },
            }
        }));
    }
    let mut outs = Vec::new();
    for t in tasks {
        outs.push(t.await?);
    }
    let n = outs.len().max(1) as f64;
    let pct = |f: &dyn Fn(&Outcome) -> bool| 100.0 * outs.iter().filter(|o| f(o)).count() as f64 / n;
    let mut lat: Vec<f64> = outs.iter().map(|o| o.ms).collect();
    lat.sort_by(f64::total_cmp);
    let q = |p: f64| lat.get(((lat.len().max(1) - 1) as f64 * p).round() as usize).copied().unwrap_or(0.0);
    let tokens: u64 = outs.iter().map(|o| o.tokens).sum();
    let cost: f64 = outs.iter().map(|o| o.cost).sum();
    let errors = outs.iter().filter(|o| o.err.is_some()).count();
    if verbose {
        for o in outs.iter().filter(|o| !o.score.all()) {
            println!("✗ {} · {}", o.id, fab_core::snapshot::truncate(&o.instr, 100));
            if let Some(e) = &o.err {
                println!("    error: {e}");
            } else {
                if o.escalated {
                    println!("    (escalated)");
                }
                println!("    gold: {}", serde_json::to_string(&o.gold)?);
                println!("    pred: {}", serde_json::to_string(&o.pred)?);
            }
        }
    }
    let requests = outs.len() as u64 * if k.hedge > 1 && k.hedge_q <= 0.0 { k.hedge as u64 } else { 1 }
        + jev.hedges_sent.load(std::sync::atomic::Ordering::Relaxed);
    let rounds: u32 = outs.iter().map(|o| o.rounds).sum();
    println!(
        "{} cases · exact {:.1}% · commission {:.1}% · escalated {:.1}% · click {:.1}% · writes {:.1}% · done {:.1}% · errors {errors} · {:.2} rounds/case · latency p50 {:.0} ms p95 {:.0} ms · {:.0} tok/case · {requests} requests · ${cost:.4} · {:.1}s",
        outs.len(),
        pct(&|o| o.score.all()),
        pct(&|o| o.score.commission),
        pct(&|o| o.escalated),
        pct(&|o| o.score.click_ok),
        pct(&|o| o.score.writes_ok),
        pct(&|o| o.score.done_ok),
        rounds as f64 / n,
        q(0.5),
        q(0.95),
        tokens as f64 / n,
        t0.elapsed().as_secs_f64()
    );
    if let Some(path) = out {
        let report = json!({
            "knobs": serde_json::to_value(&*k)?,
            "n": outs.len(),
            "exact": pct(&|o| o.score.all()),
            "click": pct(&|o| o.score.click_ok),
            "writes": pct(&|o| o.score.writes_ok),
            "done": pct(&|o| o.score.done_ok),
            "p50_ms": q(0.5), "p95_ms": q(0.95),
            "tokens_per_case": tokens as f64 / n,
            "cost": cost,
            "cases": outs.iter().map(|o| json!({"id": o.id, "ok": o.score.all(), "escalated": o.escalated, "rounds": o.rounds, "score": o.score, "trace": o.trace, "ms": o.ms, "tokens": o.tokens, "gold": o.gold, "pred": o.pred, "err": o.err})).collect::<Vec<_>>(),
        });
        std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
        eprintln!("wrote {}", path.display());
    }
    Ok(())
}

async fn paraphrase(files: Vec<PathBuf>, n: usize, out: PathBuf, model: &str) -> Result<()> {
    let cases = load(&files)?;
    let llm = fab_core::llm::Llm::from_env(model)?;
    let mut uniq: Vec<String> = cases.iter().map(|c| c.instr.clone()).collect();
    uniq.sort();
    uniq.dedup();
    let sys = "You rewrite instructions that a user gives to a browser assistant. Produce different wordings a real user might type: vary verbs, word order, formality and length. Keep the meaning and every requirement. Every text inside double quotes must appear unchanged, inside double quotes. Keep names, numbers, emails, prices and dates exactly. Don't add requirements or UI hints. Reply with JSON only: {\"paraphrases\": [\"...\"]}";
    let mut map: std::collections::HashMap<String, Vec<String>> = Default::default();
    let jobs = uniq.iter().map(|instr| {
        let llm = llm.clone();
        async move {
            let body = json!({
                "messages": [{"role": "system", "content": sys}, {"role": "user", "content": format!("Give {n} paraphrases of: {instr}")}],
                "response_format": {"type": "json_object"},
                "temperature": 0.7,
            });
            let res = llm.chat(body).await;
            (instr.clone(), res)
        }
    });
    let results = futures_join_all(jobs).await;
    let (mut kept, mut dropped) = (0, 0);
    for (instr, res) in results {
        let Ok((msg, _, _)) = res else { continue };
        let text = msg["content"].as_str().unwrap_or("{}");
        let text = text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```");
        let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
        let lits = fab_core::spans::quoted(&instr);
        let good: Vec<String> = v["paraphrases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().map(str::to_string))
            .filter(|p| {
                let ok = p != &instr && lits.iter().all(|l| p.contains(&format!("\"{l}\"")));
                if ok { kept += 1 } else { dropped += 1 }
                ok
            })
            .take(n)
            .collect();
        map.insert(instr, good);
    }
    let mut outc = Vec::new();
    for c in &cases {
        for (k, p) in map.get(&c.instr).into_iter().flatten().enumerate() {
            let mut x = c.clone();
            x.instr = p.clone();
            x.source = "paraphrase".into();
            x.id = format!("{}~p{k}", c.id);
            outc.push(x);
        }
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::File::create(&out)?;
    for c in &outc {
        writeln!(f, "{}", serde_json::to_string(c)?)?;
    }
    eprintln!("{} instructions → {kept} paraphrases kept, {dropped} dropped → {} cases in {}", uniq.len(), outc.len(), out.display());
    Ok(())
}

async fn futures_join_all<F: std::future::Future>(fs: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let handles: Vec<_> = fs.into_iter().collect();
    let mut out = Vec::with_capacity(handles.len());
    // Bounded concurrency without extra deps: run in chunks of 8.
    let mut it = handles.into_iter().peekable();
    while it.peek().is_some() {
        let chunk: Vec<F> = it.by_ref().take(8).collect();
        let mut futs: Vec<std::pin::Pin<Box<F>>> = chunk.into_iter().map(Box::pin).collect();
        for f in futs.iter_mut() {
            out.push(f.as_mut().await);
        }
    }
    out
}
