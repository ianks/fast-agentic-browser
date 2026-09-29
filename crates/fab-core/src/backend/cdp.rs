//! Chrome DevTools Protocol backend. One websocket to the browser endpoint;
//! each tab is a flattened session on it. fab either launches a
//! Chromium-family browser (see `discover`) or attaches to one that is
//! already running with remote debugging on.

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::{Point, SNAPSHOT_JS};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// A protocol event, with the session (tab) it came from.
#[derive(Debug, Clone)]
pub struct Event {
    pub session: Option<String>,
    pub method: String,
    pub params: Value,
}

/// One websocket to the browser endpoint, shared by every tab.
pub struct Conn {
    tx: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next: AtomicU64,
    pub events: broadcast::Sender<Event>,
}

impl Conn {
    pub async fn open(ws_url: &str) -> Result<Arc<Self>> {
        // Full-page screenshots arrive as one large message.
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
            loop {
                let msg = match stream.next().await {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        // A browser that was closed resets the socket: expected.
                        let msg = e.to_string();
                        if msg.contains("reset without closing handshake") {
                            tracing::debug!("cdp websocket closed: {msg}");
                        } else {
                            tracing::warn!("cdp websocket read failed: {msg}");
                        }
                        break;
                    }
                    None => break,
                };
                let Message::Text(txt) = msg else { continue };
                // Pages can put lone UTF-16 surrogates into strings, which
                // Chrome escapes as-is and serde_json rejects: repair them
                // rather than drop the reply (which left its caller waiting).
                let v = match serde_json::from_str::<Value>(&txt) {
                    Ok(v) => v,
                    Err(_) => match serde_json::from_str::<Value>(&repair_surrogates(&txt)) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("cdp: unparseable message ({e}): {}", txt.chars().take(120).collect::<String>());
                            continue;
                        }
                    },
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = p2.lock().await.remove(&id) {
                        let r = match v.get("error") {
                            Some(e) => Err(anyhow!("cdp: {}", e)),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(r);
                    }
                } else if let Some(m) = v.get("method").and_then(Value::as_str) {
                    let session = v.get("sessionId").and_then(Value::as_str).map(str::to_string);
                    let _ = ev2.send(Event { session, method: m.to_string(), params: v.get("params").cloned().unwrap_or(Value::Null) });
                }
            }
            // Connection closed: fail everything still waiting.
            for (_, tx) in p2.lock().await.drain() {
                let _ = tx.send(Err(anyhow!("cdp connection closed")));
            }
        });

        Ok(Arc::new(Self { tx, pending, next: AtomicU64::new(1), events }))
    }

    fn message(&self, session: Option<&str>, method: &str, params: Value) -> (u64, Message) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        (id, Message::Text(msg.to_string().into()))
    }

    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let what = params.get("expression").and_then(Value::as_str).map(|e| e.chars().take(80).collect::<String>()).unwrap_or_default();
        let (id, msg) = self.message(session, method, params);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.tx.send(msg).map_err(|_| anyhow!("cdp closed"))?;
        // A browser that died without closing its socket must not hang the run.
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(r) => r.map_err(|_| anyhow!("cdp dropped reply"))?,
            Err(_) => {
                self.pending.lock().await.remove(&id);
                tracing::warn!("cdp {method} timed out: {what}");
                bail!("cdp {method}: no reply in 30 s (browser gone?)")
            }
        }
    }

    /// Sends a command without waiting for its reply.
    pub fn send(&self, session: Option<&str>, method: &str, params: Value) {
        let (_, msg) = self.message(session, method, params);
        let _ = self.tx.send(msg);
    }
}

/// A browser process with remote debugging on `port`. A throwaway profile is
/// removed after the process is killed on drop.
pub struct Chrome {
    child: tokio::process::Child,
    pub port: u16,
    /// Path of the browser websocket, e.g. `/devtools/browser/<id>`.
    pub ws_path: String,
    profile: PathBuf,
    temp: bool,
}

/// How to launch a browser.
pub struct Launch {
    pub bin: PathBuf,
    pub headless: bool,
    /// Persistent profile directory; None = a throwaway one.
    pub profile: Option<PathBuf>,
    /// Extra switches (they win over the defaults, e.g. window placement).
    pub extra: Vec<String>,
}

