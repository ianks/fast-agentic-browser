//! Finds the browser to drive the way the system says to: an explicit choice,
//! then the environment (`FAB_BROWSER`, `CHROME_PATH`, `BROWSER`), then the
//! OS default browser, then the standard install locations. Chromium-family
//! browsers are driven over CDP, Firefox over WebDriver BiDi.

use anyhow::{Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Chromium,
    Firefox,
    Webkit,
}

pub struct Spec {
    pub id: &'static str,
    pub name: &'static str,
    pub engine: Engine,
    /// Why fab can't drive it, if it can't.
    pub unsupported: Option<&'static str>,
    /// macOS bundle ids (LaunchServices).
    pub mac_ids: &'static [&'static str],
    /// Executable inside /Applications.
    pub mac_exe: &'static str,
    pub linux_bins: &'static [&'static str],
    /// Desktop entries (xdg-settings / xdg-mime).
    pub linux_desktop: &'static [&'static str],
    /// URL-association ProgId prefixes (Windows registry).
    pub win_progids: &'static [&'static str],
    /// Executable relative to %ProgramFiles%, %ProgramFiles(x86)% or %LOCALAPPDATA%.
    pub win_exe: &'static [&'static str],
    /// User data directory relative to ~/Library/Application Support, ~/.config, %LOCALAPPDATA%.
    pub data: [&'static str; 3],
}

