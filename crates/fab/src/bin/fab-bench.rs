//! fab-bench: the benchmark and development harness (not part of fab's agent
//! surface). Suites, live races, the decision dataset, and fixtures.

use anyhow::Result;
use clap::{Parser, Subcommand};
use fab_cli::{bench, decisions, evals, race, server};
use fab_core::{Knobs, Session};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "fab-bench", version, about = "fab's benchmark and development harness", arg_required_else_help = true)]
struct Cli {
    /// Show decisions and progress as they happen (on stderr).
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Run browsers without a window.
    #[arg(long, global = true, env = "FAB_HEADLESS")]
    headless: bool,
    /// Knob overrides, e.g. --set backend=camofox --set exec=trusted.
    #[arg(long = "set", global = true)]
    set: Vec<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a scenario suite.
    Bench {
        #[arg(long, default_value = "bench/scenarios.toml")]
        suite: PathBuf,
        /// Only these scenario names (comma separated).
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// experiment (LLM + fab) | goal (LLM + fab `do`) | agent (fab runs the task; LLM as coprocessor) | vm (engine only, no LLM) | control (LLM + chrome-devtools-mcp) | scripted (fixed instructions, no LLM)
        #[arg(long, default_value = "experiment")]
        mode: String,
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        #[arg(long)]
        label: Option<String>,
        /// Planner models for agent modes (comma separated).
        #[arg(long, value_delimiter = ',', default_value = "google/gemini-3.1-flash-lite")]
        planner_model: Vec<String>,
        /// Goal wordings per scenario: 1 = canonical goal, N = canonical + N-1 paraphrases.
        #[arg(long, default_value_t = 1)]
        paraphrases: usize,
        /// Only check expected/forbidden records (don't fail on unlisted mutations).
        #[arg(long)]
        lenient: bool,
        /// Scripted mode: append decision cases from passing runs to this JSONL file.
        #[arg(long)]
        record: Option<PathBuf>,
        #[arg(long, default_value = "bench/results")]
        out: PathBuf,
        /// Run this many scenarios at once (separate browsers). 1 for timing.
        #[arg(long, default_value_t = 1)]
        jobs: usize,
    },
    /// Watch the same goals raced live, side by side in visible browsers:
    /// the planner LLM driving chrome-devtools-mcp (left) vs the same LLM with fab (right).
    Race {
        /// Suite file(s), comma separated.
        #[arg(long, value_delimiter = ',', default_value = "bench/hard.toml")]
        suite: Vec<PathBuf>,
        /// Only these scenario names (comma separated).
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        #[arg(long, default_value = "inception/mercury-2.5")]
        planner_model: String,
        /// Right-hand arm: goal (fab `do`) | agent (agent mode) | vm (engine only) | experiment (fab act/run).
        #[arg(long, default_value = "goal")]
        right: String,
        /// Goal wording: 0 = canonical, N = the Nth paraphrase.
        #[arg(long, default_value_t = 0)]
        para: usize,
        /// Pause between scenarios, ms.
        #[arg(long, default_value_t = 2500)]
        pause: u64,
        /// Once both browsers show the start page, wait this long (ms) before starting both clocks.
        #[arg(long, default_value_t = 2000)]
        lead: u64,
        /// Dashboard port (0 = any free port).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Keep the windows open this many seconds after the last scenario.
        #[arg(long, default_value_t = 120)]
        hold: u64,
        #[arg(long)]
        label: Option<String>,
    },
    /// The decision dataset, evaluated offline (no browser).
    Decisions {
        #[command(subcommand)]
        cmd: decisions::Cmd,
    },
    /// Compare two result files.
    Compare { a: PathBuf, b: PathBuf },
    /// Arm-vs-arm summary over paired result files: pass rate, McNemar exact p, wall ratio, LLM turns and cost per task.
    Summary {
        /// Result files to compare, by label (bench/results/<label>.json).
        labels: Vec<String>,
        #[arg(long, default_value = "bench/results")]
        results: PathBuf,
    },
    /// Site-shape transfer: learn each app's shape from its first task (fixture order in the suite), then run its later tasks with and without the learned shapes, paired.
    Warm {
        #[arg(long, default_value = "bench/heldout2.toml")]
        suite: PathBuf,
        /// Labels are <tag>-learn, <tag>-use, <tag>-cold.
        #[arg(long, default_value = "warm")]
        tag: String,
        #[arg(long, value_delimiter = ',', default_value = "inception/mercury-2.5,stepfun/step-3.7-flash")]
        planner_model: Vec<String>,
        #[arg(long, default_value_t = 2)]
        paraphrases: usize,
        #[arg(long, default_value_t = 4)]
        jobs: usize,
        #[arg(long, default_value = "bench/results")]
        out: PathBuf,
    },
    /// Pilot report: pass rate, wall time, turns and cost per model and arm, over the pilot-* result files.
    PilotReport {
        #[arg(long, default_value = "bench/results")]
        results: PathBuf,
    },
    /// Re-apply the current suite checks to a stored result file (in place).
    Rescore {
        file: PathBuf,
        #[arg(long)]
        suite: PathBuf,
        #[arg(long)]
        lenient: bool,
    },
    /// Print the table for a result file.
    Show { file: PathBuf },
    /// Serve the benchmark fixtures for manual poking.
    Serve {
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
    /// The scraping eval (bench/scrape): run cases through the `fab` CLI and score them against gold.
    Scrape {
        #[command(subcommand)]
        cmd: ScrapeCmd,
    },
    /// Page-pool stress: many clients drive one session at once, then the same work sequentially.
    Pool {
        #[arg(long, default_value_t = 12)]
        n: usize,
        #[arg(long)]
        browser: Option<String>,
        #[arg(long, default_value_t = 6)]
        max: usize,
        #[arg(long, default_value_t = 4)]
        idle: u64,
        #[arg(long, default_value = "pooltest")]
        session: String,
        #[arg(long, default_value_t = 1)]
        rounds: usize,
    },
    /// A fresh browser runs instructions against a URL (`fixture:<file>` for a fixture), then closes.
    Oneshot {
        #[arg(long)]
        url: String,
        #[arg(long)]
        trace: bool,
        instructions: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ScrapeCmd {
    /// Generate the fixture sites (fixtures/scrape, ignored by git) and their gold records (bench/scrape/gold). Seeded: always the same bytes.
    Gen {
        #[arg(long)]
        fixtures: Option<PathBuf>,
        #[arg(long)]
        gold: Option<PathBuf>,
    },
    /// Run cases (all when none are named) and score each against its gold.
    Run {
        cases: Vec<String>,
        /// Ask in words, or as a structured request whose records schema comes from the gold.
        #[arg(long, value_enum, default_value = "words")]
        arm: evals::Arm,
        /// Run everything this many times.
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Score a saved `fab` output stream against a case's gold.
    Score { case: String, stream: PathBuf },
    /// Each model compiles every case N times; programs are checked structurally.
    Pilot {
        n: usize,
        models: Vec<String>,
    },
    /// Check "top 3 replies of every front-page story" records against Hacker News itself (network).
    VerifyHn { stream: PathBuf },
}

#[tokio::main]
async fn main() -> ExitCode {
    fab_cli::load_env();
    tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into())).with_writer(std::io::stderr).init();
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let mut k = Knobs::default();
    k.apply(&cli.set)?;
    let set = cli.set.clone();
    let explicit = |knob: &str| set.iter().any(|s| s.starts_with(&format!("{knob}=")));
    match cli.cmd {
        Cmd::Bench { suite, only, tags, mode, repeat, label, planner_model, paraphrases, lenient, record, out, jobs } => {
            let mode = if mode == "planner" { "experiment".to_string() } else { mode };
            // fab arms that run goals use the decision VM unless told otherwise.
            if matches!(mode.as_str(), "goal" | "agent" | "vm" | "script" | "do") && !explicit("engine") {
                k.set("engine", "dvm")?;
            }
            bench_defaults(&explicit, &mut k)?;
            let label = label.unwrap_or_else(|| format!("{}-{}", mode, k.backend));
            let opts = bench::BenchOpts {
                suite,
                only,
                tags,
                mode,
                repeat,
                label,
                planner_models: planner_model,
                paraphrases,
                strict: !lenient,
                record,
                out_dir: out,
                verbose: cli.verbose,
                jobs,
            };
            bench::bench(k, opts).await.map(|_| ())
        }
        Cmd::Race { suite, only, planner_model, right, para, pause, lead, port, hold, label } => {
            // The Jev arm runs the decision VM unless told otherwise.
            if !explicit("engine") {
                k.set("engine", "dvm")?;
            }
            bench_defaults(&explicit, &mut k)?;
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let opts = race::RaceOpts {
                suites: suite,
                only,
                model: planner_model,
                right,
                para,
                pause_ms: pause,
                lead_ms: lead,
                headless: cli.headless,
                port,
                hold_s: hold,
                label: label.unwrap_or_else(|| format!("race-{secs}")),
            };
            race::race(k, opts).await
        }
        Cmd::Decisions { cmd } => decisions::run(k, cmd).await,
        Cmd::Compare { a, b } => bench::compare(&a, &b),
        Cmd::Summary { labels, results } => {
            if labels.is_empty() {
                anyhow::bail!("give at least one label, e.g. `fab-bench summary g2-control g2-goal`");
            }
            bench::summary(&results, &labels)
        }
        Cmd::Warm { suite, tag, planner_model, paraphrases, jobs, out } => {
            bench::warm(bench::WarmOpts { suite, tag, planner_models: planner_model, paraphrases, jobs, out_dir: out, sets: set }).await
        }
        Cmd::PilotReport { results } => bench::pilot_report(&results),
        Cmd::Rescore { file, suite, lenient } => bench::rescore(&file, &suite, !lenient),
        Cmd::Show { file } => {
            let r: bench::Report = serde_json::from_str(&std::fs::read_to_string(file)?)?;
            bench::print_table(&r);
            Ok(())
        }
        Cmd::Serve { port } => {
            let s = server::start(port).await?;
            eprintln!("serving {} at http://{}", server::fixtures_dir().display(), s.addr);
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Cmd::Oneshot { url, trace, instructions } => oneshot(k, url, trace, instructions).await,
        Cmd::Scrape { cmd } => scrape(cmd).await,
        Cmd::Pool { n, browser, max, idle, session, rounds } => evals::pool_stress(&evals::Stress { n, browser, max, idle, session, rounds }).await.map(|_| ()),
    }
}

async fn scrape(cmd: ScrapeCmd) -> Result<()> {
    match cmd {
        ScrapeCmd::Gen { fixtures, gold } => {
            let fixtures = fixtures.unwrap_or_else(|| evals::root().join("fixtures/scrape"));
            let gold = gold.unwrap_or_else(|| evals::root().join("bench/scrape/gold"));
            for (name, n) in fab_cli::scrape_gen::generate(&fixtures, &gold)? {
                println!("{name:28} {n:4} records");
            }
            Ok(())
        }
        ScrapeCmd::Run { cases, arm, repeat, out } => {
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let tag = if arm == evals::Arm::Schema { "schema" } else { "words" };
            let base = out.unwrap_or_else(|| evals::root().join(format!("bench/results/scrape/{secs}-{tag}")));
            for r in 0..repeat {
                let dir = if repeat == 1 { base.clone() } else { base.join(format!("run-{r}")) };
                evals::run_scrape(&cases, arm, &dir).await?;
            }
            Ok(())
        }
        ScrapeCmd::Score { case, stream } => {
            let c = evals::cases()?.into_iter().find(|c| c.name == case).ok_or_else(|| anyhow::anyhow!("no case {case}"))?;
            let s = evals::score(&c, &evals::gold(&case)?, &evals::records(&std::fs::read_to_string(stream)?));
            println!("{}", serde_json::to_string(&s)?);
            Ok(())
        }
        ScrapeCmd::Pilot { n, models } => evals::pilot(n, &models, &evals::root().join("bench/results/scrape/pilot.jsonl")).await.map(|_| ()),
        ScrapeCmd::VerifyHn { stream } => evals::verify_hn(&stream).await.map(|_| ()),
    }
}

/// Benchmarks measure cold, headless runs with the settings they were tuned
/// with, unless a knob is set explicitly.
fn bench_defaults(explicit: &dyn Fn(&str) -> bool, k: &mut Knobs) -> Result<()> {
    if !explicit("shape") {
        k.set("shape", "off")?;
    }
    if !explicit("headful") && !explicit("headless") {
        k.set("headful", "false")?;
    }
    // Every recorded result, and the chrome-devtools-mcp control, ran on Chrome.
    let chrome = fab_core::backend::discover::spec("chrome").and_then(fab_core::backend::discover::locate);
    if !explicit("browser") && chrome.is_some() {
        k.set("browser", "chrome")?;
    }
    Ok(())
}

async fn oneshot(k: Knobs, url: String, trace: bool, instructions: Vec<String>) -> Result<()> {
    let srv = server::start(0).await?;
    let url = match url.strip_prefix("fixture:") {
        Some(f) => srv.url(f),
        None => url,
    };
    let t = Instant::now();
    let mut sess = Session::new(k).await?;
    sess.keep_trace = trace;
    eprintln!("session ready in {} ms", t.elapsed().as_millis());
    let t = Instant::now();
    let page = sess.goto(&url).await?;
    eprintln!("goto {} ms\n{}", t.elapsed().as_millis(), page);
    for ins in &instructions {
        if let Some(q) = ins.strip_prefix("observe:") {
            println!("{}", sess.observe(q.trim()).await?);
        } else if let Some(rest) = ins.strip_prefix("collect:") {
            // collect:<op>|<what>|<where>|<of>|<by>
            let f: Vec<&str> = rest.split('|').collect();
            let g = |i: usize| f.get(i).copied().filter(|x| !x.is_empty());
            let v = sess.collect(g(1).unwrap_or("rows"), g(2).unwrap_or(""), true, g(0).unwrap_or("list"), g(3), g(4)).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else if ins == "snap:" {
            println!("{}", serde_json::to_string_pretty(sess.snapshot().await?)?);
        } else if let Some(q) = ins.strip_prefix("read:") {
            println!("{}", sess.read(q.trim(), 8000).await?);
        } else if let Some(q) = ins.strip_prefix('?') {
            let r = sess.extract(q.trim()).await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        } else {
            let r = sess.act(ins).await;
            if trace {
                for t in &sess.trace {
                    eprintln!("{}", serde_json::to_string_pretty(t)?);
                }
            }
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
    }
    let recs = srv.records.take();
    if !recs.is_empty() {
        eprintln!("records: {}", serde_json::to_string(&recs)?);
    }
    eprintln!("{}", sess.page_summary().await?);
    sess.close().await;
    Ok(())
}