impl Chrome {
    /// Launches Chrome (else the discovered Chromium-family browser) with a
    /// throwaway profile: this speaks the DevTools protocol only.
    pub async fn spawn(headless: bool, extra: &[String]) -> Result<Self> {
        let (found, _) = super::discover::pick("chrome").or_else(|_| super::discover::pick("chromium")).or_else(|_| super::discover::pick(""))?;
        Self::start(&Launch { bin: found.path, headless, profile: None, extra: extra.to_vec() }).await
    }

    pub async fn start(o: &Launch) -> Result<Self> {
        let (profile, temp) = match &o.profile {
            Some(p) => (p.clone(), false),
            None => (std::env::temp_dir().join(format!("fab-chrome-{}-{}", std::process::id(), rand_suffix())), true),
        };
        std::fs::create_dir_all(&profile)?;
        // A persistent profile keeps the last run's port file: never read that one.
        let port_file = profile.join("DevToolsActivePort");
        let _ = std::fs::remove_file(&port_file);
        let mut cmd = tokio::process::Command::new(&o.bin);
        cmd.arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-background-networking",
                "--disable-sync",
                "--disable-component-update",
                "--disable-default-apps",
                "--disable-renderer-backgrounding",
                "--disable-backgrounding-occluded-windows",
                "--disable-background-timer-throttling",
                "--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,PaintHolding",
                "--hide-crash-restore-bubble",
                // Links with target=_blank open a tab even from a scripted
                // click; fab then follows it (see `follow_popup`).
                "--disable-popup-blocking",
                "--password-store=basic",
                "--use-mock-keychain",
                "--window-size=1280,900",
            ]);
        if o.headless {
            cmd.arg("--headless=new");
        }
        cmd.args(&o.extra);
        cmd.arg("about:blank").stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
        let child = cmd.spawn().with_context(|| format!("launch {}", o.bin.display()))?;

        // The browser writes its port and websocket path once it is listening.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let (port, ws_path) = loop {
            if let Ok(s) = std::fs::read_to_string(&port_file) {
                let mut l = s.lines();
                if let (Some(p), Some(path)) = (l.next().and_then(|l| l.trim().parse::<u16>().ok()), l.next()) {
                    break (p, path.trim().to_string());
                }
            }
            if std::time::Instant::now() > deadline {
                bail!("timed out waiting for {} to open its DevTools port", o.bin.display());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        Ok(Self { child, port, ws_path, profile, temp })
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        if self.temp {
            let p = self.profile.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                let _ = std::fs::remove_dir_all(p);
            });
        }
    }
}

/// Whether a browser that is still running holds `profile`: Chrome's
/// SingletonLock and Firefox's lock are links to "<host>-<pid>" and
/// "<ip>:+<pid>".
pub fn profile_in_use(profile: &Path) -> bool {
    ["SingletonLock", "lock"].iter().any(|f| {
        let Ok(target) = std::fs::read_link(profile.join(f)) else { return false };
        let t = target.to_string_lossy();
        t.rsplit(['-', '+']).next().and_then(|p| p.parse::<i32>().ok()).is_some_and(pid_alive)
    })
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    // Signal 0 checks for existence; EPERM still means the process exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    false
}

/// What JS dialogs (alert, confirm, prompt) get: "accept" or "dismiss".
/// `beforeunload` is always accepted, so leaving a page never hangs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialogs {
    Accept,
    Dismiss,
}

/// The browser: its connection and the process when fab launched it.
pub struct Host {
    pub conn: Arc<Conn>,
    chrome: std::sync::Mutex<Option<Chrome>>,
    /// Tabs fab opened in a browser it attached to (closed when done).
    opened: std::sync::Mutex<Vec<String>>,
    /// Page targets opened by other pages: (target, opener).
    popups: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    gone: Arc<std::sync::Mutex<HashSet<String>>>,
    /// One session per tab: attaching twice would double every event.
    tabs: std::sync::Mutex<HashMap<String, Arc<Tab>>>,
    /// Tabs that belong to pool pages (see [`Cdp::page`]): the main page
    /// never moves onto one.
    claimed: std::sync::Mutex<HashSet<String>>,
    /// The browser product, e.g. "Chrome/141.0.7390.54".
    pub product: String,
}

