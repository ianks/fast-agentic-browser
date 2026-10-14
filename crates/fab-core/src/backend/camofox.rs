//! camofox-browser backend (REST server wrapping Camoufox/Firefox + Playwright).
//! Everything page-side goes through `/evaluate`; trusted input uses its
//! `/click`, `/type` and `/press` routes by CSS selector.
//!
//! fab is one client of a camofox server others may share. With a joined
//! user (`CAMOFOX_USER_ID`) the user's context belongs to everyone using it,
//! so fab closes only the tabs it opened and never the user's session.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Knobs;

/// The server, the user and the tab group every page of a session shares.
pub struct Conn {
    http: reqwest::Client,
    base: String,
    user: String,
    session: String,
    /// `CAMOFOX_API_KEY`, sent on every request. Read from the environment
    /// only: knobs travel as argv and into results.
    key: String,
    /// The user is someone else's (joined): never end its session.
    joined: bool,
    /// Tabs open now: a user fab made is ended with its last tab.
    open: std::sync::atomic::AtomicUsize,
}

impl Conn {
    pub fn new(k: &Knobs) -> Result<Arc<Self>> {
        let http = reqwest::Client::builder().tcp_nodelay(true).timeout(Duration::from_secs(60)).build()?;
        let joined = !k.camofox_user.is_empty();
        let user = if joined { k.camofox_user.clone() } else { format!("fab-{}-{}", std::process::id(), super::nonce()) };
        let key = std::env::var("CAMOFOX_API_KEY").unwrap_or_default();
        Ok(Arc::new(Self { http, base: k.camofox_url.trim_end_matches('/').to_string(), user, session: k.camofox_session.clone(), key, joined, open: Default::default() }))
    }

    fn auth(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.key.is_empty() { r } else { r.bearer_auth(&self.key) }
    }

    async fn open_tab(&self) -> Result<String> {
        let r = self.auth(self.http.post(format!("{}/tabs", self.base))).json(&json!({"userId": self.user, "sessionKey": self.session})).send().await?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        let Some(tab) = v["tabId"].as_str() else { bail!("camofox: could not open a tab ({status}): {}", v.get("error").unwrap_or(&v)) };
        self.open.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(tab.to_string())
    }

