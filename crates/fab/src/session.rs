//! Named browser sessions for the CLI. The first command on a session starts
//! a background daemon (`fab __serve`) that owns the browser; every command
//! talks to it over a Unix socket in fab's run directory, so the page, tabs
//! and logins carry from one command to the next. A session ends with
//! `fab close`, or by itself after sitting idle. Commands that arrive while
//! another runs get pages of their own in the same browser (see `pool`).

use anyhow::{Context, Result, bail};
use fab_core::{Knobs, Session};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};

use crate::api;
use crate::pool;

pub fn socket(name: &str) -> PathBuf {
    let p = fab_core::paths::run_dir().join(format!("{name}.sock"));
    // Unix socket paths are limited to about 100 bytes: under a long fab home,
    // use a short per-user directory instead.
    if p.as_os_str().len() < 100 {
        return p;
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in fab_core::paths::run_dir().as_os_str().as_encoded_bytes() {
        h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/fab-{uid}-{:08x}", h as u32)).join(format!("{name}.sock"))
}

fn socket_dir(name: &str) -> Result<()> {
    if let Some(d) = socket(name).parent() {
        std::fs::create_dir_all(d)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

pub fn log_file(name: &str) -> PathBuf {
    fab_core::paths::run_dir().join(format!("{name}.log"))
}

pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !name.starts_with('.');
    if !ok {
        bail!("session names are letters, digits, '-', '_' and '.' (at most 40)");
    }
    if name == "serve" {
        bail!("\"serve\" is fab serve's own socket: pick another session name");
    }
    Ok(())
}

/// `FAB_REMOTE=1`: this process is a client of a `fab serve` that owns the
/// browsers and the secrets (another container sharing fab's run
/// directory). It never starts a session daemon of its own.
pub fn remote() -> bool {
    matches!(std::env::var("FAB_REMOTE").as_deref(), Ok("1" | "true" | "yes" | "on"))
}

/// `fab serve`'s control socket.
pub fn serve_socket() -> PathBuf {
    fab_core::paths::run_dir().join("serve.sock")
}

/// No session could be reached or started for a remote client: `fab serve`
/// is down, or could not start it (code `session_unavailable`).
#[derive(Debug)]
pub struct Unavailable(pub String);
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unavailable {}

fn run_dir() -> Result<PathBuf> {
    let dir = fab_core::paths::run_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

/// How a session starts; ignored when it is already running.
#[derive(Clone, Default)]
pub struct Start {
    /// Knob overrides, `k=v`.
    pub set: Vec<String>,
    pub model: Option<String>,
    /// Close after this many idle seconds.
    pub idle: u64,
}

/// Sends one request to session `name`, starting the session first when
/// `start` is given. `on_log` gets progress lines as they come.
/// Sends one request; `on_stream` receives each `{"log": …}` or
/// `{"event": …}` line before the final reply, which is returned.
pub async fn request(name: &str, start: Option<&Start>, req: &Value, mut on_stream: impl FnMut(&Value)) -> Result<Value> {
    let stream = match start {
        Some(s) => connect_or_start(name, s).await?,
        None => match UnixStream::connect(socket(name)).await {
            Ok(s) => s,
            Err(_) => bail!("no session \"{name}\" is running"),
        },
    };
    let (rd, mut wr) = stream.into_split();
    wr.write_all(format!("{req}\n").as_bytes()).await?;
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        let v: Value = serde_json::from_str(&line).context("bad reply from session")?;
        if v.get("log").is_some() || v.get("event").is_some() {
            on_stream(&v);
            continue;
        }
        return Ok(v);
    }
    bail!("session \"{name}\" stopped before answering (log: {})", log_file(name).display())
}

async fn connect_or_start(name: &str, start: &Start) -> Result<UnixStream> {
    let path = socket(name);
    if remote() {
        return match UnixStream::connect(&path).await {
            Ok(s) => Ok(s),
            Err(_) => via_serve(name, start).await,
        };
    }
    Ok(start_local(name, start).await?.0)
}

/// Asks `fab serve` to start session `name`, then connects to it.
async fn via_serve(name: &str, start: &Start) -> Result<UnixStream> {
    let path = socket(name);
    let run = fab_core::paths::run_dir();
    if !path.starts_with(&run) {
        return Err(Unavailable(format!("fab's run directory {} is too long a path to share a socket with fab serve: use a shorter FAB_HOME", run.display())).into());
    }
    let ctl = serve_socket();
    let down = || Unavailable(format!("fab serve is not running (FAB_REMOTE=1 and nothing answers on {})", ctl.display()));
    let stream = UnixStream::connect(&ctl).await.map_err(|_| down())?;
    let (rd, mut wr) = stream.into_split();
    let req = json!({"op": "start", "session": name, "set": start.set, "model": start.model, "idle": start.idle});
    wr.write_all(format!("{req}\n").as_bytes()).await.map_err(|_| down())?;
    let line = tokio::time::timeout(Duration::from_secs(30), BufReader::new(rd).lines().next_line()).await;
    let reply: Value = match line {
        Ok(Ok(Some(l))) => serde_json::from_str(&l).unwrap_or(Value::Null),
        Ok(_) => return Err(down().into()),
        Err(_) => return Err(Unavailable(format!("fab serve did not start session \"{name}\" within 30 s")).into()),
    };
    if reply["ok"] != true {
        return Err(Unavailable(format!("fab serve could not start session \"{name}\": {}", reply["error"].as_str().unwrap_or("no reason given"))).into());
    }
    UnixStream::connect(&path).await.map_err(|e| Unavailable(format!("fab serve started session \"{name}\", but its socket does not answer here ({e}): is {} shared with fab serve?", run.display())).into())
}

/// Connects to session `name`, starting its daemon here when none answers.
/// The child is returned when this call started it.
async fn start_local(name: &str, start: &Start) -> Result<(UnixStream, Option<std::process::Child>)> {
    let path = socket(name);
    match UnixStream::connect(&path).await {
        Ok(s) => return Ok((s, None)),
        // Left behind by a session that died: start over.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            let _ = std::fs::remove_file(&path);
        }
        Err(_) => {}
    }
    let mut child = spawn(name, start)?;
    let t0 = Instant::now();
    loop {
        if let Ok(s) = UnixStream::connect(&path).await {
            return Ok((s, Some(child)));
        }
        // The daemon exited: say why instead of waiting out the timeout.
        if let Ok(Some(_)) = child.try_wait() {
            // Another command may have started the session first.
            if let Ok(s) = UnixStream::connect(&path).await {
                return Ok((s, None));
            }
            let log = std::fs::read_to_string(log_file(name)).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(3).collect();
            bail!("session \"{name}\" could not start: {}", tail.into_iter().rev().collect::<Vec<_>>().join(" / "));
        }
        if t0.elapsed() > Duration::from_secs(15) {
            bail!("session \"{name}\" did not start (log: {})", log_file(name).display());
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

fn spawn(name: &str, start: &Start) -> Result<std::process::Child> {
    run_dir()?;
    socket_dir(name)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_file(name))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("__serve").arg("--session").arg(name).arg("--idle").arg(start.idle.to_string());
    for s in &start.set {
        cmd.arg("--set").arg(s);
    }
    if let Some(m) = &start.model {
        cmd.arg("--model").arg(m);
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
    // Its own process group: a Ctrl-C meant for this command leaves it running.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("could not start the session daemon")
}

/// Running sessions: (name, info) where info comes from the session itself.
pub async fn list() -> Vec<(String, Value)> {
    let dir = socket("x").parent().map(PathBuf::from).unwrap_or_else(fab_core::paths::run_dir);
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut names: Vec<String> = rd.filter_map(|e| e.ok()?.file_name().to_str()?.strip_suffix(".sock").map(str::to_string)).filter(|n| n != "serve").collect();
    names.sort();
    let mut out = vec![];
    for n in names {
        match tokio::time::timeout(Duration::from_secs(3), request(&n, None, &json!({"op": "info"}), |_| {})).await {
            Ok(Ok(v)) => out.push((n, v)),
            // Not answering: a leftover socket.
            Ok(Err(_)) => {
                let _ = std::fs::remove_file(socket(&n));
            }
            Err(_) => out.push((n, json!({"busy": true}))),
        }
    }
    out
}

/// Start options a remote client may not choose: which browser backend, and
/// its address and credentials, are the server's alone.
fn served(set: &[String]) -> Vec<String> {
    set.iter()
        .filter(|kv| {
            let k = kv.split_once('=').map_or(kv.as_str(), |(k, _)| k).trim();
            !(matches!(k, "backend" | "connect" | "browser") || k.starts_with("camofox"))
        })
        .cloned()
        .collect()
}

/// `fab serve`: starts session daemons for remote clients (`FAB_REMOTE=1`)
/// on request, each with this process's environment (backend, keys,
/// password manager). Never exits on its own; sessions still close when idle.
pub async fn supervise() -> Result<()> {
    run_dir()?;
    let ctl = serve_socket();
    if UnixStream::connect(&ctl).await.is_ok() {
        bail!("fab serve is already running ({})", ctl.display());
    }
    let _ = std::fs::remove_file(&ctl);
    let listener = UnixListener::bind(&ctl).with_context(|| format!("could not listen on {}", ctl.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ctl, std::fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("[{}] fab serve: listening on {} (pid {})", now(), ctl.display(), std::process::id());
    let starting: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = Default::default();
    use tokio::signal::unix::{SignalKind, signal};
    let (mut term, mut int, mut hup) = (signal(SignalKind::terminate())?, signal(SignalKind::interrupt())?, signal(SignalKind::hangup())?);
    loop {
        tokio::select! {
            r = listener.accept() => {
                if let Ok((stream, _)) = r {
                    let starting = starting.clone();
                    tokio::spawn(async move {
                        if let Err(e) = control(stream, starting).await {
                            eprintln!("[{}] fab serve: request failed: {e:#}", now());
                        }
                    });
                }
            }
            _ = term.recv() => break,
            _ = int.recv() => break,
            _ = hup.recv() => {}
        }
    }
    let _ = std::fs::remove_file(&ctl);
    for (name, _) in list().await {
        let _ = request(&name, None, &json!({"op": "close"}), |_| {}).await;
    }
    eprintln!("[{}] fab serve: stopped", now());
    Ok(())
}

/// One control request: `start` a session, or run `doctor` here.
async fn control(stream: UnixStream, starting: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let Some(line) = BufReader::new(rd).lines().next_line().await? else { return Ok(()) };
    let req: Value = serde_json::from_str(&line)?;
    match req["op"].as_str() {
        Some("start") => {
            let name = req["session"].as_str().unwrap_or_default().to_string();
            let start = Start {
                set: served(&req["set"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>()),
                model: req["model"].as_str().map(str::to_string),
                idle: req["idle"].as_u64().unwrap_or(3600),
            };
            let reply = match check_name(&name) {
                Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
                Ok(()) => {
                    // One start per session at a time; the rest find it running.
                    let lock = starting.lock().await.entry(name.clone()).or_default().clone();
                    let _one = lock.lock().await;
                    match start_local(&name, &start).await {
                        Ok((_, child)) => {
                            if let Some(mut c) = child {
                                eprintln!("[{}] fab serve: started session {name}", now());
                                std::thread::spawn(move || c.wait());
                            }
                            json!({"ok": true})
                        }
                        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
                    }
                }
            };
            wr.write_all(format!("{reply}\n").as_bytes()).await?;
        }
        // The server's own checks (backend, password manager), for a
        // remote client's `fab doctor`: its event lines, as they come.
        Some("doctor") => {
            let mut child = tokio::process::Command::new(std::env::current_exe()?)
                .arg("doctor")
                .env_remove("FAB_REMOTE")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let mut out = BufReader::new(child.stdout.take().context("doctor's output")?).lines();
            while let Some(l) = out.next_line().await? {
                wr.write_all(format!("{l}\n").as_bytes()).await?;
            }
            let _ = child.wait().await;
        }
        _ => wr.write_all(format!("{}\n", json!({"ok": true, "pid": std::process::id()})).as_bytes()).await?,
    }
    Ok(())
}

struct Shared {
    name: String,
    /// The browser's pages, once it has started.
    pool: tokio::sync::OnceCell<Arc<pool::Browser>>,
    cfg: pool::Config,
    /// A request holding a page longer than this is stopped.
    lease_limit: Duration,
    boot: tokio::sync::Mutex<Option<tokio::task::JoinHandle<Result<Session>>>>,
    ctx: api::Ctx,
    last: std::sync::Mutex<Instant>,
    busy: AtomicUsize,
    /// What `fab sessions` shows without waiting for a running command.
    info: std::sync::Mutex<Value>,
    start_set: Vec<String>,
    quit: tokio::sync::Notify,
    closed: std::sync::atomic::AtomicBool,
}

/// The daemon: owns the browser and serves requests until closed or idle.
pub async fn serve(name: &str, k: Knobs, start_set: Vec<String>, model: Option<String>, idle: Duration) -> Result<()> {
    check_name(name)?;
    let task_store = crate::task_runtime::store()?;
    let _session_guard = task_store.acquire_session(name)?;
    run_dir()?;
    socket_dir(name)?;
    let path = socket(name);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(_) => {
            if UnixStream::connect(&path).await.is_ok() {
                // Another daemon already serves this session.
                return Ok(());
            }
            let _ = std::fs::remove_file(&path);
            UnixListener::bind(&path)?
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let cfg = pool::Config::from_env();
    let lease_limit = Duration::from_secs(std::env::var("FAB_POOL_LEASE").ok().and_then(|v| v.parse().ok()).unwrap_or(900));
    eprintln!("[{}] session {name} (pid {}) starting · pages {}..{}, idle {} s", now(), std::process::id(), cfg.min, cfg.max, cfg.idle.as_secs());
    // Idle pages are closed on this tick.
    let every = (cfg.idle / 2).clamp(Duration::from_secs(1), Duration::from_secs(10));
    let sh = Arc::new(Shared {
        name: name.to_string(),
        pool: Default::default(),
        cfg,
        lease_limit,
        // The browser starts now, while the first command is on its way.
        boot: tokio::sync::Mutex::new(Some(tokio::spawn(Session::new(k)))),
        ctx: api::Ctx::new(model),
        last: std::sync::Mutex::new(Instant::now()),
        busy: AtomicUsize::new(0),
        info: std::sync::Mutex::new(json!({"pid": std::process::id()})),
        start_set,
        quit: Default::default(),
        closed: Default::default(),
    });
    use tokio::signal::unix::{SignalKind, signal};
    let (mut term, mut int, mut hup) = (signal(SignalKind::terminate())?, signal(SignalKind::interrupt())?, signal(SignalKind::hangup())?);
    let mut tick = tokio::time::interval(every);
    loop {
        tokio::select! {
            r = listener.accept() => {
                if let Ok((stream, _)) = r {
                    let sh = sh.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle(sh, stream).await {
                            eprintln!("[{}] request failed: {e:#}", now());
                        }
                    });
                }
            }
            _ = tick.tick() => {
                if sh.busy.load(Ordering::SeqCst) == 0 && sh.last.lock().unwrap().elapsed() > idle {
                    eprintln!("[{}] idle for {} s: closing", now(), idle.as_secs());
                    break;
                }
                if let Some(p) = sh.pool.get() {
                    let p = p.clone();
                    tokio::spawn(async move {
                        let n = p.reap().await;
                        if n > 0 {
                            eprintln!("[{}] closed {n} idle page{} ({} open)", now(), if n == 1 { "" } else { "s" }, p.stats()["pages"]);
                        }
                    });
                }
            }
            _ = sh.quit.notified() => break,
            _ = term.recv() => break,
            _ = int.recv() => break,
            // Outlive the terminal that started it.
            _ = hup.recv() => {}
        }
    }
    shutdown(&sh).await;
    Ok(())
}

fn now() -> String {
    let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{:02}:{:02}:{:02}Z", s / 3600 % 24, s / 60 % 60, s % 60)
}

/// Closes the browser (gracefully, so a persistent profile keeps its
/// cookies) and removes the socket. Idempotent.
async fn shutdown(sh: &Shared) {
    if sh.closed.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Err(e) = crate::task_runtime::interrupt_scope(&sh.name) { tracing::warn!("could not interrupt task journal: {e:#}"); }
    // Nothing new connects to a closing session.
    let _ = std::fs::remove_file(socket(&sh.name));
    // Started, or starting: take the browser either way.
    let boot = sh.boot.lock().await.take();
    if let Some(h) = boot
        && let Ok(Ok(mut s)) = h.await
    {
        s.flush_shapes();
        s.close().await;
    }
    if let Some(p) = sh.pool.get() {
        // Running commands get a moment to finish.
        p.shutdown(Duration::from_secs(10)).await;
    }
    eprintln!("[{}] session {} closed", now(), sh.name);
}

async fn write(wr: &tokio::sync::Mutex<OwnedWriteHalf>, v: &Value) -> Result<()> {
    wr.lock().await.write_all(format!("{v}\n").as_bytes()).await?;
    Ok(())
}

async fn handle(sh: Arc<Shared>, stream: UnixStream) -> Result<()> {
    let (rd, wr) = stream.into_split();
    let wr = Arc::new(tokio::sync::Mutex::new(wr));
    let mut lines = BufReader::new(rd).lines();
    let Some(line) = lines.next_line().await? else { return Ok(()) };
    let req: Value = serde_json::from_str(&line)?;
    let reply = match req["op"].as_str().unwrap_or("call") {
        "info" => {
            let mut v = sh.info.lock().unwrap().clone();
            v["busy"] = json!(sh.busy.load(Ordering::SeqCst) > 0);
            if let Some(p) = sh.pool.get() {
                v["pool"] = p.stats();
            }
            v["idle_s"] = json!(sh.last.lock().unwrap().elapsed().as_secs());
            v
        }
        "close" => {
            shutdown(&sh).await;
            sh.quit.notify_one();
            json!({"ok": true, "text": format!("closed session {}", sh.name)})
        }
        _ => {
            sh.busy.fetch_add(1, Ordering::SeqCst);
            let r = call(&sh, &req, wr.clone()).await;
            *sh.last.lock().unwrap() = Instant::now();
            sh.busy.fetch_sub(1, Ordering::SeqCst);
            r
        }
    };
    write(&wr, &reply).await
}

/// The browser's pages, starting the pool when the browser is up.
async fn pages(sh: &Arc<Shared>) -> Result<Arc<pool::Browser>, String> {
    let r = sh
        .pool
        .get_or_try_init(|| async {
            let boot = sh.boot.lock().await.take();
            let Some(h) = boot else { return Err("the session is closing".to_string()) };
            let s = match h.await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return Err(format!("could not start the browser: {e:#}")),
                Err(e) => return Err(format!("could not start the browser: {e}")),
            };
            {
                let mut info = sh.info.lock().unwrap();
                info["browser"] = json!(s.browser.describe());
                // How the browser started (a profile in use, a re-used
                // window): told with the first reply.
                info["notes"] = json!(s.browser.notes());
                info["notes_told"] = json!(false);
            }
            let p = pool::around(s, sh.cfg.clone());
            let warm = p.clone();
            tokio::spawn(async move { warm.warm().await });
            Ok(p)
        })
        .await;
    match r {
        Ok(p) => Ok(p.clone()),
        Err(e) => {
            sh.quit.notify_one();
            Err(e)
        }
    }
}

/// The final line of a call: its `end` event, and the human report (`-v`).
fn finish(reply: &crate::api::Reply) -> Value {
    json!({"end": reply.end(), "report": reply.text})
}

fn failed(failure: crate::events::Failure) -> Value {
    finish(&crate::api::Reply::failure(failure))
}

async fn call(sh: &Arc<Shared>, req: &Value, wr: Arc<tokio::sync::Mutex<OwnedWriteHalf>>) -> Value {
    use crate::events::{ErrorCode, Failure};
    if sh.closed.load(Ordering::SeqCst) {
        return failed(Failure::new(ErrorCode::Interrupted, "the session is closing"));
    }
    let tool_name = req["tool"].as_str().unwrap_or_default();
    let resume = if tool_name == "tasks" {
        let command = serde_json::from_value::<crate::task_commands::TaskCommand>(req["args"].clone());
        let outcome = match command {
            Ok(command) => crate::task_runtime::control(&sh.name, command).await,
            Err(e) => Err(e.into()),
        };
        match outcome {
            Ok(crate::task_commands::TaskCommandOutcome::Resume { record, adopt_page }) => Some((record, adopt_page)),
            Ok(crate::task_commands::TaskCommandOutcome::Observe { record, effect, reason }) => {
                let pool = match pages(sh).await {
                    Ok(p) => p,
                    Err(e) => return failed(Failure::new(ErrorCode::Internal, e)),
                };
                let client = req["client"].as_str().unwrap_or_default();
                return match crate::task_runtime::observe(&pool, client, &record.id, &effect, &reason).await.and_then(|t| Ok(serde_json::to_value(t)?)) {
                    Ok(task) => json!({"end": crate::events::End::ok(task), "report": ""}),
                    Err(e) => failed(Failure::of(&e)),
                };
            }
            Ok(result) => match result.end() {
                Ok(Some(end)) => return json!({"end": end, "report": ""}),
                Ok(None) => return json!({"items": [result], "end": crate::events::End::ok(Value::Null), "report": ""}),
                Err(e) => return failed(Failure::of(&e)),
            },
            Err(e) => return failed(Failure::of(&e)),
        }
    } else {
        if let Err(failure) = crate::task_runtime::validate(tool_name, &req["args"]) {
            return failed(failure);
        }
        None
    };
    let pool = match pages(sh).await {
        Ok(p) => p,
        Err(e) => return failed(Failure::new(ErrorCode::Internal, e)),
    };
    let tool = req["tool"].as_str().unwrap_or_default().to_string();
    // The task's `start` and its records stream to the caller as events
    // before the `end`; progress lines too, when the caller is verbose.
    let verbose = req["verbose"].as_bool() == Some(true);
    let (live, fwd) = {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let wr2 = wr.clone();
        let fwd = tokio::spawn(async move {
            // `start` waits for the records' schema: it is sent when the
            // schema is known, before the first record, or at the end.
            let mut start: Option<Value> = None;
            while let Some(l) = rx.recv().await {
                let msg = if let Some(task) = l.strip_prefix(crate::api::START) {
                    start = Some(json!({"event": {"t": "start", "v": crate::events::VERSION, "ts": crate::events::now(), "cmd": "", "task": task}}));
                    continue;
                } else if let Some(schema) = l.strip_prefix(crate::api::SCHEMA) {
                    let Some(mut s) = start.take() else { continue };
                    if let Ok(schema) = serde_json::from_str::<Value>(schema) {
                        s["event"]["schema"] = schema;
                    }
                    s
                } else if let Some(record) = l.strip_prefix(crate::api::RECORD) {
                    match serde_json::from_str::<Value>(record) {
                        Ok(record) => {
                            if let Some(s) = start.take() {
                                let _ = write(&wr2, &s).await;
                            }
                            json!({"event": record})
                        }
                        Err(_) => continue,
                    }
                } else if verbose {
                    json!({"log": l})
                } else {
                    continue;
                };
                let _ = write(&wr2, &msg).await;
            }
            if let Some(s) = start {
                let _ = write(&wr2, &s).await;
            }
        });
        let f: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |l: String| {
            let _ = tx.send(l);
        });
        (Some(f), Some(fwd))
    };
    // Requests from one client (a shell, an agent's FAB_CLIENT) continue on the page
    // its last one left.
    let client = req["client"].as_str().unwrap_or_default();
    let result = match resume {
        Some((record, adopt_page)) => crate::task_runtime::resume(record, adopt_page, &pool, &sh.ctx, client, live, sh.lease_limit).await,
        None => crate::task_runtime::call(&pool, &sh.ctx, &sh.name, client, &tool, &req["args"], live, sh.lease_limit).await,
    };
    let done = match result {
        Ok(result) => result.done,
        Err(e) => {
            if let Some(f) = fwd { let _ = f.await; }
            return failed(Failure::of(&e));
        }
    };
    if let Some(f) = fwd {
        let _ = f.await;
    }
    let mut r = done.reply;
    let mut text = std::mem::take(&mut r.text);
    {
        let mut info = sh.info.lock().unwrap();
        if info["notes_told"] == json!(false) {
            info["notes_told"] = json!(true);
            for n in info["notes"].as_array().into_iter().flatten().filter_map(|n| n.as_str()) {
                text.push_str(&format!("\n[browser] {n}"));
            }
        }
    }
    let want: Vec<String> = req["start"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect();
    if !want.is_empty() && want != sh.start_set {
        text.push_str(&format!("\n[session] \"{}\" was started with other options; they apply after `fab -s {} close`", sh.name, sh.name));
    }
    if let Some((url, title)) = done.home {
        let mut info = sh.info.lock().unwrap();
        info["url"] = json!(url);
        info["title"] = json!(title);
    }
    r.text = text;
    finish(&r)
}

#[cfg(test)]
mod serve_tests {
    use super::*;

    #[test]
    fn a_remote_client_cannot_choose_the_backend_or_its_credentials() {
        let set: Vec<String> = ["engine=dvm", "profile=a", "backend=cdp", "camofox_url=http://evil", "camofox_user=x", "connect=9222", "browser=chrome", " backend = cdp"].map(String::from).to_vec();
        assert_eq!(served(&set), ["engine=dvm", "profile=a"]);
    }

    #[test]
    fn serve_is_not_a_session_name() {
        assert!(check_name("serve").is_err());
        assert!(check_name("server").is_ok());
    }
}