impl Host {
    async fn new(conn: Arc<Conn>, chrome: Option<Chrome>, dialogs: Dialogs) -> Result<Arc<Self>> {
        let popups: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
        let gone: Arc<std::sync::Mutex<HashSet<String>>> = Arc::default();
        listen(&conn, popups.clone(), gone.clone(), dialogs);
        let (d, v) = tokio::join!(
            conn.call(None, "Target.setDiscoverTargets", json!({"discover": true})),
            conn.call(None, "Browser.getVersion", json!({})),
        );
        d?;
        let product = v.ok().and_then(|v| v["product"].as_str().map(str::to_string)).unwrap_or_default();
        Ok(Arc::new(Self {
            conn,
            chrome: std::sync::Mutex::new(chrome),
            opened: Default::default(),
            popups,
            gone,
            tabs: Default::default(),
            claimed: Default::default(),
            product,
        }))
    }

    /// Ends the browser: one fab launched is closed (gracefully, so a
    /// persistent profile keeps its cookies); in one fab attached to, only
    /// the tabs it opened are closed.
    pub async fn shutdown(&self) {
        let chrome = self.chrome.lock().unwrap().take();
        match chrome {
            Some(mut chrome) => {
                if !chrome.temp {
                    let _ = tokio::time::timeout(Duration::from_secs(2), self.conn.call(None, "Browser.close", json!({}))).await;
                    let _ = tokio::time::timeout(Duration::from_secs(3), chrome.child.wait()).await;
                }
                drop(chrome);
            }
            None => {
                let opened: Vec<String> = std::mem::take(&mut *self.opened.lock().unwrap());
                for id in opened {
                    let _ = self.conn.call(None, "Target.closeTarget", json!({"targetId": id})).await;
                }
            }
        }
    }

    async fn close_target(&self, id: &str) {
        self.tabs.lock().unwrap().remove(id);
        self.claimed.lock().unwrap().remove(id);
        let _ = tokio::time::timeout(Duration::from_secs(2), self.conn.call(None, "Target.closeTarget", json!({"targetId": id}))).await;
    }

    async fn pages(&self) -> Result<Vec<Value>> {
        let r = self.conn.call(None, "Target.getTargets", json!({})).await?;
        let gone = self.gone.lock().unwrap().clone();
        Ok(r["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "page" && !gone.contains(t["targetId"].as_str().unwrap_or_default()))
            .cloned()
            .collect())
    }

    async fn create(&self, url: &str) -> Result<String> {
        self.create_with(json!({"url": url})).await
    }

    async fn create_with(&self, params: Value) -> Result<String> {
        let r = self.conn.call(None, "Target.createTarget", params).await?;
        let id = r["targetId"].as_str().map(str::to_string).context("createTarget: no targetId")?;
        // In a browser fab attached to, every tab it opens is closed at the end.
        if self.chrome.lock().unwrap().is_none() {
            self.opened.lock().unwrap().push(id.clone());
        }
        Ok(id)
    }
}

/// Answers JS dialogs at once (a page with an open dialog blocks every
/// script evaluation), and tracks popups and closed tabs.
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
                "Page.javascriptDialogOpening" => {
                    let kind = p["type"].as_str().unwrap_or("alert");
                    let accept = dialogs == Dialogs::Accept || matches!(kind, "alert" | "beforeunload");
                    if let Some(c) = conn.upgrade() {
                        let mut args = json!({"accept": accept});
                        if kind == "prompt" && accept {
                            args["promptText"] = p["defaultPrompt"].clone();
                        }
                        c.send(e.session.as_deref(), "Page.handleJavaScriptDialog", args);
                    }
                }
                "Target.targetCreated" => {
                    let t = &p["targetInfo"];
                    if let (Some("page"), Some(id), Some(opener)) = (t["type"].as_str(), t["targetId"].as_str(), t["openerId"].as_str()) {
                        popups.lock().unwrap().push((id.to_string(), opener.to_string()));
                    }
                }
                "Target.targetDestroyed" => {
                    if let Some(id) = p["targetId"].as_str() {
                        gone.lock().unwrap().insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    });
}

/// One tab: a flattened session on the browser connection.
pub struct Tab {
    host: Arc<Host>,
    pub session: String,
    pub target: String,
    /// The tab that opened this one, if a page did.
    pub opener: Option<String>,
}

impl Tab {
    async fn attach(host: Arc<Host>, target: &str, opener: Option<String>) -> Result<Arc<Self>> {
        let r = host.conn.call(None, "Target.attachToTarget", json!({"targetId": target, "flatten": true})).await?;
        let session = r["sessionId"].as_str().context("attachToTarget: no sessionId")?.to_string();
        let tab = Arc::new(Self { host, session, target: target.to_string(), opener });
        tab.init().await?;
        Ok(tab)
    }

