//! WebDriver BiDi backend: drives Firefox over the W3C protocol. It mirrors
//! the CDP backend: a profile fab owns, the snapshot script preloaded into
//! every document, trusted input, popups and dialogs.

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::cdp::{Dialogs, Launch, rand_suffix, repair_surrogates};
use super::{Point, SNAPSHOT_JS};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

#[derive(Debug, Clone)]
struct Event {
    method: String,
    params: Value,
}

/// The session websocket.
struct Conn {
    tx: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next: AtomicU64,
    events: broadcast::Sender<Event>,
}

impl Conn {
    async fn open(ws_url: &str) -> Result<Arc<Self>> {
        let cfg = WebSocketConfig::default().max_message_size(Some(512 << 20)).max_frame_size(Some(512 << 20));
        let (ws, _) = tokio_tungstenite::connect_async_with_config(ws_url, Some(cfg), true).await.with_context(|| format!("connect {ws_url}"))?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let pending: Pending = Arc::default();
        let (events, _) = broadcast::channel(4096);
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
        });
        let p2 = pending.clone();
        let ev2 = events.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(txt) = msg else { continue };
                let v = match serde_json::from_str::<Value>(&txt) {
                    Ok(v) => v,
                    Err(_) => match serde_json::from_str::<Value>(&repair_surrogates(&txt)) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("bidi: unparseable message ({e})");
                            continue;
                        }
                    },
                };
                if v["type"] == "event" {
                    let _ = ev2.send(Event { method: v["method"].as_str().unwrap_or_default().to_string(), params: v["params"].clone() });
                } else if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = p2.lock().await.remove(&id) {
                        let r = if v["type"] == "error" {
                            Err(anyhow!("{}: {}", v["error"].as_str().unwrap_or("error"), v["message"].as_str().unwrap_or_default()))
                        } else {
                            Ok(v.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = tx.send(r);
                    }
                }
            }
            for (_, tx) in p2.lock().await.drain() {
                let _ = tx.send(Err(anyhow!("bidi connection closed")));
            }
        });
        Ok(Arc::new(Self { tx, pending, next: AtomicU64::new(1), events }))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let msg = json!({"id": id, "method": method, "params": params});
        self.tx.send(Message::Text(msg.to_string().into())).map_err(|_| anyhow!("bidi closed"))?;
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(r) => r.map_err(|_| anyhow!("bidi dropped reply"))?,
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("bidi {method}: no reply in 30 s (browser gone?)")
            }
        }
    }

    fn send(&self, method: &str, params: Value) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"id": id, "method": method, "params": params});
        let _ = self.tx.send(Message::Text(msg.to_string().into()));
    }
}

/// Preferences for a profile fab drives: no first-run pages, update checks,
/// telemetry or slow-script prompts; focus works in background windows.
const PREFS: &[(&str, &str)] = &[
    ("browser.shell.checkDefaultBrowser", "false"),
    ("browser.startup.homepage_override.mstone", "\"ignore\""),
    ("browser.startup.page", "0"),
    ("browser.startup.homepage", "\"about:blank\""),
    ("startup.homepage_welcome_url", "\"about:blank\""),
    ("startup.homepage_welcome_url.additional", "\"\""),
    ("browser.aboutwelcome.enabled", "false"),
    ("browser.newtabpage.enabled", "false"),
    ("browser.uitour.enabled", "false"),
    ("datareporting.policy.dataSubmissionEnabled", "false"),
    ("datareporting.healthreport.uploadEnabled", "false"),
    ("toolkit.telemetry.reportingpolicy.firstRun", "false"),
    ("browser.tabs.warnOnClose", "false"),
    ("browser.tabs.warnOnCloseOtherTabs", "false"),
    ("browser.sessionstore.resume_from_crash", "false"),
    ("app.update.disabledForTesting", "true"),
    ("app.update.checkInstallTime", "false"),
    ("extensions.update.enabled", "false"),
    ("browser.translations.automaticallyPopup", "false"),
    ("dom.disable_open_during_load", "false"),
    ("dom.max_script_run_time", "0"),
    ("focusmanager.testmode", "true"),
    // Pool pages are background tabs: their timers (settle polls with them)
    // run at full rate, as Chrome's --disable-background-timer-throttling.
    ("dom.min_background_timeout_value", "4"),
    ("dom.min_background_timeout_value_without_budget_throttling", "4"),
    ("dom.timeout.enable_budget_timer_throttling", "false"),
    ("network.captive-portal-service.enabled", "false"),
    ("network.connectivity-service.enabled", "false"),
];

