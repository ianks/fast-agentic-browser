//! Site shape: a compact, persistent map of a site's structure, learned from
//! every visit and reused on the next.
//!
//! Sites are mostly templates (a settings page, a list, a detail page, a
//! dialog) joined by the same navigation controls. We keep exactly that and
//! nothing about the data in it:
//! - page templates: the route with ID-like segments masked (`#/accounts/:x`),
//!   the open dialog's name, and a short description from template headings;
//! - edges: "this navigation control on template A leads to template B", the
//!   control stored only as a 64-bit hash of its masked role, name and context.
//!
//! Never stored: field values, the task's literals, anything inside a record
//! (table rows, cards, list items), digits, emails, URLs, query strings,
//! network calls or model answers. Only non-committing controls (R0/R1:
//! navigation, tabs, menus, reveals) become edges.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use crate::license::{self, Risk};
use crate::snapshot::{El, Snapshot};

const MAX_TEMPLATES: usize = 300;
const MAX_EDGES: usize = 3000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Template {
    /// Route pattern plus open dialog, e.g. `#/settings/filters [dialog: create a new filter]`.
    pub key: String,
    /// Short human description for choosing a destination.
    pub desc: String,
    pub seen: u32,
    pub last: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Edge {
    pub from: u32,
    /// Hash of the control's masked role, name and context.
    pub ctl: u64,
    pub to: u32,
    pub n: u32,
    pub last: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Site {
    pub key: String,
    pub templates: Vec<Template>,
    pub edges: Vec<Edge>,
}

impl Site {
    pub fn find(&self, key: &str) -> Option<u32> {
        self.templates.iter().position(|t| t.key == key).map(|i| i as u32)
    }

    fn template(&mut self, key: &str, desc: &str, now: u64) -> u32 {
        if let Some(i) = self.find(key) {
            let t = &mut self.templates[i as usize];
            t.seen += 1;
            t.last = now;
            if !desc.is_empty() {
                t.desc = desc.to_string();
            }
            return i;
        }
        if self.templates.len() >= MAX_TEMPLATES {
            self.evict_template();
        }
        self.templates.push(Template { key: key.to_string(), desc: desc.to_string(), seen: 1, last: now });
        (self.templates.len() - 1) as u32
    }

    /// Drops the least recently seen template and its edges, renumbering the rest.
    fn evict_template(&mut self) {
        let Some((old, _)) = self.templates.iter().enumerate().min_by_key(|(_, t)| t.last) else { return };
        let old = old as u32;
        self.templates.remove(old as usize);
        self.edges.retain(|e| e.from != old && e.to != old);
        for e in self.edges.iter_mut() {
            if e.from > old {
                e.from -= 1;
            }
            if e.to > old {
                e.to -= 1;
            }
        }
    }

    fn edge(&mut self, from: u32, ctl: u64, to: u32, now: u64) {
        if from == to {
            return;
        }
        if let Some(e) = self.edges.iter_mut().find(|e| e.from == from && e.ctl == ctl && e.to == to) {
            e.n += 1;
            e.last = now;
            return;
        }
        if self.edges.len() >= MAX_EDGES {
            if let Some((i, _)) = self.edges.iter().enumerate().min_by_key(|(_, e)| (e.n, e.last)) {
                self.edges.remove(i);
            }
        }
        self.edges.push(Edge { from, ctl, to, n: 1, last: now });
    }

    /// Shortest path of navigation edges from `from` to `to` (most-used edge
    /// per hop).
    pub fn path(&self, from: u32, to: u32) -> Option<Vec<Edge>> {
        if from == to {
            return Some(vec![]);
        }
        let mut prev: HashMap<u32, usize> = HashMap::new();
        let mut seen: HashSet<u32> = HashSet::from([from]);
        let mut q = VecDeque::from([from]);
        while let Some(u) = q.pop_front() {
            let mut out: Vec<(usize, &Edge)> = self.edges.iter().enumerate().filter(|(_, e)| e.from == u).collect();
            out.sort_by_key(|(_, e)| std::cmp::Reverse(e.n));
            for (i, e) in out {
                if seen.insert(e.to) {
                    prev.insert(e.to, i);
                    if e.to == to {
                        let mut path = Vec::new();
                        let mut v = to;
                        while v != from {
                            let e = &self.edges[prev[&v]];
                            path.push(e.clone());
                            v = e.from;
                        }
                        path.reverse();
                        return Some(path);
                    }
                    q.push_back(e.to);
                }
            }
        }
        None
    }

    /// Folds another copy of this site in (another process's learning),
    /// matching templates by key. Counts take the max, so merging the same
    /// data twice doesn't inflate them.
    fn merge_from(&mut self, other: &Site) {
        let map: Vec<u32> = other
            .templates
            .iter()
            .map(|t| match self.find(&t.key) {
                Some(i) => {
                    let m = &mut self.templates[i as usize];
                    m.seen = m.seen.max(t.seen);
                    m.last = m.last.max(t.last);
                    if m.desc.is_empty() {
                        m.desc = t.desc.clone();
                    }
                    i
                }
                None => {
                    self.templates.push(t.clone());
                    (self.templates.len() - 1) as u32
                }
            })
            .collect();
        for e in &other.edges {
            let (Some(&f), Some(&to)) = (map.get(e.from as usize), map.get(e.to as usize)) else { continue };
            match self.edges.iter_mut().find(|x| x.from == f && x.ctl == e.ctl && x.to == to) {
                Some(x) => {
                    x.n = x.n.max(e.n);
                    x.last = x.last.max(e.last);
                }
                None => self.edges.push(Edge { from: f, ctl: e.ctl, to, n: e.n, last: e.last }),
            }
        }
        while self.templates.len() > MAX_TEMPLATES {
            self.evict_template();
        }
        while self.edges.len() > MAX_EDGES {
            let Some((i, _)) = self.edges.iter().enumerate().min_by_key(|(_, e)| (e.n, e.last)) else { break };
            self.edges.remove(i);
        }
    }

    /// Templates reachable from `from`, nearest first, with their hop count.
    pub fn reachable(&self, from: u32, max: usize) -> Vec<(u32, usize)> {
        let mut dist: HashMap<u32, usize> = HashMap::from([(from, 0)]);
        let mut q = VecDeque::from([from]);
        let mut out = Vec::new();
        while let Some(u) = q.pop_front() {
            for e in self.edges.iter().filter(|e| e.from == u) {
                if !dist.contains_key(&e.to) {
                    let d = dist[&u] + 1;
                    dist.insert(e.to, d);
                    out.push((e.to, d));
                    q.push_back(e.to);
                }
            }
        }
        out.truncate(max);
        out
    }
}

/// The store for `dir`, shared by every session in this process (bench
/// workers run in parallel), so they learn together instead of overwriting
/// each other's files.
pub fn shared(dir: PathBuf) -> std::sync::Arc<std::sync::Mutex<ShapeStore>> {
    static STORES: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, std::sync::Arc<std::sync::Mutex<ShapeStore>>>>> =
        std::sync::OnceLock::new();
    let mut m = STORES.get_or_init(Default::default).lock().unwrap();
    m.entry(dir.clone()).or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(ShapeStore::open(dir)))).clone()
}

