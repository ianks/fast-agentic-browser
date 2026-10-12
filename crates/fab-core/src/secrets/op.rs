//! 1Password through its CLI (`op`). The first call of a session may wait
//! for the user to approve fab in the 1Password app.

use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;
use zeroize::Zeroizing;

use super::store::{Field, Full, Item, Kind, Status, month_year, run};

/// Long enough for the user to notice and approve the 1Password prompt.
const WAIT: Duration = Duration::from_secs(90);

pub struct Op {
    pub account: Option<String>,
    /// Where new logins go (default: the account's default vault). A
    /// service account also reads only from it.
    pub vault: Option<String>,
    /// Signed in with `OP_SERVICE_ACCOUNT_TOKEN`: no accounts to list, no
    /// `--account`, and every item call names its vault.
    pub service: bool,
    /// The program (`op`; a stub in tests).
    pub bin: String,
    /// A service account's vault when none is configured: its only one.
    found: tokio::sync::OnceCell<String>,
}

impl Op {
    /// `[secrets]` op_account/op_vault, overridden by `FAB_OP_ACCOUNT` /
    /// `FAB_OP_VAULT`; a service account when `OP_SERVICE_ACCOUNT_TOKEN` is set.
    pub fn new(account: Option<String>, vault: Option<String>) -> Self {
        let env = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
        Self {
            account: env("FAB_OP_ACCOUNT").or(account),
            vault: env("FAB_OP_VAULT").or(vault),
            service: env("OP_SERVICE_ACCOUNT_TOKEN").is_some(),
            bin: "op".into(),
            found: Default::default(),
        }
    }

