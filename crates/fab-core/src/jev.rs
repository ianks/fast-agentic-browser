//! Minimal client for TypeSafe's System One API (Jev), served directly or via OpenRouter.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/systemone";
pub const TYPESAFE_URL: &str = "https://api.typesafe.ai/v1/systemone";

#[derive(Clone)]
pub struct Jev {
    http: reqwest::Client,
    url: String,
    key: String,
    pub model: String,
    /// Identical requests raced per question batch; first answer wins (tail-latency cut).
    pub hedge: usize,
    /// Send the backup copy only if the primary is still out at this quantile of
    /// recent latencies (0 = send all copies at once).
    pub hedge_q: f64,
    lat: std::sync::Arc<std::sync::Mutex<LatencyRing>>,
    /// Backup requests actually sent (for accounting).
    pub hedges_sent: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Recent warm request latencies, for the hedge delay.
#[derive(Default)]
struct LatencyRing {
    ms: Vec<f64>,
    next: usize,
}

impl LatencyRing {
    const CAP: usize = 128;

    fn push(&mut self, ms: f64) {
        if self.ms.len() < Self::CAP {
            self.ms.push(ms);
        } else {
            self.ms[self.next] = ms;
        }
        self.next = (self.next + 1) % Self::CAP;
    }

    /// The q-quantile, or a conservative default until enough samples exist.
    fn quantile(&self, q: f64) -> f64 {
        if self.ms.len() < 8 {
            return 400.0;
        }
        let mut v = self.ms.clone();
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * q).round() as usize]
    }
}

/// A batch of typed questions, keyed by an id we choose.
#[derive(Default, Debug, Clone)]
pub struct Questions(pub Map<String, Value>);

impl Questions {
    pub fn noul(&mut self, id: impl Into<String>, instructions: impl Into<Value>) {
        self.0.insert(id.into(), json!({"type": "noul", "instructions": instructions.into()}));
    }

    pub fn noul_criteria(&mut self, id: impl Into<String>, instructions: impl Into<Value>, yes: &str, no: &str) {
        self.0.insert(
            id.into(),
            json!({"type": "noul", "instructions": instructions.into(), "criteria": {"true": yes, "false": no}}),
        );
    }

    /// `options` is an ordered list of (option key, optional description).
    pub fn choice<K: Into<String>>(
        &mut self,
        id: impl Into<String>,
        instructions: impl Into<Value>,
        options: impl IntoIterator<Item = (K, Option<String>)>,
    ) {
        let criteria: Map<String, Value> = options
            .into_iter()
            .map(|(k, d)| (k.into(), d.map(Value::String).unwrap_or(Value::Null)))
            .collect();
        debug_assert!(criteria.len() >= 2 && criteria.len() <= 255, "choice needs 2..=255 options");
        self.0.insert(
            id.into(),
            json!({"type": "choice", "instructions": instructions.into(), "criteria": criteria}),
        );
    }

    /// A yes/no gate: a Noul, or (`as_choice`) a two-option Choice with neutral
    /// keys "A" = yes / "B" = no, for models whose Nouls follow their labels.
    pub fn gate(&mut self, id: impl Into<String>, instructions: impl Into<Value>, yes: &str, no: &str, as_choice: bool) {
        if as_choice {
            self.choice(id, instructions, [("A", Some(yes.to_string())), ("B", Some(no.to_string()))]);
        } else {
            self.noul_criteria(id, instructions, yes, no);
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: crate::domain::Probability,
    },
    Choice {
        choice: String,
        probabilities: HashMap<String, crate::domain::Probability>,
        confidence: crate::domain::Probability,
    },
    Score {
        score: f64,
        probabilities: HashMap<String, crate::domain::Probability>,
        confidence: crate::domain::Probability,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cost: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Answers {
    pub model: String,
    pub answers: HashMap<String, Answer>,
    pub usage: Usage,
    pub latency: Duration,
}

impl Answers {
    pub fn noul(&self, id: &str) -> Option<f64> {
        match self.answers.get(id)? {
            Answer::Noul { noul } => Some((*noul).into()),
            _ => None,
        }
    }

    /// P(yes) of a gate asked with `Questions::gate` (Noul or A/B Choice).
    pub fn yes(&self, id: &str) -> Option<f64> {
        match self.answers.get(id)? {
            Answer::Noul { noul } => Some((*noul).into()),
            Answer::Choice { probabilities, .. } => probabilities.get("A").copied().map(Into::into),
            _ => None,
        }
    }

    /// Returns (choice, probability of that choice, confidence).
    pub fn choice(&self, id: &str) -> Option<(&str, f64, f64)> {
        match self.answers.get(id)? {
            Answer::Choice { choice, probabilities, confidence } => {
                Some((choice.as_str(), probabilities.get(choice).copied().map(f64::from).unwrap_or(0.0), (*confidence).into()))
            }
            _ => None,
        }
    }

    /// Choice options sorted by probability, highest first.
    pub fn ranked(&self, id: &str) -> Vec<(&str, f64)> {
        match self.answers.get(id) {
            Some(Answer::Choice { probabilities, .. }) => {
                let mut v: Vec<(&str, f64)> = probabilities.iter().map(|(k, p)| (k.as_str(), (*p).into())).collect();
                v.sort_by(|a, b| b.1.total_cmp(&a.1));
                v
            }
            _ => vec![],
        }
    }
}

/// Why a fuzzy instruction can't run without a key, and what still works.
pub const NO_KEY: &str = "high-level instructions need a Jev key: set OPENROUTER_API_KEY (or TYPESAFE_API_KEY) in the environment or in ~/.config/fab/env. Precise commands (click e12, type \"x\" into e5, press Escape), open, read, screenshots, tabs and logs work without one";

/// Something that answers question batches: live Jev, or canned answers in tests.
pub trait Oracle: Sync {
    fn answer(&self, state: &Value, questions: &Questions) -> impl std::future::Future<Output = Result<Answers>> + Send;
}

impl Oracle for Jev {
    fn answer(&self, state: &Value, questions: &Questions) -> impl std::future::Future<Output = Result<Answers>> + Send {
        self.ask(state, questions)
    }
}

#[derive(Deserialize)]
struct RawResponse {
    model: String,
    answers: HashMap<String, Answer>,
    #[serde(default)]
    usage: Usage,
}

impl Jev {
    /// Builds a client from the environment: `TYPESAFE_API_KEY` (direct) or `OPENROUTER_API_KEY`.
    pub fn from_env() -> Result<Self> {
        let model = std::env::var("FAB_JEV_MODEL").unwrap_or_else(|_| "jev-latest".into());
        if let Ok(key) = std::env::var("TYPESAFE_API_KEY") {
            let url = std::env::var("TYPESAFE_BASE_URL")
                .map(|b| format!("{}/v1/systemone", b.trim_end_matches('/')))
                .unwrap_or_else(|_| TYPESAFE_URL.into());
            return Self::new(url, key, model);
        }
        let key = std::env::var("OPENROUTER_API_KEY").context("set TYPESAFE_API_KEY or OPENROUTER_API_KEY")?;
        Self::new(OPENROUTER_URL.into(), key, model)
    }

    pub fn new(url: String, key: String, model: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(300))
            .pool_max_idle_per_host(8)
            .http2_keep_alive_interval(Duration::from_secs(15))
            .http2_keep_alive_while_idle(true)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            url,
            key,
            model,
            hedge: 1,
            hedge_q: 0.75,
            lat: Default::default(),
            hedges_sent: Default::default(),
        })
    }

