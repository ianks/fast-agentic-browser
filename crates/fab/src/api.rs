//! fab's agent surface: one step, in words. The agent says what it wants
//! done; fab navigates, fills, signs in, reads and answers (agent mode:
//! Jev decides, an LLM is consulted only for reading and computing). Every
//! session call (`fab do`, `fab run`, `fab tasks`) lands here.

use crate::events::{End, ErrorCode, Failure};
use fab_core::Session;
use fab_core::secrets::redact;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

/// What a call returns: the human report (`text`, shown with `-v`) and the
/// data of the stream's `end` event.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Reply {
    pub text: String,
    pub ok: bool,
    /// LLM calls and cost behind the reply (scripts).
    pub turns: u32,
    pub cost: f64,
    /// The program's `return` value, or the step's answer.
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub error: Option<Failure>,
    /// Where the browser was left.
    #[serde(default)]
    pub url: Option<String>,
    /// Records the task has committed in all.
    #[serde(default)]
    pub records: u64,
}

impl Reply {
    pub(crate) fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), ok: true, ..Default::default() }
    }

    pub(crate) fn fail(code: ErrorCode, text: impl Into<String>) -> Self {
        let text = text.into();
        Self { error: Some(Failure::new(code, text.clone())), text, ok: false, ..Default::default() }
    }

    pub(crate) fn failure(failure: Failure) -> Self {
        Self { text: failure.message.clone(), error: Some(failure), ok: false, ..Default::default() }
    }

    /// The stream's last event.
    pub fn end(&self) -> End {
        let error = match (&self.error, self.ok) {
            (Some(f), _) => Some(f.clone()),
            (None, true) => None,
            (None, false) => Some(Failure::new(ErrorCode::StepFailed, self.text.lines().next().unwrap_or("failed"))),
        };
        End { ts: crate::events::now(), url: self.url.clone(), ok: self.ok && error.is_none(), value: self.value.clone(), records: self.records, error }
    }
}

/// Per-server state beyond the session.
#[derive(Default)]
/// Per-call context; clones share the fixture server (concurrent calls each
/// get a clone).
#[derive(Clone)]
pub struct Ctx {
    /// Coprocessor LLM (None = the engine alone).
    pub model: Option<String>,
    pub(crate) program_journal: Option<Arc<crate::durable_program::Journal>>,
    /// A durable `do`/`step`: journals each call of a planner loop that ends
    /// the call (never one inside a program, see `run_script`).
    pub(crate) call_journal: Option<Arc<dyn crate::planner::CallJournal>>,
    /// The output the caller declared (a structured request, `run --records`).
    pub(crate) shape: Arc<crate::compile::Shape>,
    /// Serves `fixture:<file>` URLs from the repo's fixtures (development).
    fixtures: Fixtures,
}

/// The fixture server of a call, started the first time a `fixture:` URL is asked for.
pub(crate) type Fixtures = Arc<tokio::sync::OnceCell<crate::server::FixtureServer>>;

impl Ctx {
    pub fn new(model: Option<String>) -> Self {
        Self { model, fixtures: Default::default(), program_journal: None, call_journal: None, shape: Default::default() }
    }

    pub(crate) async fn url(&mut self, url: &str) -> anyhow::Result<String> {
        let Some(f) = url.strip_prefix("fixture:") else { return Ok(normalize_url(url)) };
        // Generated, not committed.
        if f.trim_start_matches("throttled/").starts_with("scrape/") && !crate::server::fixtures_dir().join("scrape").is_dir() {
            anyhow::bail!("the scrape fixtures are generated: run `fab-bench scrape gen`");
        }
        let srv = self.fixtures.get_or_try_init(|| crate::server::start(0)).await?;
        Ok(srv.url(f))
    }

    pub(crate) fn fixtures(&self) -> Fixtures { self.fixtures.clone() }
}

/// `example.com` → `https://example.com`; `localhost:3000` → `http://localhost:3000`.
pub fn normalize_url(u: &str) -> String {
    let u = u.trim();
    if u.contains("://") || u.starts_with("about:") || u.starts_with("data:") || u.starts_with("file:") || u.starts_with("chrome:") {
        return u.to_string();
    }
    let host = u.split(['/', ':']).next().unwrap_or("");
    let local = host == "localhost" || host.starts_with("127.") || host == "0.0.0.0" || host.ends_with(".local") || host.ends_with(".localhost");
    format!("{}://{u}", if local { "http" } else { "https" })
}

/// The default LLM: `FAB_MODEL`, else a fast cheap one when an OpenRouter
/// key is set, else none (the engine alone).
pub fn default_model(flag: Option<&str>) -> Option<String> {
    let m = flag.map(str::to_string).or_else(|| std::env::var("FAB_MODEL").ok()).filter(|m| !m.is_empty());
    match m.as_deref() {
        Some("none") => None,
        Some(_) => m,
        None => std::env::var("OPENROUTER_API_KEY").ok().filter(|k| !k.is_empty()).map(|_| "inception/mercury-2.5".to_string()),
    }
}

/// Runs one call. `live` receives decisions and progress as they happen.
pub async fn call(sess: &mut Session, ctx: &mut Ctx, name: &str, args: &Value, live: Option<Arc<dyn Fn(String) + Send + Sync>>) -> Reply {
    // Whatever fab says, as it happens or at the end, never holds a typed secret.
    let live = live.map(|f| Arc::new(move |l: String| f(if redact::active() { redact::text(&l) } else { l })) as Arc<dyn Fn(String) + Send + Sync>);
    sess.live = live.clone();
    let mut r = match shape_of(args) {
        Err(failure) => Reply::failure(failure),
        Ok(shape) => {
            ctx.shape = Arc::new(shape);
            if let (Some(r), Some(f)) = (&ctx.shape.records, &sess.live) {
                f(format!("{SCHEMA}{}", r.raw));
            }
            let mut r = call_tool(sess, ctx, name, args, live).await;
            // A declared answer holds on every path (agent mode included).
            if let (true, Some(f)) = (r.ok, &ctx.shape.returns) {
                match f.coerce(Some(&r.value), r.url.as_deref()) {
                    Ok(v) => r.value = v,
                    Err(why) => {
                        r.ok = false;
                        r.error = Some(Failure::new(ErrorCode::SchemaMismatch, format!("the answer does not fit the declared `returns`: {why}")));
                    }
                }
            }
            r
        }
    };
    sess.live = None;
    if redact::active() {
        r.text = redact::text(&r.text);
    }
    r
}

/// The output a call's arguments declare.
pub fn shape_of(args: &Value) -> Result<crate::compile::Shape, Failure> {
    Ok(crate::compile::Shape {
        records: args.get("records").filter(|v| !v.is_null()).map(crate::request::RecordSchema::parse).transpose()?,
        returns: args.get("returns").filter(|v| !v.is_null()).map(crate::request::returns).transpose()?,
    })
}

