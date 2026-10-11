//! Where the LLM and Jev live, read from the env-var names other tools
//! already use, so an existing setup plugs in unchanged.
//!
//! LLM (OpenAI-compatible chat completions): `OPENAI_BASE_URL` (+ optional
//! `OPENAI_API_KEY`), else `OPENROUTER_API_KEY`, else `OPENAI_API_KEY` on
//! api.openai.com. OpenRouter beats a bare `OPENAI_API_KEY`: a stray OpenAI
//! key in the shell must not silently switch providers.
//!
//! Jev (System One API): `SYSTEM_ONE_BASE_URL` (Pydantic AI's spelling;
//! `SYSTEMONE_BASE_URL` / `SYSTEMONE_ENDPOINT` also read) with an optional
//! `SYSTEM_ONE_API_KEY`, else a System One key on TypeSafe, else
//! `TYPESAFE_API_KEY` (+ `TYPESAFE_BASE_URL`), else `OPENROUTER_API_KEY`.

use crate::jev::{OPENROUTER_URL, TYPESAFE_URL};

pub const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
pub const OPENAI_BASE: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    /// The full request URL (`…/chat/completions` or `…/v1/systemone`).
    pub url: String,
    /// Empty for a keyless (local) server.
    pub key: String,
    /// The env var that picked this endpoint, for `fab doctor`.
    pub source: &'static str,
    /// OpenRouter takes extras (usage accounting, `/key`) others reject.
    pub openrouter: bool,
}

/// An env var, with an empty value read as unset.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn first(get: &impl Fn(&str) -> Option<String>, names: &[&'static str]) -> Option<(&'static str, String)> {
    names.iter().find_map(|n| get(n).map(|v| (*n, v)))
}

pub fn llm() -> Option<Endpoint> {
    llm_from(var)
}

pub fn jev() -> Option<Endpoint> {
    jev_from(var)
}

/// Jev's model: `FAB_JEV_MODEL`, then `SYSTEM_ONE_MODEL`, then `jev-latest`.
pub fn jev_model() -> String {
    first(&var, &["FAB_JEV_MODEL", "SYSTEM_ONE_MODEL", "SYSTEMONE_MODEL"]).map_or_else(|| "jev-latest".into(), |(_, m)| m)
}

fn llm_from(get: impl Fn(&str) -> Option<String>) -> Option<Endpoint> {
    let chat = |base: &str| join(base, "/chat/completions", "");
    if let Some(base) = get("OPENAI_BASE_URL") {
        let key = get("OPENAI_API_KEY").unwrap_or_default();
        return Some(Endpoint { openrouter: is_openrouter(&base), url: chat(&base), key, source: "OPENAI_BASE_URL" });
    }
    if let Some(key) = get("OPENROUTER_API_KEY") {
        return Some(Endpoint { url: chat(OPENROUTER_BASE), key, source: "OPENROUTER_API_KEY", openrouter: true });
    }
    let key = get("OPENAI_API_KEY")?;
    Some(Endpoint { url: chat(OPENAI_BASE), key, source: "OPENAI_API_KEY", openrouter: false })
}

fn jev_from(get: impl Fn(&str) -> Option<String>) -> Option<Endpoint> {
    let s1 = |base: &str| join(base, "/systemone", "/v1");
    let key = first(&get, &["SYSTEM_ONE_API_KEY", "SYSTEMONE_API_KEY"]);
    if let Some((source, base)) = first(&get, &["SYSTEM_ONE_BASE_URL", "SYSTEMONE_BASE_URL", "SYSTEMONE_ENDPOINT"]) {
        let key = key.map(|(_, k)| k).unwrap_or_default();
        return Some(Endpoint { openrouter: is_openrouter(&base), url: s1(&base), key, source });
    }
    if let Some((source, key)) = key {
        return Some(Endpoint { url: TYPESAFE_URL.into(), key, source, openrouter: false });
    }
    if let Some(key) = get("TYPESAFE_API_KEY") {
        let url = get("TYPESAFE_BASE_URL").map_or_else(|| TYPESAFE_URL.into(), |b| s1(&b));
        return Some(Endpoint { url, key, source: "TYPESAFE_API_KEY", openrouter: false });
    }
    let key = get("OPENROUTER_API_KEY")?;
    Some(Endpoint { url: OPENROUTER_URL.into(), key, source: "OPENROUTER_API_KEY", openrouter: true })
}