/// A Firefox process on a profile fab owns; killed on drop.
struct Firefox {
    child: tokio::process::Child,
    profile: PathBuf,
    temp: bool,
}

impl Firefox {
    async fn start(o: &Launch) -> Result<(Self, u16)> {
        let (profile, temp) = match &o.profile {
            Some(p) => (p.clone(), false),
            None => (std::env::temp_dir().join(format!("fab-firefox-{}-{}", std::process::id(), rand_suffix())), true),
        };
        std::fs::create_dir_all(&profile)?;
        let prefs: String = PREFS.iter().map(|(k, v)| format!("user_pref(\"{k}\", {v});\n")).collect();
        std::fs::write(profile.join("user.js"), prefs)?;
        let mut cmd = tokio::process::Command::new(&o.bin);
        cmd.args(["--remote-debugging-port", "0", "--no-remote", "--new-instance", "--profile"]).arg(&profile);
        if o.headless {
            cmd.arg("--headless");
        }
        cmd.arg("about:blank").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("launch {}", o.bin.display()))?;
        let mut lines = BufReader::new(child.stderr.take().context("no stderr")?).lines();
        // Firefox says where BiDi listens on stderr.
        let find = async {
            while let Some(l) = lines.next_line().await? {
                if let Some(rest) = l.split("WebDriver BiDi listening on ws://").nth(1) {
                    let port = rest.trim().rsplit(':').next().and_then(|p| p.trim_end_matches('/').parse::<u16>().ok());
                    if let Some(p) = port {
                        return Ok(p);
                    }
                }
            }
            bail!("Firefox exited before opening WebDriver BiDi")
        };
        let port = tokio::time::timeout(Duration::from_secs(30), find).await.context("timed out waiting for Firefox to open WebDriver BiDi")??;
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        Ok((Self { child, profile, temp }, port))
    }
}

impl Drop for Firefox {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        if self.temp {
            let p = self.profile.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(800));
                let _ = std::fs::remove_dir_all(p);
            });
        }
    }
}

/// A BiDi remote value as plain JSON.
fn remote_value(v: &Value) -> Value {
    let val = &v["value"];
    match v["type"].as_str().unwrap_or("undefined") {
        "undefined" | "null" => Value::Null,
        "string" | "boolean" => val.clone(),
        // NaN, Infinity and -0 arrive as strings.
        "number" => val.clone(),
        "bigint" | "date" => val.clone(),
        "regexp" => json!(format!("/{}/{}", val["pattern"].as_str().unwrap_or_default(), val["flags"].as_str().unwrap_or_default())),
        "array" | "set" | "nodelist" | "htmlcollection" => Value::Array(val.as_array().into_iter().flatten().map(remote_value).collect()),
        "object" | "map" => {
            let mut m = serde_json::Map::new();
            for pair in val.as_array().into_iter().flatten() {
                let key = match &pair[0] {
                    Value::String(k) => k.clone(),
                    k => remote_value(k).to_string(),
                };
                m.insert(key, remote_value(&pair[1]));
            }
            Value::Object(m)
        }
        "node" => json!(format!("<{}>", val["localName"].as_str().unwrap_or("node"))),
        other => json!(format!("[{other}]")),
    }
}

/// WebDriver key values for named keys.
fn key_value(k: &str) -> Option<String> {
    let v = match k.to_ascii_lowercase().as_str() {
        "enter" | "return" => "\u{E007}",
        "tab" => "\u{E004}",
        "escape" | "esc" => "\u{E00C}",
        "backspace" => "\u{E003}",
        "delete" | "del" => "\u{E017}",
        "space" | " " => " ",
        "arrowleft" | "left" => "\u{E012}",
        "arrowup" | "up" => "\u{E013}",
        "arrowright" | "right" => "\u{E014}",
        "arrowdown" | "down" => "\u{E015}",
        "home" => "\u{E011}",
        "end" => "\u{E010}",
        "pageup" => "\u{E00E}",
        "pagedown" => "\u{E00F}",
        "insert" => "\u{E016}",
        "shift" => "\u{E008}",
        "control" | "ctrl" => "\u{E009}",
        "alt" | "option" => "\u{E00A}",
        "meta" | "cmd" | "command" | "super" => "\u{E03D}",
        "controlormeta" | "mod" => {
            if cfg!(target_os = "macos") {
                "\u{E03D}"
            } else {
                "\u{E009}"
            }
        }
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)) => {
            let n: u32 = f[1..].parse().ok()?;
            return char::from_u32(0xE030 + n).map(String::from);
        }
        _ => {
            let mut cs = k.chars();
            let c = cs.next()?;
            return cs.next().is_none().then(|| c.to_string());
        }
    };
    Some(v.to_string())
}

