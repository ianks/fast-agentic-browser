use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fab_cli::{SKILL, api};
use fab_cli::events::{self, End, ErrorCode, Event, Failure, Writer};
#[cfg(unix)]
use fab_cli::session;
use fab_core::Knobs;
use fab_core::jev::Jev;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const AFTER_HELP: &str = "\
Say what you want done, one step at a time. fab does the clicking, typing,
reading and signing in, and answers:

  fab do \"open news.ycombinator.com and tell me the top story's points\"
  fab do \"log in\" --url github.com            # with your saved login
  fab do \"star the ianks/fast-agentic-browser repo\"
  fab close

When a job has logic you know in advance (repeat until, for each, if/else,
a value carried to a later step), write it as a small program and `run` it;
fab decides each step on the live page. `do --print` drafts one for you:

  fab do \"open the first story on news.ycombinator.com with comments and tell me the top commenter\" --print > hn.fab
  fab run hn.fab
  printf 'read \"the amount due\" -> due\\nif due > 200\\n  return due\\nend\\ndo \"pay $due from checking\"\\n' | fab run

Program grammar:

  steps  do \"<sub-goal>\"             fab works out the pages and clicks; yields what it reports
         read \"<question>\" -> x       a value from the site
         test \"<yes/no question>\"     a check of the page (-> x, or inline: if test \"…\")
  logic  set x = <expr>   if <cond> … else … end   while <cond> … end   for x in <list> … end
         break   fail \"<why>\"   return <values>
         break   stop
  values \"text with $x\"  12.5  x  row.price  null  [a, b]  a + 1  x > 200 and not done  contains(a, \"b\")
  scrape items \"<what>\"   extract \"<fields>\" [from item] -> x   open item [\"<which link>\"]   back
         next page (false at the end)   emit x, y (streams one JSON record)
         {{plain words}}: a secret from the password manager

Steps on one session (-s NAME) share a browser, its page and its logins.
Agents: `fab --skill` prints how to drive fab from the CLI (`--install` adds it to an agent).";