/// Known browsers, in the order they are preferred when nothing else decides.
pub const SPECS: &[Spec] = &[
    Spec {
        id: "chrome",
        name: "Google Chrome",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.google.chrome"],
        mac_exe: "Google Chrome.app/Contents/MacOS/Google Chrome",
        linux_bins: &["google-chrome-stable", "google-chrome", "/opt/google/chrome/chrome"],
        linux_desktop: &["google-chrome.desktop"],
        win_progids: &["ChromeHTML"],
        win_exe: &["Google\\Chrome\\Application\\chrome.exe"],
        data: ["Google/Chrome", "google-chrome", "Google\\Chrome\\User Data"],
    },
    Spec {
        id: "chromium",
        name: "Chromium",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["org.chromium.chromium"],
        mac_exe: "Chromium.app/Contents/MacOS/Chromium",
        linux_bins: &["chromium", "chromium-browser", "/snap/bin/chromium"],
        linux_desktop: &["chromium.desktop", "chromium-browser.desktop", "chromium_chromium.desktop"],
        win_progids: &["ChromiumHTM"],
        win_exe: &["Chromium\\Application\\chrome.exe"],
        data: ["Chromium", "chromium", "Chromium\\User Data"],
    },
    Spec {
        id: "edge",
        name: "Microsoft Edge",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.microsoft.edgemac"],
        mac_exe: "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        linux_bins: &["microsoft-edge-stable", "microsoft-edge", "/opt/microsoft/msedge/msedge"],
        linux_desktop: &["microsoft-edge.desktop"],
        win_progids: &["MSEdgeHTM"],
        win_exe: &["Microsoft\\Edge\\Application\\msedge.exe"],
        data: ["Microsoft Edge", "microsoft-edge", "Microsoft\\Edge\\User Data"],
    },
    Spec {
        id: "brave",
        name: "Brave",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.brave.browser"],
        mac_exe: "Brave Browser.app/Contents/MacOS/Brave Browser",
        linux_bins: &["brave-browser", "brave", "/opt/brave.com/brave/brave"],
        linux_desktop: &["brave-browser.desktop", "brave_brave.desktop", "com.brave.Browser.desktop"],
        win_progids: &["BraveHTML"],
        win_exe: &["BraveSoftware\\Brave-Browser\\Application\\brave.exe"],
        data: ["BraveSoftware/Brave-Browser", "BraveSoftware/Brave-Browser", "BraveSoftware\\Brave-Browser\\User Data"],
    },
    Spec {
        id: "chrome-beta",
        name: "Google Chrome Beta",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.google.chrome.beta"],
        mac_exe: "Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta",
        linux_bins: &["google-chrome-beta"],
        linux_desktop: &["google-chrome-beta.desktop"],
        win_progids: &["ChromeBHTML"],
        win_exe: &["Google\\Chrome Beta\\Application\\chrome.exe"],
        data: ["Google/Chrome Beta", "google-chrome-beta", "Google\\Chrome Beta\\User Data"],
    },
    Spec {
        id: "chrome-dev",
        name: "Google Chrome Dev",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.google.chrome.dev"],
        mac_exe: "Google Chrome Dev.app/Contents/MacOS/Google Chrome Dev",
        linux_bins: &["google-chrome-unstable"],
        linux_desktop: &["google-chrome-unstable.desktop"],
        win_progids: &["ChromeDHTML"],
        win_exe: &["Google\\Chrome Dev\\Application\\chrome.exe"],
        data: ["Google/Chrome Dev", "google-chrome-unstable", "Google\\Chrome Dev\\User Data"],
    },
    Spec {
        id: "chrome-canary",
        name: "Google Chrome Canary",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.google.chrome.canary"],
        mac_exe: "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        linux_bins: &["google-chrome-canary"],
        linux_desktop: &["google-chrome-canary.desktop"],
        win_progids: &["ChromeSSHTM"],
        win_exe: &["Google\\Chrome SxS\\Application\\chrome.exe"],
        data: ["Google/Chrome Canary", "google-chrome-canary", "Google\\Chrome SxS\\User Data"],
    },
    Spec {
        id: "edge-beta",
        name: "Microsoft Edge Beta",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.microsoft.edgemac.beta"],
        mac_exe: "Microsoft Edge Beta.app/Contents/MacOS/Microsoft Edge Beta",
        linux_bins: &["microsoft-edge-beta", "/opt/microsoft/msedge-beta/msedge"],
        linux_desktop: &["microsoft-edge-beta.desktop"],
        win_progids: &["MSEdgeBHTML"],
        win_exe: &["Microsoft\\Edge Beta\\Application\\msedge.exe"],
        data: ["Microsoft Edge Beta", "microsoft-edge-beta", "Microsoft\\Edge Beta\\User Data"],
    },
    Spec {
        id: "edge-dev",
        name: "Microsoft Edge Dev",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.microsoft.edgemac.dev"],
        mac_exe: "Microsoft Edge Dev.app/Contents/MacOS/Microsoft Edge Dev",
        linux_bins: &["microsoft-edge-dev", "/opt/microsoft/msedge-dev/msedge"],
        linux_desktop: &["microsoft-edge-dev.desktop"],
        win_progids: &["MSEdgeDHTML"],
        win_exe: &["Microsoft\\Edge Dev\\Application\\msedge.exe"],
        data: ["Microsoft Edge Dev", "microsoft-edge-dev", "Microsoft\\Edge Dev\\User Data"],
    },
    Spec {
        id: "edge-canary",
        name: "Microsoft Edge Canary",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.microsoft.edgemac.canary"],
        mac_exe: "Microsoft Edge Canary.app/Contents/MacOS/Microsoft Edge Canary",
        linux_bins: &[],
        linux_desktop: &[],
        win_progids: &["MSEdgeSSHTM"],
        win_exe: &["Microsoft\\Edge SxS\\Application\\msedge.exe"],
        data: ["Microsoft Edge Canary", "", "Microsoft\\Edge SxS\\User Data"],
    },
    Spec {
        id: "brave-beta",
        name: "Brave Beta",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.brave.browser.beta"],
        mac_exe: "Brave Browser Beta.app/Contents/MacOS/Brave Browser Beta",
        linux_bins: &["brave-browser-beta", "/opt/brave.com/brave-beta/brave"],
        linux_desktop: &["brave-browser-beta.desktop"],
        win_progids: &["BraveBHTML"],
        win_exe: &["BraveSoftware\\Brave-Browser-Beta\\Application\\brave.exe"],
        data: ["BraveSoftware/Brave-Browser-Beta", "BraveSoftware/Brave-Browser-Beta", "BraveSoftware\\Brave-Browser-Beta\\User Data"],
    },
    Spec {
        id: "brave-nightly",
        name: "Brave Nightly",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.brave.browser.nightly"],
        mac_exe: "Brave Browser Nightly.app/Contents/MacOS/Brave Browser Nightly",
        linux_bins: &["brave-browser-nightly", "/opt/brave.com/brave-nightly/brave"],
        linux_desktop: &["brave-browser-nightly.desktop"],
        win_progids: &["BraveSSHTM"],
        win_exe: &["BraveSoftware\\Brave-Browser-Nightly\\Application\\brave.exe"],
        data: ["BraveSoftware/Brave-Browser-Nightly", "BraveSoftware/Brave-Browser-Nightly", "BraveSoftware\\Brave-Browser-Nightly\\User Data"],
    },
    Spec {
        id: "vivaldi",
        name: "Vivaldi",
        engine: Engine::Chromium,
        unsupported: None,
        mac_ids: &["com.vivaldi.vivaldi"],
        mac_exe: "Vivaldi.app/Contents/MacOS/Vivaldi",
        linux_bins: &["vivaldi-stable", "vivaldi"],
        linux_desktop: &["vivaldi-stable.desktop"],
        win_progids: &["VivaldiHTM"],
        win_exe: &["Vivaldi\\Application\\vivaldi.exe"],
        data: ["Vivaldi", "vivaldi", "Vivaldi\\User Data"],
    },
    Spec {
        id: "arc",
        name: "Arc",
        engine: Engine::Chromium,
        unsupported: Some("Arc can't be started with a separate automation profile"),
        mac_ids: &["company.thebrowser.browser"],
        mac_exe: "Arc.app/Contents/MacOS/Arc",
        linux_bins: &[],
        linux_desktop: &[],
        win_progids: &["Arc"],
        win_exe: &[],
        data: ["Arc/User Data", "", ""],
    },
    Spec {
        id: "dia",
        name: "Dia",
        engine: Engine::Chromium,
        unsupported: Some("Dia can't be started with a separate automation profile"),
        mac_ids: &["company.thebrowser.dia"],
        mac_exe: "Dia.app/Contents/MacOS/Dia",
        linux_bins: &[],
        linux_desktop: &[],
        win_progids: &[],
        win_exe: &[],
        data: ["", "", ""],
    },
    Spec {
        id: "firefox",
        name: "Firefox",
        engine: Engine::Firefox,
        unsupported: None,
        mac_ids: &["org.mozilla.firefox", "org.mozilla.firefoxdeveloperedition", "org.mozilla.nightly"],
        mac_exe: "Firefox.app/Contents/MacOS/firefox",
        linux_bins: &["firefox", "firefox-esr"],
        linux_desktop: &["firefox.desktop", "firefox_firefox.desktop", "org.mozilla.firefox.desktop", "firefox-esr.desktop"],
        win_progids: &["FirefoxURL", "FirefoxHTML"],
        win_exe: &["Mozilla Firefox\\firefox.exe"],
        data: ["", "", ""],
    },
    Spec {
        id: "safari",
        name: "Safari",
        engine: Engine::Webkit,
        unsupported: Some("Safari has no remote debugging protocol fab can drive"),
        mac_ids: &["com.apple.safari"],
        mac_exe: "Safari.app/Contents/MacOS/Safari",
        linux_bins: &[],
        linux_desktop: &[],
        win_progids: &[],
        win_exe: &[],
        data: ["", "", ""],
    },
];

