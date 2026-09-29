//! Signing in with the user's saved login, inside a step: "open acme.dev and
//! log in", or any step that lands on a sign-in form. The username, password
//! and one-time code come from the password manager (`fab_core::secrets`) and
//! no model sees them.

use anyhow::{Result, bail};
use fab_core::Session;
use fab_core::secrets;
use fab_core::snapshot::Snapshot;
use std::time::{Duration, Instant};

#[derive(Debug, Default, serde::Deserialize)]
struct LoginFields {
    user: Option<usize>,
    pass: Option<usize>,
    otp: Option<usize>,
    submit: Option<usize>,
    signin: Option<usize>,
    /// The way from a passkey or push step to an authenticator-app code.
    alt: Option<usize>,
    #[serde(rename = "inForm", default)]
    in_form: bool,
    #[serde(rename = "userValue", default)]
    user_value: String,
}

impl LoginFields {
    fn any(&self) -> bool {
        self.user.is_some() || self.pass.is_some() || self.otp.is_some()
    }
}

async fn fields(sess: &Session) -> LoginFields {
    sess.browser.eval("__ub.loginFields()").await.ok().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

/// Whether a step asks to sign in.
pub fn asks_signin(step: &str) -> bool {
    let l = step.to_lowercase();
    ["log in", "log into", "login", "sign in", "sign into", "signin", "log me in", "sign me in", "authenticate"].iter().any(|w| {
        l.match_indices(w).any(|(i, _)| {
            let before = l[..i].chars().last().is_none_or(|c| !c.is_alphanumeric());
            let after = l[i + w.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
            before && after
        })
    })
}

/// Whether signing in is all the step asks ("log in", "sign in to github.com
/// with my saved login", "log in as ops@acme.com").
pub fn only_signin(step: &str) -> bool {
    let mut l = step.trim().trim_end_matches(['.', '!']).to_lowercase();
    for p in ["please ", "now "] {
        if let Some(r) = l.strip_prefix(p) {
            l = r.to_string();
        }
    }
    let Some(rest) = ["log in", "login", "sign in", "log me in", "sign me in"].iter().find_map(|p| l.strip_prefix(p).map(str::to_string)) else { return false };
    let mut words: Vec<&str> = rest.split_whitespace().collect();
    // Allowed tails: "to <site>", "as <user>", "with/using my saved login|credentials|password|account".
    while !words.is_empty() {
        match words.as_slice() {
            ["to" | "into" | "on" | "at" | "as", _, tail @ ..] => words = tail.to_vec(),
            ["with" | "using", tail @ ..] => {
                let t: Vec<&str> = tail.iter().copied().skip_while(|w| matches!(*w, "my" | "the" | "a" | "saved" | "stored" | "usual")).collect();
                return t.len() == 1 && matches!(t[0], "login" | "credentials" | "password" | "account" | "details");
            }
            _ => return false,
        }
    }
    true
}

/// What a step asks beyond signing in: "log in to github.com as ianks and make
/// the repo public" → "make the repo public". The step itself when it doesn't
/// start by signing in.
pub fn after_signin(step: &str) -> String {
    let t = step.trim();
    let l = t.to_lowercase();
    let Some(p) = ["log in", "login", "sign in", "log me in", "sign me in"].iter().find(|p| l.starts_with(**p)) else { return t.to_string() };
    // The sign-in clause ends at the first "and", "then", "," or ";" after it.
    let rest = &l[p.len()..];
    let cut = [" and then ", ", then ", " then ", " and ", ", ", "; "].iter().filter_map(|sep| rest.find(sep).map(|i| (i, sep.len()))).min_by_key(|x| x.0);
    match cut {
        Some((i, n)) => t[p.len() + i + n..].trim().to_string(),
        None => String::new(),
    }
}

/// "log in as ops@acme.com" → the account to use.
fn username_in(step: &str) -> Option<String> {
    let l = step.to_lowercase();
    let i = l.find(" as ")?;
    let w = step[i + 4..].split_whitespace().next()?.trim_matches(|c: char| c == '"' || c == '\'' || c == ',' || c == '.');
    (w.contains('@') || !["a", "an", "the", "my", "admin"].contains(&w)).then(|| w.to_string()).filter(|w| !w.is_empty())
}

/// Signs in first when the step asks for it or the page is a sign-in form,
/// and a saved login exists for the site. A step that brings its own
/// credentials ('log in as "ada" with password "…"') is left to the engine.
/// Returns what happened, or None when there was nothing to do.
pub async fn before_step(sess: &mut Session, step: &str) -> Result<Option<String>> {
    let asked = asks_signin(step);
    if asked && !fab_core::spans::quoted(step).is_empty() && step.to_lowercase().contains("password") {
        return Ok(None);
    }
    let here = sess.browser.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    if here.is_empty() || here.starts_with("about:") {
        return Ok(None);
    }
    let f = fields(sess).await;
    // A sign-in form on the page counts only with a password or code field
    // (an email box alone may be a newsletter).
    let form = f.pass.is_some() || f.otp.is_some();
    if !asked && !form {
        return Ok(None);
    }
    let origin = origin_of(&here);
    let vault = secrets::vault();
    let saved = vault.logins_for(&origin).await.unwrap_or_default();
    if saved.is_empty() {
        return Ok(None);
    }
    sign_in(sess, &origin, username_in(step).as_deref()).await.map(Some)
}

fn origin_of(url: &str) -> String {
    match url.split_once("://") {
        Some((s, rest)) => format!("{s}://{}", rest.split(['/', '?', '#']).next().unwrap_or_default()),
        None => url.to_string(),
    }
}

const REJECTED: &[&str] = &["incorrect", "invalid", "wrong password", "wrong username", "doesn't match", "does not match", "not recognized", "couldn't find your", "could not find your", "try again", "failed to sign in", "login failed"];
const APPROVE: &[&str] = &["approve", "check your phone", "check your device", "sent a notification", "sent a push", "security key", "passkey", "confirm it's you", "confirm it is you", "tap yes", "open the app"];

fn page_says<'a>(snap: &'a Snapshot, words: &[&str]) -> Option<&'a str> {
    snap.texts.iter().map(|t| t.x.as_str()).find(|x| {
        let l = x.to_lowercase();
        x.len() < 300 && words.iter().any(|w| l.contains(w))
    })
}