#[derive(Parser)]
#[command(
    name = "fab",
    version,
    about = "fab, the fast agentic browser: say what to do on the web, fab does it",
    after_help = AFTER_HELP,
    arg_required_else_help = true
)]
struct Cli {
    /// Print the agent skill (how an agent should use fab); with --install, add it to an agent.
    #[arg(long)]
    skill: bool,
    /// With --skill: install it for claude (default), codex, agents (~/.agents/skills), or into a directory.
    #[arg(long, requires = "skill", num_args = 0..=1, default_missing_value = "claude", value_name = "WHERE")]
    install: Option<String>,
    /// Print the JSON Schemas of a structured request and of the output events.
    #[arg(long)]
    schema: bool,
    /// Session: steps on the same session share one browser.
    #[arg(short = 's', long, global = true, env = "FAB_SESSION", default_value = "default")]
    session: String,
    /// Deduplicate a submitted operation within this session.
    #[arg(long, global = true)]
    request_id: Option<String>,
    /// Show fab's decisions as they happen (on stderr).
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Run the browser without a window.
    #[arg(long, global = true, env = "FAB_HEADLESS", help_heading = "Browser (used when a session starts)")]
    headless: bool,
    /// Browser to launch: chrome, firefox, edge, brave, … or a path. Default: your default browser.
    #[arg(long, global = true, help_heading = "Browser (used when a session starts)")]
    browser: Option<String>,
    /// Persistent profile (logins, cookies): a name or a directory. Default: the session name.
    #[arg(long, global = true, env = "FAB_PROFILE", help_heading = "Browser (used when a session starts)")]
    profile: Option<String>,
    /// Use a throwaway profile.
    #[arg(long, global = true, env = "FAB_ISOLATED", help_heading = "Browser (used when a session starts)")]
    isolated: bool,
    /// Work in your running Chrome instead of launching one: auto, a port, http://host:port or ws://…
    #[arg(long, global = true, env = "FAB_CONNECT", help_heading = "Browser (used when a session starts)")]
    connect: Option<String>,
    /// LLM fab consults for reading and computing (OpenRouter model id; "none": the engine alone).
    #[arg(long, global = true, env = "FAB_MODEL", help_heading = "Browser (used when a session starts)")]
    model: Option<String>,
    /// Close a session after this many idle seconds (0: never).
    #[arg(long, global = true, env = "FAB_IDLE", default_value_t = 3600, help_heading = "Browser (used when a session starts)")]
    idle: u64,
    /// Engine knob overrides (development).
    #[arg(long = "set", global = true, hide = true)]
    set: Vec<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum TaskAction {
    List,
    Show { task: fab_core::domain::TaskId },
    Output { task: fab_core::domain::TaskId, #[arg(long)] after: Option<u64> },
    Resume { task: fab_core::domain::TaskId, #[arg(long)] adopt_page: bool },
    Cancel { task: fab_core::domain::TaskId, #[arg(long)] reason: String },
    Resolve {
        task: fab_core::domain::TaskId,
        #[arg(long)] effect: u64,
        #[arg(long)] reason: String,
        /// Assert the pending effect did not execute.
        #[arg(long, conflicts_with_all = ["result", "observe"], required_unless_present_any = ["result", "observe"])] not_applied: bool,
        /// Applied result JSON: a tool reply object or the requested program value.
        #[arg(long, conflicts_with_all = ["not_applied", "observe"], required_unless_present_any = ["not_applied", "observe"])] result: Option<String>,
        /// Resolve an interrupted `items`, `next page`, `open` or `back` from what its page shows now.
        #[arg(long, conflicts_with_all = ["not_applied", "result"])] observe: bool,
    },
}

impl TaskAction {
    fn command(&self) -> Result<fab_cli::task_commands::TaskCommand> {
        use fab_cli::task_commands::{TaskCommand as C, ResolutionCommand as R};
        Ok(match self {
            Self::List => C::List { session: None },
            Self::Show { task } => C::Show { task: task.clone() },
            Self::Output { task, after } => C::Output { task: task.clone(), after: *after },
            Self::Resume { task, adopt_page } => C::Resume { task: task.clone(), adopt_page: *adopt_page },
            Self::Cancel { task, reason } => C::Cancel { task: task.clone(), reason: reason.clone() },
            Self::Resolve { task, effect, reason, not_applied, result, observe } => C::Resolve {
                task: task.clone(), effect: fab_cli::task_store::EffectId { task: task.clone(), sequence: *effect },
                resolution: if *not_applied { R::NotApplied { reason: reason.clone() } }
                else if *observe { R::Observed { reason: reason.clone() } }
                else {
                    let record = fab_cli::task_runtime::store()?.get(task)?;
                    if record.pending_effect.as_ref().is_some_and(|pending| pending.kind.is_program()) {
                        R::ProgramApplied { reason: reason.clone(), result: serde_json::from_str(result.as_deref().unwrap_or("")).map_err(|e| invalid(format!("--result: {e}")))? }
                    } else { R::Applied { reason: reason.clone(), result: serde_json::from_str(result.as_deref().unwrap_or("")).map_err(|e| invalid(format!("--result: {e}")))? } }
                },
            },
        })
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Inspect, explicitly resume, cancel, or resolve durable tasks.
    Tasks {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Do something: fab navigates, fills, signs in, reads and answers.
    Do {
        /// What to do, in words ("find the cheapest nonstop flight to Berlin on May 3"),
        /// or a JSON request declaring the output: '{"do": "…", "records": <JSON Schema>}'
        /// (see `fab --skill`). "-" reads it from stdin.
        #[arg(required = true)]
        step: Vec<String>,
        /// Open this first.
        #[arg(long)]
        url: Option<String>,
        /// Write the request's logic as a program first, then run it.
        #[arg(long)]
        program: bool,
        /// Print the program without running it (for `fab run`).
        #[arg(long)]
        print: bool,
    },
    /// Run a program (from FILE or stdin) exactly as written.
    Run {
        /// The program; stdin when omitted or "-".
        file: Option<PathBuf>,
        /// Open this first.
        #[arg(long)]
        url: Option<String>,
        /// Only check that the program parses.
        #[arg(long)]
        check: bool,
        /// JSON Schema every emitted record must fit (flat scalar fields).
        #[arg(long)]
        records: Option<String>,
        /// JSON Schema the returned value must fit.
        #[arg(long)]
        returns: Option<String>,
    },
    /// Running sessions.
    Sessions,
    /// Close the session and its browser (--all: every session).
    Close {
        #[arg(long)]
        all: bool,
    },
    /// Password managers fab signs in and fills from, and the approvals you've given.
    Secrets {
        #[command(subcommand)]
        cmd: Option<SecretsCmd>,
    },
    /// Check the setup: key, browsers, password managers, agent skill.
    Doctor,
    /// Download Chrome for Testing (for machines without a browser).
    Install,
    /// The session daemon (started by `fab do`).
    #[command(name = "__serve", hide = true)]
    ServeSession,
}

#[derive(Subcommand, Clone)]
enum SecretsCmd {
    /// Stores, whether they're ready, and approvals (the default).
    Status,
    /// Resolve a name as a step on URL would; prints where it comes from, never the value.
    Test {
        /// e.g. "{{password}}", "{{Visa card number}}", "{{Stripe test secret key}}"
        name: String,
        #[arg(long)]
        url: String,
    },
    /// Allow an item (a card, an identity, a key) to be typed on a site without asking.
    Allow {
        name: String,
        #[arg(long)]
        url: String,
    },
    /// Remove approvals for a site (all of them, or one item's: --item "Stripe").
    Revoke {
        #[arg(long)]
        url: String,
        #[arg(long)]
        item: Option<String>,
    },
    /// Forget a site's paused generated password, so the next signup generates a new one.
    Reset {
        #[arg(long)]
        url: String,
    },
}

impl Cli {
    /// The `cmd` of this command's `start` event.
    fn command_name(&self) -> &'static str {
        match &self.cmd {
            _ if self.skill => "skill",
            _ if self.schema => "schema",
            None => "fab",
            Some(Cmd::Tasks { .. }) => "tasks",
            Some(Cmd::Do { .. }) => "do",
            Some(Cmd::Run { .. }) => "run",
            Some(Cmd::Sessions) => "sessions",
            Some(Cmd::Close { .. }) => "close",
            Some(Cmd::Secrets { .. }) => "secrets",
            Some(Cmd::Doctor) => "doctor",
            Some(Cmd::Install) => "install",
            Some(Cmd::ServeSession) => "serve",
        }
    }

    /// Knob overrides for a browser this command starts.
    fn start_set(&self, default_profile: &str) -> Vec<String> {
        // The decision VM: the engine every agent-mode result was measured with.
        let mut v = vec!["engine=dvm".to_string()];
        if self.headless {
            v.push("headful=false".to_string());
        }
        if let Some(b) = &self.browser {
            v.push(format!("browser={b}"));
        }
        if let Some(c) = &self.connect {
            v.push(format!("connect={c}"));
        }
        let profile = if self.isolated { "" } else { self.profile.as_deref().unwrap_or(default_profile) };
        v.push(format!("profile={profile}"));
        v.extend(self.set.iter().cloned());
        v
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    fab_cli::load_env();
    tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into())).with_writer(std::io::stderr).init();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Help and the version are documentation, printed as text.
        Err(e) if matches!(e.kind(), clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand) => e.exit(),
        Err(e) => {
            eprint!("{e}");
            let first = e.to_string().lines().next().unwrap_or("invalid arguments").trim_start_matches("error: ").to_string();
            return events::abort("fab", Failure::new(ErrorCode::InvalidArgs, first));
        }
    };
    let cmd = cli.command_name();
    // A panic still ends the stream (after the default report on stderr).
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        events::abort(cmd, Failure::new(ErrorCode::Internal, format!("fab crashed: {info}")));
    }));
    match run(cli).await {
        Ok(code) => code,
        // Whatever escaped a command still ends its stream.
        Err(e) => events::abort(cmd, Failure::of(&e)),
    }
}

