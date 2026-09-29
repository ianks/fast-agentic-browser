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

/// fab's config file: `$XDG_CONFIG_HOME/fab/env` or `~/.config/fab/env`.
pub fn config_env() -> Option<PathBuf> {
    Some(fab_core::paths::config_dir().join("env"))
}

/// Keys come from the environment, then fab's config file, then (in a source
/// checkout) the repo's .env. Variables already set are never overridden.
pub fn load_env() {
    if let Some(p) = config_env() {
        let _ = dotenvy::from_path(p);
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    let _ = dotenvy::from_path(root);
}

pub mod task_runtime;
pub mod task_commands;

pub mod durable_program;