    async fn close_tab(&self, tab: &str) {
        // The query names the user (what camofox reads on DELETE); the body too.
        if let Ok(url) = reqwest::Url::parse_with_params(&format!("{}/tabs/{tab}", self.base), [("userId", &self.user)]) {
            let _ = self.auth(self.http.delete(url)).json(&json!({"userId": self.user})).send().await;
        }
        if self.open.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            self.shutdown().await;
        }
    }

    /// Ends a user fab made for itself; a joined user's session is left alone.
    pub async fn shutdown(&self) {
        if !self.joined {
            let _ = self.auth(self.http.delete(format!("{}/sessions/{}", self.base, self.user))).send().await;
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn joined(&self) -> bool {
        self.joined
    }

    /// Server health, then the key against a route that checks it
    /// (`/sessions/{user}/traces` is read-only and needs no tab).
    pub async fn check(&self) -> Result<String> {
        let h = self.http.get(format!("{}/health", self.base)).send().await?;
        if !h.status().is_success() {
            bail!("{}/health: HTTP {}", self.base, h.status());
        }
        let r = self.auth(self.http.get(format!("{}/sessions/{}/traces", self.base, self.user))).send().await?;
        match r.status().as_u16() {
            401 | 403 if self.key.is_empty() => bail!("reachable, but the server wants a key: set CAMOFOX_API_KEY"),
            401 | 403 => bail!("reachable, but CAMOFOX_API_KEY was refused"),
            _ => Ok(if self.key.is_empty() { "reachable, no key".into() } else { "reachable, key accepted".into() }),
        }
    }
}

pub struct Camofox {
    conn: Arc<Conn>,
    pub tab: String,
}

impl Camofox {
    pub async fn open(k: &Knobs) -> Result<Self> {
        Self::page(&Conn::new(k)?).await
    }

    /// Another tab of the same user and tab group.
    pub async fn page(conn: &Arc<Conn>) -> Result<Self> {
        Ok(Self { tab: conn.open_tab().await?, conn: conn.clone() })
    }

    pub fn conn(&self) -> Arc<Conn> {
        self.conn.clone()
    }

    async fn post(&self, route: &str, mut body: Value) -> Result<Value> {
        let c = &self.conn;
        body["userId"] = c.user.clone().into();
        let r = c.auth(c.http.post(format!("{}/tabs/{}/{route}", c.base, self.tab))).json(&body).send().await?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        // camofox reaps idle tabs (TAB_INACTIVITY_MS): the page is gone, and
        // nothing reached it.
        if status == reqwest::StatusCode::NOT_FOUND && v["error"].as_str().is_some_and(|e| e.to_lowercase().contains("tab not found")) {
            return Err(super::driver::PageLost(self.tab.clone()).into());
        }
        if !status.is_success() || v.get("error").is_some() {
            bail!("camofox {route} {status}: {}", v.get("error").unwrap_or(&v));
        }
        Ok(v)
    }

    /// Raw transport evaluation; the shared runtime supplies the bridge guard.
    pub async fn eval(&self, expr: &str) -> Result<Value> {
        let v = self.post("evaluate", json!({"expression": expr})).await?;
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn goto(&self, url: &str) -> Result<()> {
        self.post("navigate", json!({"url": url})).await?;
        Ok(())
    }

    pub async fn click_selector(&self, selector: &str) -> Result<()> {
        self.post("click", json!({"selector": selector})).await?;
        Ok(())
    }

    pub async fn type_selector(&self, selector: &str, text: &str) -> Result<()> {
        self.post("type", json!({"selector": selector, "text": text})).await?;
        Ok(())
    }

    /// Waits for network idle. Our instrumentation is injected lazily on
    /// camofox, so requests a new document started before it can't be seen;
    /// Playwright's network-idle wait covers them.
    pub async fn wait_ready(&self) -> Result<()> {
        self.post("wait", json!({"timeout": 5000, "waitForNetwork": true})).await?;
        Ok(())
    }

    pub async fn press(&self, key: &str) -> Result<()> {
        self.post("press", json!({"key": key})).await?;
        Ok(())
    }

    /// A PNG of the tab. camofox takes the viewport or the whole page; a
    /// box is cut from the whole page here.
    pub async fn screenshot(&self, shot: &super::driver::Shot) -> Result<Vec<u8>> {
        let c = &self.conn;
        let full = shot.full_page || shot.clip.is_some();
        let url = reqwest::Url::parse_with_params(&format!("{}/tabs/{}/screenshot", c.base, self.tab), [("userId", c.user.as_str()), ("fullPage", if full { "true" } else { "false" })])?;
        let r = c.auth(c.http.get(url)).send().await?;
        let status = r.status();
        if !status.is_success() {
            let v: Value = r.json().await.unwrap_or(Value::Null);
            if status == reqwest::StatusCode::NOT_FOUND && v["error"].as_str().is_some_and(|e| e.to_lowercase().contains("tab not found")) {
                return Err(super::driver::PageLost(self.tab.clone()).into());
            }
            bail!("camofox screenshot {status}: {}", v.get("error").unwrap_or(&v));
        }
        let png = r.bytes().await?.to_vec();
        match shot.clip {
            Some(clip) => super::crop(&png, clip, shot.dpr),
            None => Ok(png),
        }
    }

    /// A fresh blank tab in place of this one (reaped, crashed or hung).
    pub async fn renew(&mut self) -> Result<()> {
        let old = std::mem::replace(&mut self.tab, self.conn.open_tab().await?);
        self.conn.close_tab(&old).await;
        Ok(())
    }

    /// Closes this tab only; other clients' tabs of a joined user stay open.
    pub async fn close(&self) {
        self.conn.close_tab(&self.tab).await;
    }
}