/// A rejected argument: the command ends before doing anything.
fn invalid(message: impl Into<String>) -> anyhow::Error {
    Failure::new(ErrorCode::InvalidArgs, message).into()
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let mut k = Knobs::default();
    k.apply(&cli.set).map_err(|e| invalid(format!("{e:#}")))?;
    let ok = |_: ()| ExitCode::SUCCESS;
    if cli.skill {
        return skill(cli.install.as_deref());
    }
    if cli.schema {
        // Documentation, not a result: plain JSON.
        println!("{}", serde_json::to_string_pretty(&events::schemas())?);
        return Ok(ExitCode::SUCCESS);
    }
    let Some(cmd) = &cli.cmd else { return Err(invalid("say what to do: fab do \"…\" (fab --help)")) };
    let w = Writer::new(cli.command_name());
    match cmd {
        Cmd::Do { step, url, program, print } => {
            let args = do_args(step, url.as_deref(), *program)?;
            if !*print {
                return session_call(&cli, w, "do", args).await;
            }
            // Compiling needs no browser: no session starts.
            let shape = api::shape_of(&args)?;
            let compiler = api::default_model(cli.model.as_deref()).map(|_| fab_cli::compile::model());
            let here = args["url"].as_str().map(api::normalize_url);
            let sentence = args["step"].as_str().unwrap_or_default();
            let script = match (&compiler, shape.records.is_some() || shape.returns.is_some()) {
                (Some(model), true) => fab_cli::compile::compile(model, sentence, here.as_deref(), &shape).await?.0,
                (None, true) => return Err(invalid("declared records need an LLM to write their program: set OPENROUTER_API_KEY or FAB_MODEL")),
                (_, false) => {
                    let (script, note, _) = api::compile_or_sentence(compiler.as_deref(), sentence, here.as_deref()).await;
                    if cli.verbose || note.contains("one step") {
                        eprintln!("script{note}");
                    }
                    script
                }
            };
            Ok(w.end(End::ok(Value::String(script))))
        }
        Cmd::Run { file, url, check, records, returns } => {
            let script = read_script(file.as_deref())?;
            // Syntax errors show before any browser starts.
            fab_core::script::parse(&script).map_err(|e| Failure::new(ErrorCode::InvalidProgram, format!("script error, {e} (nothing was run)")))?;
            let schema = |flag: &str, v: &Option<String>| -> Result<Value> {
                v.as_deref().map(|s| serde_json::from_str::<Value>(s).map_err(|e| invalid(format!("--{flag}: {e}")))).transpose().map(|v| v.unwrap_or(Value::Null))
            };
            let args = json!({"script": script, "url": url, "records": schema("records", records)?, "returns": schema("returns", returns)?});
            api::shape_of(&args)?;
            if *check {
                return Ok(w.end(End::ok(Value::Null)));
            }
            session_call(&cli, w, "run", args).await
        }
        Cmd::Tasks { action } => {
            let command = action.command()?;
            // Work that needs the session's browser runs in the session.
            if matches!(command, fab_cli::task_commands::TaskCommand::Resume { .. } | fab_cli::task_commands::TaskCommand::Resolve { resolution: fab_cli::task_commands::ResolutionCommand::Observed { .. }, .. }) {
                return session_call(&cli, w, "tasks", serde_json::to_value(command)?).await;
            }
            #[cfg(unix)]
            if matches!(command, fab_cli::task_commands::TaskCommand::Cancel { .. }) && tokio::net::UnixStream::connect(session::socket(&cli.session)).await.is_ok() {
                return session_call(&cli, w, "tasks", serde_json::to_value(command)?).await;
            }
            let task = command.task_id().cloned();
            let result = fab_cli::task_runtime::control(&cli.session, command).await?;
            tasks_outcome(w, task, result)
        }
        Cmd::Sessions => sessions(w).await,
        Cmd::Close { all } => close(&cli, w, *all).await,
        Cmd::Secrets { cmd } => secrets(w, cmd.clone().unwrap_or(SecretsCmd::Status)).await,
        Cmd::Doctor => doctor(&cli, w, &k).await,
        Cmd::Install => install(w).await,
        Cmd::ServeSession => serve_session(&cli, k).await.map(ok),
    }
}