    async fn init(&self) -> Result<()> {
        let (a, b, c) = tokio::join!(
            self.call("Page.enable", json!({})),
            self.call("Page.addScriptToEvaluateOnNewDocument", json!({"source": SNAPSHOT_JS})),
            self.call("Emulation.setFocusEmulationEnabled", json!({"enabled": true})),
        );
        a?;
        b?;
        c?;
        // A document loaded before fab attached has no instrumentation yet.
        if self.eval("typeof __ub").await.ok().as_ref().and_then(Value::as_str) == Some("undefined") {
            let _ = self.eval(SNAPSHOT_JS).await;
        }
        Ok(())
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.host.conn.call(Some(&self.session), method, params).await
    }

    pub async fn eval(&self, expr: &str) -> Result<Value> {
        let t0 = std::time::Instant::now();
        let r = self
            .call(
                "Runtime.evaluate",
                // Chrome terminates a script still running after this and
                // reports where it was, instead of the page hanging for good.
                json!({"expression": expr, "returnByValue": true, "awaitPromise": true, "timeout": 20000}),
            )
            .await;
        tracing::debug!("eval {:.0} ms: {}", t0.elapsed().as_secs_f64() * 1e3, expr.chars().take(90).collect::<String>());
        let r = r?;
        if let Some(ex) = r.get("exceptionDetails") {
            let msg = ex
                .pointer("/exception/description")
                .and_then(Value::as_str)
                .or_else(|| ex.get("text").and_then(Value::as_str))
                .unwrap_or("js exception");
            bail!("js: {msg}");
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    /// Waits for the load event of a navigation `start` began; a page whose
    /// DOM is ready gets 5 more seconds to finish loading, not forever.
    async fn load(&self, start: impl std::future::Future<Output = Result<Value>>) -> Result<Value> {
        let mut ev = self.host.conn.events.subscribe();
        let r = start.await?;
        // A failed navigation is reported by the caller; a same-document one
        // (a #hash) loads nothing.
        if r.get("errorText").is_some() || (r.get("loaderId").is_none() && r.as_object().is_some_and(|o| !o.is_empty())) {
            return Ok(r);
        }
        let me = self.session.clone();
        let wait = async {
            loop {
                let e = match ev.recv().await {
                    Ok(e) => e,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return,
                };
                if e.session.as_deref() != Some(me.as_str()) {
                    continue;
                }
                match e.method.as_str() {
                    "Page.loadEventFired" => return,
                    "Page.domContentEventFired" => break,
                    _ => {}
                }
            }
            let rest = async {
                loop {
                    match ev.recv().await {
                        Ok(e) if e.session.as_deref() == Some(me.as_str()) && e.method == "Page.loadEventFired" => return,
                        Err(RecvError::Closed) => return,
                        _ => {}
                    }
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(5), rest).await;
        };
        tokio::time::timeout(Duration::from_secs(30), wait).await.context("page load timed out")?;
        Ok(r)
    }

    pub async fn goto(&self, url: &str) -> Result<()> {
        let r = self.load(self.call("Page.navigate", json!({"url": url}))).await?;
        if let Some(e) = r.get("errorText").and_then(Value::as_str) {
            bail!("navigate {url}: {e}");
        }
        Ok(())
    }

    /// Trusted mouse click at viewport coordinates.
    pub async fn click_at(&self, p: Point) -> Result<()> {
        let base = json!({"x": p.x, "y": p.y, "button": "left", "clickCount": 1});
        let mut down = base.clone();
        down["type"] = "mousePressed".into();
        let mut up = base;
        up["type"] = "mouseReleased".into();
        // Pipelined: both are queued before waiting on either.
        let (a, b) = tokio::join!(self.call("Input.dispatchMouseEvent", down), self.call("Input.dispatchMouseEvent", up));
        a?;
        b?;
        Ok(())
    }

    pub async fn insert_text(&self, text: &str) -> Result<()> {
        self.call("Input.insertText", json!({"text": text})).await?;
        Ok(())
    }

    pub async fn press_enter(&self) -> Result<()> {
        self.press("Enter").await
    }

    /// Presses a key or chord: "Enter", "Escape", "ArrowDown", "Control+A",
    /// "Shift+Tab", "Meta+Enter". "ControlOrMeta" is Meta on macOS.
    pub async fn press(&self, chord: &str) -> Result<()> {
        let parts: Vec<&str> = if chord == "+" { vec!["+"] } else { chord.split('+').map(str::trim).filter(|s| !s.is_empty()).collect() };
        let Some((key, mods)) = parts.split_last() else { bail!("no key given") };
        let mut bits = 0;
        let mut downs = vec![];
        for m in mods {
            let (name, code, vk, bit) = modifier(m).with_context(|| format!("unknown modifier {m} (Alt, Control, Meta, Shift, ControlOrMeta)"))?;
            bits |= bit;
            self.call("Input.dispatchKeyEvent", json!({"type": "rawKeyDown", "key": name, "code": code, "windowsVirtualKeyCode": vk, "modifiers": bits})).await?;
            downs.push((name, code, vk));
        }
        let k = key_def(key).with_context(|| format!("unknown key {key}"))?;
        // Text is typed only without modifiers other than Shift.
        let text = if bits & !8 == 0 { k.text.clone() } else { String::new() };
        let mut down = json!({"type": if text.is_empty() { "rawKeyDown" } else { "keyDown" }, "key": k.key, "code": k.code, "windowsVirtualKeyCode": k.vk, "modifiers": bits});
        if !text.is_empty() {
            down["text"] = json!(text);
            down["unmodifiedText"] = json!(text);
        }
        // macOS routes editing shortcuts through commands, not key events.
        if cfg!(target_os = "macos") && bits == 4 {
            if let Some(cmd) = mac_command(&k.key.to_ascii_lowercase()) {
                down["commands"] = json!([cmd]);
            }
        }
        self.call("Input.dispatchKeyEvent", down).await?;
        self.call("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": k.key, "code": k.code, "windowsVirtualKeyCode": k.vk, "modifiers": bits})).await?;
        for (name, code, vk) in downs.into_iter().rev() {
            self.call("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": name, "code": code, "windowsVirtualKeyCode": vk})).await?;
        }
        Ok(())
    }

}