async fn call_tool(sess: &mut Session, ctx: &mut Ctx, name: &str, args: &Value, live: Option<Arc<dyn Fn(String) + Send + Sync>>) -> Reply {
    match name {
        "do" | "task" => do_sentence(sess, ctx, args).await,
        "run" => match fab_core::script::parse(args["script"].as_str().unwrap_or_default()) {
            Ok(prog) => run_script(sess, ctx, &prog, args["url"].as_str(), None).await,
            Err(e) => Reply::fail(ErrorCode::InvalidProgram, format!("script error, {e} (nothing was run)")),
        },
        // One step as written, without compiling (bench and tests).
        "step" => do_step(sess, ctx, args, live).await,
        other => Reply::fail(ErrorCode::InvalidArgs, format!("unknown tool {other}: fab's tools are `do` and `run`")),
    }
}

/// "open acme.dev and log in" → ("https://acme.dev", "log in"). Only a step
/// that starts by opening an address.
pub fn leading_url(step: &str) -> Option<(String, String)> {
    let s = step.trim();
    let l = s.to_lowercase();
    // "log in to github.com": open it, and the whole step stays (fab signs in).
    let signin = ["log in to ", "log into ", "login to ", "sign in to ", "sign into "].iter().find_map(|p| l.starts_with(p).then(|| &s[p.len()..]));
    let rest = match signin {
        Some(r) => r,
        None => ["open ", "go to ", "goto ", "visit ", "navigate to ", "browse to "].iter().find_map(|p| l.starts_with(p).then(|| &s[p.len()..]))?,
    };
    let token = rest.split_whitespace().next()?.trim_end_matches([',', '.', ';']);
    let host = token.split("://").last()?.split(['/', ':', '?']).next()?;
    let looks_like = token.contains("://") || host == "localhost" || host.parse::<std::net::IpAddr>().is_ok() || (host.contains('.') && host.rsplit('.').next().is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())));
    if !looks_like {
        return None;
    }
    let after = rest.trim_start()[token.len()..].trim_start().trim_start_matches([',', '.', ';']).trim_start();
    if signin.is_some() {
        return Some((token.to_string(), s.to_string()));
    }
    let after = ["and then ", "and ", "then "].iter().find_map(|p| after.to_lowercase().starts_with(p).then(|| &after[p.len()..])).unwrap_or(after);
    Some((token.to_string(), after.trim().to_string()))
}

/// A request in words. By default fab does it in agent mode. With `program`,
/// the LLM first writes its known logic as a program (`fab_core::script`),
/// which then runs; `print` returns the program without running it. With no
/// LLM, or a program that doesn't parse, the request runs as said.
async fn do_sentence(sess: &mut Session, ctx: &mut Ctx, args: &Value) -> Reply {
    let sentence = args["step"].as_str().or(args["task"].as_str()).unwrap_or_default().trim().to_string();
    if sentence.is_empty() {
        return Reply::fail(ErrorCode::InvalidArgs, "say what to do, e.g. \"open example.com and find the pricing page\"");
    }
    let model = args["model"].as_str().map(str::to_string).or_else(|| ctx.model.clone()).filter(|m| m != "none");
    let print = args["print"].as_bool() == Some(true);
    // Declared records (or a typed answer) come from a program, never from
    // one agent-mode step: its `emit`s and `return` have the declared shape.
    let declared = ctx.shape.records.is_some() || ctx.shape.returns.as_ref().is_some_and(|f| f.kind != crate::request::Kind::String);
    if declared && model.is_none() {
        return Reply::fail(ErrorCode::InvalidArgs, "declared records need an LLM to write their program: set OPENROUTER_API_KEY or FAB_MODEL");
    }
    let forced = declared || args["program"].as_bool() == Some(true);
    // Open the page first: the script is written for the page it starts on.
    let mut url = args["url"].as_str().filter(|u| !u.trim().is_empty()).map(str::to_string);
    if url.is_none() && !print {
        if let Some((u, _)) = leading_url(&sentence) {
            url = Some(u);
        }
    }
    if !print && !forced {
        if let Some(u) = &url {
            if let Err(x) = open(sess, ctx, u).await {
                return Reply::fail(ErrorCode::NavigationFailed, format!("error: {x:#}"));
            }
        }
    }
    // A request to collect data from a list (scrape, export, "every …") runs
    // as a program: its loops are code. Anything else is agent mode on the
    // request as said: on held-out-3 it passed 35/40 and compiled programs
    // 17/40 (0/5 of the tasks that state a condition).
    if !print && !forced && !(model.is_some() && collects_data(sess, &sentence).await) {
        return do_step(sess, ctx, &json!({"step": sentence, "model": args["model"]}), None).await;
    }
    // From the request alone: shown the page, models plan clicks and branch
    // on guesses about the UI.
    let compiler = model.as_ref().map(|_| crate::compile::model());
    let here = if print { None } else if forced { url.clone() } else { sess.browser.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).filter(|u| u.starts_with("http")) };
    let (script, note, compile_cost) = if declared {
        let compiler = compiler.as_deref().expect("declared requests need a model");
        match crate::compile::compile(compiler, &sentence, here.as_deref(), &ctx.shape).await {
            Ok((s, u)) => (s, format!(" ({compiler} · ${:.5})", u.cost), u.cost),
            // No program that fits the request: nothing ran.
            Err(e) => {
                let failure = Failure::of(&e);
                return Reply::failure(if failure.code == ErrorCode::Internal { Failure::new(ErrorCode::InvalidProgram, format!("could not write a program for this request: {e:#}")) } else { failure });
            }
        }
    } else {
        compile_or_sentence(compiler.as_deref(), &sentence, here.as_deref()).await
    };
    if let Some(f) = &sess.live {
        f(format!("script{note}:\n{script}"));
    }
    if print {
        let mut r = Reply::ok(script.clone());
        r.value = Value::String(script);
        return r;
    }
    // Not compiled: the request runs as one agent-mode step, as `do` always did.
    if note == ONE_STEP_NO_LLM || note.starts_with(ONE_STEP_FAILED) {
        if forced { if let Some(u) = &url { if let Err(e) = open(sess, ctx, u).await { return Reply::fail(ErrorCode::NavigationFailed, format!("{e:#}")); } } }
        let mut r = do_step(sess, ctx, &json!({"step": sentence, "model": args["model"]}), None).await;
        r.text = format!("script{note}\n\n{}", r.text);
        return r;
    }
    let prog = match fab_core::script::parse(&script) {
        Ok(p) => p,
        Err(e) => return Reply::fail(ErrorCode::InvalidProgram, format!("script error, {e} (nothing was run)\n{script}")),
    };
    // Undeclared: the records' shape as far as the program names it.
    if !declared {
        if let (Some(schema), Some(f)) = (crate::request::inferred(&prog), &sess.live) {
            f(format!("{SCHEMA}{schema}"));
        }
    }
    if forced { if let Some(u) = &url { if let Err(e) = open(sess, ctx, u).await { return Reply::fail(ErrorCode::NavigationFailed, format!("{e:#}")); } } }
    // A program with no logic (only `do` steps) adds nothing to the request:
    // fab does the request as it was said.
    if !declared && !has_logic(&prog) {
        let mut r = do_step(sess, ctx, &json!({"step": sentence, "model": args["model"]}), None).await;
        r.text = format!("script{note}:\n{}\n(no logic to run: doing the request as said)\n\n{}", indent(&script), r.text);
        r.turns += u32::from(compile_cost > 0.0);
        r.cost += compile_cost;
        return r;
    }
    let mut r = run_script(sess, ctx, &prog, None, Some(&sentence)).await;
    r.text = format!("script{note}:\n{}\n\n{}", indent(&script), r.text);
    r.turns += u32::from(compile_cost > 0.0);
    r.cost += compile_cost;
    r
}

