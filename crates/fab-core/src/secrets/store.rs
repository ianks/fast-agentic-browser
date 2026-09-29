//! Secret stores: 1Password (`op`), Bitwarden (`bw`), the macOS Keychain,
//! and helper executables (`fab-secret-<name>`) for anything else. Stores
//! list metadata (titles, sites, usernames, field labels) freely; a value is
//! fetched only when a field is about to be typed.

use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

use super::{bw, helper, keychain, op};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Login,
    Card,
    Identity,
    Other,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Login => "login",
            Kind::Card => "card",
            Kind::Identity => "identity",
            Kind::Other => "item",
        }
    }
}

/// An item's metadata. Never holds a value.
#[derive(Debug, Clone)]
pub struct Item {
    /// Index of its store in the vault.
    pub store: usize,
    pub id: String,
    pub title: String,
    pub kind: Kind,
    pub urls: Vec<String>,
    pub username: Option<String>,
    /// What tells similar items apart: a card's last digits, a vault name.
    pub note: Option<String>,
    /// Field labels, when the store lists them without values (helpers, Keychain).
    pub labels: Vec<String>,
}

/// One field of an item, value included. Lives only while a fill resolves.
pub struct Field {
    pub label: String,
    /// Standard autofill name, when the store says what the field is.
    pub token: Option<&'static str>,
    pub concealed: bool,
    pub value: Zeroizing<String>,
}

pub struct Full {
    pub fields: Vec<Field>,
    pub totp: bool,
}

impl Full {
    pub fn get(&self, token: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.token == Some(token) && !f.value.is_empty())
    }
}

/// Whether a store can be used right now.
#[derive(Debug, Clone)]
pub struct Status {
    pub ready: bool,
    pub note: String,
}

pub enum Store {
    OnePassword(op::Op),
    Bitwarden(bw::Bw),
    Keychain(keychain::Keychain),
    Helper(helper::Helper),
}

impl Store {
    pub fn name(&self) -> String {
        match self {
            Store::OnePassword(_) => "1Password".into(),
            Store::Bitwarden(_) => "Bitwarden".into(),
            Store::Keychain(_) => "Keychain".into(),
            Store::Helper(h) => h.name.clone(),
        }
    }

    /// The key used in config and approvals: 1password, bitwarden, keychain, or the helper's name.
    pub fn key(&self) -> String {
        match self {
            Store::OnePassword(_) => "1password".into(),
            Store::Bitwarden(_) => "bitwarden".into(),
            Store::Keychain(_) => "keychain".into(),
            Store::Helper(h) => h.name.clone(),
        }
    }

    pub async fn status(&self) -> Status {
        match self {
            Store::OnePassword(s) => s.status().await,
            Store::Bitwarden(s) => s.status().await,
            Store::Keychain(s) => s.status().await,
            Store::Helper(s) => s.status(),
        }
    }

    pub async fn list(&self) -> Result<Vec<Item>> {
        match self {
            Store::OnePassword(s) => s.list().await,
            Store::Bitwarden(s) => s.list().await,
            Store::Keychain(s) => s.list().await,
            Store::Helper(s) => s.list().await,
        }
    }

    /// Every field of an item, with values. Helpers answer per field instead
    /// (see [`Store::field`]).
    pub async fn full(&self, it: &Item) -> Result<Option<Full>> {
        Ok(Some(match self {
            Store::OnePassword(s) => s.full(it).await?,
            Store::Bitwarden(s) => s.full(it).await?,
            Store::Keychain(s) => s.full(it).await?,
            Store::Helper(_) => return Ok(None),
        }))
    }

    /// One field by standard name or label (helpers).
    pub async fn field(&self, it: &Item, token: &str, label: &str) -> Result<Option<Zeroizing<String>>> {
        match self {
            Store::Helper(s) => s.get(it, token, label).await,
            _ => bail!("{} answers whole items", self.name()),
        }
    }

    /// The current one-time code.
    pub async fn otp(&self, it: &Item) -> Result<Zeroizing<String>> {
        match self {
            Store::OnePassword(s) => s.otp(it).await,
            Store::Bitwarden(s) => s.otp(it).await,
            Store::Keychain(_) => bail!("the Keychain has no one-time codes"),
            Store::Helper(s) => s.get(it, "one-time-code", "").await?.context("the helper has no one-time code for this item"),
        }
    }

    /// Saves a new login; returns what to call it in messages.
    pub async fn save_login(&self, url: &str, title: &str, username: &str, password: &str) -> Result<String> {
        match self {
            Store::OnePassword(s) => s.save_login(url, title, username, password).await,
            Store::Bitwarden(s) => s.save_login(url, title, username, password).await,
            Store::Keychain(s) => s.save_login(url, username, password),
            Store::Helper(s) => s.save_login(url, title, username, password).await,
        }
    }
}

/// Runs a store's CLI: stdin fed (never argv) when given, stdout captured,
/// stderr passed through to the user's terminal is not possible from a
/// daemon, so it is captured and shown only as an error.
pub async fn run(bin: &str, args: &[&str], stdin: Option<&[u8]>, envs: &[(&str, &str)], timeout: Duration) -> Result<Zeroizing<Vec<u8>>> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args).stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() });
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().with_context(|| format!("could not run {bin}"))?;
    if let (Some(data), Some(mut w)) = (stdin, child.stdin.take()) {
        w.write_all(data).await?;
        drop(w);
    }
    // A helper that does not answer is killed and its output is lost with it,
    // so the message has to say what to do: every store here is a program
    // that can be waiting on a lock, a keychain prompt or a network.
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(o) => o?,
        Err(_) => anyhow::bail!("{bin} did not answer within {} s (a password manager waiting to be unlocked or approved; unlock it, or set FAB_SECRETS=none to skip it)", timeout.as_secs()),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").trim_start_matches("[ERROR]").trim();
        bail!("{bin} {} failed ({}): {}", args.first().copied().unwrap_or(""), out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()), super::redact::text(first));
    }
    Ok(Zeroizing::new(out.stdout))
}

/// Whether `bin` is on PATH.
pub fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| is_exec(&d.join(bin))))
}

pub fn is_exec(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// "202712", "12/2027", "12/27", "2027-12" → (month, four-digit year).
pub fn month_year(s: &str) -> Option<(u32, u32)> {
    let d: Vec<&str> = s.split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).collect();
    let year = |y: &str| -> Option<u32> {
        let n: u32 = y.parse().ok()?;
        Some(if y.len() <= 2 { 2000 + n } else { n })
    };
    let (m, y) = match d.as_slice() {
        [one] if one.len() == 6 => (one[4..].parse().ok()?, one[..4].parse().ok()?),
        [one] if one.len() == 4 => (one[..2].parse().ok()?, year(&one[2..])?),
        [a, b] if a.len() == 4 => (b.parse().ok()?, a.parse().ok()?),
        [a, b] => (a.parse().ok()?, year(b)?),
        _ => return None,
    };
    (1..=12).contains(&m).then_some((m, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_formats() {
        assert_eq!(month_year("202712"), Some((12, 2027)));
        assert_eq!(month_year("12/2027"), Some((12, 2027)));
        assert_eq!(month_year("03/27"), Some((3, 2027)));
        assert_eq!(month_year("2027-03"), Some((3, 2027)));
        assert_eq!(month_year("0327"), Some((3, 2027)));
        assert_eq!(month_year("13/27"), None);
    }
}
