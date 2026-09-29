//! camofox-browser backend (REST server wrapping Camoufox/Firefox + Playwright).
//! Everything page-side goes through `/evaluate`; trusted input uses its
//! `/click`, `/type` and `/press` routes by CSS selector.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::time::Duration;


pub struct Camofox {
    http: reqwest::Client,
    base: String,
    user: String,
    pub tab: String,
}

impl Camofox {
    pub async fn open(base: &str) -> Result<Self> {
        let http = reqwest::Client::builder().tcp_nodelay(true).timeout(Duration::from_secs(60)).build()?;
        let user = format!("fab-{}-{}", std::process::id(), super::nonce());
        let base = base.trim_end_matches('/').to_string();
        let v: Value = http
            .post(format!("{base}/tabs"))
            .json(&json!({"userId": user, "sessionKey": "fab"}))
            .send()
            .await?
            .json()
            .await?;
        let Some(tab) = v["tabId"].as_str() else { bail!("camofox: no tabId in {v}") };
        Ok(Self { http, base, user, tab: tab.to_string() })
    }

    async fn post(&self, route: &str, mut body: Value) -> Result<Value> {
        body["userId"] = self.user.clone().into();
        let r = self.http.post(format!("{}/tabs/{}/{route}", self.base, self.tab)).json(&body).send().await?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
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

    pub async fn close(&self) {
        let _ = self.http.delete(format!("{}/sessions/{}", self.base, self.user)).send().await;
    }
}