pub fn spec(id: &str) -> Option<&'static Spec> {
    let id = id.to_ascii_lowercase();
    let alias = match id.as_str() {
        "google-chrome" | "chrome-stable" | "google chrome" => "chrome",
        "msedge" | "microsoft-edge" | "edge-stable" => "edge",
        "brave-browser" => "brave",
        "chromium-browser" => "chromium",
        "chrome-unstable" => "chrome-dev",
        "canary" | "chrome-sxs" => "chrome-canary",
        "msedge-beta" => "edge-beta",
        "msedge-dev" => "edge-dev",
        "msedge-canary" => "edge-canary",
        other => other,
    };
    SPECS.iter().find(|s| s.id == alias)
}

/// A browser executable and how it was chosen.
#[derive(Debug, Clone, Serialize)]
pub struct Found {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub engine: Engine,
    /// What chose it: "--browser", "FAB_BROWSER", "CHROME_PATH", "BROWSER",
    /// "system default", "installed".
    pub source: String,
}

impl Found {
    fn of(s: &Spec, path: PathBuf, source: &str) -> Self {
        Self { id: s.id.into(), name: s.name.into(), path, engine: s.engine, source: source.into() }
    }
}

/// The OS default browser: its raw identifier (bundle id, desktop entry or
/// ProgId) and the known browser it names.
pub fn system_default() -> Option<(String, Option<&'static Spec>)> {
    let raw = if cfg!(target_os = "macos") {
        mac_default()
    } else if cfg!(windows) {
        win_default()
    } else {
        linux_default()
    }?;
    let l = raw.to_ascii_lowercase();
    let s = SPECS.iter().find(|s| {
        s.mac_ids.iter().any(|b| *b == l)
            || s.linux_desktop.iter().any(|d| d.eq_ignore_ascii_case(&raw))
            || s.win_progids.iter().any(|p| raw.starts_with(p))
    });
    Some((raw, s))
}

