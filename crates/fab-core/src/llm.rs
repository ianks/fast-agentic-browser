//! OpenRouter chat-completions client for the benchmark's agent loop.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};


const URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Whether a reply is worth sending again. A refusal that is not a rate
/// limit or a server fault (a malformed request, a bad key, no credits) is
/// final: the same request gets the same answer, so retrying it only spends
/// the backoff. A success without a message is retried: a proxy can return
/// an empty or error-shaped body once.
fn retryable(status: u16, has_message: bool) -> bool {
    status == 429 || (500..600).contains(&status) || (status < 400 && !has_message)
}

#[derive(Clone)]
pub struct Llm {
    http: reqwest::Client,
    key: String,
    pub model: String,
    /// Recent call latency (EWMA, ms), shared by clones: sets when a slow
    /// call gets a hedge.
    ewma_ms: std::sync::Arc<std::sync::Mutex<f64>>,
}

#[derive(Debug, Clone, Default)]
pub struct ChatUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost: f64,
}

impl Llm {
    pub fn from_env(model: &str) -> Result<Self> {
        let key = std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY not set")?;
        let http = reqwest::Client::builder()
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(300))
            .http2_keep_alive_interval(Duration::from_secs(15))
            .http2_keep_alive_while_idle(true)
            // A planner turn takes ~1-2 s; a request still open after this is
            // hung upstream, and a retry is faster than waiting it out.
            .timeout(Duration::from_secs(45))
            .build()?;
        Ok(Self { http, key, model: model.to_string(), ewma_ms: std::sync::Arc::new(std::sync::Mutex::new(2000.0)) })
    }

    /// One request: status and body.
    async fn once(&self, body: &Value) -> Result<(reqwest::StatusCode, Value), reqwest::Error> {
        let r = self.http.post(URL).bearer_auth(&self.key).json(body).send().await?;
        let status = r.status();
        Ok((status, r.json().await?))
    }

    /// A request that races a second copy once the first is slower than
    /// usual (3x the recent average, at least 6 s): an upstream stall then
    /// costs seconds, not the whole timeout. Only slow calls are duplicated.
    async fn hedged(&self, body: &Value) -> Result<(reqwest::StatusCode, Value), reqwest::Error> {
        let after = Duration::from_millis((*self.ewma_ms.lock().unwrap() * 3.0).max(6000.0) as u64);
        let t0 = Instant::now();
        let first = self.once(body);
        tokio::pin!(first);
        let r = tokio::select! {
            r = &mut first => r,
            _ = tokio::time::sleep(after) => {
                tracing::warn!("llm call slower than {:.1} s: racing a second copy", after.as_secs_f64());
                let second = self.once(body);
                tokio::pin!(second);
                tokio::select! {
                    r = &mut first => r,
                    r = &mut second => r,
                }
            }
        };
        if r.is_ok() {
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            let mut e = self.ewma_ms.lock().unwrap();
            *e = 0.8 * *e + 0.2 * ms;
        }
        r
    }

    /// Opens the pooled connection (TLS + HTTP/2) ahead of the first real call.
    pub async fn warm(&self) {
        let _ = self.http.get("https://openrouter.ai/api/v1/key").bearer_auth(&self.key).send().await;
    }

    /// Sends a chat-completions request (model is filled in) and returns the first
    /// choice's message plus usage and wall time. Transport errors, timeouts,
    /// 429/5xx and empty replies are retried with backoff.
    pub async fn chat(&self, mut body: Value) -> Result<(Value, ChatUsage, Duration)> {
        const ATTEMPTS: u32 = 5;
        body["model"] = self.model.clone().into();
        body["usage"] = json!({"include": true});
        let t0 = Instant::now();
        let mut attempt = 0;
        let backoff = |attempt: u32| Duration::from_millis((400u64 << attempt.min(4)).min(5000));
        let (v, msg): (Value, Value) = loop {
            attempt += 1;
            let (status, v) = match self.hedged(&body).await {
                Ok(r) => r,
                Err(e) if attempt < ATTEMPTS => {
                    tracing::warn!("llm transport error (retrying): {e}");
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let msg = v.pointer("/choices/0/message").cloned();
            if status.is_success() && v.get("error").is_none() {
                if let Some(m) = msg {
                    break (v, m);
                }
            }
            if attempt < ATTEMPTS && retryable(status.as_u16(), msg.is_some()) {
                tracing::warn!("llm HTTP {status} (retrying): {}", crate::snapshot::truncate(&v.to_string(), 200));
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
            bail!("llm HTTP {status}: {v}");
        };
        let u = &v["usage"];
        let usage = ChatUsage {
            prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
            cost: u["cost"].as_f64().unwrap_or(0.0),
        };
        Ok((msg, usage, t0.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::retryable;

    #[test]
    fn a_final_refusal_is_not_retried() {
        // No credits, a bad key, a malformed request: the answer will not
        // change, so the caller hears about it at once.
        for status in [400, 401, 402, 403, 404, 422] {
            assert!(!retryable(status, false), "HTTP {status} was retried");
        }
        assert!(!retryable(402, false), "no credits was retried");
    }

    #[test]
    fn a_rate_limit_or_server_fault_is_retried() {
        assert!(retryable(429, false));
        assert!(retryable(500, false));
        assert!(retryable(503, false));
        assert!(retryable(502, false));
    }

    #[test]
    fn an_empty_success_is_retried() {
        assert!(retryable(200, false), "a 200 without a message was not retried");
        assert!(!retryable(200, true), "a real answer was retried");
    }
}
