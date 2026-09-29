//! Minimal MCP stdio client (newline-delimited JSON-RPC), used to drive
//! chrome-devtools-mcp as the control toolset.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct McpClient {
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next: AtomicU64,
    _child: Child,
    pub tools: Vec<Value>,
}

impl McpClient {
    pub async fn spawn(cmd: &str, args: &[&str]) -> Result<Self> {
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {cmd}"))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pending: Pending = Arc::default();
        let p2 = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = p2.lock().await.remove(&id) {
                        let _ = tx.send(v);
                    }
                }
            }
        });
        let mut c = Self { stdin: Mutex::new(stdin), pending, next: AtomicU64::new(1), _child: child, tools: vec![] };
        c.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "fab-bench", "version": "0"}}),
        )
        .await?;
        c.notify("notifications/initialized").await?;
        let t = c.request("tools/list", json!({})).await?;
        c.tools = t["tools"].as_array().cloned().unwrap_or_default();
        Ok(c)
    }

    async fn write(&self, v: Value) -> Result<()> {
        let mut s = self.stdin.lock().await;
        s.write_all(format!("{v}\n").as_bytes()).await?;
        s.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str) -> Result<()> {
        self.write(json!({"jsonrpc": "2.0", "method": method})).await
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        let v = tokio::time::timeout(std::time::Duration::from_secs(120), rx)
            .await
            .map_err(|_| anyhow!("mcp {method} timed out"))?
            .map_err(|_| anyhow!("mcp server exited"))?;
        if let Some(e) = v.get("error") {
            bail!("mcp {method}: {e}");
        }
        Ok(v["result"].clone())
    }

    /// Calls a tool; returns (text content, isError). Images are summarized.
    pub async fn call(&self, name: &str, args: &Value) -> (String, bool) {
        match self.request("tools/call", json!({"name": name, "arguments": args})).await {
            Ok(r) => {
                let mut out = String::new();
                for c in r["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str() {
                        Some("text") => {
                            out.push_str(c["text"].as_str().unwrap_or_default());
                            out.push('\n');
                        }
                        Some(t) => out.push_str(&format!("[{t} content omitted]\n")),
                        None => {}
                    }
                }
                (out, r["isError"].as_bool().unwrap_or(false))
            }
            Err(e) => (format!("error: {e:#}"), true),
        }
    }
}