/// Whether the request asks to collect data from many items of a list
/// (scrape, export, "every …", "all …"). One Jev question, no LLM.
async fn collects_data(sess: &mut Session, request: &str) -> bool {
    let mut q = fab_core::jev::Questions::default();
    q.noul_criteria(
        "collect",
        "Does `request` ask to collect information from many items of a list or table (every item, all pages, each result), as opposed to doing one thing or answering one question?",
        "The request wants data gathered item by item: a scrape, an export, a list of every/all/each result with fields.",
        "The request is one job (fill a form, change a setting, buy, book) or asks for one value or one item.",
    );
    for _ in 0..2 {
        if let Ok(a) = sess.decider.0.ask(&json!({"request": request}), &q).await {
            if let Some(p) = a.yes("collect") {
                return p >= 0.5;
            }
        }
    }
    // Jev unavailable: the words that ask for a collection.
    let l = request.to_lowercase();
    ["every ", "all ", "each ", "scrape", "export", "go through", "across", "page by page", "scroll through", "list of"].iter().any(|w| l.contains(w))
}

/// Whether a program holds logic beyond a list of `do` steps: a value read
/// or tested, a variable, a loop or a condition.
fn has_logic(p: &fab_core::script::Program) -> bool {
    use fab_core::script::{Leaf, Op};
    p.ops().iter().any(|o| !matches!(o, Op::Leaf { kind: Leaf::Do, save: None, .. }))
}

/// The compiled script, or the sentence itself as a one-line script, with a
/// note saying why.
pub async fn compile_or_sentence(model: Option<&str>, sentence: &str, page: Option<&str>) -> (String, String, f64) {
    let one_line = sentence.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some(model) = model else { return (one_line, ONE_STEP_NO_LLM.into(), 0.0) };
    match crate::compile::compile(model, sentence, page, &Default::default()).await {
        Ok((s, u)) => (s, format!(" ({model} · ${:.5})", u.cost), u.cost),
        Err(e) => (one_line, format!("{ONE_STEP_FAILED}{})", crate::trunc(&format!("{e:#}"), 200)), 0.0),
    }
}

const ONE_STEP_NO_LLM: &str = " (no LLM: the request as one step)";
const ONE_STEP_FAILED: &str = " (compile failed, running the request as one step: ";

fn indent(t: &str) -> String {
    t.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
}