/// The browser behind every view of it: the one WebDriver session (Firefox
/// allows one per browser), the process, and tab bookkeeping.
#[derive(Clone)]
pub struct Shared {
    conn: Arc<Conn>,
    ff: Arc<std::sync::Mutex<Option<Firefox>>>,
    /// Re-used a Firefox that was already running on the profile.
    attached: Arc<std::sync::atomic::AtomicBool>,
    popups: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    gone: Arc<std::sync::Mutex<HashSet<String>>>,
    openers: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Tabs that belong to pool pages: the main view never moves onto one.
    claimed: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl Shared {
    /// Closes Firefox, letting it save a persistent profile first.
    pub async fn shutdown(&self) {
        // A re-used Firefox is fab's (it runs on fab's profile): close it too,
        // or it would hold the profile after this session ends.
        if self.attached.load(Ordering::Relaxed) {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.conn.call("browser.close", json!({}))).await;
            return;
        }
        let ff = self.ff.lock().unwrap().take();
        if let Some(mut ff) = ff {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.conn.call("browser.close", json!({}))).await;
            let _ = tokio::time::timeout(Duration::from_secs(4), ff.child.wait()).await;
            drop(ff);
        }
    }

    async fn create(&self, background: bool) -> Result<String> {
        let r = self.conn.call("browsingContext.create", json!({"type": "tab", "background": background})).await?;
        r["context"].as_str().map(str::to_string).context("browsingContext.create: no context")
    }

    async fn close_context(&self, c: &str) {
        self.claimed.lock().unwrap().remove(c);
        let _ = tokio::time::timeout(Duration::from_secs(2), self.conn.call("browsingContext.close", json!({"context": c}))).await;
    }
}

/// Firefox driven over WebDriver BiDi: the current tab plus the others. The
/// main view owns the browser; pool pages ([`Bidi::page`]) are views of their
/// own tabs in it.
pub struct Bidi {
    sh: Shared,
    cur: std::sync::Mutex<String>,
    /// A pool page: closing it closes only its own tabs.
    page: bool,
    /// Tabs a pool page opened or followed.
    own: std::sync::Mutex<Vec<String>>,
    pub about: String,
    pub notes: Vec<String>,
    pub product: String,
}

/// A Firefox already running on `profile` (it holds the profile's lock).
pub enum Running {
    /// Listening for WebDriver BiDi on this port (a Firefox fab started).
    Bidi(u16),
    /// Running without remote control (opened by hand): fab can't drive it.
    Plain,
}

/// Whether a Firefox is running on `profile`, and how to reach it.
pub fn running(profile: &std::path::Path) -> Option<Running> {
    if !profile_locked(profile) {
        return None;
    }
    // Firefox writes where BiDi listens into the profile while it's on.
    let port = std::fs::read_to_string(profile.join("WebDriverBiDiServer.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["ws_port"].as_u64())
        .and_then(|p| u16::try_from(p).ok());
    Some(match port {
        Some(p) if std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], p).into(), Duration::from_millis(300)).is_ok() => Running::Bidi(p),
        _ => Running::Plain,
    })
}

/// Firefox holds `.parentlock` (fcntl) or the `lock` symlink while running.
fn profile_locked(profile: &std::path::Path) -> bool {
    if super::cdp::profile_in_use(profile) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(profile.join(".parentlock")) else { return false };
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as _;
        fl.l_whence = libc::SEEK_SET as _;
        // F_GETLK: who would block a write lock (none: F_UNLCK).
        let r = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETLK, &mut fl) };
        r == 0 && i64::from(fl.l_type) != i64::from(libc::F_UNLCK)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