/// Signs in with the saved login for `origin`: username, password and
/// one-time code, over as many pages as the site uses.
async fn sign_in(sess: &mut Session, origin: &str, username: Option<&str>) -> Result<String> {
    let wait = Duration::from_secs(90);
    let item = secrets::vault().choose_login(origin, username).await?;
    let store = secrets::vault().stores[item.store].name();
    let who = item.username.clone().unwrap_or_default();
    let mode = sess.k.exec;
    let k = sess.k.clone();
    let mut steps: Vec<String> = vec![];
    let mut last_keys: Option<(Option<usize>, Option<usize>, Option<usize>)> = None;
    let mut switches = 0;
    let has_otp = secrets::vault().has_otp(&item).await;
    for _ in 0..10 {
        let f = fields(sess).await;
        let keys = (f.user, f.pass, f.otp);
        if !f.any() {
            if let (Some(s), true) = (f.signin, steps.is_empty()) {
                let doc = sess.browser.doc_id().await.unwrap_or_default();
                sess.browser.click(s, mode).await?;
                let _ = sess.browser.settle(&k, &doc).await;
                steps.push("opened the sign-in form".into());
                continue;
            }
            // A passkey or push step, and the login has a one-time code: take the code route.
            if let (Some(a), true, true) = (f.alt, has_otp, switches < 3) {
                let doc = sess.browser.doc_id().await.unwrap_or_default();
                sess.browser.click(a, mode).await?;
                let _ = sess.browser.settle(&k, &doc).await;
                if switches == 0 {
                    steps.push("chose the authenticator code over the passkey".into());
                }
                switches += 1;
                continue;
            }
            // A push or security-key step: wait for the user.
            sess.invalidate();
            let asks = sess.snapshot().await.ok().and_then(|s| page_says(s, APPROVE).map(str::to_string));
            if let Some(t) = asks {
                if let Some(f) = &sess.live {
                    f(format!("waiting for you to approve the sign-in: {t}"));
                }
                let t0 = Instant::now();
                let url0 = sess.browser.eval("location.href").await.unwrap_or_default();
                while t0.elapsed() < wait {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let url = sess.browser.eval("location.href").await.unwrap_or_default();
                    sess.invalidate();
                    let still = sess.snapshot().await.ok().and_then(|s| page_says(s, APPROVE).map(str::to_string));
                    if url != url0 || still.is_none() {
                        break;
                    }
                }
                steps.push("waited for the sign-in to be approved".into());
                if fields(sess).await.any() {
                    continue;
                }
                sess.invalidate();
                if sess.snapshot().await.ok().and_then(|s| page_says(s, APPROVE)).is_some() {
                    bail!("the sign-in is waiting for approval on your device (gave up after {:.0} s)", wait.as_secs_f64());
                }
            }
            break;
        }
        if last_keys == Some(keys) {
            // Submitting didn't move on: the site said why, or it needs something else.
            sess.invalidate();
            match sess.snapshot().await.ok().and_then(|s| page_says(s, REJECTED).map(str::to_string)) {
                Some(w) => bail!("the site did not accept the sign-in: \"{w}\""),
                None => bail!("the sign-in form is still there after submitting (a captcha or another step may need you)"),
            }
        }
        last_keys = Some(keys);
        let doc = sess.browser.doc_id().await.unwrap_or_default();
        let mut typed = vec![];
        if let Some(o) = f.otp {
            sess.browser.fill(o, "{{one-time code}}", mode).await?;
            typed.push((o, "the one-time code"));
        } else {
            if let Some(u) = f.user {
                if who.is_empty() || f.user_value.trim().to_lowercase() != who.to_lowercase() {
                    sess.browser.fill(u, "{{username}}", mode).await?;
                    typed.push((u, "the username"));
                }
            }
            if let Some(p) = f.pass {
                sess.browser.fill(p, "{{password}}", mode).await?;
                typed.push((p, "the password"));
            }
        }
        let _ = secrets::vault().take_log().await;
        let last = typed.last().map(|t| t.0).or(f.user);
        match (f.in_form, last, f.submit) {
            (true, Some(l), _) => sess.browser.enter(Some(l), mode).await?,
            (false, _, Some(b)) => sess.browser.click(b, mode).await?,
            (_, Some(l), None) => sess.browser.enter(Some(l), mode).await?,
            _ => {}
        }
        steps.push(format!("entered {}", typed.iter().map(|t| t.1).collect::<Vec<_>>().join(" and ")));
        let _ = sess.browser.settle(&k, &doc).await;
    }
    sess.invalidate();
    let site = secrets::site_of(origin);
    if steps.is_empty() {
        return Ok(format!("Already signed in to {site}."));
    }
    let as_who = if who.is_empty() { String::new() } else { format!(" as {who}") };
    Ok(format!("Signed in to {site}{as_who} with {store} \"{}\" ({}).", item.title, steps.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signin_steps() {
        assert!(asks_signin("open acme.dev and log in"));
        assert!(asks_signin("Sign in, then open the billing page"));
        assert!(!asks_signin("change the display name"));
        assert!(only_signin("log in"));
        assert!(only_signin("Please sign in to github.com with my saved login."));
        assert!(only_signin("log in as ops@acme.com"));
        assert!(!only_signin("log in and cancel order 1042"));
        assert!(!only_signin("sign in to github.com and star the repo"));
        assert_eq!(username_in("log in as ops@acme.com").as_deref(), Some("ops@acme.com"));
        assert_eq!(username_in("log in"), None);
        assert_eq!(after_signin("log in to github.com as ianks and make the ianks/fab repo public"), "make the ianks/fab repo public");
        assert_eq!(after_signin("Sign in, then open the billing page"), "open the billing page");
        assert_eq!(after_signin("log in"), "");
        assert_eq!(after_signin("cancel order 1042"), "cancel order 1042");
    }
}