/// Runs a program on the session. The program holds the request's known
/// logic; each leaf is decided on the live page: `do` runs its sub-goal in
/// agent mode, `read` yields a value, `test` is one Jev yes/no check. Scraping
/// ops (`items`, `extract`, `open`, `back`, `next page`) run in code (see
/// `scrape`), and each `emit` streams one JSON record to the live feed as it
/// happens. Stops at `return`, `fail`, a step that fails, or the step budget.
pub async fn run_script(sess: &mut Session, ctx: &mut Ctx, prog: &fab_core::script::Program, url: Option<&str>, request: Option<&str>) -> Reply {
    use fab_core::program_machine::{Event, Outcome, ProgramMachine, RequestKind, Response};
    use fab_core::script::{Eff, Leaf, Op, Value};

    const MAX_STEPS: usize = 400;
    // A program's `do` steps are followed by more of the program: a planner
    // inside one does not end the call, so its calls are not journaled apart.
    ctx.call_journal = None;
    let t0 = Instant::now();
    let mut machine = match ProgramMachine::new(prog.clone()) {
        Ok(machine) => machine,
        Err(error) => return Reply::fail(ErrorCode::Internal, format!("error: {error}")),
    };
    let mut out: Vec<String> = Vec::new();
    // Records emitted by this run; earlier runs' are in the task outbox.
    let mut records: Vec<serde_json::Value> = Vec::new();
    let mut emitted_before = 0usize;
    let (mut turns, mut cost) = (0u32, 0.0f64);
    let mut steps = 0usize;
    let mut sc = crate::scrape::Scraper::default();
    sc.request = request.unwrap_or_default().to_string();
    let journal = ctx.program_journal.clone();
    let mut binding = None;
    let mut navigation = url.filter(|u| !u.trim().is_empty()).map(str::to_owned);
    let mut arrived: Option<String> = None;
    // While a detail page is open: the program as it was before it opened it.
    let mut reopen: Option<crate::durable_program::Reopen> = None;
    let adopt = journal.as_ref().is_some_and(|j| j.adopt_page);
    let saved = match journal.as_ref().map(|j| j.load()).transpose() {
        Ok(saved) => saved.flatten(),
        Err(e) => return Reply::fail(ErrorCode::Internal, format!("checkpoint error: {e:#}")),
    };
    if let Some(saved) = saved {
        if saved.machine.program() != prog { return Reply::fail(ErrorCode::Internal, "checkpoint belongs to a different program"); }
        if let Some(expected) = &saved.binding && !adopt {
            match crate::durable_program::PageBinding::observe(sess).await {
                Ok(current) if &current == expected => {},
                _ => {
                    if let Some(j) = &journal { let _ = j.pause("the checkpoint's page observation is no longer available"); }
                    return Reply::fail(ErrorCode::Paused, "paused: the checkpoint's page observation is no longer available");
                }
            }
        }
        machine = saved.machine;
        sc = saved.scraper;
        // (With a detail page open, adoption first returns to its list page, below.)
        if adopt && !sc.in_detail() {
            if let Err(e) = sc.adopt_page() { return Reply::fail(ErrorCode::InvalidArgs, format!("cannot adopt page: {e:#}")); }
        }
        reopen = saved.reopen;
        out = saved.log;
        emitted_before = saved.emitted;
        turns = saved.turns;
        cost = saved.cost;
        steps = saved.steps;
        binding = saved.binding;
        navigation = saved.navigation;
        arrived = saved.arrived;
    }
    macro_rules! state {
        () => { {
            // A checkpoint keeps only the items the program still holds.
            sc.forget_items(&machine.item_handles().into_iter().collect());
            crate::durable_program::State {
            machine: machine.clone(), scraper: sc.clone(), log: crate::durable_program::log_tail(&out), emitted: emitted_before + records.len(),
            turns, cost, steps, binding: binding.clone(), navigation: navigation.clone(), arrived: arrived.clone(), reopen: reopen.clone(),
        } } };
    }
    macro_rules! journal_try {
        ($value:expr) => { match $value {
            Ok(value) => value,
            Err(e) => {
                if let Some(j) = &journal { let _ = j.pause("program checkpoint or effect could not be confirmed"); }
                return Reply::fail(ErrorCode::Paused, format!("paused: {e:#}"));
            }
        } };
    }
    // Adopting a page with a detail page open: return to the list page it was
    // opened from (journaled; navigation only, a no-op when the browser never
    // left the list), continuing from before the item was opened, which the
    // program then opens again.
    if let Some(j) = journal.clone().filter(|_| adopt && sc.in_detail()) {
        let (after, target) = match crate::durable_program::returned(state!()) {
            Ok(r) => r,
            Err(e) => {
                let _ = j.pause("the open detail page cannot be left safely to adopt another page");
                return Reply::fail(ErrorCode::Paused, format!("paused: cannot return to the list page to adopt this one: {e:#}"));
            }
        };
        let pending = journal_try!(j.begin(&state!(), crate::task_store::EffectKind::ProgramReturn, &json!({"url": target})));
        if let Some(u) = &target { journal_try!(open(sess, ctx, u).await); }
        machine = after.machine;
        sc = after.scraper;
        reopen = None;
        out.push(format!("returned to {} to adopt the page", target.as_deref().unwrap_or("the list page")));
        binding = Some(journal_try!(crate::durable_program::PageBinding::observe(sess).await));
        journal_try!(j.complete(&pending, &json!({"returned": target}), &state!()));
    }
    if let Some(u) = navigation.clone() {
        let pending = journal.as_ref().map(|j| j.begin(&state!(), crate::task_store::EffectKind::ProgramNavigation, &json!({"url": u}))).transpose();
        let pending = journal_try!(pending);
        if let Err(e) = open(sess, ctx, &u).await {
            if journal.is_none() { return Reply::fail(ErrorCode::NavigationFailed, format!("error: {e:#}")); }
            journal_try!(Err::<(), _>(e));
        }
        navigation = None;
        if let Some(j) = &journal {
            binding = Some(journal_try!(crate::durable_program::PageBinding::observe(sess).await));
            journal_try!(j.complete(pending.as_ref().unwrap(), &json!({"arrived": true}), &state!()));
        }
    }
    // Resolved as arrived without being observed: only that page continues.
    if let Some(u) = arrived.take() {
        let want = match ctx.url(&u).await { Ok(u) => reqwest::Url::parse(&u).ok(), Err(_) => None };
        let here = current_url(sess).await.and_then(|h| reqwest::Url::parse(&h).ok());
        let same = matches!((&want, &here), (Some(w), Some(h)) if w.origin() == h.origin() && w.path() == h.path());
        if !same {
            if let Some(j) = &journal { let _ = j.pause("the browser is not on the page the navigation was asserted to reach"); }
            return Reply::fail(ErrorCode::Paused, format!("paused: the browser is not on {u}, where the navigation was asserted to arrive"));
        }
    }
    let wants = loop_wants(prog);
    let (more_ok, more_what) = self_limited_loops(prog);
    // The returned values (shown, and as JSON), or why the program failed.
    let mut result: Option<Result<(String, serde_json::Value), Failure>> = None;
    let budget = |steps: usize| Failure::new(ErrorCode::BudgetExhausted, format!("stopped after {steps} page steps without a new record (the step budget)"));
    loop {
        if steps > MAX_STEPS {
            result = Some(Err(budget(steps)));
            break;
        }
        match machine.advance() {
            Event::Finished(Outcome::Returned(values)) => {
                let shown = values.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
                let value = match values.as_slice() {
                    [one] => one.to_json(),
                    many => serde_json::Value::Array(many.iter().map(Value::to_json).collect()),
                };
                result = Some(match &ctx.shape.returns {
                    None => Ok((shown, value)),
                    Some(f) => match f.coerce(Some(&value), page_url(sess).as_deref()) {
                        Ok(value) => Ok((value.to_string(), value)),
                        Err(why) => Err(Failure::new(ErrorCode::SchemaMismatch, format!("the returned value does not fit the declared `returns`: {why}"))),
                    },
                });
                break;
            }
            Event::Finished(Outcome::Failed { line, reason }) => {
                result = Some(Err(if matches!(prog.ops().get(machine.pc()), Some(Op::Fail(_))) {
                    Failure::new(ErrorCode::StepFailed, reason).at(line)
                } else if reason == "the instruction budget was exceeded" {
                    budget(steps)
                } else {
                    Failure::new(ErrorCode::StepFailed, format!("line {line}: {reason}")).at(line)
                }));
                break;
            }
            Event::Finished(Outcome::Ended) => break,
            Event::Record(record) => {
                let url = match sc.source_url() { Some(u) => Some(u.to_string()), None => current_url(sess).await };
                let data = record.as_object().cloned().unwrap_or_default();
                // Declared records leave in the declared shape, or not at all.
                let data = match &ctx.shape.records {
                    None => data,
                    Some(schema) => match schema.conform(&data, url.as_deref()) {
                        Ok(data) => data,
                        Err((field, got)) => {
                            let seq = emitted_before + records.len();
                            result = Some(Err(Failure::new(ErrorCode::SchemaMismatch, format!("record {seq}: `{field}` does not fit the declared records (got {got})")).at(machine_line(prog, machine.pc()))));
                            break;
                        }
                    },
                };
                let record = serde_json::Value::Object(data.clone());
                records.push(record.clone());
                steps = 0;
                let committed = crate::events::Record { ts: crate::events::now(), url, data };
                // Committed without its `seq` (its outbox position); streamed
                // with it. Unjournaled, the task runner assigns it.
                let mut line = journal_try!(serde_json::to_value(&committed));
                line["t"] = json!("record");
                if let Some(j) = &journal {
                    let seq = journal_try!(j.save(&state!(), Some(line.clone())));
                    line["seq"] = json!(seq);
                }
                if let Some(f) = &sess.live { f(format!("{RECORD}{line}")); }
            }
            Event::Request(asked) => {
                let line = asked.line;
                let pc = machine.pc();
                // Opening the outermost detail page: remember the program as it
                // was, to open it again after a return to the list.
                let before = (journal.is_some() && !sc.in_detail() && matches!(asked.kind, RequestKind::Open { .. })).then(|| state!());
                if let (Some(r), RequestKind::Leaf { kind, .. }) = (&mut reopen, &asked.kind) {
                    if *kind == Leaf::Do || (*kind == Leaf::Read && !sc.is_open()) { r.acted = true; }
                }
                let pending = if let Some(j) = &journal {
                    binding = Some(journal_try!(crate::durable_program::PageBinding::observe(sess).await));
                    Some(journal_try!(j.begin(&state!(), crate::task_store::EffectKind::ProgramRequest, &journal_try!(serde_json::to_value(&asked)))))
                } else { None };
                let response: Result<Response, String> = match asked.kind {
                    RequestKind::Leaf { kind: kind @ (Leaf::Read | Leaf::Test), text } if sc.is_open() => {
                        page_question(sess, &mut sc, &text, kind == Leaf::Test).await
                            .map(Response::Value).map_err(|error| format!("{error:#}"))
                    }
                    RequestKind::Leaf { kind, text } => {
                        steps += 1;
                        match kind {
                            Leaf::Test => {
                                live_line(sess, line, &format!("test {text}"));
                                let (yes, _) = sess.expect(&text).await;
                                out.push(format!("✓ {line} test {text} → {}", if yes { "yes" } else { "no" }));
                                Ok(Response::Value(Value::Bool(yes)))
                            }
                            Leaf::Do | Leaf::Read => {
                                let word = if kind == Leaf::Do { "do" } else { "read" };
                                live_line(sess, line, &format!("{word} {text}"));
                                let asked_text = if kind == Leaf::Read { format!("{text} (reply with just the value)") } else { text.clone() };
                                let reply = step(sess, ctx, &asked_text, None, None).await;
                                turns += reply.turns;
                                cost += reply.cost;
                                let answer = reply.answer.clone().unwrap_or_default();
                                let answer = if kind == Leaf::Read {
                                    answer.trim().trim_matches('"').trim_end_matches('.').trim().to_string()
                                } else { answer };
                                let shown = if kind == Leaf::Read || matches!(prog.ops().get(pc), Some(Op::Leaf { save: Some(_), .. })) {
                                    format!("{word} {text} → {}", crate::trunc(&answer, 200))
                                } else { format!("{word} {text}") };
                                if reply.ok {
                                    out.push(format!("✓ {line} {shown}"));
                                    Ok(Response::Value(Value::Str(answer)))
                                } else {
                                    out.push(format!("✗ {line} {shown}: {}", reply.lines.join("; ")));
                                    Err("did not finish".into())
                                }
                            }
                        }
                    }
                    RequestKind::Effect { kind, query } => {
                        steps += 1;
                        match kind {
                            Eff::Test if sc.is_open() => page_question(sess, &mut sc, &query, true).await
                                .map(Response::Value).map_err(|error| format!("{error:#}")),
                            Eff::Test => {
                                live_line(sess, line, &format!("test {query}"));
                                let (yes, _) = sess.expect(&query).await;
                                out.push(format!("✓ {line} test {query} → {}", if yes { "yes" } else { "no" }));
                                Ok(Response::Value(Value::Bool(yes)))
                            }
                            Eff::Next => sc.next(sess, &query).await.map(|more| Response::Value(Value::Bool(more))).map_err(|error| format!("{error:#}")),
                            Eff::Items => {
                                let want = wants.get(&pc).cloned().unwrap_or_default();
                                sc.items(sess, &query, &want).await.map(|items| Response::Value(Value::List(items))).map_err(|error| format!("{error:#}"))
                            }
                        }
                    }
                    RequestKind::Extract { fields, from } => sc.extract(sess, &fields, from.as_ref()).await
                        .map(Response::Value).map_err(|error| format!("{error:#}")),
                    RequestKind::Open { item, leaf, .. } => {
                        steps += 1;
                        sc.open(sess, &item, leaf).await.map(|_| Response::Done).map_err(|error| format!("{error:#}"))
                    }
                    RequestKind::Back => sc.back(sess).await.map(|_| Response::Done).map_err(|error| format!("{error:#}")),
                    RequestKind::ResolveItem { handle } => Ok(Response::Value(sc.record(handle))),
                    RequestKind::ForNextExhausted { had_items, .. } => {
                        // A failure after the pager may have acted is not "no more items".
                        async {
                            let mut more = vec![];
                            if had_items && more_ok.contains(&pc) {
                                if let Some(what) = more_what.get(&pc) {
                                    if sc.next(sess, "page").await? {
                                        more = sc.items(sess, what, &wants.get(&(pc - 1)).cloned().unwrap_or_default()).await?;
                                    }
                                }
                            }
                            anyhow::Ok(Response::IteratorItems(more))
                        }.await.map_err(|error| format!("{error:#}"))
                    }
                };
                for log in sc.log.drain(..) {
                    live_line(sess, line, &log);
                    out.push(format!("  {line} {log}"));
                }
                if let (Some(j), Err(reason)) = (&journal, &response) {
                    journal_try!(j.pause("the program request failed after dispatch; its effects need reconciliation"));
                    return Reply::failure(Failure::new(ErrorCode::Paused, format!("paused at line {line}: {reason}")).at(line));
                }
                let receipt = journal_try!(serde_json::to_value(&response));
                if let Err(error) = machine.complete(&asked.token, response) {
                    result = Some(Err(Failure::new(ErrorCode::Internal, format!("line {line}: {error}")).at(line)));
                    break;
                }
                if !sc.in_detail() {
                    reopen = None;
                } else if let Some(b) = before {
                    reopen = Some(crate::durable_program::Reopen { machine: b.machine, scraper: b.scraper, emitted: b.emitted, acted: false });
                }
                if let Some(j) = &journal {
                    binding = Some(journal_try!(crate::durable_program::PageBinding::observe(sess).await));
                    journal_try!(j.complete(pending.as_ref().unwrap(), &receipt, &state!()));
                }
            }
        }
    }
    if let Some(j) = &journal { journal_try!(j.save(&state!(), None)); }
    turns += sc.turns;
    cost += sc.cost;
    let emitted = emitted_before + records.len();
    if emitted > 0 {
        out.push(format!("{emitted} record{}", if emitted == 1 { "" } else { "s" }));
    }
    let (value, error) = match result {
        Some(Ok((shown, value))) => {
            if !shown.is_empty() { out.push(format!("answer: {shown}")); }
            (value, None)
        }
        Some(Err(failure)) => {
            out.push(format!("failed: {}", failure.message));
            (serde_json::Value::Null, Some(failure))
        }
        None => (serde_json::Value::Null, None),
    };
    out.push(format!("({:.1} s · {})", t0.elapsed().as_secs_f64(), llm_note(turns, cost)));
    let recs = records.iter().map(|record| record.to_string()).collect::<Vec<_>>().join("\n");
    let text = if recs.is_empty() { format!("{}\n\n{}", out.join("\n"), brief(sess).await) } else { format!("{recs}\n\n{}", out.join("\n")) };
    Reply { text, ok: error.is_none(), turns, cost, value, error, url: current_url(sess).await, records: emitted as u64 }
}