/// A `do` call's arguments: the words, or a structured request (a body
/// starting with `{`, or "-" for one on stdin).
fn do_args(step: &[String], url: Option<&str>, program: bool) -> Result<Value> {
    let body = if step == ["-"] { read_script(None)? } else { step.join(" ") };
    Ok(match fab_cli::request::DoRequest::parse(&body)? {
        Some(r) => json!({"step": r.goal, "url": url.map(str::to_string).or(r.url), "records": r.records, "returns": r.returns, "program": program}),
        None => json!({"step": body.trim(), "url": url, "program": program}),
    })
}

/// A script from a file, or stdin ("-" or no file).
fn read_script(file: Option<&std::path::Path>) -> Result<String> {
    use std::io::{IsTerminal, Read};
    match file.filter(|f| f.as_os_str() != "-") {
        Some(f) => std::fs::read_to_string(f).map_err(|e| invalid(format!("{}: {e}", f.display()))),
        None => {
            if std::io::stdin().is_terminal() {
                return Err(invalid("pipe a script in, e.g. fab run FILE, or a program on stdin"));
            }
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
    }
}

/// Prints the skill, or installs it where an agent finds skills.
fn skill(install: Option<&str>) -> Result<ExitCode> {
    let Some(to) = install else {
        // Documentation, not a result: plain text.
        print!("{SKILL}");
        return Ok(ExitCode::SUCCESS);
    };
    let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?);
    let dir = match to {
        "claude" => home.join(".claude/skills"),
        "codex" => home.join(".codex/skills"),
        "agents" => home.join(".agents/skills"),
        other => PathBuf::from(other),
    };
    let path = dir.join("fab").join("SKILL.md");
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, SKILL)?;
    Ok(Writer::new("skill").end(End::ok(json!({"installed": path}))))
}