    /// Opens the TLS/HTTP2 connection ahead of time so the first real decision is warm.
    /// A client without a key: every question fails, saying how to add one.
    /// Precise commands, navigation, reading and screenshots never ask.
    pub fn without_key() -> Self {
        Self::new(String::new(), String::new(), "none".into()).expect("http client")
    }

    pub fn has_key(&self) -> bool {
        !self.key.is_empty()
    }

    pub async fn warm(&self) -> Result<Duration> {
        let mut q = Questions::default();
        q.noul("warm", "Is this text non-empty?");
        Ok(self.ask(&json!("warm"), &q).await?.latency)
    }

    pub async fn ask(&self, state: &Value, questions: &Questions) -> Result<Answers> {
        if self.key.is_empty() {
            anyhow::bail!(NO_KEY);
        }
        let body = json!({"model": self.model, "state": state, "questions": questions.0});
        let body = serde_json::to_vec(&body)?;
        let t0 = Instant::now();
        let res = if self.hedge <= 1 {
            self.send(body).await
        } else if self.hedge_q <= 0.0 {
            let races = (0..self.hedge).map(|_| Box::pin(self.send(body.clone())));
            futures_util::future::select_ok(races).await.map(|x| x.0)
        } else {
            // Delayed hedge: a backup goes out only if the primary is slower than
            // the recent q-quantile, so most decisions cost one request.
            let delay = self.lat.lock().unwrap().quantile(self.hedge_q);
            let primary = self.send(body.clone());
            tokio::pin!(primary);
            tokio::select! {
                r = &mut primary => r,
                _ = tokio::time::sleep(Duration::from_millis(delay as u64)) => {
                    self.hedges_sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let backup = self.send(body);
                    tokio::pin!(backup);
                    // First success wins; if one fails, wait for the other.
                    tokio::select! {
                        r = &mut primary => match r { Ok(a) => Ok(a), Err(_) => backup.await },
                        r = &mut backup => match r { Ok(a) => Ok(a), Err(_) => primary.await },
                    }
                }
            }
        };
        if let Ok(a) = &res {
            let mut a2 = a.clone();
            a2.latency = t0.elapsed();
            self.lat.lock().unwrap().push(a2.latency.as_secs_f64() * 1e3);
            return Ok(a2);
        }
        res
    }

    async fn send(&self, body: Vec<u8>) -> Result<Answers> {
        let t0 = Instant::now();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = self
                .http
                .post(&self.url)
                .bearer_auth(&self.key)
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                // A connect or TLS failure is worth a moment's pause, not an
                // immediate second try: without it a blackholed endpoint is
                // hammered three times back to back (and the caller's own
                // timeout is what finally ends the wait).
                Err(e) if attempt < 3 => {
                    tracing::warn!("jev transport error (retrying): {e}");
                    tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = resp.status();
            // A body cut off mid-read, or a 200 whose body doesn't parse, is a
            // transient upstream failure like a 5xx: retry it too.
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) if attempt < 4 => {
                    tracing::warn!("jev body error (retrying): {e}");
                    tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if status.is_success() {
                match serde_json::from_slice::<RawResponse>(&bytes) {
                    Ok(raw) => return Ok(Answers { model: raw.model, answers: raw.answers, usage: raw.usage, latency: t0.elapsed() }),
                    Err(e) if attempt < 4 => {
                        tracing::warn!("bad jev response (retrying): {e}");
                        tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await;
                        continue;
                    }
                    Err(e) => return Err(e).with_context(|| format!("bad jev response: {}", String::from_utf8_lossy(&bytes))),
                }
            }
            let retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504 | 529);
            if retryable && attempt < 4 {
                tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await;
                continue;
            }
            bail!("jev HTTP {status}: {}", String::from_utf8_lossy(&bytes));
        }
    }
}