    fn args<'a>(&'a self, base: &[&'a str]) -> Vec<&'a str> {
        let mut a = base.to_vec();
        if let (Some(acc), false) = (&self.account, self.service) {
            a.extend(["--account", acc.as_str()]);
        }
        a
    }

    /// `item` call args: a service account's vault named on every one.
    async fn item_args<'a>(&'a self, base: &[&'a str]) -> Result<Vec<&'a str>> {
        let mut a = self.args(base);
        if self.service {
            a.extend(["--vault", self.read_vault().await?]);
        }
        Ok(a)
    }

    /// The vault a service account reads: the configured one, else the
    /// only vault it can see (service account tokens are usually minted
    /// for one vault).
    async fn read_vault(&self) -> Result<&str> {
        if let Some(v) = &self.vault {
            return Ok(v);
        }
        self.found
            .get_or_try_init(|| async {
                let out = run(&self.bin, &["vault", "list", "--format", "json"], None, &[], Duration::from_secs(20)).await?;
                let vaults: Vec<Value> = serde_json::from_slice(&out).context("unexpected `op vault list` output")?;
                match vaults.as_slice() {
                    [v] => v["name"].as_str().or(v["id"].as_str()).map(str::to_string).context("a vault without a name"),
                    [] => anyhow::bail!("the service account can see no vault"),
                    _ => anyhow::bail!("the service account can see {} vaults: set FAB_OP_VAULT to the one fab should use", vaults.len()),
                }
            })
            .await
            .map(String::as_str)
    }

    pub async fn status(&self) -> Status {
        if self.service {
            return match run(&self.bin, &["whoami", "--format", "json"], None, &[], Duration::from_secs(10)).await {
                Ok(_) => match self.read_vault().await {
                    Ok(v) => Status { ready: true, note: format!("service account · vault {v}") },
                    Err(e) => Status { ready: false, note: format!("service account: {e:#}") },
                },
                Err(e) => Status { ready: false, note: format!("service account (OP_SERVICE_ACCOUNT_TOKEN): {e:#}") },
            };
        }
        match run(&self.bin, &self.args(&["account", "list", "--format", "json"]), None, &[], Duration::from_secs(10)).await {
            Ok(out) => {
                let n = serde_json::from_slice::<Vec<Value>>(&out).map(|v| v.len()).unwrap_or(0);
                if n == 0 {
                    Status { ready: false, note: "no account: set OP_SERVICE_ACCOUNT_TOKEN (servers, containers), or run `op signin` or turn on the 1Password app's CLI integration".into() }
                } else {
                    Status { ready: true, note: format!("{n} account{} (1Password may ask you to approve fab)", if n == 1 { "" } else { "s" }) }
                }
            }
            Err(e) => Status { ready: false, note: format!("{e:#}") },
        }
    }

    pub async fn list(&self) -> Result<Vec<Item>> {
        let out = run(&self.bin, &self.item_args(&["item", "list", "--format", "json"]).await?, None, &[], WAIT).await?;
        let items: Vec<Value> = serde_json::from_slice(&out).context("unexpected `op item list` output")?;
        Ok(items.iter().filter_map(meta).collect())
    }

    pub async fn full(&self, it: &Item) -> Result<Full> {
        let out = run(&self.bin, &self.item_args(&["item", "get", &it.id, "--format", "json"]).await?, None, &[], WAIT).await?;
        let v: Value = serde_json::from_slice(&out).context("unexpected `op item get` output")?;
        Ok(parse_full(&v))
    }

    pub async fn otp(&self, it: &Item) -> Result<Zeroizing<String>> {
        let out = run(&self.bin, &self.item_args(&["item", "get", &it.id, "--otp"]).await?, None, &[], WAIT).await?;
        Ok(Zeroizing::new(String::from_utf8_lossy(&out).trim().to_string()))
    }

    pub async fn save_login(&self, url: &str, title: &str, username: &str, password: &str) -> Result<String> {
        // The item goes in on stdin, so the password is never in argv.
        let tmpl = serde_json::json!({
            "title": title,
            "category": "LOGIN",
            "fields": [
                {"id": "username", "type": "STRING", "purpose": "USERNAME", "label": "username", "value": username},
                {"id": "password", "type": "CONCEALED", "purpose": "PASSWORD", "label": "password", "value": password},
            ],
            "urls": [{"href": url, "primary": true}],
        });
        let body = Zeroizing::new(serde_json::to_vec(&tmpl)?);
        let mut base = vec!["item", "create", "--format", "json"];
        if self.service {
            base.extend(["--vault", self.read_vault().await?]);
        } else if let Some(v) = &self.vault {
            base.extend(["--vault", v.as_str()]);
        }
        base.push("-");
        let out = run(&self.bin, &self.args(&base), Some(&body), &[], WAIT).await?;
        let v: Value = serde_json::from_slice(&out).unwrap_or_default();
        let vault = v["vault"]["name"].as_str().map(|n| format!(" in {n}")).unwrap_or_default();
        Ok(format!("1Password \"{title}\"{vault}"))
    }
}

fn meta(v: &Value) -> Option<Item> {
    let kind = match v["category"].as_str()? {
        "LOGIN" => Kind::Login,
        "CREDIT_CARD" => Kind::Card,
        "IDENTITY" => Kind::Identity,
        _ => Kind::Other,
    };
    let urls = v["urls"].as_array().into_iter().flatten().filter_map(|u| u["href"].as_str().map(str::to_string)).collect();
    let info = v["additional_information"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let vault = v["vault"]["name"].as_str().unwrap_or_default();
    let (username, note) = match kind {
        Kind::Login => (info, Some(vault.to_string())),
        _ => (None, Some([info.unwrap_or_default(), vault.to_string()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" · "))),
    };
    Some(Item { store: 0, id: v["id"].as_str()?.to_string(), title: v["title"].as_str().unwrap_or_default().to_string(), kind, urls, username, note, labels: vec![] })
}

/// Standard names for 1Password's built-in field ids (Login, Credit Card, Identity).
fn token(id: &str, purpose: &str, ty: &str) -> Option<&'static str> {
    Some(match (purpose, id) {
        ("USERNAME", _) => "username",
        ("PASSWORD", _) => "current-password",
        (_, "ccnum") => "cc-number",
        (_, "cvv") => "cc-csc",
        (_, "expiry") => "cc-exp",
        (_, "cardholder") => "cc-name",
        (_, "type") if ty == "CREDIT_CARD_TYPE" => "cc-type",
        (_, "firstname") => "given-name",
        (_, "initial") => "additional-name",
        (_, "lastname") => "family-name",
        (_, "address") => "street-address",
        (_, "defphone" | "cellphone") => "tel",
        (_, "email") => "email",
        (_, "company") => "organization",
        (_, "birthdate") => "bday",
        _ => return None,
    })
}

pub(crate) fn parse_full(v: &Value) -> Full {
    let mut fields = vec![];
    let mut totp = false;
    for f in v["fields"].as_array().into_iter().flatten() {
        let ty = f["type"].as_str().unwrap_or_default();
        if ty == "OTP" {
            totp = true;
            continue;
        }
        let id = f["id"].as_str().unwrap_or_default();
        let purpose = f["purpose"].as_str().unwrap_or_default();
        if purpose == "NOTES" {
            continue;
        }
        let raw = match &f["value"] {
            Value::String(s) => s.clone(),
            Value::Null => continue,
            other => other.to_string(),
        };
        let mut tok = token(id, purpose, ty);
        let mut value = raw;
        if tok == Some("cc-exp") {
            if let Some((m, y)) = month_year(&value) {
                value = format!("{m:02}/{y}");
            }
        }
        // A second phone field doesn't override the default one.
        if tok == Some("tel") && fields.iter().any(|x: &Field| x.token == Some("tel")) {
            tok = None;
        }
        let concealed = matches!(ty, "CONCEALED" | "CREDIT_CARD_NUMBER") || purpose == "PASSWORD";
        fields.push(Field { label: f["label"].as_str().unwrap_or(id).to_string(), token: tok, concealed, value: Zeroizing::new(value) });
    }
    Full { fields, totp }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An `op` that logs its argv and answers `vault list` with `vaults`.
    #[cfg(unix)]
    fn stub(dir: &std::path::Path, vaults: &str) -> (String, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join("argv.log");
        let bin = dir.join("op");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1 $2\" in\n  'vault list') echo '{vaults}' ;;\n  'item list') echo '[]' ;;\n  *) echo '{{}}' ;;\nesac\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        (bin.display().to_string(), log)
    }

    #[cfg(unix)]
    fn op(bin: String, account: Option<&str>, vault: Option<&str>, service: bool) -> Op {
        Op { account: account.map(Into::into), vault: vault.map(Into::into), service, bin, found: Default::default() }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_service_account_reads_its_only_vault() {
        let dir = std::env::temp_dir().join(format!("fab-op-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let (bin, log) = stub(&dir, r#"[{"id":"v1","name":"Agents"}]"#);
        let o = op(bin, Some("my.1password.com"), None, true);
        let st = o.status().await;
        assert!(st.ready, "{}", st.note);
        assert_eq!(st.note, "service account · vault Agents");
        o.list().await.unwrap();
        let it = Item { id: "i1".into(), ..meta(&json!({"id": "i1", "title": "t", "category": "LOGIN"})).unwrap() };
        o.full(&it).await.unwrap();
        let argv = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            ["whoami --format json", "vault list --format json", "item list --format json --vault Agents", "item get i1 --format json --vault Agents"],
            "account list is never run, --account never passed, and the vault is listed once"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_service_account_with_many_vaults_needs_one_named() {
        let dir = std::env::temp_dir().join(format!("fab-op-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let (bin, log) = stub(&dir, r#"[{"name":"A"},{"name":"B"}]"#);
        let st = op(bin.clone(), None, None, true).status().await;
        assert!(!st.ready && st.note.contains("FAB_OP_VAULT"), "{}", st.note);
        let named = op(bin, None, Some("B"), true);
        named.list().await.unwrap();
        assert!(std::fs::read_to_string(&log).unwrap().ends_with("item list --format json --vault B\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_signed_in_user_reads_every_vault() {
        let dir = std::env::temp_dir().join(format!("fab-op-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let (bin, log) = stub(&dir, "[]");
        op(bin, Some("me"), Some("Saved"), false).list().await.unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "item list --format json --account me\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_and_fields() {
        let it = meta(&json!({"id": "a1", "title": "GitHub", "category": "LOGIN", "additional_information": "ada", "urls": [{"href": "https://github.com/login", "primary": true}], "vault": {"name": "Private"}})).unwrap();
        assert_eq!((it.kind, it.username.as_deref(), it.urls[0].as_str()), (Kind::Login, Some("ada"), "https://github.com/login"));
        let card = meta(&json!({"id": "c1", "title": "Personal Visa", "category": "CREDIT_CARD", "additional_information": "*4242", "vault": {"name": "Private"}})).unwrap();
        assert_eq!(card.note.as_deref(), Some("*4242 · Private"));
        let f = parse_full(&json!({"fields": [
            {"id": "username", "type": "STRING", "purpose": "USERNAME", "label": "username", "value": "ada"},
            {"id": "password", "type": "CONCEALED", "purpose": "PASSWORD", "label": "password", "value": "pw"},
            {"id": "x", "type": "OTP", "label": "one-time password", "value": "otpauth://…"},
            {"id": "expiry", "type": "MONTH_YEAR", "label": "expiry date", "value": "202712"},
            {"id": "k1", "type": "CONCEALED", "label": "test secret key", "value": "sk_test_1"},
            {"id": "notesPlain", "type": "STRING", "purpose": "NOTES", "label": "notesPlain", "value": "n"}
        ]}));
        assert!(f.totp);
        assert_eq!(f.get("current-password").map(|x| x.value.as_str()), Some("pw"));
        assert_eq!(f.get("cc-exp").map(|x| x.value.as_str()), Some("12/2027"));
        let k = f.fields.iter().find(|x| x.label == "test secret key").unwrap();
        assert!(k.concealed && k.token.is_none());
        assert_eq!(f.fields.len(), 4);
    }
}