#[cfg(unix)]
async fn session_call(cli: &Cli, mut w: Writer, name: &str, mut args: Value) -> Result<ExitCode> {
    session::check_name(&cli.session).map_err(|e| invalid(format!("{e:#}")))?;
    if let Some(id) = &cli.request_id { args["request_id"] = json!(id); }
    let start = session::Start { set: cli.start_set(&cli.session), model: api::default_model(cli.model.as_deref()), idle: cli.idle };
    // Only start options given on this command are checked against a running session's.
    let explicit = cli.headless || cli.browser.is_some() || cli.profile.is_some() || cli.isolated || cli.connect.is_some() || !cli.set.is_empty();
    let want: Vec<String> = if explicit { start.set.clone() } else { vec![] };
    // FAB_CLIENT names the caller: its commands continue on their own page
    // when several callers share a session at once.
    let client = std::env::var("FAB_CLIENT").unwrap_or_default();
    let req = json!({"op": "call", "tool": name, "args": args, "verbose": cli.verbose, "start": want, "client": client});
    let verbose = cli.verbose;
    // Events go to stdout the moment they arrive; progress to stderr (-v).
    let v = session::request(&cli.session, Some(&start), &req, |msg| {
        if let Some(l) = msg["log"].as_str() {
            if verbose { eprintln!("{l}"); }
        } else if let Ok(event) = serde_json::from_value::<Event>(msg["event"].clone()) {
            w.relay(&event);
        }
    })
    .await
    .map_err(|e| Failure::new(ErrorCode::Interrupted, format!("{e:#}")))?;
    if verbose {
        if let Some(report) = v["report"].as_str().filter(|r| !r.is_empty()) { eprintln!("{}", report.trim_end()); }
    }
    for item in v["items"].as_array().into_iter().flatten() {
        w.item(item.clone());
    }
    let end: End = serde_json::from_value(v["end"].clone()).context("the session's reply has no end")?;
    Ok(w.end(end))
}

#[cfg(not(unix))]
async fn session_call(_: &Cli, _: Writer, _: &str, _: Value) -> Result<ExitCode> {
    bail!("CLI sessions need a Unix system for now")
}