/// Per-site shapes on disk: one small JSON file per site.
pub struct ShapeStore {
    dir: PathBuf,
    sites: HashMap<String, Site>,
    dirty: HashSet<String>,
}

impl ShapeStore {
    pub fn open(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self { dir, sites: HashMap::new(), dirty: HashSet::new() }
    }

    /// The default location: `shapes` under fab's home (see `paths`).
    pub fn default_dir() -> PathBuf {
        crate::paths::shapes()
    }

    fn file(&self, site: &str) -> PathBuf {
        self.dir.join(format!("{:016x}.json", fnv(site.as_bytes())))
    }

    pub fn site(&mut self, key: &str) -> &mut Site {
        if !self.sites.contains_key(key) {
            let loaded = std::fs::read(self.file(key))
                .ok()
                .and_then(|b| serde_json::from_slice::<Site>(&b).ok())
                .filter(|s| s.key == key)
                .unwrap_or_else(|| Site { key: key.to_string(), ..Default::default() });
            self.sites.insert(key.to_string(), loaded);
        }
        self.sites.get_mut(key).unwrap()
    }

    pub fn note_template(&mut self, site: &str, key: &str, desc: &str) -> u32 {
        let now = now_secs();
        let i = self.site(site).template(key, desc, now);
        self.dirty.insert(site.to_string());
        i
    }