impl Bidi {
    pub async fn start(o: &Launch, dialogs: Dialogs) -> Result<Self> {
        let (ff, port) = Firefox::start(o).await?;
        Self::open(port, Some(ff), dialogs).await
    }

    /// Re-uses the Firefox already running on a profile (a fab Firefox whose
    /// driver is gone). Fails when another program is driving it: Firefox
    /// allows one WebDriver session at a time.
    pub async fn attach(port: u16, dialogs: Dialogs) -> Result<Self> {
        let b = Self::open(port, None, dialogs).await?;
        b.sh.attached.store(true, Ordering::Relaxed);
        Ok(b)
    }

    async fn open(port: u16, ff: Option<Firefox>, dialogs: Dialogs) -> Result<Self> {
        let conn = Conn::open(&format!("ws://127.0.0.1:{port}/session")).await?;
        let caps = json!({"capabilities": {"alwaysMatch": {"unhandledPromptBehavior": {"default": "ignore"}}}});
        let s = conn.call("session.new", caps).await?;
        let product = format!("Firefox/{}", s["capabilities"]["browserVersion"].as_str().unwrap_or("?"));
        let popups: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
        let gone: Arc<std::sync::Mutex<HashSet<String>>> = Arc::default();
        listen(&conn, popups.clone(), gone.clone(), dialogs);
        let events = ["browsingContext.contextCreated", "browsingContext.contextDestroyed", "browsingContext.userPromptOpened"];
        conn.call("session.subscribe", json!({"events": events})).await?;
        conn.call("script.addPreloadScript", json!({"functionDeclaration": format!("() => {{\n{SNAPSHOT_JS}\n}}")})).await?;
        let tree = conn.call("browsingContext.getTree", json!({"maxDepth": 0})).await?;
        let ctx = tree["contexts"][0]["context"].as_str().context("Firefox has no window")?.to_string();
        let sh = Shared { conn, ff: Arc::new(std::sync::Mutex::new(ff)), attached: Default::default(), popups, gone, openers: Default::default(), claimed: Default::default() };
        let b = Self {
            sh,
            cur: std::sync::Mutex::new(ctx.clone()),
            page: false,
            own: Default::default(),
            about: String::new(),
            notes: vec![],
            product,
        };
        b.ensure_script(&ctx).await;
        Ok(b)
    }

    /// The browser, to open more pages in it or close it.
    pub fn shared(&self) -> Shared {
        self.sh.clone()
    }

    /// A new pool page: a background tab of its own in the same browser.
    pub async fn page(sh: &Shared, product: String) -> Result<Self> {
        let ctx = sh.create(true).await?;
        sh.claimed.lock().unwrap().insert(ctx.clone());
        let b = Self { sh: sh.clone(), cur: std::sync::Mutex::new(ctx.clone()), page: true, own: std::sync::Mutex::new(vec![ctx.clone()]), about: String::new(), notes: vec![], product };
        b.ensure_script(&ctx).await;
        Ok(b)
    }

    pub(super) fn ctx(&self) -> String {
        self.cur.lock().unwrap().clone()
    }

    /// Opens a blank tab that belongs to this view.
    async fn open_tab(&self) -> Result<String> {
        let c = self.sh.create(self.page).await?;
        if self.page {
            self.sh.claimed.lock().unwrap().insert(c.clone());
            self.own.lock().unwrap().push(c.clone());
        }
        self.ensure_script(&c).await;
        Ok(c)
    }

    /// Moves this view to a fresh tab and closes the one it was on (crashed,
    /// hung or closed): the page starts over, blank.
    pub async fn renew(&self) -> Result<()> {
        let old = self.ctx();
        let c = self.open_tab().await?;
        *self.cur.lock().unwrap() = c;
        self.own.lock().unwrap().retain(|o| *o != old);
        self.sh.close_context(&old).await;
        Ok(())
    }

    /// A lost page must be explicitly renewed by its owner.
    async fn live(&self) -> Result<String> {
        let c = self.ctx();
        if self.sh.gone.lock().unwrap().contains(&c) {
            return Err(super::driver::PageLost(c).into());
        }
        Ok(c)
    }

    async fn ensure_script(&self, ctx: &str) {
        if self.eval_in(ctx, "typeof __ub").await.ok().as_ref().and_then(Value::as_str) == Some("undefined") {
            let _ = self.sh.conn.call("script.evaluate", json!({"expression": SNAPSHOT_JS, "target": {"context": ctx}, "awaitPromise": false})).await;
        }
    }