/// A task command's result, as events: listings are `item`s; `output`
/// replays the task's records and its `end`.
fn tasks_outcome(mut w: Writer, task: Option<fab_core::domain::TaskId>, outcome: fab_cli::task_commands::TaskCommandOutcome) -> Result<ExitCode> {
    use fab_cli::task_commands::TaskCommandOutcome as O;
    w.start(task.as_ref().map(ToString::to_string), None);
    match &outcome {
        O::List(tasks) => {
            for t in tasks { w.item(serde_json::to_value(t)?); }
            Ok(w.end(End::ok(Value::Null)))
        }
        O::Show(_) | O::Cancelled(_) | O::Resolved(_) | O::AlreadyFinished(_) | O::Resume { .. } | O::Observe { .. } => Ok(w.end(outcome.end()?.expect("one task"))),
        O::Output(outputs) => {
            let task = task.context("output names a task")?;
            let mut end = None;
            for o in outputs {
                match o.value["t"].as_str() {
                    Some("end") => end = Some(serde_json::from_value::<End>(o.value.clone())?),
                    // Committed before this contract: shown as it was stored.
                    None => w.item(o.value.clone()),
                    _ => {
                        let mut line = o.value.clone();
                        line["seq"] = json!(o.sequence);
                        w.relay(&serde_json::from_value::<Event>(line)?);
                    }
                }
            }
            // Not ended yet: the end says why, from the task's state.
            let end = match end {
                Some(end) => end,
                None => {
                    use fab_cli::task_store::TaskState;
                    let state = fab_cli::task_runtime::store()?.get(&task)?.state;
                    let code = if matches!(state, TaskState::Paused { .. }) { ErrorCode::Paused } else { ErrorCode::Interrupted };
                    End::fail(Failure::new(code, format!("task {task} has not ended ({}); see fab tasks show {task}", serde_json::to_value(&state)?["status"].as_str().unwrap_or("unknown"))))
                }
            };
            Ok(w.end(end))
        }
    }
}

#[cfg(unix)]
async fn serve_session(cli: &Cli, k: Knobs) -> Result<()> {
    let idle = if cli.idle == 0 { std::time::Duration::MAX } else { std::time::Duration::from_secs(cli.idle.max(10)) };
    session::serve(&cli.session, k, cli.set.clone(), api::default_model(cli.model.as_deref()), idle).await
}

#[cfg(not(unix))]
async fn serve_session(_: &Cli, _: Knobs) -> Result<()> {
    bail!("CLI sessions need a Unix system for now")
}

#[cfg(unix)]
async fn sessions(mut w: Writer) -> Result<ExitCode> {
    for (name, mut v) in session::list().await {
        v["name"] = json!(name);
        w.item(v);
    }
    Ok(w.end(End::ok(Value::Null)))
}

#[cfg(not(unix))]
async fn sessions(_: Writer) -> Result<ExitCode> {
    bail!("CLI sessions need a Unix system for now")
}

#[cfg(unix)]
async fn close(cli: &Cli, mut w: Writer, all: bool) -> Result<ExitCode> {
    let names: Vec<String> = if all { session::list().await.into_iter().map(|(n, _)| n).collect() } else { vec![cli.session.clone()] };
    let mut failed = vec![];
    for n in names {
        match session::request(&n, None, &json!({"op": "close"}), |_| {}).await {
            Ok(_) => w.item(json!({"session": n, "closed": true})),
            Err(e) => {
                w.item(json!({"session": n, "closed": false, "error": format!("{e:#}")}));
                failed.push(n);
            }
        }
    }
    Ok(w.end(if failed.is_empty() { End::ok(Value::Null) } else { End::fail(Failure::new(ErrorCode::StepFailed, format!("could not close {}", failed.join(", ")))) }))
}

#[cfg(not(unix))]
async fn close(_: &Cli, _: Writer, _: bool) -> Result<ExitCode> {
    bail!("CLI sessions need a Unix system for now")
}