struct KeyDef {
    key: String,
    code: String,
    vk: u32,
    text: String,
}

fn modifier(m: &str) -> Option<(&'static str, &'static str, u32, u32)> {
    Some(match m.to_ascii_lowercase().as_str() {
        "alt" | "option" => ("Alt", "AltLeft", 18, 1),
        "control" | "ctrl" => ("Control", "ControlLeft", 17, 2),
        "meta" | "cmd" | "command" | "super" => ("Meta", "MetaLeft", 91, 4),
        "shift" => ("Shift", "ShiftLeft", 16, 8),
        "controlormeta" | "mod" => {
            if cfg!(target_os = "macos") {
                ("Meta", "MetaLeft", 91, 4)
            } else {
                ("Control", "ControlLeft", 17, 2)
            }
        }
        _ => return None,
    })
}

fn key_def(k: &str) -> Option<KeyDef> {
    let named = |key: &str, code: &str, vk: u32, text: &str| KeyDef { key: key.into(), code: code.into(), vk, text: text.into() };
    Some(match k.to_ascii_lowercase().as_str() {
        "enter" | "return" => named("Enter", "Enter", 13, "\r"),
        "tab" => named("Tab", "Tab", 9, ""),
        "escape" | "esc" => named("Escape", "Escape", 27, ""),
        "backspace" => named("Backspace", "Backspace", 8, ""),
        "delete" | "del" => named("Delete", "Delete", 46, ""),
        "space" | " " => named(" ", "Space", 32, " "),
        "arrowup" | "up" => named("ArrowUp", "ArrowUp", 38, ""),
        "arrowdown" | "down" => named("ArrowDown", "ArrowDown", 40, ""),
        "arrowleft" | "left" => named("ArrowLeft", "ArrowLeft", 37, ""),
        "arrowright" | "right" => named("ArrowRight", "ArrowRight", 39, ""),
        "home" => named("Home", "Home", 36, ""),
        "end" => named("End", "End", 35, ""),
        "pageup" => named("PageUp", "PageUp", 33, ""),
        "pagedown" => named("PageDown", "PageDown", 34, ""),
        "insert" => named("Insert", "Insert", 45, ""),
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)) => {
            let n: u32 = f[1..].parse().ok()?;
            named(&format!("F{n}"), &format!("F{n}"), 111 + n, "")
        }
        _ => {
            let mut cs = k.chars();
            let c = cs.next()?;
            if cs.next().is_some() {
                return None;
            }
            let (code, vk) = if c.is_ascii_alphabetic() {
                (format!("Key{}", c.to_ascii_uppercase()), c.to_ascii_uppercase() as u32)
            } else if c.is_ascii_digit() {
                (format!("Digit{c}"), c as u32)
            } else {
                (String::new(), 0)
            };
            KeyDef { key: c.to_string(), code, vk, text: c.to_string() }
        }
    })
}