    async fn eval_in(&self, ctx: &str, expr: &str) -> Result<Value> {
        // Expressions (all of fab's own calls) come back as one JSON string:
        // much less to serialize than BiDi's typed values.
        let wrapped = format!("Promise.resolve((\n{expr}\n)).then(v => JSON.stringify(v === undefined ? null : v))");
        let args = json!({"expression": wrapped, "target": {"context": ctx}, "awaitPromise": true, "resultOwnership": "none"});
        let r = self.sh.conn.call("script.evaluate", args).await?;
        if r["type"] == "exception" {
            let d = &r["exceptionDetails"];
            let text = d["text"].as_str().or(d["exception"]["value"].as_str()).unwrap_or("exception").to_string();
            // Several statements (`a = 1; b`) are a script, not an expression:
            // run it as one and take its completion value.
            if text.contains("SyntaxError") {
                let args = json!({"expression": expr, "target": {"context": ctx}, "awaitPromise": true, "resultOwnership": "none", "serializationOptions": {"maxObjectDepth": 20, "maxDomDepth": 0}});
                let r = self.sh.conn.call("script.evaluate", args).await?;
                if r["type"] == "exception" {
                    let d = &r["exceptionDetails"];
                    bail!("js: {}", d["text"].as_str().or(d["exception"]["value"].as_str()).unwrap_or("exception"));
                }
                return Ok(remote_value(&r["result"]));
            }
            bail!("js: {text}");
        }
        match r["result"]["value"].as_str() {
            Some(s) => Ok(serde_json::from_str(s).or_else(|_| serde_json::from_str(&repair_surrogates(s)))?),
            None => Ok(Value::Null),
        }
    }

    pub async fn eval(&self, expr: &str) -> Result<Value> {
        let ctx = self.live().await?;
        self.eval_in(&ctx, expr).await
    }