async fn secrets(mut w: Writer, cmd: SecretsCmd) -> Result<ExitCode> {
    use fab_core::secrets;
    let v = secrets::vault();
    let value = match cmd {
        SecretsCmd::Status => {
            // Stores in the order fab looks, then approvals.
            for (name, ready, note) in v.status().await {
                w.item(json!({"store": name, "ready": ready, "note": note}));
            }
            for a in secrets::approvals() {
                w.item(json!({"approval": a.title, "site": a.site}));
            }
            json!({"config": secrets::config_path(), "approvals": fab_core::paths::config_dir().join("secrets-allow.toml")})
        }
        SecretsCmd::Test { name, url } => {
            let url = api::normalize_url(&url);
            let (n, concealed, src) = v.test(&name, &url).await.map_err(|e| Failure::new(ErrorCode::SecretUnavailable, format!("{e:#}")))?;
            json!({"source": src, "chars": n, "concealed": concealed})
        }
        SecretsCmd::Allow { name, url } => json!(v.allow(&name, &api::normalize_url(&url)).await.map_err(|e| Failure::new(ErrorCode::SecretUnavailable, format!("{e:#}")))?),
        SecretsCmd::Revoke { url, item } => {
            let site = secrets::site_of(&api::normalize_url(&url));
            json!({"site": site, "removed": secrets::revoke(&site, item.as_deref())?})
        }
        SecretsCmd::Reset { url } => {
            let url = api::normalize_url(&url);
            let site = secrets::site_of(&url);
            let cleared = secrets::reset_generated(&url).map_err(|e| Failure::new(ErrorCode::SecretUnavailable, format!("{e:#}")))?;
            // One item per cleared workflow: never a password, only whose it was.
            if let Some(r) = &cleared {
                w.item(json!({"site": r.site, "username": r.username}));
            }
            json!({"site": site, "cleared": usize::from(cleared.is_some())})
        }
    };
    Ok(w.end(End::ok(value)))
}

/// One `item` per check: `{"check", "ok", "detail"}`.
async fn doctor(cli: &Cli, mut w: Writer, k: &Knobs) -> Result<ExitCode> {
    use fab_core::backend::discover;
    let mut check = |name: &str, ok: bool, detail: String| w.item(json!({"check": name, "ok": ok, "detail": detail}));
    check("version", true, env!("CARGO_PKG_VERSION").into());
    let cfg = fab_cli::config_env().map(|p| p.display().to_string()).unwrap_or_else(|| "~/.config/fab/env".into());
    let key = ["TYPESAFE_API_KEY", "OPENROUTER_API_KEY"].into_iter().find(|v| std::env::var(v).is_ok_and(|x| !x.is_empty()));
    match key {
        Some(var) => match Jev::from_env() {
            Ok(j) => {
                let mut ts = vec![];
                let mut failed = None;
                for _ in 0..3 {
                    match j.warm().await {
                        Ok(d) => ts.push(d.as_millis()),
                        Err(e) => {
                            failed = Some(format!("{e:#}"));
                            break;
                        }
                    }
                }
                match (failed, ts.first()) {
                    (Some(e), _) => check("jev", false, e),
                    (None, Some(cold)) => check("jev", true, format!("{} via {var} · cold {cold} ms · warm {:?} ms", j.model, &ts[1..])),
                    (None, None) => check("jev", false, "no answer".into()),
                }
            }
            Err(e) => check("jev", false, format!("{e:#}")),
        },
        None => check("jev", false, format!("no key: high-level instructions need OPENROUTER_API_KEY (environment or {cfg}); precise commands work without one")),
    }
    let model = api::default_model(cli.model.as_deref());
    check("task", model.is_some(), model.unwrap_or_else(|| "engine only (set OPENROUTER_API_KEY or FAB_MODEL for an LLM coprocessor)".into()));
    let rep = discover::report(&k.browser);
    match &rep.system_default {
        Some((raw, Some(name))) => check("default", true, format!("{name} ({raw})")),
        Some((raw, None)) => check("default", false, format!("{raw} (unknown to fab)")),
        None => check("default", false, "(none found)".into()),
    }
    for n in &rep.notes {
        check("note", true, n.clone());
    }
    match &rep.chosen {
        Some(f) => {
            let t = Instant::now();
            let mut kk = k.clone();
            kk.headful = false;
            kk.profile.clear();
            kk.connect.clear();
            match fab_core::backend::Browser::launch(&kk).await {
                Ok(b) => {
                    check("browser", true, format!("{} ({}) {} · launches in {} ms · {}", f.name, f.source, f.path.display(), t.elapsed().as_millis(), b.describe()));
                    b.close().await;
                }
                Err(e) => check("browser", false, format!("{} ({}) {}: {e:#}", f.name, f.source, f.path.display())),
            }
        }
        None => check("browser", false, format!("none: {}", rep.error.unwrap_or_default())),
    }
    let others: Vec<String> = rep
        .installed
        .iter()
        .map(|f| match discover::spec(&f.id).and_then(|s| s.unsupported) {
            Some(_) => format!("{} (not drivable)", f.name),
            None => f.name.clone(),
        })
        .collect();
    check("installed", !others.is_empty(), if others.is_empty() { "(none)".into() } else { others.join(", ") });
    let dbg: Vec<String> = discover::debuggable().iter().map(|(s, port, _)| format!("{} on port {port}", s.name)).collect();
    check("attach", !dbg.is_empty(), if dbg.is_empty() { "no running browser with remote debugging (Chrome 144+: chrome://inspect/#remote-debugging, then --connect auto)".into() } else { dbg.join(", ") });
    check("home", true, format!("{} (profiles, sessions, screenshots, site shapes)", fab_core::paths::home().display()));
    let stores = fab_core::secrets::vault().status().await;
    check(
        "secrets",
        !stores.is_empty(),
        if stores.is_empty() { "no password manager found (fab secrets)".to_string() } else { stores.iter().map(|(n, ready, _)| format!("{n}{}", if *ready { "" } else { " (not ready)" })).collect::<Vec<_>>().join(", ") },
    );
    let health = format!("{}/health", k.camofox_url);
    match reqwest::get(&health).await {
        Ok(r) if r.status().is_success() => check("camofox", true, k.camofox_url.clone()),
        _ => check("camofox", false, "not running (optional: npx @askjo/camofox-browser, then --set backend=camofox)".into()),
    }
    check("skill", true, "fab --skill --install (teaches Claude Code the CLI; codex, agents or a directory after --install)".into());
    Ok(w.end(End::ok(Value::Null)))
}