fn mac_command(key: &str) -> Option<&'static str> {
    Some(match key {
        "a" => "selectAll",
        "c" => "copy",
        "x" => "cut",
        "v" => "paste",
        "z" => "undo",
        _ => return None,
    })
}

/// A Chromium-family browser: its current tab plus the others. The main
/// view owns the browser; pool pages ([`Cdp::page`]) are views of their own
/// tabs in it, sharing the connection, cookies and logins.
pub struct Cdp {
    pub host: Arc<Host>,
    cur: std::sync::Mutex<Arc<Tab>>,
    /// A pool page: closing it closes only its own tabs.
    page: bool,
    /// Tabs a pool page opened or followed.
    own: std::sync::Mutex<Vec<String>>,
    /// How the browser was chosen and started, for status lines.
    pub about: String,
    /// Why a more preferred choice was passed over (e.g. the default browser
    /// isn't supported, the profile was in use).
    pub notes: Vec<String>,
}

impl Cdp {
    pub async fn start(o: &Launch, dialogs: Dialogs) -> Result<Self> {
        let chrome = Chrome::start(o).await?;
        let ws = format!("ws://127.0.0.1:{}{}", chrome.port, chrome.ws_path);
        let conn = Conn::open(&ws).await?;
        let host = Host::new(conn, Some(chrome), dialogs).await?;
        // The tab the browser opened with.
        let mut first = None;
        for _ in 0..100 {
            let pages = host.pages().await?;
            first = pages.iter().find(|t| t["url"] == "about:blank").or(pages.first()).and_then(|t| t["targetId"].as_str()).map(str::to_string);
            if first.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let id = match first {
            Some(id) => id,
            None => host.create("about:blank").await?,
        };
        let tab = Tab::attach(host.clone(), &id, None).await?;
        Ok(Self::with(host, tab, String::new()))
    }

    fn with(host: Arc<Host>, tab: Arc<Tab>, about: String) -> Self {
        host.tabs.lock().unwrap().insert(tab.target.clone(), tab.clone());
        Self { host, cur: std::sync::Mutex::new(tab), page: false, own: Default::default(), about, notes: vec![] }
    }

    /// A new pool page: a tab of its own in the same browser, opened in the
    /// background so it doesn't take over the window.
    pub async fn page(host: &Arc<Host>) -> Result<Self> {
        let id = host.create_with(json!({"url": "about:blank", "background": true})).await?;
        host.claimed.lock().unwrap().insert(id.clone());
        let tab = match Tab::attach(host.clone(), &id, None).await {
            Ok(t) => t,
            Err(e) => {
                host.close_target(&id).await;
                return Err(e);
            }
        };
        host.tabs.lock().unwrap().insert(id.clone(), tab.clone());
        Ok(Self { host: host.clone(), cur: std::sync::Mutex::new(tab), page: true, own: std::sync::Mutex::new(vec![id]), about: String::new(), notes: vec![] })
    }

    /// The session for `target`, attaching once.
    async fn tab_for(&self, target: &str, opener: Option<String>) -> Result<Arc<Tab>> {
        if let Some(t) = self.host.tabs.lock().unwrap().get(target) {
            return Ok(t.clone());
        }
        let t = Tab::attach(self.host.clone(), target, opener).await?;
        self.host.tabs.lock().unwrap().insert(target.to_string(), t.clone());
        Ok(t)
    }

    /// Opens a blank tab that belongs to this view.
    async fn open_tab(&self) -> Result<Arc<Tab>> {
        let id = self.host.create_with(json!({"url": "about:blank", "background": self.page})).await?;
        if self.page {
            self.host.claimed.lock().unwrap().insert(id.clone());
            self.own.lock().unwrap().push(id.clone());
        }
        self.tab_for(&id, None).await
    }

    /// Moves this view to a fresh tab and closes the one it was on (crashed,
    /// hung or closed): the page starts over, blank.
    pub async fn renew(&self) -> Result<()> {
        let old = self.tab();
        let tab = self.open_tab().await?;
        *self.cur.lock().unwrap() = tab;
        self.own.lock().unwrap().retain(|t| *t != old.target);
        self.host.close_target(&old.target).await;
        Ok(())
    }

    /// Attaches to a running browser: `auto` (a local Chromium-family browser
    /// with remote debugging on), a port, an `http://host:port` DevTools
    /// address or a `ws://` browser endpoint. fab works in a tab of its own.
    pub async fn attach(endpoint: &str, dialogs: Dialogs) -> Result<Self> {
        let (ws, about) = resolve_endpoint(endpoint).await?;
        // Chrome asks the user to allow each connection: one attempt, with
        // time to click (retrying would stack up prompts).
        let conn = match tokio::time::timeout(Duration::from_secs(60), Conn::open(&ws)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                return Err(e.context(format!(
                    "could not attach to {about}; in Chrome, turn on remote debugging at chrome://inspect/#remote-debugging and allow the connection"
                )));
            }
            Err(_) => bail!("{about} did not accept the connection within 60 s: click Allow in the browser's remote debugging prompt"),
        };
        let host = Host::new(conn, None, dialogs).await?;
        let id = host.create("about:blank").await?;
        let tab = Tab::attach(host.clone(), &id, None).await?;
        let _ = host.conn.call(None, "Target.activateTarget", json!({"targetId": id})).await;
        Ok(Self::with(host, tab, about))
    }