/// `for x in items "…"` loops that `break` on their own (a limit), in a
/// program without `next page`: their ForNext pcs, and each one's items.
fn self_limited_loops(prog: &fab_core::script::Program) -> (std::collections::HashSet<usize>, std::collections::HashMap<usize, String>) {
    use fab_core::script::{Eff, Op};
    let mut ok = std::collections::HashSet::new();
    let mut what = std::collections::HashMap::new();
    let pages = prog.ops().iter().any(|o| match o {
        Op::JumpUnless { cond: e, .. } | Op::Set { expr: e, .. } | Op::ForInit { list: e, .. } => e.effects().iter().any(|(k, _)| *k == Eff::Next),
        _ => false,
    });
    if pages {
        return (ok, what);
    }
    for (i, o) in prog.ops().iter().enumerate() {
        let Op::ForInit { list, .. } = o else { continue };
        let Some((_, w)) = list.effects().into_iter().find(|(k, _)| *k == Eff::Items) else { continue };
        let Some(Op::ForNext { exit, .. }) = prog.ops().get(i + 1) else { continue };
        let breaks = prog.ops()[i + 2..*exit].iter().any(|o| matches!(o, Op::Jump { to } if to == exit));
        if breaks {
            ok.insert(i + 1);
            what.insert(i + 1, w);
        }
    }
    (ok, what)
}

