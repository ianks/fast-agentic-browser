//! The macOS login Keychain: internet passwords (logins by server) and
//! generic passwords (API tokens CLIs keep there). Items the Passwords app
//! syncs through iCloud live in a keychain the command line can't reach.

use anyhow::Result;
use std::time::Duration;
use zeroize::Zeroizing;

use super::store::{Field, Full, Item, Kind, Status, run};

pub struct Keychain;

const SEP: char = '\u{1f}';

impl Keychain {
    pub async fn status(&self) -> Status {
        if cfg!(target_os = "macos") {
            Status { ready: true, note: "login keychain (iCloud Passwords-app items aren't reachable from the command line)".into() }
        } else {
            Status { ready: false, note: "macOS only".into() }
        }
    }

    pub async fn list(&self) -> Result<Vec<Item>> {
        // Attributes only: listing never prompts and never reads a password.
        let out = run("security", &["dump-keychain"], None, &[], Duration::from_secs(20)).await?;
        Ok(parse_dump(&String::from_utf8_lossy(&out)))
    }

    pub async fn full(&self, it: &Item) -> Result<Full> {
        let mut parts = it.id.split(SEP);
        let (class, svc, acct) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let cmd = if class == "inet" { "find-internet-password" } else { "find-generic-password" };
        // macOS asks the user before handing the password to fab.
        let out = run("security", &[cmd, "-s", svc, "-a", acct, "-w"], None, &[], Duration::from_secs(90)).await?;
        let pw = Zeroizing::new(String::from_utf8_lossy(&out).trim_end_matches('\n').to_string());
        let mut fields = vec![Field { label: "password".into(), token: (class == "inet").then_some("current-password"), concealed: true, value: pw }];
        if !acct.is_empty() {
            fields.push(Field { label: "account".into(), token: Some("username"), concealed: false, value: Zeroizing::new(acct.to_string()) });
        }
        Ok(Full { fields, totp: false })
    }

    pub fn save_login(&self, url: &str, username: &str, password: &str) -> Result<String> {
        let host = url.split("://").nth(1).unwrap_or(url).split(['/', ':']).next().unwrap_or_default().to_string();
        #[cfg(target_os = "macos")]
        {
            use security_framework::os::macos::passwords::{SecAuthenticationType, SecProtocolType};
            let proto = if url.starts_with("http://") { SecProtocolType::HTTP } else { SecProtocolType::HTTPS };
            security_framework::passwords::set_internet_password(&host, None, username, "", None, proto, SecAuthenticationType::Default, password.as_bytes())?;
            Ok(format!("Keychain ({host})"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (host, username, password);
            anyhow::bail!("the Keychain is macOS only")
        }
    }
}

/// Items from `security dump-keychain` (attributes only).
fn parse_dump(s: &str) -> Vec<Item> {
    let mut items = vec![];
    let mut class = String::new();
    let mut attrs: std::collections::HashMap<String, String> = Default::default();
    let mut flush = |class: &str, a: &mut std::collections::HashMap<String, String>| {
        let get = |k: &str| a.get(k).cloned().unwrap_or_default();
        match class {
            "inet" if !get("srvr").is_empty() => {
                let scheme = if get("ptcl") == "http" { "http" } else { "https" };
                let port = get("port").parse::<u32>().ok().filter(|p| *p != 0).map(|p| format!(":{p}")).unwrap_or_default();
                items.push(Item {
                    store: 0,
                    id: format!("inet{SEP}{}{SEP}{}", get("srvr"), get("acct")),
                    title: if get("labl").is_empty() { get("srvr") } else { get("labl") },
                    kind: Kind::Login,
                    urls: vec![format!("{scheme}://{}{port}", get("srvr"))],
                    username: Some(get("acct")).filter(|a| !a.is_empty()),
                    note: None,
                    labels: vec!["password".into()],
                });
            }
            "genp" if !get("svce").is_empty() => items.push(Item {
                store: 0,
                id: format!("genp{SEP}{}{SEP}{}", get("svce"), get("acct")),
                title: if get("labl").is_empty() { get("svce") } else { get("labl") },
                kind: Kind::Other,
                urls: vec![],
                username: Some(get("acct")).filter(|a| !a.is_empty()),
                note: Some(get("svce")),
                labels: vec!["password".into()],
            }),
            _ => {}
        }
        a.clear();
    };
    for line in s.lines() {
        if line.starts_with("keychain:") {
            flush(&class, &mut attrs);
            class.clear();
        } else if let Some(c) = line.strip_prefix("class: ") {
            class = c.trim().trim_matches('"').to_string();
        } else if let Some(rest) = line.trim_start().strip_prefix('"') {
            let Some((key, rest)) = rest.split_once('"') else { continue };
            let Some((_, val)) = rest.split_once('=') else { continue };
            attrs.insert(key.to_string(), attr_value(val.trim()));
        } else if let Some(rest) = line.trim_start().strip_prefix("0x00000007 ") {
            // The label attribute is printed by number.
            if let Some((_, val)) = rest.split_once('=') {
                attrs.insert("labl".into(), attr_value(val.trim()));
            }
        }
    }
    flush(&class, &mut attrs);
    items
}

/// `"text"`, `0x6A6F…  "jo…"` (hex wins: the quoted part is escaped) or `<NULL>`.
fn attr_value(v: &str) -> String {
    if let Some(hex) = v.strip_prefix("0x") {
        let hex: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()).collect();
        return String::from_utf8_lossy(&bytes).trim_end_matches('\0').to_string();
    }
    if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
        return v[1..v.len() - 1].to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump() {
        let s = r#"keychain: "/k.keychain-db"
version: 512
class: "inet"
attributes:
    0x00000007 <blob>="github.com (ada)"
    "acct"<blob>="ada"
    "port"<uint32>=0x00000000
    "ptcl"<uint32>="htps"
    "srvr"<blob>="github.com"
keychain: "/k.keychain-db"
class: "genp"
attributes:
    "acct"<blob>=0x6AC3B6 "j\303\266"
    "labl"<blob>=<NULL>
    "svce"<blob>="gh:github.com"
keychain: "/k.keychain-db"
class: 0x00000010
attributes:
    0x00000001 <blob>="x"
"#;
        let items = parse_dump(s);
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].kind, items[0].urls[0].as_str(), items[0].username.as_deref()), (Kind::Login, "https://github.com", Some("ada")));
        assert_eq!(items[0].title, "github.com (ada)");
        assert_eq!((items[1].title.as_str(), items[1].username.as_deref()), ("gh:github.com", Some("jö")));
    }
}