    /// The current tab.
    pub fn tab(&self) -> Arc<Tab> {
        self.cur.lock().unwrap().clone()
    }

    /// A lost page must be explicitly renewed by its owner.
    pub async fn live(&self) -> Result<Arc<Tab>> {
        let t = self.tab();
        if self.host.gone.lock().unwrap().contains(&t.target) {
            return Err(super::driver::PageLost(t.target.clone()).into());
        }
        Ok(t)
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.live().await?.call(method, params).await
    }

    pub async fn eval(&self, expr: &str) -> Result<Value> {
        self.live().await?.eval(expr).await
    }

    pub async fn goto(&self, url: &str) -> Result<()> {
        self.live().await?.goto(url).await
    }

    pub async fn click_at(&self, p: Point) -> Result<()> {
        self.live().await?.click_at(p).await
    }

    pub async fn insert_text(&self, text: &str) -> Result<()> {
        self.live().await?.insert_text(text).await
    }

    pub async fn press_enter(&self) -> Result<()> {
        self.live().await?.press_enter().await
    }

    /// Switches to a tab the current one just opened (a target=_blank link,
    /// window.open), as a person would. Returns whether it switched.
    pub async fn follow_popup(&self) -> Result<bool> {
        let cur = self.tab();
        let found = {
            let mut p = self.host.popups.lock().unwrap();
            let gone = self.host.gone.lock().unwrap();
            let pick = p.iter().rev().find(|(t, o)| *o == cur.target && !gone.contains(t)).map(|(t, _)| t.clone());
            p.retain(|(_, o)| *o != cur.target);
            pick
        };
        let Some(id) = found else { return Ok(false) };
        if self.page {
            self.host.claimed.lock().unwrap().insert(id.clone());
            self.own.lock().unwrap().push(id.clone());
        }
        let tab = self.tab_for(&id, Some(cur.target.clone())).await?;
        *self.cur.lock().unwrap() = tab.clone();
        // The new tab may still be on its initial about:blank.
        for _ in 0..200 {
            if let Ok(v) = tab.eval("[location.href, document.readyState]").await {
                if v[0] != "about:blank" && v[1] != "loading" {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if tab.eval("typeof __ub").await.ok().as_ref().and_then(Value::as_str) == Some("undefined") {
            let _ = tab.eval(SNAPSHOT_JS).await;
        }
        Ok(true)
    }

    /// Ends the session: see [`Host::shutdown`]. A pool page closes only
    /// its own tabs.
    pub async fn shutdown(&self) {
        if !self.page {
            return self.host.shutdown().await;
        }
        let own: Vec<String> = std::mem::take(&mut *self.own.lock().unwrap());
        for id in own {
            self.host.close_target(&id).await;
        }
    }
}

/// The browser websocket for an attach endpoint, and a description of it.
async fn resolve_endpoint(endpoint: &str) -> Result<(String, String)> {
    let e = endpoint.trim();
    if e.starts_with("ws://") || e.starts_with("wss://") {
        return Ok((e.to_string(), e.to_string()));
    }
    if e == "auto" || e.is_empty() {
        // Browsers with remote debugging turned on (chrome://inspect) write
        // DevToolsActivePort into their user data directory; there is no
        // HTTP discovery in that mode.
        for (s, port, path) in super::discover::debuggable() {
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return Ok((format!("ws://127.0.0.1:{port}{path}"), format!("{} (remote debugging on port {port})", s.name)));
            }
        }
        // A browser started with --remote-debugging-port on a usual port.
        for port in [9222u16, 9229] {
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                if let Ok(found) = Box::pin(resolve_endpoint(&port.to_string())).await {
                    return Ok(found);
                }
            }
        }
        bail!(
            "no running browser with remote debugging found. In Chrome 144+, open chrome://inspect/#remote-debugging and turn it on (Edge: edge://inspect, Brave: brave://inspect), or start a browser with --remote-debugging-port=9222"
        );
    }
    let base = if e.chars().all(|c| c.is_ascii_digit()) { format!("http://127.0.0.1:{e}") } else { e.trim_end_matches('/').to_string() };
    // A --connect host that is not answering must not hang on the OS TCP
    // timeout (minutes): the read is short, so the budget is short too.
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().context("no HTTP client for the DevTools endpoint")?;
    let v: Value = http.get(format!("{base}/json/version")).send().await
        .with_context(|| format!("no DevTools endpoint at {base}"))?
        .error_for_status()
        .with_context(|| format!("no DevTools endpoint at {base}"))?
        .json()
        .await
        .with_context(|| format!("{base}/json/version is not a DevTools endpoint"))?;
    let ws = v["webSocketDebuggerUrl"].as_str().context("no webSocketDebuggerUrl in /json/version")?.to_string();
    let what = format!("{} at {base}", v["Browser"].as_str().unwrap_or("browser"));
    Ok((ws, what))
}

/// Replaces `\uXXXX` escapes of unpaired UTF-16 surrogates with U+FFFD.
pub(crate) fn repair_surrogates(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |i: usize| -> Option<u32> { std::str::from_utf8(b.get(i..i + 4)?).ok().and_then(|h| u32::from_str_radix(h, 16).ok()) };
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && b.get(i + 1) == Some(&b'u') {
            if let Some(c) = hex(i + 2) {
                let high = (0xD800..=0xDBFF).contains(&c);
                let low = (0xDC00..=0xDFFF).contains(&c);
                let paired_next = high && b.get(i + 6) == Some(&b'\\') && b.get(i + 7) == Some(&b'u') && hex(i + 8).is_some_and(|d| (0xDC00..=0xDFFF).contains(&d));
                if high && paired_next {
                    out.push_str(&s[i..i + 12]);
                    i += 12;
                    continue;
                }
                if high || low {
                    out.push_str("\\ufffd");
                    i += 6;
                    continue;
                }
                out.push_str(&s[i..i + 6]);
                i += 6;
                continue;
            }
        }
        // Any other escape ("\\\\", "\\\"") is copied as a pair, so an escaped
        // backslash followed by "u" isn't read as a \u escape.
        if b[i] == b'\\' && i + 1 < b.len() && b[i + 1].is_ascii() {
            out.push_str(&s[i..i + 2]);
            i += 2;
            continue;
        }
        // Copy one whole UTF-8 character.
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub(crate) fn rand_suffix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_lone_surrogates_only() {
        // A lone high surrogate (half an emoji) becomes U+FFFD; the JSON parses.
        let bad = r#"{"n":"\ud83d","ok":"📁","esc":"\\ud83d"}"#;
        assert!(serde_json::from_str::<serde_json::Value>(bad).is_err());
        let fixed = repair_surrogates(bad);
        let v: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(v["n"], "\u{fffd}");
        assert_eq!(v["ok"], "\u{1f4c1}");
        // An escaped backslash followed by "ud83d" is text, left alone.
        assert_eq!(v["esc"], "\\ud83d");
    }

    #[test]
    fn keys() {
        assert_eq!(key_def("Enter").unwrap().vk, 13);
        assert_eq!(key_def("a").unwrap().code, "KeyA");
        assert_eq!(key_def("F5").unwrap().vk, 116);
        assert!(key_def("NotAKey").is_none());
        assert_eq!(modifier("ctrl").unwrap().3, 2);
    }
}
