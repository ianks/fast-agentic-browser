//! Tunable design knobs. Every one of these is benchmarked; defaults are the
//! current winners (see bench/LOG.md).

use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecMode {
    /// Synthetic DOM events dispatched from page JS (fastest).
    Js,
    /// Real input events through the browser (CDP Input.* / Playwright).
    Trusted,
}

#[derive(Debug, Clone, Serialize)]
pub struct Knobs {
    pub backend: String,
    pub camofox_url: String,
    /// Browser to launch: a name (chrome, edge, brave, chromium, …) or an
    /// executable path; "" = discover (`FAB_BROWSER`, `CHROME_PATH`,
    /// `BROWSER`, the OS default browser, then what is installed).
    pub browser: String,
    /// Attach to a running browser instead of launching one: "auto", a port,
    /// `http://host:port` or a `ws://` endpoint ("" = launch).
    pub connect: String,
    /// Persistent profile: a name (kept under fab's home, per browser) or a
    /// directory; "" = a throwaway profile.
    pub profile: String,
    /// JS dialogs (alert/confirm/prompt): "accept" or "dismiss".
    pub dialog: String,
    pub headful: bool,
    /// Browser window placement "x,y,w,h" in screen points (headful CDP only).
    pub window: Option<String>,
    pub exec: ExecMode,
    /// DOM must be mutation-free this long before an action counts as settled.
    pub quiet_ms: u32,
    pub settle_cap_ms: u32,
    /// setTimeout callbacks at or below this delay hold settle open.
    pub timer_max_ms: u32,
    /// Max clickable candidates offered to the decider.
    pub prune_k: usize,
    /// Max form fields asked about per step.
    pub max_fields: usize,
    /// Max page text snippets included in decision state.
    pub max_text: usize,
    /// Put each element's description in the Choice criteria (vs null + state only).
    pub opt_desc: bool,
    /// Include container context ("in: Blue Oxford Shirt") in element lines.
    pub ctx: bool,
    /// Fill every relevant field and click in the same step.
    pub fanout: bool,
    /// Skip the verification step when the decider predicts this step finishes the instruction.
    pub trust_final: f64,
    pub done_threshold: f64,
    pub max_steps: usize,
    /// Reuse the previous snapshot when the DOM version is unchanged.
    pub snapshot_cache: bool,
    /// Refuse to act when the click choice's confidence is below this.
    pub min_conf: f64,
    /// Parallel identical Jev requests per decision (first wins).
    pub hedge: usize,
    /// Backup requests go out only past this quantile of recent latency (0 = immediately).
    pub hedge_q: f64,
    /// Decision engine for fuzzy instructions: "legacy" (decide.rs loop) or "dvm".
    pub engine: String,
    /// Offer latent (hidden, revealable) elements as candidates; needs reveal macros.
    pub latent: bool,
    /// dvm: emit exactly the legacy questions and use the legacy stop rule.
    pub dvm_parity: bool,
    /// dvm: refine rounds allowed on one observation before escalating.
    pub r_max: u32,
    /// dvm: commit thresholds by risk class R0..R3 on the click probability.
    pub tau: [f64; 4],
    /// dvm: VERIFY threshold for R3 commits.
    pub nu: f64,
    /// dvm: an R2 commit is blocked when its VERIFY answer is below this (strong "no").
    pub nu2: f64,
    /// dvm: stop as impossible when the page shows the request is unavailable (Noul ≥ this).
    pub impossible: f64,
    /// Ask yes/no gates (done, verify, impossible) as A/B Choices instead of Nouls,
    /// for decision models whose Nouls follow their labels (e.g. Laya).
    pub gates_as_choice: bool,
    /// dvm: before an R2/R3 commit, check the filled form against the task in
    /// one Jev round: "off", "shadow" (record only) or "on" (re-decide on a
    /// mismatch instead of committing).
    pub audit: String,
    /// dvm: the form audit passes when P(form matches the task) is at least this.
    pub audit_tau: f64,
    /// Skip the final clause check for a single-clause goal that ended on an
    /// audited commit (p ≥ 0.8) followed by a page change and no error.
    pub clause_skip: bool,
    /// dvm: an R2 click the verb lexicon doesn't license is still allowed when
    /// the same-request VERIFY says the task asks for it with at least this
    /// probability (above 1 disables). R3 is never licensed this way.
    pub sem_license: f64,
    /// Model for one-shot clarifications ("" = off): when a decision is too
    /// uncertain to act on, one small-LLM call picks among the candidates
    /// instead of handing the whole task back.
    pub clarify: String,
    /// Clarifications allowed per act.
    pub clarify_max: u32,
    /// Site-shape cache (see `shape.rs`): "off", "record" (learn only), "use"
    /// (navigate with what is known, learn nothing), or "on" (both).
    pub shape: String,
    /// Where site shapes are kept ("" = `~/.cache/fab/shapes`).
    pub shape_dir: String,
    /// Jump to a known page only when Jev picks it with at least this probability.
    pub shape_tau: f64,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            backend: "cdp".into(),
            camofox_url: std::env::var("CAMOFOX_URL").unwrap_or_else(|_| "http://127.0.0.1:9377".into()),
            browser: String::new(),
            connect: String::new(),
            profile: String::new(),
            dialog: "accept".into(),
            // A visible window by default; benchmarks run headless.
            headful: true,
            window: None,
            exec: ExecMode::Js,
            quiet_ms: 20,
            settle_cap_ms: 3000,
            timer_max_ms: 600,
            prune_k: 64,
            max_fields: 16,
            max_text: 12,
            opt_desc: true,
            ctx: true,
            fanout: true,
            trust_final: 0.6,
            done_threshold: 0.5,
            max_steps: 6,
            snapshot_cache: true,
            min_conf: 0.0,
            hedge: 1,
            hedge_q: 0.75,
            engine: "legacy".into(),
            latent: false,
            dvm_parity: false,
            r_max: 1,
            // R1 (navigation) at 0.4: on held-out, 15 of 24 "best guess"
            // escalations were links at p 0.42-0.49, and most were right.
            tau: [0.35, 0.4, 0.5, 0.7],
            nu: 0.5,
            nu2: 0.2,
            impossible: 0.85,
            gates_as_choice: false,
            audit: "on".into(),
            // Measured: 0.3 / 0.5 gave 85.0% / 86.2% on held-out (not
            // significant); borderline blocks near 0.45 were false (defaults of
            // settings the task doesn't mention).
            audit_tau: 0.4,
            clause_skip: true,
            sem_license: 0.85,
            clarify: String::new(),
            clarify_max: 3,
            // Learn by default; acting on the map ("on"/"use") is opt-in: on
            // held-out-2/3 it was neutral (91.2→90.0%, 92.5→93.8%, 0.90×/1.01×).
            shape: "record".into(),
            shape_dir: String::new(),
            // A wrong jump only costs a navigation (nothing is committed).
            shape_tau: 0.45,
        }
    }
}