    /// Waits for the document to finish loading: at most 5 s after it is interactive.
    async fn loaded(&self, ctx: &str) {
        for _ in 0..100 {
            if self.eval_in(ctx, "document.readyState").await.ok().as_ref().and_then(Value::as_str) == Some("complete") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn goto(&self, url: &str) -> Result<()> {
        let ctx = self.live().await?;
        self.sh.conn
            .call("browsingContext.navigate", json!({"context": ctx, "url": url, "wait": "interactive"}))
            .await
            .map_err(|e| anyhow!("navigate {url}: {e}"))?;
        self.loaded(&ctx).await;
        Ok(())
    }

    async fn actions(&self, actions: Value) -> Result<()> {
        let ctx = self.live().await?;
        self.sh.conn.call("input.performActions", json!({"context": ctx, "actions": actions})).await?;
        Ok(())
    }

    fn mouse(steps: Value) -> Value {
        json!([{"type": "pointer", "id": "fab-mouse", "parameters": {"pointerType": "mouse"}, "actions": steps}])
    }

    pub async fn click_at(&self, p: Point) -> Result<()> {
        let (x, y) = (p.x.round() as i64, p.y.round() as i64);
        self.actions(Self::mouse(json!([
            {"type": "pointerMove", "x": x, "y": y},
            {"type": "pointerDown", "button": 0},
            {"type": "pointerUp", "button": 0}
        ])))
        .await
    }

    pub async fn insert_text(&self, text: &str) -> Result<()> {
        let keys: Vec<Value> = text
            .chars()
            .flat_map(|c| [json!({"type": "keyDown", "value": c.to_string()}), json!({"type": "keyUp", "value": c.to_string()})])
            .collect();
        self.actions(json!([{"type": "key", "id": "fab-keyboard", "actions": keys}])).await
    }

    pub async fn press(&self, chord: &str) -> Result<()> {
        let parts: Vec<&str> = if chord == "+" { vec!["+"] } else { chord.split('+').map(str::trim).filter(|s| !s.is_empty()).collect() };
        let Some((key, mods)) = parts.split_last() else { bail!("no key given") };
        let mut keys = vec![];
        let mut held = vec![];
        for m in mods {
            let v = key_value(m).filter(|v| v.chars().count() == 1 && ('\u{E008}'..='\u{E03D}').contains(&v.chars().next().unwrap()));
            let v = v.with_context(|| format!("unknown modifier {m} (Alt, Control, Meta, Shift, ControlOrMeta)"))?;
            keys.push(json!({"type": "keyDown", "value": v}));
            held.push(v);
        }
        let v = key_value(key).with_context(|| format!("unknown key {key}"))?;
        keys.push(json!({"type": "keyDown", "value": v}));
        keys.push(json!({"type": "keyUp", "value": v}));
        for v in held.into_iter().rev() {
            keys.push(json!({"type": "keyUp", "value": v}));
        }
        self.actions(json!([{"type": "key", "id": "fab-keyboard", "actions": keys}])).await
    }

    pub async fn press_enter(&self) -> Result<()> {
        self.press("Enter").await
    }

    /// Switches to a tab the current one just opened. Returns whether it switched.
    pub async fn follow_popup(&self) -> Result<bool> {
        let cur = self.ctx();
        let found = {
            let mut p = self.sh.popups.lock().unwrap();
            let gone = self.sh.gone.lock().unwrap();
            let pick = p.iter().rev().find(|(c, o)| *o == cur && !gone.contains(c)).map(|(c, _)| c.clone());
            p.retain(|(_, o)| *o != cur);
            pick
        };
        let Some(c) = found else { return Ok(false) };
        if self.page {
            self.sh.claimed.lock().unwrap().insert(c.clone());
            self.own.lock().unwrap().push(c.clone());
        }
        self.sh.openers.lock().unwrap().insert(c.clone(), cur);
        *self.cur.lock().unwrap() = c.clone();
        for _ in 0..200 {
            if let Ok(v) = self.eval_in(&c, "[location.href, document.readyState]").await {
                if v[0] != "about:blank" && v[1] != "loading" {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.ensure_script(&c).await;
        Ok(true)
    }

    /// Closes Firefox (see [`Shared::shutdown`]); a pool page closes only
    /// its own tabs.
    pub async fn shutdown(&self) {
        if !self.page {
            return self.sh.shutdown().await;
        }
        let own: Vec<String> = std::mem::take(&mut *self.own.lock().unwrap());
        for c in own {
            self.sh.close_context(&c).await;
        }
    }
}

/// Answers prompts at once and tracks popups and closed tabs.
fn listen(
    conn: &Arc<Conn>,
    popups: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    gone: Arc<std::sync::Mutex<HashSet<String>>>,
    dialogs: Dialogs,
) {
    let mut rx = conn.events.subscribe();
    let conn = Arc::downgrade(conn);
    tokio::spawn(async move {
        loop {
            let e = match rx.recv().await {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            };
            let p = &e.params;
            match e.method.as_str() {
                "browsingContext.userPromptOpened" => {
                    let kind = p["type"].as_str().unwrap_or("alert");
                    let accept = dialogs == Dialogs::Accept || matches!(kind, "alert" | "beforeunload");
                    if let Some(c) = conn.upgrade() {
                        let mut args = json!({"context": p["context"], "accept": accept});
                        if kind == "prompt" && accept {
                            if let Some(d) = p["defaultValue"].as_str() {
                                args["userText"] = json!(d);
                            }
                        }
                        c.send("browsingContext.handleUserPrompt", args);
                    }
                }
                "browsingContext.contextCreated" => {
                    if let (Some(c), Some(o), true) = (p["context"].as_str(), p["originalOpener"].as_str(), p["parent"].is_null()) {
                        popups.lock().unwrap().push((c.to_string(), o.to_string()));
                    }
                }
                "browsingContext.contextDestroyed" => {
                    if let Some(c) = p["context"].as_str() {
                        gone.lock().unwrap().insert(c.to_string());
                    }
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_values() {
        let v = json!({"type": "object", "value": [["a", {"type": "number", "value": 1}], ["b", {"type": "array", "value": [{"type": "string", "value": "x"}, {"type": "null"}]}]]});
        assert_eq!(remote_value(&v), json!({"a": 1, "b": ["x", null]}));
        assert_eq!(remote_value(&json!({"type": "undefined"})), Value::Null);
    }

    #[test]
    fn keys() {
        assert_eq!(key_value("Enter").as_deref(), Some("\u{E007}"));
        assert_eq!(key_value("a").as_deref(), Some("a"));
        assert_eq!(key_value("F1").as_deref(), Some("\u{E031}"));
        assert!(key_value("Nope").is_none());
    }
}