fn mac_default() -> Option<String> {
    let out = Command::new("defaults").args(["read", "com.apple.LaunchServices/com.apple.launchservices.secure", "LSHandlers"]).output().ok()?;
    parse_ls_handlers(&String::from_utf8_lossy(&out.stdout)).or_else(|| Some("com.apple.safari".into()))
}

/// The https (else http) handler in `defaults read … LSHandlers` output.
fn parse_ls_handlers(text: &str) -> Option<String> {
    let role = |block: &str| {
        let i = block.find("LSHandlerRoleAll")?;
        let v = block[i..].split_once('=')?.1;
        let v = v.split(';').next()?.trim().trim_matches('"');
        (!v.is_empty() && v != "-").then(|| v.to_string())
    };
    for scheme in ["https", "http"] {
        for block in text.split('}') {
            let has = block.lines().any(|l| {
                let l = l.trim();
                l == format!("LSHandlerURLScheme = {scheme};") || l == format!("LSHandlerURLScheme = \"{scheme}\";")
            });
            if has {
                if let Some(r) = role(block) {
                    return Some(r);
                }
            }
        }
    }
    None
}

fn linux_default() -> Option<String> {
    let run = |cmd: &str, args: &[&str]| {
        let out = Command::new(cmd).args(args).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !s.is_empty()).then_some(s)
    };
    run("xdg-settings", &["get", "default-web-browser"]).or_else(|| run("xdg-mime", &["query", "default", "x-scheme-handler/https"]))
}

fn win_default() -> Option<String> {
    let base = "HKCU\\Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\https";
    // Windows 11 24H2+ writes only UserChoiceLatest (a ProgId subkey's default
    // value); the older UserChoice value can be stale.
    let queries = [vec![format!("{base}\\UserChoiceLatest\\ProgId"), "/ve".to_string()], vec![format!("{base}\\UserChoice"), "/v".to_string(), "ProgId".to_string()]];
    for q in queries {
        let Ok(out) = Command::new("reg").arg("query").args(&q).output() else { continue };
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(v) = text.lines().find(|l| l.contains("REG_SZ")).and_then(|l| l.split_whitespace().last()) {
            return Some(v.to_string());
        }
    }
    None
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(bin)).find(|p| p.is_file())
}

/// Where `s` is installed, if it is.
pub fn locate(s: &Spec) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(h) = home() {
            roots.push(h.join("Applications"));
        }
        if let Some(p) = roots.iter().map(|r| r.join(s.mac_exe)).find(|p| p.is_file()) {
            return Some(p);
        }
        // Installed somewhere else: ask Spotlight by bundle id.
        let inner = s.mac_exe.split_once(".app/").map(|(_, rest)| rest)?;
        for id in s.mac_ids {
            let q = format!("kMDItemCFBundleIdentifier == '{id}'c");
            let Ok(out) = Command::new("mdfind").arg(q).output() else { continue };
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(p) = text.lines().map(|app| Path::new(app).join(inner)).find(|p| p.is_file()) {
                return Some(p);
            }
        }
        None
    } else if cfg!(windows) {
        let roots: Vec<PathBuf> =
            ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"].iter().filter_map(|v| std::env::var_os(v)).map(PathBuf::from).collect();
        s.win_exe.iter().flat_map(|e| roots.iter().map(move |r| r.join(e))).find(|p| p.is_file())
    } else {
        s.linux_bins.iter().find_map(|b| if b.starts_with('/') { Some(PathBuf::from(b)).filter(|p| p.is_file()) } else { which(b) })
    }
}

/// Chrome for Testing and Chromium downloaded by fab (`fab install`),
/// Puppeteer or Playwright: the newest executable found.
pub fn downloaded() -> Option<PathBuf> {
    let mut roots = vec![crate::paths::home().join("browsers")];
    let cache = |var: &str, rel: &str| std::env::var_os(var).map(PathBuf::from).or_else(|| home().map(|h| h.join(rel)));
    roots.extend(cache("PUPPETEER_CACHE_DIR", ".cache/puppeteer"));
    let pw = if cfg!(target_os = "macos") { "Library/Caches/ms-playwright" } else { ".cache/ms-playwright" };
    roots.extend(cache("PLAYWRIGHT_BROWSERS_PATH", pw));
    let names = ["Google Chrome for Testing", "Chromium", "chrome", "chrome.exe"];
    let mut found: Vec<PathBuf> = vec![];
    fn walk(dir: &Path, depth: u32, names: &[&str], out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            // Playwright's headless shell and Firefox/WebKit builds are not wanted.
            if name.contains("headless") || name.starts_with("firefox") || name.starts_with("webkit") {
                continue;
            }
            if p.is_dir() {
                if depth > 0 {
                    walk(&p, depth - 1, names, out);
                }
            } else if names.contains(&name.as_str()) {
                out.push(p);
            }
        }
    }
    for r in roots {
        walk(&r, 6, &names, &mut found);
    }
    found.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    found.pop()
}

