//! Where fab keeps things: `$FAB_HOME`, else `$XDG_CACHE_HOME/fab`, else
//! `~/.cache/fab`.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("FAB_HOME") {
        return PathBuf::from(h);
    }
    cache_base().join("fab")
}

fn cache_base() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
}

/// fab's config: `$FAB_CONFIG`, else `$XDG_CONFIG_HOME/fab`, else `~/.config/fab`.
pub fn config_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("FAB_CONFIG") {
        return PathBuf::from(h);
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(std::env::temp_dir)
        .join("fab")
}

/// Durable state (the task journal, generated-password workflow references):
/// `$FAB_STATE_DIR`, else `$FAB_HOME/state`, else `$XDG_STATE_HOME/fab`, else
/// `~/.local/state/fab`. None without any of them.
pub fn state_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FAB_STATE_DIR") {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("FAB_HOME") {
        return Some(PathBuf::from(p).join("state"));
    }
    if let Some(p) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(p).join("fab"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/fab"))
}

/// Persistent browser profiles, one directory per browser and name.
pub fn profiles() -> PathBuf {
    home().join("profiles")
}

/// Sockets and logs of running sessions.
pub fn run_dir() -> PathBuf {
    home().join("run")
}

/// Learned site shapes. The first call moves shapes learned under the
/// project's old name (usebrowser) into place.
pub fn shapes() -> PathBuf {
    let dir = home().join("shapes");
    let old = cache_base().join("usebrowser").join("shapes");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::create_dir_all(home());
        let _ = std::fs::rename(&old, &dir);
    }
    dir
}