/// `base` + `path`, accepting a base that already ends in `path`, and
/// inserting `version` (e.g. `/v1`) when the base stops at the host.
fn join(base: &str, path: &str, version: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.ends_with(path) {
        b.to_string()
    } else if version.is_empty() || b.ends_with(version) {
        format!("{b}{path}")
    } else {
        format!("{b}{version}{path}")
    }
}

fn is_openrouter(url: &str) -> bool {
    url.contains("openrouter.ai")
}

/// The scheme and host of an endpoint, for display (never the key).
pub fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |n| m.iter().find(|(k, v)| k == n && !v.is_empty()).map(|(_, v)| v.clone())
    }

    #[test]
    fn llm_explicit_base_wins_and_may_be_keyless() {
        let e = llm_from(env(&[("OPENAI_BASE_URL", "http://localhost:11434/v1/"), ("OPENROUTER_API_KEY", "or")])).unwrap();
        assert_eq!(e.url, "http://localhost:11434/v1/chat/completions");
        assert_eq!(e.key, "");
        assert!(!e.openrouter);
    }

    #[test]
    fn llm_openrouter_beats_a_stray_openai_key() {
        let e = llm_from(env(&[("OPENAI_API_KEY", "oa"), ("OPENROUTER_API_KEY", "or")])).unwrap();
        assert_eq!(e.url, "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(e.key, "or");
        assert!(e.openrouter);
        let e = llm_from(env(&[("OPENAI_API_KEY", "oa")])).unwrap();
        assert_eq!(e.url, "https://api.openai.com/v1/chat/completions");
        assert!(llm_from(env(&[("OPENROUTER_API_KEY", "")])).is_none(), "empty is unset");
    }

    #[test]
    fn llm_full_url_is_kept_and_openrouter_base_is_recognized() {
        let e = llm_from(env(&[("OPENAI_BASE_URL", "https://openrouter.ai/api/v1/chat/completions"), ("OPENAI_API_KEY", "k")])).unwrap();
        assert_eq!(e.url, "https://openrouter.ai/api/v1/chat/completions");
        assert!(e.openrouter);
    }

    #[test]
    fn jev_base_url_forms_normalize() {
        for base in ["http://localhost:8093", "http://localhost:8093/", "http://localhost:8093/v1", "http://localhost:8093/v1/systemone"] {
            let e = jev_from(env(&[("SYSTEM_ONE_BASE_URL", base)])).unwrap();
            assert_eq!(e.url, "http://localhost:8093/v1/systemone", "{base}");
            assert_eq!(e.key, "");
        }
    }

    #[test]
    fn jev_precedence() {
        let e = jev_from(env(&[("SYSTEMONE_ENDPOINT", "https://opencode.ai/zen/v1/systemone"), ("SYSTEMONE_API_KEY", "s1"), ("TYPESAFE_API_KEY", "ts")])).unwrap();
        assert_eq!((e.url.as_str(), e.key.as_str(), e.source), ("https://opencode.ai/zen/v1/systemone", "s1", "SYSTEMONE_ENDPOINT"));
        let e = jev_from(env(&[("SYSTEM_ONE_API_KEY", "s1"), ("OPENROUTER_API_KEY", "or")])).unwrap();
        assert_eq!((e.url.as_str(), e.key.as_str()), (TYPESAFE_URL, "s1"));
        let e = jev_from(env(&[("TYPESAFE_API_KEY", "ts"), ("TYPESAFE_BASE_URL", "http://h:1"), ("OPENROUTER_API_KEY", "or")])).unwrap();
        assert_eq!((e.url.as_str(), e.key.as_str()), ("http://h:1/v1/systemone", "ts"));
        let e = jev_from(env(&[("OPENROUTER_API_KEY", "or")])).unwrap();
        assert_eq!(e.url, OPENROUTER_URL);
        assert!(e.openrouter);
        assert!(jev_from(env(&[("OPENAI_API_KEY", "oa")])).is_none());
    }

    #[test]
    fn host_strips_scheme_and_path() {
        assert_eq!(host("https://api.typesafe.ai/v1/systemone"), "api.typesafe.ai");
        assert_eq!(host("http://localhost:8093/v1"), "localhost:8093");
    }
}