/// Every known browser that is installed.
pub fn installed() -> Vec<Found> {
    SPECS.iter().filter_map(|s| locate(s).map(|p| Found::of(s, p, "installed"))).collect()
}

/// A path or command given by the user: the known browser it is, if any.
fn identify(path: &Path) -> Option<&'static Spec> {
    let file = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    let full = path.to_string_lossy().to_ascii_lowercase();
    SPECS.iter().find(|s| {
        s.linux_bins.iter().any(|b| *b == file)
            || s.win_exe.iter().any(|e| full.ends_with(&e.to_ascii_lowercase()))
            || full.ends_with(&s.mac_exe.to_ascii_lowercase())
    })
}

/// Resolves `value` (a known browser name, an executable path or a command
/// on PATH) to a browser fab can drive.
fn resolve(value: &str, source: &str) -> Result<Found> {
    let value = value.trim();
    if let Some(s) = spec(value) {
        if let Some(why) = s.unsupported {
            bail!("{} ({source}) is not supported: {why}", s.name);
        }
        return match locate(s) {
            Some(p) => Ok(Found::of(s, p, source)),
            None => bail!("{} ({source}) is not installed", s.name),
        };
    }
    let p = PathBuf::from(value);
    let path = if p.is_absolute() || value.contains(std::path::MAIN_SEPARATOR) { p } else { which(value).unwrap_or(p) };
    if !path.is_file() {
        bail!("{source}: {value} is neither a known browser nor an executable");
    }
    match identify(&path) {
        Some(s) if s.unsupported.is_some() => bail!("{} ({source}) is not supported: {}", s.name, s.unsupported.unwrap()),
        Some(s) => Ok(Found::of(s, path, source)),
        // An unknown executable: trust that it is Chromium-based.
        None => Ok(Found { id: "custom".into(), name: path.display().to_string(), path, engine: Engine::Chromium, source: source.into() }),
    }
}

/// What discovery saw, for `fab doctor`.
#[derive(Debug, Serialize)]
pub struct Report {
    pub chosen: Option<Found>,
    pub error: Option<String>,
    /// The OS default browser (raw id, name if known).
    pub system_default: Option<(String, Option<String>)>,
    pub installed: Vec<Found>,
    pub notes: Vec<String>,
}

/// Picks the browser: `pref` (a name or path; "" = decide), then `FAB_BROWSER`,
/// `CHROME_PATH` and `BROWSER`, then the OS default browser, then the first
/// installed one. Notes say why a more preferred choice was passed over.
pub fn pick(pref: &str) -> Result<(Found, Vec<String>)> {
    let mut notes = vec![];
    if !pref.is_empty() {
        return resolve(pref, "--browser").map(|f| (f, notes));
    }
    for var in ["FAB_BROWSER", "CHROME_PATH", "PUPPETEER_EXECUTABLE_PATH"] {
        if let Some(v) = std::env::var(var).ok().filter(|v| !v.trim().is_empty()) {
            return resolve(&v, var).map(|f| (f, notes));
        }
    }
    // $BROWSER: the Unix preferred-browser list ("firefox:google-chrome").
    if let Ok(v) = std::env::var("BROWSER") {
        for cmd in v.split(':').filter(|c| !c.trim().is_empty()) {
            let cmd = cmd.split_whitespace().next().unwrap_or(cmd);
            match resolve(cmd, "BROWSER") {
                Ok(f) => return Ok((f, notes)),
                Err(e) => notes.push(format!("{e}")),
            }
        }
    }
    if let Some((raw, s)) = system_default() {
        match s {
            Some(s) if s.unsupported.is_none() => {
                if let Some(p) = locate(s) {
                    return Ok((Found::of(s, p, "system default"), notes));
                }
                notes.push(format!("the default browser, {}, was not found where it is normally installed", s.name));
            }
            Some(s) => notes.push(format!("the default browser is {}: {}", s.name, s.unsupported.unwrap_or_default())),
            None => notes.push(format!("the default browser ({raw}) is not one fab knows")),
        }
    }
    for s in SPECS.iter().filter(|s| s.unsupported.is_none()) {
        if let Some(p) = locate(s) {
            return Ok((Found::of(s, p, "installed"), notes));
        }
    }
    if let Some(p) = downloaded() {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        return Ok((Found { id: "chrome-for-testing".into(), name, path: p, engine: Engine::Chromium, source: "downloaded".into() }, notes));
    }
    let mut msg = String::from("no supported browser found. Run `fab install` to download Chrome for Testing, install Chrome (or Chromium, Edge, Brave), or point FAB_BROWSER at one");
    if !notes.is_empty() {
        msg.push_str(&format!(" ({})", notes.join("; ")));
    }
    bail!(msg)
}

