//! In-process fixture server: static pages plus a record sink that pages post
//! their outcome events to, so the bench can check what actually happened.

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower_http::services::ServeDir;

#[derive(Clone, Default)]
pub struct Records(Arc<Mutex<Vec<Value>>>);

impl Records {
    pub fn take(&self) -> Vec<Value> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

pub struct FixtureServer {
    pub addr: SocketAddr,
    pub records: Records,
}

impl FixtureServer {
    /// A fixture's address; a scenario may also name a live site (`https://…`).
    pub fn url(&self, path: &str) -> String {
        if path.contains("://") {
            return path.to_string();
        }
        format!("http://{}/{}", self.addr, path.trim_start_matches('/'))
    }

    /// The `fixture:` name of one of this server's addresses. The server's
    /// port is gone once its daemon is, so an address kept across a restart
    /// is kept by name and resolved against the server serving it then.
    pub fn portable(&self, url: &str) -> Option<String> {
        url.strip_prefix(&format!("http://{}/", self.addr)).map(|p| format!("fixture:{p}"))
    }
}

pub fn fixtures_dir() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    here.canonicalize().unwrap_or(here)
}

pub async fn start(port: u16) -> anyhow::Result<FixtureServer> {
    let records = Records::default();
    let app = Router::new()
        .route("/__record", post(record))
        .route("/__records", get(list))
        .route("/api/delay", get(delay))
        .route("/throttled/{*path}", get(throttled))
        .with_state(records.clone())
        .fallback_service(ServeDir::new(fixtures_dir()));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(FixtureServer { addr, records })
}

async fn record(State(r): State<Records>, Json(v): Json<Value>) -> Json<Value> {
    r.0.lock().unwrap().push(v);
    Json(json!({"ok": true}))
}

async fn list(State(r): State<Records>) -> Json<Value> {
    Json(Value::Array(r.0.lock().unwrap().clone()))
}

/// `/throttled/<fixture path>`: the fixture, behind a rate limit like real
/// sites have: more than 4 requests in 500 ms get HTTP 503 with Retry-After.
async fn throttled(axum::extract::Path(path): axum::extract::Path<String>) -> axum::response::Response {
    use axum::response::IntoResponse;
    static WINDOW: std::sync::Mutex<(Option<std::time::Instant>, u32)> = std::sync::Mutex::new((None, 0));
    let busy = {
        let mut w = WINDOW.lock().unwrap();
        let now = std::time::Instant::now();
        if w.0.is_none_or(|t| now.duration_since(t) > Duration::from_millis(500)) {
            *w = (Some(now), 0);
        }
        w.1 += 1;
        w.1 > 4
    };
    if busy {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "1")], "Sorry, we're not able to serve your requests this quickly.").into_response();
    }
    let file = fixtures_dir().join(path.trim_start_matches('/'));
    match std::fs::read_to_string(&file) {
        Ok(t) => ([("content-type", "text/html; charset=utf-8")], t).into_response(),
        Err(_) => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

/// `/api/delay?ms=N&...` sleeps N ms and echoes the other params back.
async fn delay(Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let ms = q.get("ms").and_then(|s| s.parse().ok()).unwrap_or(100u64);
    tokio::time::sleep(Duration::from_millis(ms)).await;
    Json(json!(q))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixture_address_is_kept_by_name_across_servers() {
        let old = FixtureServer { addr: "127.0.0.1:51105".parse().unwrap(), records: Default::default() };
        let new = FixtureServer { addr: "127.0.0.1:60000".parse().unwrap(), records: Default::default() };
        let name = old.portable("http://127.0.0.1:51105/activity.html?x=1").unwrap();
        assert_eq!(name, "fixture:activity.html?x=1");
        assert_eq!(new.url(name.strip_prefix("fixture:").unwrap()), "http://127.0.0.1:60000/activity.html?x=1");
        // Another server's page, or a live site, is not one of its fixtures.
        assert_eq!(old.portable("http://127.0.0.1:60000/activity.html"), None);
        assert_eq!(old.portable("https://example.com/"), None);
    }
}