impl Knobs {
    pub fn set(&mut self, key: &str, v: &str) -> Result<()> {
        let b = || matches!(v, "1" | "true" | "on" | "yes");
        match key {
            "backend" => self.backend = v.into(),
            "camofox_url" => self.camofox_url = v.into(),
            "browser" => self.browser = v.into(),
            "connect" => self.connect = v.into(),
            "profile" => self.profile = v.into(),
            "dialog" => {
                anyhow::ensure!(matches!(v, "accept" | "dismiss"), "dialog must be accept|dismiss");
                self.dialog = v.into();
            }
            "headful" => self.headful = b(),
            "headless" => self.headful = !b(),
            "window" => self.window = Some(v.into()),
            "exec" => {
                self.exec = match v {
                    "js" => ExecMode::Js,
                    "trusted" => ExecMode::Trusted,
                    _ => bail!("exec must be js|trusted"),
                }
            }
            "quiet_ms" => self.quiet_ms = v.parse()?,
            "settle_cap_ms" => self.settle_cap_ms = v.parse()?,
            "timer_max_ms" => self.timer_max_ms = v.parse()?,
            "prune_k" => self.prune_k = v.parse::<usize>()?.clamp(2, 254),
            "max_fields" => self.max_fields = v.parse()?,
            "max_text" => self.max_text = v.parse()?,
            "opt_desc" => self.opt_desc = b(),
            "ctx" => self.ctx = b(),
            "fanout" => self.fanout = b(),
            "trust_final" => self.trust_final = v.parse()?,
            "done_threshold" => self.done_threshold = v.parse()?,
            "max_steps" => self.max_steps = v.parse()?,
            "snapshot_cache" => self.snapshot_cache = b(),
            "min_conf" => self.min_conf = v.parse()?,
            "hedge" => self.hedge = v.parse::<usize>()?.clamp(1, 4),
            "hedge_q" => self.hedge_q = v.parse()?,
            "engine" => {
                self.engine = v.into();
                // The dvm executes reveal macros, so it can offer latent targets.
                self.latent = v == "dvm";
            }
            "latent" => self.latent = b(),
            "dvm_parity" => self.dvm_parity = b(),
            "r_max" => self.r_max = v.parse()?,
            "tau" => {
                let t: Vec<f64> = v.split(',').map(|x| x.trim().parse()).collect::<std::result::Result<_, _>>()?;
                anyhow::ensure!(t.len() == 4, "tau needs 4 comma-separated values (R0..R3)");
                self.tau = [t[0], t[1], t[2], t[3]];
            }
            "nu" => self.nu = v.parse()?,
            "nu2" => self.nu2 = v.parse()?,
            "impossible" => self.impossible = v.parse()?,
            "gates_as_choice" => self.gates_as_choice = b(),
            "audit" => {
                anyhow::ensure!(matches!(v, "off" | "shadow" | "on"), "audit must be off|shadow|on");
                self.audit = v.into();
            }
            "audit_tau" => self.audit_tau = v.parse()?,
            "clause_skip" => self.clause_skip = b(),
            "sem_license" => self.sem_license = v.parse()?,
            "clarify" => self.clarify = if matches!(v, "off" | "0" | "") { String::new() } else { v.into() },
            "clarify_max" => self.clarify_max = v.parse()?,
            "shape" => {
                anyhow::ensure!(matches!(v, "off" | "record" | "use" | "on"), "shape must be off|record|use|on");
                self.shape = v.into();
            }
            "shape_dir" => self.shape_dir = v.into(),
            "shape_tau" => self.shape_tau = v.parse()?,
            _ => bail!("unknown knob {key}"),
        }
        Ok(())
    }

    /// The persistent profile directory for browser `browser_id`, if any.
    pub fn profile_dir(&self, browser_id: &str) -> Option<std::path::PathBuf> {
        let p = self.profile.trim();
        if p.is_empty() {
            return None;
        }
        let path = std::path::Path::new(p);
        if path.is_absolute() || p.contains('/') || p.contains('\\') {
            return Some(path.to_path_buf());
        }
        Some(crate::paths::profiles().join(browser_id).join(p))
    }

    /// Parses `k=v` pairs.
    pub fn apply(&mut self, pairs: &[String]) -> Result<()> {
        for p in pairs {
            let Some((k, v)) = p.split_once('=') else { bail!("expected k=v, got {p}") };
            self.set(k.trim(), v.trim())?;
        }
        Ok(())
    }
}

/// Chrome switches for a window rectangle "x,y,w,h".
pub fn window_args(rect: &str) -> Vec<String> {
    let v: Vec<&str> = rect.split(',').map(str::trim).collect();
    if v.len() != 4 {
        return vec![];
    }
    vec![format!("--window-position={},{}", v[0], v[1]), format!("--window-size={},{}", v[2], v[3])]
}
