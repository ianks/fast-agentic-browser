//! Bitwarden through its CLI (`bw`). The vault must be unlocked, with
//! `BW_SESSION` in the environment fab's session started from.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::Duration;
use zeroize::Zeroizing;

use super::store::{Field, Full, Item, Kind, Status, run};

const WAIT: Duration = Duration::from_secs(60);

pub struct Bw;

impl Bw {
    pub async fn status(&self) -> Status {
        match run("bw", &["status"], None, &[], Duration::from_secs(15)).await {
            Ok(out) => {
                let v: Value = serde_json::from_slice(&out).unwrap_or_default();
                match v["status"].as_str().unwrap_or("unknown") {
                    "unlocked" => Status { ready: true, note: "unlocked".into() },
                    "locked" => Status { ready: false, note: "locked: run `export BW_SESSION=$(bw unlock --raw)`, then start the fab session from that shell".into() },
                    s => Status { ready: false, note: format!("{s}: run `bw login`") },
                }
            }
            Err(e) => Status { ready: false, note: format!("{e:#}") },
        }
    }

    pub async fn list(&self) -> Result<Vec<Item>> {
        // `bw list` returns whole items; only metadata is kept.
        let out = run("bw", &["list", "items"], None, &[], WAIT).await?;
        let items: Vec<Value> = serde_json::from_slice(&out).context("unexpected `bw list items` output")?;
        Ok(items.iter().filter_map(meta).collect())
    }

    pub async fn full(&self, it: &Item) -> Result<Full> {
        let out = run("bw", &["get", "item", &it.id], None, &[], WAIT).await?;
        let v: Value = serde_json::from_slice(&out).context("unexpected `bw get item` output")?;
        Ok(parse_full(&v))
    }

    pub async fn otp(&self, it: &Item) -> Result<Zeroizing<String>> {
        let out = run("bw", &["get", "totp", &it.id], None, &[], WAIT).await?;
        Ok(Zeroizing::new(String::from_utf8_lossy(&out).trim().to_string()))
    }

    pub async fn save_login(&self, url: &str, title: &str, username: &str, password: &str) -> Result<String> {
        let item = json!({
            "type": 1, "name": title, "notes": null, "favorite": false, "fields": [], "folderId": null, "organizationId": null,
            "login": {"uris": [{"match": null, "uri": url}], "username": username, "password": password, "totp": null},
        });
        use base64::Engine;
        // `bw create item` reads the encoded item from stdin: never in argv.
        let enc = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&item)?));
        let out = run("bw", &["create", "item"], Some(enc.as_bytes()), &[], WAIT).await?;
        if serde_json::from_slice::<Value>(&out).map(|v| v["id"].is_string()).unwrap_or(false) {
            Ok(format!("Bitwarden \"{title}\""))
        } else {
            bail!("Bitwarden did not confirm the new item")
        }
    }
}

fn meta(v: &Value) -> Option<Item> {
    let kind = match v["type"].as_u64()? {
        1 => Kind::Login,
        3 => Kind::Card,
        4 => Kind::Identity,
        _ => Kind::Other,
    };
    let urls = v["login"]["uris"].as_array().into_iter().flatten().filter_map(|u| u["uri"].as_str().map(str::to_string)).collect();
    let note = match kind {
        Kind::Card => {
            let n = v["card"]["number"].as_str().unwrap_or_default();
            let brand = v["card"]["brand"].as_str().unwrap_or_default();
            Some(format!("{brand} *{}", &n[n.len().saturating_sub(4)..]).trim().to_string())
        }
        _ => None,
    };
    let labels = v["fields"].as_array().into_iter().flatten().filter_map(|f| f["name"].as_str().map(str::to_string)).collect();
    Some(Item {
        store: 0,
        id: v["id"].as_str()?.to_string(),
        title: v["name"].as_str().unwrap_or_default().to_string(),
        kind,
        urls,
        username: v["login"]["username"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
        note,
        labels,
    })
}

pub(crate) fn parse_full(v: &Value) -> Full {
    let mut fields = vec![];
    let mut add = |label: &str, token: Option<&'static str>, concealed: bool, val: &Value| {
        let s = match val {
            Value::String(s) if !s.is_empty() => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return,
        };
        fields.push(Field { label: label.to_string(), token, concealed, value: Zeroizing::new(s) });
    };
    let l = &v["login"];
    add("username", Some("username"), false, &l["username"]);
    add("password", Some("current-password"), true, &l["password"]);
    let c = &v["card"];
    add("number", Some("cc-number"), true, &c["number"]);
    add("security code", Some("cc-csc"), true, &c["code"]);
    add("cardholder name", Some("cc-name"), false, &c["cardholderName"]);
    add("brand", Some("cc-type"), false, &c["brand"]);
    add("expiration month", Some("cc-exp-month"), false, &c["expMonth"]);
    add("expiration year", Some("cc-exp-year"), false, &c["expYear"]);
    let i = &v["identity"];
    for (k, label, tok) in [
        ("firstName", "first name", "given-name"),
        ("middleName", "middle name", "additional-name"),
        ("lastName", "last name", "family-name"),
        ("address1", "address 1", "address-line1"),
        ("address2", "address 2", "address-line2"),
        ("city", "city", "address-level2"),
        ("state", "state", "address-level1"),
        ("postalCode", "postal code", "postal-code"),
        ("country", "country", "country-name"),
        ("company", "company", "organization"),
        ("email", "email", "email"),
        ("phone", "phone", "tel"),
    ] {
        add(label, Some(tok), false, &i[k]);
    }
    for f in v["fields"].as_array().into_iter().flatten() {
        // Custom field types: 0 text, 1 hidden, 2 boolean.
        add(f["name"].as_str().unwrap_or_default(), None, f["type"].as_u64() == Some(1), &f["value"]);
    }
    Full { fields, totp: l["totp"].as_str().is_some_and(|t| !t.is_empty()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items() {
        let login = json!({"id": "i1", "type": 1, "name": "AWS work", "login": {"uris": [{"uri": "https://console.aws.amazon.com"}], "username": "ops@acme.com", "password": "pw", "totp": "otpauth://x"}, "fields": [{"name": "account id", "value": "1234", "type": 0}]});
        let it = meta(&login).unwrap();
        assert_eq!((it.kind, it.username.as_deref(), it.labels.len()), (Kind::Login, Some("ops@acme.com"), 1));
        let f = parse_full(&login);
        assert!(f.totp);
        assert_eq!(f.get("current-password").unwrap().value.as_str(), "pw");
        let card = json!({"id": "c", "type": 3, "name": "Visa", "card": {"number": "4242424242424242", "brand": "Visa", "expMonth": "3", "expYear": "2029", "code": "123"}});
        assert_eq!(meta(&card).unwrap().note.as_deref(), Some("Visa *4242"));
        assert!(parse_full(&card).get("cc-csc").unwrap().concealed);
    }
}