/// A question about an opened item's page, answered as a mapped field
/// (yes/no for a test): code, once the layout is mapped.
async fn page_question(sess: &mut Session, sc: &mut crate::scrape::Scraper, q: &str, yes_no: bool) -> anyhow::Result<fab_core::script::Value> {
    use fab_core::script::Value;
    let field = if yes_no { format!("yes/no: {q}") } else { q.to_string() };
    let v = sc.extract(sess, std::slice::from_ref(&field), None).await?.get(&field);
    Ok(if yes_no { Value::Bool(v.truthy()) } else { v })
}

/// Live-feed prefix of a streamed record (the CLI prints these to stdout).
/// Internal live-feed prefixes. A session relays them to the caller as
/// `start` and `record` events (see `events`); the rest of the feed is
/// progress for `-v`.
pub const RECORD: &str = "\u{1e}record ";
pub const START: &str = "\u{1e}start ";
pub const SCHEMA: &str = "\u{1e}schema ";

/// The source line of the instruction before `pc` (an `emit` has advanced).
fn machine_line(prog: &fab_core::script::Program, pc: usize) -> usize {
    prog.line(pc.saturating_sub(1)).unwrap_or(1)
}

/// The page the browser shows, when it is a web page.
fn page_url(sess: &Session) -> Option<String> {
    sess.last_snapshot().map(|s| s.url.clone()).filter(|u| u.starts_with("http") || u.starts_with("file:"))
}

/// Where the browser is: the last observation, else the live address.
/// Where the browser is: its live address, else the last observation (the
/// scraper navigates without observing).
async fn current_url(sess: &mut Session) -> Option<String> {
    let live = sess.browser.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string));
    live.filter(|u| u.starts_with("http") || u.starts_with("file:")).or_else(|| page_url(sess))
}

/// The fields read as `var.field` in an expression.
fn dotted(e: &fab_core::script::Expr, var: &str) -> Vec<String> {
    use fab_core::script::Expr;
    match e {
        Expr::Var(v) => v.strip_prefix(&format!("{var}.")).map(|f| vec![fab_core::script::field_name(f)]).unwrap_or_default(),
        Expr::List(xs) | Expr::Call(_, xs) => xs.iter().flat_map(|x| dotted(x, var)).collect(),
        Expr::Not(x) => dotted(x, var),
        Expr::Bin(_, a, b) => [dotted(a, var), dotted(b, var)].concat(),
        _ => vec![],
    }
}

/// For each `for … in items "…"`, what its body asks of the items: the fields
/// extracted from the loop variable and the link it opens.
pub(crate) fn loop_wants(prog: &fab_core::script::Program) -> std::collections::HashMap<usize, crate::scrape::Wants> {
    use fab_core::script::{Eff, Op};
    let mut out = std::collections::HashMap::new();
    use fab_core::script::Expr;
    for (i, op) in prog.ops().iter().enumerate() {
        let Op::ForInit { slot, list } = op else { continue };
        // `for x in items "…"`, or `for x in v` where `v = items "…"`.
        let at: Vec<usize> = if list.effects().iter().any(|(e, _)| *e == Eff::Items) {
            vec![i]
        } else if let Expr::Var(v) = list {
            prog.ops()
                .iter()
                .enumerate()
                .filter(|(_, o)| matches!(o, Op::Set { var, expr } if var == v && expr.effects().iter().any(|(e, _)| *e == Eff::Items)))
                .map(|(k, _)| k)
                .collect()
        } else {
            vec![]
        };
        if at.is_empty() {
            continue;
        }
        let Some(Op::ForNext { var, exit, .. }) = prog.ops().get(i + 1).filter(|o| matches!(o, Op::ForNext { slot: s, .. } if s == slot)) else { continue };
        let mut w = crate::scrape::Wants::default();
        let mut opened = false;
        for o in &prog.ops()[i + 2..(*exit).min(prog.ops().len())] {
            match o {
                Op::Open { item, .. } if item == var => opened = true,
                Op::Back => opened = false,
                _ => {}
            }
            // What the opened page is read for.
            if opened {
                let said = match o {
                    // Its own fields, or those of the items listed there.
                    Op::Extract { fields, .. } => Some(fields.join(", ")),
                    Op::Leaf { text, kind: fab_core::script::Leaf::Read | fab_core::script::Leaf::Test, .. } => Some(text.clone()),
                    Op::ForInit { list, .. } | Op::JumpUnless { cond: list, .. } | Op::Set { expr: list, .. } => {
                        list.effects().into_iter().filter(|(k, _)| *k != Eff::Next).map(|(k, q)| if k == Eff::Items { format!("a list of {q}") } else { q }).next()
                    }
                    _ => None,
                };
                if let Some(x) = said.filter(|x| !w.after.contains(x)) {
                    w.after.push(x);
                }
            }
            match o {
                Op::Extract { fields, from: Some(f), .. } if f == var && !opened => {
                    for x in fields {
                        if !w.fields.contains(x) {
                            w.fields.push(x.clone());
                        }
                    }
                }
                Op::Open { item, how, leaf } if item == var => {
                    w.open = how.clone().or(Some(String::new()));
                    w.leaf_open = *leaf;
                }
                _ => {}
            }
            // `x.field` in an expression: a field of the item too.
            let exprs: Vec<&Expr> = match o {
                Op::Set { expr, .. } => vec![expr],
                Op::JumpUnless { cond, .. } => vec![cond],
                Op::Return(es) => es.iter().collect(),
                Op::Emit(es) => es.iter().map(|(_, e)| e).collect(),
                _ => vec![],
            };
            for e in exprs {
                for f in dotted(e, var) {
                    if !w.fields.contains(&f) {
                        w.fields.push(f);
                    }
                }
            }
        }
        for k in at {
            let e: &mut crate::scrape::Wants = out.entry(k).or_default();
            for f in &w.fields {
                if !e.fields.contains(f) {
                    e.fields.push(f.clone());
                }
            }
            if w.open.is_some() {
                e.open = w.open.clone();
                e.leaf_open |= w.leaf_open;
                e.after = w.after.clone();
            }
        }
    }
    out
}