pub fn report(pref: &str) -> Report {
    let (chosen, error, notes) = match pick(pref) {
        Ok((f, n)) => (Some(f), None, n),
        Err(e) => (None, Some(format!("{e:#}")), vec![]),
    };
    Report {
        chosen,
        error,
        system_default: system_default().map(|(raw, s)| (raw, s.map(|s| s.name.to_string()))),
        installed: installed(),
        notes,
    }
}

/// The user data directory of `s` (where its DevToolsActivePort appears when
/// remote debugging is on).
pub fn user_data_dir(s: &Spec) -> Option<PathBuf> {
    let [mac, linux, win] = s.data;
    let (base, rel) = if cfg!(target_os = "macos") {
        (home()?.join("Library/Application Support"), mac)
    } else if cfg!(windows) {
        (PathBuf::from(std::env::var_os("LOCALAPPDATA")?), win)
    } else {
        let cfg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home().map(|h| h.join(".config")))?;
        (cfg, linux)
    };
    (!rel.is_empty()).then(|| base.join(rel))
}

/// Running browsers with remote debugging on: (browser, port, browser
/// websocket path), the default browser first.
pub fn debuggable() -> Vec<(&'static Spec, u16, String)> {
    let first = system_default().and_then(|(_, s)| s).map(|s| s.id);
    let mut specs: Vec<&'static Spec> = SPECS.iter().filter(|s| s.engine == Engine::Chromium).collect();
    specs.sort_by_key(|s| Some(s.id) != first);
    specs
        .into_iter()
        .filter_map(|s| {
            let text = std::fs::read_to_string(user_data_dir(s)?.join("DevToolsActivePort")).ok()?;
            let mut lines = text.lines();
            let port = lines.next()?.trim().parse().ok()?;
            let path = lines.next().unwrap_or("").trim().to_string();
            Some((s, port, path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_handlers() {
        let text = r#"(
        {
        LSHandlerContentType = "public.html";
        LSHandlerRoleAll = "com.google.chrome";
    },
        {
        LSHandlerPreferredVersions =         {
            LSHandlerRoleAll = "-";
        };
        LSHandlerRoleAll = "org.mozilla.firefox";
        LSHandlerURLScheme = https;
    },
        {
        LSHandlerRoleAll = "com.brave.browser";
        LSHandlerURLScheme = http;
    }
)"#;
        assert_eq!(parse_ls_handlers(text).as_deref(), Some("org.mozilla.firefox"));
        let http_only = text.replace("LSHandlerURLScheme = https;", "LSHandlerURLScheme = mailto;");
        assert_eq!(parse_ls_handlers(&http_only).as_deref(), Some("com.brave.browser"));
        assert_eq!(parse_ls_handlers("()"), None);
    }

    #[test]
    fn names_and_paths() {
        assert_eq!(spec("msedge").unwrap().id, "edge");
        assert_eq!(spec("Google-Chrome").unwrap().id, "chrome");
        assert_eq!(identify(Path::new("/usr/bin/chromium-browser")).unwrap().id, "chromium");
        assert_eq!(identify(Path::new("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser")).unwrap().id, "brave");
        assert_eq!(identify(Path::new("C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe")).unwrap().id, "edge");
        assert!(identify(Path::new("/opt/custom/headless_shell")).is_none());
        assert!(spec("safari").unwrap().unsupported.is_some());
        assert_eq!(spec("firefox").unwrap().engine, Engine::Firefox);
    }
}