/// Downloads the current stable Chrome for Testing, where discovery finds it
/// when no browser is installed.
async fn install(w: Writer) -> Result<ExitCode> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "mac-arm64",
        ("macos", _) => "mac-x64",
        ("linux", "x86_64") => "linux64",
        ("windows", "x86_64") => "win64",
        ("windows", _) => "win32",
        (os, arch) => bail!("Chrome for Testing has no build for {os}/{arch}; install Chromium with your package manager"),
    };
    let meta: Value = reqwest::get("https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json").await?.json().await?;
    let stable = &meta["channels"]["Stable"];
    let version = stable["version"].as_str().unwrap_or("unknown");
    let url = stable["downloads"]["chrome"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["platform"] == platform)
        .and_then(|d| d["url"].as_str())
        .ok_or_else(|| anyhow::anyhow!("no Chrome for Testing download for {platform}"))?
        .to_string();
    let dir = fab_core::paths::home().join("browsers").join(format!("chrome-{version}"));
    if dir.exists() {
        return Ok(w.end(End::ok(json!({"version": version, "installed": dir, "already": true}))));
    }
    std::fs::create_dir_all(&dir)?;
    let zip = dir.join("chrome.zip");
    eprintln!("downloading Chrome for Testing {version} ({platform})…");
    let mut resp = reqwest::get(&url).await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(&zip).await?;
    let (mut got, mut shown) = (0u64, 0u64);
    while let Some(chunk) = resp.chunk().await? {
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        got += chunk.len() as u64;
        if total > 0 && got * 10 / total > shown {
            shown = got * 10 / total;
            eprint!("\r{:>3}%", shown * 10);
        }
    }
    eprintln!();
    drop(file);
    let status = if cfg!(windows) {
        std::process::Command::new("tar").arg("-xf").arg(&zip).arg("-C").arg(&dir).status()?
    } else {
        std::process::Command::new("unzip").arg("-q").arg(&zip).arg("-d").arg(&dir).status()?
    };
    if !status.success() {
        bail!("could not unpack {}", zip.display());
    }
    let _ = std::fs::remove_file(&zip);
    let path = fab_core::backend::discover::downloaded().unwrap_or(dir);
    Ok(w.end(End::ok(json!({"version": version, "installed": path}))))
}