fn live_line(sess: &Session, n: usize, t: &str) {
    if let Some(f) = &sess.live {
        f(format!("line {n}: {t}"));
    }
}

async fn open(sess: &mut Session, ctx: &mut Ctx, u: &str) -> anyhow::Result<()> {
    let u = ctx.url(u).await?;
    sess.goto(&u).await?;
    Ok(())
}

/// One step as written, with the reply `do` gave before scripts.
async fn do_step(sess: &mut Session, ctx: &mut Ctx, args: &Value, _live: Option<Arc<dyn Fn(String) + Send + Sync>>) -> Reply {
    let text = args["step"].as_str().or(args["task"].as_str()).unwrap_or_default();
    let model = args["model"].as_str().map(str::to_string);
    let o = step(sess, ctx, text, args["url"].as_str(), model.as_deref()).await;
    let mut t = o.lines.join("\n");
    if o.ran {
        t.push_str(&format!("\n({:.1} s · {})", o.secs, llm_note(o.turns, o.cost)));
    }
    let error = (!o.ok).then(|| Failure::new(o.code.unwrap_or(ErrorCode::StepFailed), o.lines.last().cloned().unwrap_or_else(|| "failed".into())));
    let value = o.answer.clone().map(Value::String).unwrap_or(Value::Null);
    Reply { text: format!("{t}\n\n{}", brief(sess).await), ok: o.ok, turns: o.turns, cost: o.cost, value, error, url: page_url(sess), records: 0 }
}

fn llm_note(turns: u32, cost: f64) -> String {
    if turns == 0 { "no LLM".to_string() } else { format!("{turns} LLM call{} · ${cost:.4}", if turns == 1 { "" } else { "s" }) }
}

/// What one step did.
#[derive(Debug, Default)]
pub struct StepOut {
    pub ok: bool,
    /// What happened, in order; the last is the answer when the engine ran.
    pub lines: Vec<String>,
    /// The step's answer (agent mode's final reply).
    pub answer: Option<String>,
    /// Agent mode ran (the stats below mean something).
    pub ran: bool,
    pub secs: f64,
    pub turns: u32,
    pub cost: f64,
    /// Why it failed, when it did.
    pub code: Option<ErrorCode>,
}

impl StepOut {
    fn stop(ok: bool, line: impl Into<String>) -> Self {
        Self { ok, lines: vec![line.into()], ..Default::default() }
    }
    fn failed(code: ErrorCode, line: impl Into<String>) -> Self {
        Self { code: Some(code), ..Self::stop(false, line) }
    }
}

/// The first web address anywhere in a step ("… on news.ycombinator.com").
pub fn address_in(step: &str) -> Option<String> {
    step.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != ':' && c != '.' && c != '-')).find_map(|w| {
        let w = w.trim_end_matches('.');
        let host = w.split("://").last()?.split(['/', ':', '?']).next()?;
        let tld = host.rsplit('.').next()?;
        let dotted = host.contains('.') && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()) && !w.contains('@');
        (w.contains("://") || host == "localhost" || dotted).then(|| w.to_string())
    })
}

/// One step: open the address it starts with (or `url`), sign in first when
/// the step or the page calls for it, then let agent mode do the rest.
pub async fn step(sess: &mut Session, ctx: &mut Ctx, text: &str, url: Option<&str>, model: Option<&str>) -> StepOut {
    let live = sess.live.clone();
    let mut step = text.trim().to_string();
    let mut url = url.filter(|u| !u.trim().is_empty()).map(str::to_string);
    if url.is_none() {
        if let Some((u, rest)) = leading_url(&step) {
            url = Some(u);
            step = rest;
        }
    }
    // Nothing open yet and the step names a site ("open the comments of the
    // first story on news.ycombinator.com"): start there.
    if url.is_none() {
        let here = sess.browser.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        if here.is_empty() || here.starts_with("about:") || here.starts_with("chrome:") {
            url = address_in(&step);
        }
    }
    let t0 = Instant::now();
    if let Some(u) = &url {
        let u = match ctx.url(u).await {
            Ok(u) => u,
            Err(x) => return StepOut::failed(ErrorCode::InvalidArgs, format!("error: {x:#}")),
        };
        if let Err(x) = sess.goto(&u).await {
            return StepOut::failed(ErrorCode::NavigationFailed, format!("error: {x:#}"));
        }
    }
    if step.is_empty() {
        return match url {
            Some(_) => StepOut::stop(true, "opened"),
            None => StepOut::failed(ErrorCode::InvalidArgs, "say what to do, e.g. \"open example.com and find the pricing page\""),
        };
    }
    let mut done = vec![];
    match crate::signin::before_step(sess, &step).await {
        Ok(Some(s)) => {
            done.push(s);
            if crate::signin::only_signin(&step) {
                return StepOut { ok: true, lines: done, ..Default::default() };
            }
            // Signed in: the engine gets the rest of the step.
            let rest = crate::signin::after_signin(&step);
            if rest.is_empty() {
                return StepOut { ok: true, lines: done, ..Default::default() };
            }
            step = rest;
        }
        Ok(None) => {}
        Err(x) => return StepOut::failed(ErrorCode::SecretUnavailable, format!("{x:#}")),
    }
    // Every {{…}} is resolved before fab acts (see `prepare_secrets`).
    match prepare_secrets(sess, &step).await {
        Ok(s) => step = s,
        Err(x) => return StepOut::failed(ErrorCode::SecretUnavailable, format!("{x:#} (nothing was done)")),
    }
    match save_new_password(sess, &step).await {
        Some(Ok(line)) => done.push(line),
        Some(Err(x)) => return StepOut::failed(ErrorCode::SecretUnavailable, format!("{x:#} (nothing was typed)")),
        None => {}
    }
    let page = match sess.page_summary().await {
        Ok(p) => p,
        Err(x) => return StepOut::failed(ErrorCode::Internal, format!("error: {x:#}")),
    };
    let here = sess.last_snapshot().map(|s| s.url.clone()).unwrap_or_default();
    let model = model.map(str::to_string).or_else(|| ctx.model.clone()).filter(|m| m != "none");
    // Planner events go to the live feed.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::planner::Event>();
    let em = crate::planner::Emitter { arm: 0, tx, t0: Instant::now() };
    let fwd = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let Some(f) = &live {
                if !ev.text.is_empty() {
                    f(format!("{:>5.1}s {:<6} {}", ev.ms / 1e3, ev.kind, ev.text));
                }
            }
        }
    });
    let journal = ctx.call_journal.clone();
    let r = crate::autopilot::run(sess, &step, &here, &page, model.as_deref(), Some(&em), journal.as_deref()).await;
    drop(em);
    let _ = fwd.await;
    sess.flush_shapes();
    let secs = t0.elapsed().as_secs_f64();
    let (answer, ok, turns, cost) = match r {
        Ok((stats, _, _)) => (stats.answer.unwrap_or_else(|| "(no answer)".into()), true, stats.turns, stats.cost),
        Err(err) => match err.downcast::<crate::planner::AgentError>() {
            Ok(ae) => (format!("Did not finish: {}. {}", ae.msg, ae.stats.answer.unwrap_or_default()).trim().to_string(), false, ae.stats.turns, ae.stats.cost),
            Err(err) => (format!("error: {err:#}"), false, 0, 0.0),
        },
    };
    done.push(answer.clone());
    StepOut { ok, lines: done, answer: ok.then_some(answer), ran: true, secs, turns, cost, code: (!ok).then_some(ErrorCode::StepFailed) }
}

