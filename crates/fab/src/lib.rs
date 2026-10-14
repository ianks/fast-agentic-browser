//! fab's front ends: the agent surface (`fab do`, `fab run`, CLI sessions)
//! and the benchmark harness (`fab-bench`).

pub mod api;
pub mod events;
pub mod request;
pub mod autopilot;
pub mod bench;
pub mod compile;
pub mod decisions;
pub mod evals;
pub mod mcp_client;
pub mod planner;
pub mod pool;
pub mod race;
pub mod scrape;
pub mod scrape_gen;
mod pyrand;
pub mod server;
#[cfg(unix)]
pub mod session;
pub mod signin;
pub mod tools;
pub mod task_store;

use std::path::PathBuf;

/// The agent skill (`fab --skill`).
pub const SKILL: &str = include_str!("../SKILL.md");

pub fn trunc(s: &str, n: usize) -> String {
    fab_core::snapshot::truncate(s, n)
}

/// fab's dotenv file: `env` in fab's config dir (`$FAB_CONFIG`, else
/// `$XDG_CONFIG_HOME/fab`, else `~/.config/fab`).
pub fn config_env() -> Option<PathBuf> {
    Some(fab_core::paths::config_dir().join("env"))
}

/// fab's config file: `config.toml` in fab's config dir.
pub fn config_file() -> PathBuf {
    fab_core::secrets::config_path()
}

/// Settings resolve as flags, then the environment, then `env`, then
/// `config.toml`, then (in a source checkout) the repo's .env. Each layer
/// only fills variables the ones above left unset. A `config.toml` that
/// does not parse is an error, not silently skipped.
pub fn load_env() -> anyhow::Result<()> {
    // A remote client reads no config: keys and settings are fab serve's.
    if matches!(std::env::var("FAB_REMOTE").as_deref(), Ok("1" | "true" | "yes" | "on")) {
        return Ok(());
    }
    if let Some(p) = config_env() {
        let _ = dotenvy::from_path(p);
    }
    let path = config_file();
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let cfg: ConfigFile = toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            if cfg.jev.vision_model.is_some() {
                anyhow::bail!("{}: vision_model goes in [llm], not [jev]", path.display());
            }
            for (k, v) in cfg.vars() {
                if std::env::var_os(&k).is_none() {
                    // SAFETY: startup, before fab spawns any thread of its own
                    // (as dotenvy does above).
                    unsafe { std::env::set_var(&k, v) };
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!("{}: {e}", path.display()),
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    let _ = dotenvy::from_path(root);
    Ok(())
}

/// `config.toml`. Each key is another spelling of an environment variable
/// (named beside it), so the file, the environment and flags never disagree
/// about what a setting means.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    llm: EndpointConfig,
    #[serde(default)]
    jev: EndpointConfig,
    #[serde(default)]
    browser: BrowserConfig,
    /// Any other variable, verbatim (like Cargo's `[env]`).
    #[serde(default)]
    env: std::collections::BTreeMap<String, toml::Value>,
    /// Read by `fab_core::secrets`.
    #[serde(default)]
    #[allow(dead_code)]
    secrets: Option<toml::Table>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointConfig {
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    /// `[llm]` only: the model that reads screenshots.
    vision_model: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserConfig {
    headless: Option<bool>,
    profile: Option<String>,
    isolated: Option<bool>,
    connect: Option<String>,
    idle: Option<u64>,
}

impl ConfigFile {
    fn vars(&self) -> Vec<(String, String)> {
        let (l, j, b) = (&self.llm, &self.jev, &self.browser);
        let mut v: Vec<(String, String)> = [
            ("OPENAI_BASE_URL", l.base_url.clone()),
            ("OPENAI_API_KEY", l.api_key.clone()),
            ("FAB_MODEL", l.model.clone()),
            ("FAB_VISION_MODEL", l.vision_model.clone()),
            ("SYSTEM_ONE_BASE_URL", j.base_url.clone()),
            ("SYSTEM_ONE_API_KEY", j.api_key.clone()),
            ("FAB_JEV_MODEL", j.model.clone()),
            ("FAB_HEADLESS", b.headless.map(|x| x.to_string())),
            ("FAB_PROFILE", b.profile.clone()),
            ("FAB_ISOLATED", b.isolated.map(|x| x.to_string())),
            ("FAB_CONNECT", b.connect.clone()),
            ("FAB_IDLE", b.idle.map(|x| x.to_string())),
        ]
        .into_iter()
        .filter_map(|(k, v)| Some((k.to_string(), v?)))
        .collect();
        for (k, val) in &self.env {
            let s = match val {
                toml::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            v.push((k.clone(), s));
        }
        v
    }
}

#[cfg(test)]
mod config_tests {
    use super::ConfigFile;

    #[test]
    fn sections_map_to_their_variables() {
        let c: ConfigFile = toml::from_str(
            r#"
            [llm]
            base_url = "http://localhost:11434/v1"
            model = "qwen3"
            vision_model = "qwen2.5-vl"
            [jev]
            base_url = "http://localhost:8093"
            [browser]
            headless = true
            idle = 600
            [env]
            CAMOFOX_URL = "http://camofox:9377"
            FAB_POOL_MAX = 2
            [secrets]
            op_vault = "Agents"
            "#,
        )
        .unwrap();
        let v = c.vars();
        let get = |k: &str| v.iter().find(|(n, _)| n == k).map(|(_, x)| x.as_str());
        assert_eq!(get("OPENAI_BASE_URL"), Some("http://localhost:11434/v1"));
        assert_eq!(get("FAB_MODEL"), Some("qwen3"));
        assert_eq!(get("FAB_VISION_MODEL"), Some("qwen2.5-vl"));
        assert_eq!(get("SYSTEM_ONE_BASE_URL"), Some("http://localhost:8093"));
        assert_eq!(get("FAB_HEADLESS"), Some("true"));
        assert_eq!(get("FAB_IDLE"), Some("600"));
        assert_eq!(get("CAMOFOX_URL"), Some("http://camofox:9377"));
        assert_eq!(get("FAB_POOL_MAX"), Some("2"));
        assert_eq!(get("OPENAI_API_KEY"), None);
    }

    #[test]
    fn a_misspelled_key_is_an_error() {
        let e = toml::from_str::<ConfigFile>("[llm]\nbase_ulr = \"x\"\n").unwrap_err();
        assert!(e.to_string().contains("base_ulr"), "{e}");
        assert!(toml::from_str::<ConfigFile>("[brwoser]\n").is_err());
    }
}

pub mod task_runtime;
pub mod task_commands;

pub mod durable_program;