    pub fn note_edge(&mut self, site: &str, from: u32, ctl: u64, to: u32) {
        let now = now_secs();
        self.site(site).edge(from, ctl, to, now);
        self.dirty.insert(site.to_string());
    }

    /// Writes changed sites (atomically: temp file, then rename), first
    /// merging in what other processes wrote meanwhile.
    pub fn flush(&mut self) {
        for key in std::mem::take(&mut self.dirty) {
            let path = self.file(&key);
            let disk = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Site>(&b).ok()).filter(|s| s.key == key);
            let Some(site) = self.sites.get_mut(&key) else { continue };
            if let Some(d) = disk {
                site.merge_from(&d);
            }
            let tmp = path.with_extension(format!("json.{}", std::process::id()));
            if let Ok(b) = serde_json::to_vec(site) {
                if std::fs::write(&tmp, b).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }
    }
}

impl Drop for ShapeStore {
    fn drop(&mut self) {
        self.flush();
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn fnv(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for x in b {
        h ^= *x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The site a URL belongs to: its host without "www." (the port is ignored);
/// for local servers, the served file, since one host serves many apps.
pub fn site_key(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let (host_port, path) = rest.split_once('/').map(|(h, p)| (h, p)).unwrap_or((rest, ""));
    let host = host_port.rsplit_once(':').map(|(h, _)| h).unwrap_or(host_port).to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    if host.is_empty() {
        return None;
    }
    if host == "localhost" || host == "127.0.0.1" || host == "[::1]" {
        let file = path.split(['?', '#']).next().unwrap_or("");
        return Some(format!("local/{file}"));
    }
    Some(host)
}

/// Masks identity out of text: the task's literals, emails, URLs and digits.
pub fn mask(s: &str, literals: &[String]) -> String {
    let mut out = s.to_lowercase();
    for l in literals {
        let l = l.trim().to_lowercase();
        if l.chars().count() >= 2 {
            out = out.replace(&l, "\u{2026}");
        }
    }
    let words: Vec<String> = out
        .split_whitespace()
        .map(|w| {
            if w.contains('@') {
                "<email>".to_string()
            } else if w.contains("://") || w.starts_with("www.") {
                "<url>".to_string()
            } else if w.chars().any(|c| c.is_ascii_digit()) {
                // Runs of digits become one '#': "inbox 6", "inv-2026-0918".
                let mut t = String::new();
                let mut in_digits = false;
                for c in w.chars() {
                    if c.is_ascii_digit() {
                        if !in_digits {
                            t.push('#');
                        }
                        in_digits = true;
                    } else {
                        t.push(c);
                        in_digits = false;
                    }
                }
                t
            } else {
                w.to_string()
            }
        })
        .collect();
    let s = words.join(" ");
    s.chars().take(80).collect()
}

/// A path segment that names an instance rather than a place.
fn instance_segment(seg: &str) -> bool {
    // "app.html" is a place; "jane.doe" or "report.v2.final" are instances.
    let file = seg
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.contains('.') && matches!(ext, "html" | "htm" | "php" | "asp" | "aspx" | "jsp"));
    // IDs have several digits ("acc-1002", "2026", "R-5521"); a single digit
    // is usually part of a place ("v2", "q3", "step1").
    let digits = seg.chars().filter(|c| c.is_ascii_digit()).count();
    let hexish = seg.len() >= 8 && seg.chars().all(|c| c.is_ascii_hexdigit() || c == '-') && digits >= 2;
    digits >= 3 || hexish || seg.contains(['@', '%', '~']) || (seg.contains('.') && !file) || seg.chars().count() > 24
}

fn route_pattern(url: &str) -> (String, bool) {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = rest.split_once('/').map(|(_, p)| p).unwrap_or("");
    let (before_hash, hash) = path.split_once('#').unwrap_or((path, ""));
    let path_only = before_hash.split('?').next().unwrap_or("");
    let mut masked = false;
    let mut pat = |p: &str| -> String {
        p.split('/')
            .map(|seg| {
                let seg = seg.split('?').next().unwrap_or("");
                if instance_segment(seg) {
                    masked = true;
                    ":x".to_string()
                } else {
                    seg.to_lowercase()
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    };
    let p = pat(path_only);
    let h = pat(hash);
    (if h.is_empty() { format!("/{p}") } else { format!("/{p}#{h}") }, masked)
}

/// The template a page is an instance of: route pattern plus open dialog.
pub fn template_key(snap: &Snapshot, literals: &[String]) -> String {
    let (route, _) = route_pattern(&snap.url);
    match snap.modal_name.as_deref().filter(|n| !n.trim().is_empty()) {
        Some(n) => format!("{route} [dialog: {}]", mask(n, literals)),
        None => route,
    }
}

/// A short description for choosing a destination. On pages about one
/// instance (the route had an ID), headings name that instance, so they're
/// left out.
pub fn template_desc(snap: &Snapshot, literals: &[String]) -> String {
    let (route, instance) = route_pattern(&snap.url);
    let mut parts: Vec<String> = Vec::new();
    if let Some(n) = snap.modal_name.as_deref().filter(|n| !n.trim().is_empty()) {
        parts.push(format!("dialog \"{}\"", mask(n, literals)));
    }
    // The selected tab is what tells sibling pages apart ("settings" with
    // the "filters and blocked addresses" tab vs the "general" tab).
    let tabs: Vec<String> = snap
        .els
        .iter()
        .filter(|e| e.r == "tab" && e.has_flag("selected") && e.rc.is_none())
        .map(|e| mask(&e.n, literals))
        .filter(|t| !t.is_empty())
        .take(3)
        .collect();
    if !tabs.is_empty() {
        parts.push(format!("tab: {}", tabs.join(", ")));
    }
    if !instance {
        let mut hs: Vec<String> = Vec::new();
        for t in snap.texts.iter().filter(|t| t.h.is_some_and(|h| h <= 3) && t.rc.is_none()) {
            let m = mask(&t.x, literals);
            if !m.is_empty() && !hs.contains(&m) {
                hs.push(m);
            }
            if hs.len() >= 4 {
                break;
            }
        }
        if !hs.is_empty() {
            parts.push(format!("headings: {}", hs.join(" | ")));
        }
        let title = mask(snap.title.split(['·', '|', '—']).next().unwrap_or(""), literals);
        if !title.trim().is_empty() {
            parts.insert(0, title.trim().to_string());
        }
    } else {
        parts.push("page about one item".to_string());
    }
    parts.push(format!("at {route}"));
    parts.join(" · ")
}

/// Hash of a navigation control, or None when it must not be learned: inside
/// a record (that's data, not site structure), hidden, or a commit (R2/R3).
pub fn control_hash(e: &El, literals: &[String]) -> Option<u64> {
    if e.rc.is_some() || e.latent() {
        return None;
    }
    if license::risk(e).0 >= Risk::R2 {
        return None;
    }
    let name = mask(&e.n, literals);
    if name.trim().is_empty() {
        return None;
    }
    // Wizard controls move through a form, not the site: where they lead
    // depends on what was filled in, so replaying them skips required input.
    let first = name.split(|c: char| !c.is_alphanumeric()).find(|w| !w.is_empty()).unwrap_or("");
    if matches!(first, "continue" | "next" | "back" | "previous" | "prev" | "proceed" | "review" | "step" | "skip" | "finish" | "done" | "cancel" | "close") {
        return None;
    }
    let ctx = e.c.as_deref().map(|c| mask(c, literals)).unwrap_or_default();
    let ctx: String = ctx.chars().take(40).collect();
    Some(fnv(format!("{}|{}|{}", e.r, name, ctx).as_bytes()))
}

/// The task's literal values, masked out of everything stored.
pub fn literals_of(task: &str) -> Vec<String> {
    let mut v = crate::spans::quoted(task);
    for w in task.split_whitespace() {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric() && c != '@' && c != '.' && c != '-');
        if w.contains('@') || w.chars().any(|c| c.is_ascii_digit()) {
            v.push(w.to_string());
        }
    }
    // Capitalized name-like runs ("Leilani Kahananui") are people or things.
    let ws: Vec<&str> = task.split_whitespace().collect();
    let mut i = 0;
    while i < ws.len() {
        let cap = |w: &str| w.chars().next().is_some_and(|c| c.is_uppercase()) && w.chars().skip(1).any(|c| c.is_lowercase());
        if cap(ws[i]) && i > 0 {
            let mut j = i;
            while j < ws.len() && cap(ws[j]) {
                j += 1;
            }
            if j - i >= 2 {
                let run = ws[i..j].join(" ");
                v.push(run.trim_matches(|c: char| !c.is_alphanumeric()).to_string());
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    v.retain(|s| s.chars().count() >= 2);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_and_route_keys() {
        assert_eq!(site_key("https://www.Example.com:8443/a/b?x=1").as_deref(), Some("example.com"));
        assert_eq!(site_key("http://127.0.0.1:62184/heldout2/mail.html#/settings").as_deref(), Some("local/heldout2/mail.html"));
        assert_eq!(route_pattern("http://h/app.html#/accounts/acc-1002").0, "/app.html#/accounts/:x");
        assert_eq!(route_pattern("http://h/mail.html#/search/from%3Abilling%40q.io").0, "/mail.html#/search/:x");
        assert_eq!(route_pattern("https://h/orders?page=2").0, "/orders");
        assert_eq!(route_pattern("http://h/heldout2/mail.html#/settings/filters").0, "/heldout2/mail.html#/settings/filters");
        assert_eq!(route_pattern("https://h/api/v2/users/8f3e2a91c0").0, "/api/v2/users/:x");
    }

    #[test]
    fn masking_removes_identity() {
        let lits = literals_of("Move the email from Leilani Kahananui about \"Q4 venue shortlist\" to 2026");
        let m = mask("Q4 venue shortlist from Leilani Kahananui, INV-2026-0918, x@y.io", &lits);
        assert!(!m.contains("leilani") && !m.contains("venue") && !m.contains("2026") && !m.contains("x@y"), "{m}");
    }

    #[test]
    fn merge_keeps_both_sides() {
        let mut a = Site::default();
        let x = a.template("/#/", "home", 1);
        let y = a.template("/#/settings", "settings", 1);
        a.edge(x, 1, y, 1);
        let mut b = Site::default();
        let bx = b.template("/#/", "home", 2);
        let bz = b.template("/#/billing", "billing", 2);
        b.edge(bx, 2, bz, 2);
        a.merge_from(&b);
        let home = a.find("/#/").unwrap();
        assert_eq!(a.reachable(home, 10).len(), 2);
        a.merge_from(&b);
        assert_eq!(a.edges.len(), 2);
    }

    #[test]
    fn graph_paths() {
        let mut s = Site::default();
        let a = s.template("/#/", "home", 1);
        let b = s.template("/#/settings", "settings", 1);
        let c = s.template("/#/settings/filters", "filters", 1);
        s.edge(a, 11, b, 1);
        s.edge(b, 22, c, 1);
        let p = s.path(a, c).unwrap();
        assert_eq!(p.iter().map(|e| e.ctl).collect::<Vec<_>>(), vec![11, 22]);
        assert_eq!(s.reachable(a, 10), vec![(b, 1), (c, 2)]);
        assert!(s.path(c, a).is_none());
    }
}