/// Resolves the step's `{{…}}` names before anything happens: a missing
/// item or a refused approval stops the step here, before anything is typed
/// (and before any fallback could improvise). Plain values (a name, an
/// address, a country) go into the step as quoted text, which the engine
/// matches to fields and list options like any literal; concealed ones stay
/// placeholders and are typed only when their field is filled.
async fn prepare_secrets(sess: &mut Session, step: &str) -> anyhow::Result<String> {
    use fab_core::secrets::{self, names};
    let found = secrets::placeholders(step);
    if found.is_empty() {
        return Ok(step.to_string());
    }
    let origin = sess.browser.eval("location.origin").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    let mut out = String::new();
    let mut last = 0;
    for (range, name) in found {
        out.push_str(&step[last..range.start]);
        last = range.end;
        if names::parse(name).want == names::Want::NewPassword {
            out.push_str(&step[range.clone()]);
            continue;
        }
        let (plain, _) = secrets::vault().prepare(name, &origin).await?;
        match plain {
            Some(v) => {
                let quoted = step[..range.start].ends_with(['"', '“']);
                if quoted { out.push_str(&v) } else { out.push_str(&format!("\"{}\"", v.as_str())) }
            }
            None => out.push_str(&step[range.clone()]),
        }
    }
    out.push_str(&step[last..]);
    let _ = secrets::vault().take_log().await;
    Ok(out)
}

/// A step that types `{{new password}}` (signing up, changing a password):
/// the password is generated and saved to the password manager, under the
/// email or username quoted in the step, before anything is typed.
async fn save_new_password(sess: &mut Session, step: &str) -> Option<anyhow::Result<String>> {
    use anyhow::Context;
    use fab_core::secrets::{self, names};
    if !secrets::placeholders(step).iter().any(|(_, n)| names::parse(n).want == names::Want::NewPassword) {
        return None;
    }
    let origin = sess.browser.eval("location.origin").await.ok()?.as_str()?.to_string();
    Some(
        async {
            let vault = secrets::vault();
            // Always through the workflow: it answers "saved earlier" for the
            // same account and refuses another account's password.
            let user = fab_core::spans::quoted(step)
                .into_iter()
                .find(|l| !l.contains("{{") && (l.contains('@') || !l.contains(' ')))
                .context("to save the new password, put the email or username it is for in double quotes")?;
            let at = vault.save_generated_login(&origin, &user).await?;
            let _ = vault.take_log().await;
            Ok(if at.starts_with("the login saved earlier") { format!("using the login saved earlier for {user}") } else { format!("saved a new login for {user} to {at}") })
        }
        .await,
    )
}

/// Where the step left the browser: title, address and the page's text
/// (no element ids: nothing takes them).
async fn brief(sess: &mut Session) -> String {
    sess.invalidate();
    match sess.snapshot().await {
        Ok(s) => format!("now at: {}", s.brief(700)),
        Err(e) => format!("now at: (no page: {e:#})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn where_condition_fields_are_wanted_from_the_items() {
        let p = fab_core::script::parse("for c in items \"comments\" where c.pinned == null\n  emit c.author\nend").unwrap();
        let wants = loop_wants(&p);
        assert_eq!(wants.len(), 1);
        let w = wants.values().next().unwrap();
        assert!(w.fields.contains(&"pinned".to_string()) && w.fields.contains(&"author".to_string()), "{:?}", w.fields);
    }

    #[test]
    fn urls() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("localhost:3000/x"), "http://localhost:3000/x");
        assert_eq!(normalize_url("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(normalize_url("http://a.io"), "http://a.io");
        assert_eq!(normalize_url("about:blank"), "about:blank");
    }

    #[test]
    fn steps_that_open_an_address() {
        let l = |s: &str| leading_url(s).map(|(u, r)| (u, r));
        assert_eq!(l("open acme.dev and log in"), Some(("acme.dev".into(), "log in".into())));
        assert_eq!(l("Go to https://news.ycombinator.com, then tell me the top story"), Some(("https://news.ycombinator.com".into(), "tell me the top story".into())));
        assert_eq!(l("open localhost:3000/login"), Some(("localhost:3000/login".into(), "".into())));
        assert_eq!(l("log in to github.com"), Some(("github.com".into(), "log in to github.com".into())));
        assert_eq!(l("open the settings page"), None);
        assert_eq!(l("open order 1042 and cancel it"), None);
        assert_eq!(l("email ada@acme.dev the invoice"), None);
    }

    #[test]
    fn addresses_anywhere() {
        assert_eq!(address_in("open the comments of the first story on news.ycombinator.com"), Some("news.ycombinator.com".into()));
        assert_eq!(address_in("search eBay (https://www.ebay.com/sch) for a Leica"), Some("https://www.ebay.com/sch".into()));
        assert_eq!(address_in("email ada@acme.dev the invoice"), None);
        assert_eq!(address_in("find the price of v1.4"), None);
    }


}
