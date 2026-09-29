//! Any other store, through a helper executable `fab-secret-<name>` on PATH.
//! The protocol is git's credential-helper format: `key=value` lines on
//! stdin and stdout, with the web's standard autofill names as field names.
//!
//! ```text
//! fab-secret-NAME list    in:  (nothing)
//!                         out: records separated by blank lines:
//!                              id=… title=… kind=login|card|identity|other
//!                              url=… (repeatable) username=… field=<label> (repeatable)
//! fab-secret-NAME get     in:  id=… field=<standard name> label=<field label>
//!                         out: value=…
//! fab-secret-NAME store   in:  url=… title=… username=… password=…
//!                         out: title=…   (optional)
//! exit 0: ok · 1: not found · anything else: error (first stderr line is shown)
//! ```

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

use super::store::{Item, Kind, Status};

pub struct Helper {
    pub name: String,
    pub path: PathBuf,
}

/// Every `fab-secret-*` executable on PATH (first one wins per name).
pub fn discover() -> Vec<Helper> {
    let mut out: Vec<Helper> = vec![];
    let Some(p) = std::env::var_os("PATH") else { return out };
    for dir in std::env::split_paths(&p) {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut found: Vec<(String, PathBuf)> = rd
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                let name = n.strip_prefix("fab-secret-")?.to_string();
                (!name.is_empty() && super::store::is_exec(&e.path())).then(|| (name, e.path()))
            })
            .collect();
        found.sort();
        for (name, path) in found {
            if !out.iter().any(|h| h.name == name) {
                out.push(Helper { name, path });
            }
        }
    }
    out
}

fn kv(pairs: &[(&str, &str)]) -> Zeroizing<String> {
    let mut s = String::new();
    for (k, v) in pairs {
        if !v.is_empty() {
            // Values can't span lines (as in git's protocol).
            s.push_str(&format!("{k}={}\n", v.replace(['\n', '\r'], " ")));
        }
    }
    Zeroizing::new(s)
}

/// The helper process never started, so it received nothing.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct NotStarted(String);

impl Helper {
    pub fn status(&self) -> Status {
        Status { ready: true, note: format!("helper {}", self.path.display()) }
    }

    /// (exit code, stdout).
    async fn call(&self, action: &str, input: &str) -> Result<(i32, Zeroizing<String>)> {
        let mut child = tokio::process::Command::new(&self.path)
            .arg(action)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| NotStarted(format!("could not run {}: {e}", self.path.display())))?;
        if let Some(mut w) = child.stdin.take() {
            w.write_all(input.as_bytes()).await?;
        }
        let out = tokio::time::timeout(Duration::from_secs(90), child.wait_with_output())
            .await
            .with_context(|| format!("fab-secret-{} {action} did not answer within 90 s", self.name))??;
        let code = out.status.code().unwrap_or(-1);
        if code != 0 && code != 1 {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("fab-secret-{} {action} failed ({code}): {}", self.name, super::redact::text(err.lines().next().unwrap_or("").trim()));
        }
        Ok((code, Zeroizing::new(String::from_utf8_lossy(&out.stdout).to_string())))
    }

    pub async fn list(&self) -> Result<Vec<Item>> {
        let (code, out) = self.call("list", "").await?;
        if code == 1 {
            return Ok(vec![]);
        }
        Ok(parse_list(&out))
    }

    pub async fn get(&self, it: &Item, token: &str, label: &str) -> Result<Option<Zeroizing<String>>> {
        let input = kv(&[("id", &it.id), ("field", token), ("label", label)]);
        let (code, out) = self.call("get", &input).await?;
        if code == 1 {
            return Ok(None);
        }
        let v = out.lines().find_map(|l| l.strip_prefix("value=")).map(|v| Zeroizing::new(v.to_string()));
        v.map(Some).context("the helper answered without a value= line")
    }

    pub async fn save_login(&self, url: &str, title: &str, username: &str, password: &str) -> Result<String> {
        let input = kv(&[("url", url), ("title", title), ("username", username), ("password", password)]);
        let (code, out) = self.call("store", &input).await.map_err(|e| match e.downcast::<NotStarted>() {
            Ok(NotStarted(why)) => super::SaveNotSent(why).into(),
            Err(e) => e,
        })?;
        // Exit 1 is the helper saying it stored nothing.
        if code == 1 {
            return Err(super::SaveNotSent(format!("fab-secret-{} can't store logins", self.name)).into());
        }
        let t = out.lines().find_map(|l| l.strip_prefix("title=")).unwrap_or(title).to_string();
        Ok(format!("{} \"{t}\"", self.name))
    }
}

pub(crate) fn parse_list(out: &str) -> Vec<Item> {
    let mut items = vec![];
    for rec in out.split("\n\n") {
        let mut it = Item { store: 0, id: String::new(), title: String::new(), kind: Kind::Other, urls: vec![], username: None, note: None, labels: vec![] };
        for line in rec.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "id" => it.id = v.to_string(),
                "title" => it.title = v.to_string(),
                "kind" => {
                    it.kind = match v.trim() {
                        "login" => Kind::Login,
                        "card" => Kind::Card,
                        "identity" => Kind::Identity,
                        _ => Kind::Other,
                    }
                }
                "url" => it.urls.push(v.to_string()),
                "username" => it.username = Some(v.to_string()),
                "field" => it.labels.push(v.to_string()),
                "note" => it.note = Some(v.to_string()),
                _ => {}
            }
        }
        if !it.id.is_empty() {
            if it.title.is_empty() {
                it.title = it.id.clone();
            }
            items.push(it);
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_records() {
        let items = parse_list("id=gh\ntitle=GitHub\nkind=login\nurl=https://github.com\nusername=ada\nfield=password\n\nid=stripe\nkind=other\nfield=test secret key\nfield=live secret key\n");
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].kind, items[0].username.as_deref()), (Kind::Login, Some("ada")));
        assert_eq!(items[1].title, "stripe");
        assert_eq!(items[1].labels, vec!["test secret key", "live secret key"]);
    }
}
