//! The scraping side of the program runner: lists, fields, links, detail
//! pages and pagination, done in code by the in-page runtime (`SCRAPE_JS`).
//! An LLM is asked once per page layout, to say which list holds the items
//! and which structural path holds each requested field; every item on every
//! page is then extracted by code. Leaf detail pages are fetched in parallel
//! and read without leaving the list; pages that need a browser to render
//! are opened for real.

use anyhow::{Context, Result, bail};
use fab_core::Session;
use fab_core::script::{SCRAPE_JS, Value};
use serde_json::{Value as J, json};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// The page of an item that has no link to follow: nothing on it.
const EMPTY: &str = "about:blank#fab-empty";

/// The model that maps fields to paths (`FAB_SCRAPE_MODEL`, else the compile model).
pub fn model() -> String {
    std::env::var("FAB_SCRAPE_MODEL").ok().filter(|m| !m.is_empty()).unwrap_or_else(crate::compile::model)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    sel: String,
    span: u64,
    /// A pager control was seen for this list (so its absence means the end).
    pager: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    list: String,
    key: String,
    i: u64,
    level: u64,
    fields: J,
    link: Option<String>,
    /// The item's own distinctive attributes (name → bool | short string),
    /// bounded by [`MAX_ITEM_ATTRS`] and [`MAX_ITEM_ATTR_VALUE`].
    #[serde(default)]
    attrs: std::collections::BTreeMap<String, J>,
}

const MAX_ITEM_ATTRS: usize = 8;
const MAX_ITEM_ATTR_VALUE: usize = 40;

/// Whether `attrs` is a well-formed attribute set: few names, each a boolean
/// or a short string.
fn valid_attrs(attrs: &std::collections::BTreeMap<String, J>) -> bool {
    attrs.len() <= MAX_ITEM_ATTRS
        && attrs.iter().all(|(k, v)| {
            !k.is_empty() && k.len() <= 32 && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && match v {
                J::Bool(_) => true,
                J::String(s) => s.chars().count() <= MAX_ITEM_ATTR_VALUE,
                _ => false,
            }
        })
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum Nav {
    /// Opened by address: go back to this one.
    Url(String),
    /// Opened by a click in a client-rendered app: history.back().
    History(String),
}

/// What a list shows on the page now, against what the scraper produced.
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct Sighting {
    pub list: String,
    /// The keys of the list's items on the page, in page order.
    pub shown: Vec<String>,
    /// Those the scraper has not produced.
    pub unseen: Vec<String>,
}

/// What the loop over a list will ask of its items (from the program).
#[derive(Clone, Debug, Default)]
pub struct Wants {
    pub fields: Vec<String>,
    /// What the program reads on an item's page after opening it (fields,
    /// lists, questions): picks which of the item's links to follow.
    pub after: Vec<String>,
    pub open: Option<String>,
    pub leaf_open: bool,
}

/// The scraper's state. It is persisted in durable program checkpoints, so it
/// deserializes through [`ScraperDto`] and is validated (see
/// [`Scraper::validate`]): a checkpoint can't hold an open item that doesn't
/// exist, a fetched page without an open item, or seen items never produced.
///
/// Page-bound (meaningful only on the page/document they were captured on,
/// which the checkpoint's page binding guards): `lists`, `maps`, `seen`,
/// `virt` (a page fetched into the live document's cache), `open_item`,
/// `navs` (the tab's history) and `last_list`. The rest is knowledge about
/// page layouts (`doc_maps`, `real_layouts`, …) or accounting.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "ScraperDto")]
pub struct Scraper {
    /// The user's request, when the program was compiled from one: context
    /// for mapping (which items, which link).
    pub request: String,
    lists: HashMap<String, List>,
    /// list selector -> (fields mapped, the mapping, link path)
    maps: HashMap<String, (Vec<String>, J, Option<String>)>,
    /// layout|fields → mappings learned (a layout may have variants).
    doc_maps: HashMap<String, Vec<J>>,
    /// Lists restricted to one nesting level (top-level comments of a thread).
    only_level: HashMap<String, u64>,
    /// Lists whose link to follow was asked for (None may be the answer).
    link_asked: HashSet<String>,
    /// Lists whose link was re-picked after none of the pages it led to
    /// could be read (one try each).
    /// (Absent in checkpoints written before links were re-picked.)
    #[serde(default)]
    link_retried: HashSet<String>,
    /// Re-learning attempts per layout|fields (bounded).
    relearned: HashMap<String, u32>,
    seen: HashMap<String, HashSet<String>>,
    items: Vec<Item>,
    /// Detail pages that need a real browser (client-rendered), by layout.
    real_layouts: HashSet<String>,
    checked_layouts: HashSet<String>,
    virt: Option<String>,
    /// The item whose page is open (until `back`).
    open_item: Option<u32>,
    /// Detail pages fetched so far (samples for mapping a layout).
    fetched: Vec<String>,
    navs: Vec<Nav>,
    last_list: Option<String>,
    pub turns: u32,
    pub cost: f64,
    pub llm_ms: f64,
    pub log: Vec<String>,
}

/// The wire form of [`Scraper`]: the same fields, checked on the way in.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ScraperDto {
    request: String,
    lists: HashMap<String, List>,
    maps: HashMap<String, (Vec<String>, J, Option<String>)>,
    doc_maps: HashMap<String, Vec<J>>,
    only_level: HashMap<String, u64>,
    link_asked: HashSet<String>,
    /// (Absent in checkpoints written before links were re-picked.)
    #[serde(default)]
    link_retried: HashSet<String>,
    relearned: HashMap<String, u32>,
    seen: HashMap<String, HashSet<String>>,
    items: Vec<Item>,
    real_layouts: HashSet<String>,
    checked_layouts: HashSet<String>,
    virt: Option<String>,
    open_item: Option<u32>,
    fetched: Vec<String>,
    navs: Vec<Nav>,
    last_list: Option<String>,
    turns: u32,
    cost: f64,
    llm_ms: f64,
    log: Vec<String>,
}

impl TryFrom<ScraperDto> for Scraper {
    type Error = String;
    fn try_from(d: ScraperDto) -> Result<Self, String> {
        let s = Scraper {
            request: d.request,
            lists: d.lists,
            maps: d.maps,
            doc_maps: d.doc_maps,
            only_level: d.only_level,
            link_asked: d.link_asked,
            link_retried: d.link_retried,
            relearned: d.relearned,
            seen: d.seen,
            items: d.items,
            real_layouts: d.real_layouts,
            checked_layouts: d.checked_layouts,
            virt: d.virt,
            open_item: d.open_item,
            fetched: d.fetched,
            navs: d.navs,
            last_list: d.last_list,
            turns: d.turns,
            cost: d.cost,
            llm_ms: d.llm_ms,
            log: d.log,
        };
        s.validate().map_err(|e| format!("invalid scraper checkpoint: {e}"))?;
        Ok(s)
    }
}

/// The attribute set the page reported for an item, as much of it as is
/// well-formed (booleans and short strings, at most [`MAX_ITEM_ATTRS`]).
fn item_attrs(v: &J) -> std::collections::BTreeMap<String, J> {
    let mut out = std::collections::BTreeMap::new();
    for (k, v) in v.as_object().into_iter().flatten() {
        let one = std::collections::BTreeMap::from([(k.clone(), v.clone())]);
        if out.len() < MAX_ITEM_ATTRS && valid_attrs(&one) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn is_http(u: &str) -> bool {
    u.starts_with("http://") || u.starts_with("https://")
}

/// An address without its fragment: what a page is, not where in it.
fn bare(u: &str) -> &str {
    u.split('#').next().unwrap_or(u)
}

/// Two addresses on one site.
fn same_origin(a: &str, b: &str) -> bool {
    match (reqwest::Url::parse(a), reqwest::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

/// Link paths tried in place of one that led nowhere readable.
const LINK_TRIES: usize = 3;
/// Pages read to prove a link path opens something.
const LINK_PROBES: usize = 3;

/// The @href paths the page shows on its items other than `before`, an
/// item's own-site links first (one to another site is usually elsewhere: an
/// external article, an author), at most `LINK_TRIES`. `leaves` is what
/// `sampleLeaves` returns: per sampled item, `[path, value]` pairs.
fn link_candidates(leaves: &J, here: &str, before: &str) -> Vec<String> {
    let mut out: Vec<(bool, String)> = vec![];
    for pair in leaves.as_array().into_iter().flatten().flat_map(|s| s.as_array().into_iter().flatten()) {
        let (Some(p), Some(v)) = (pair[0].as_str(), pair[1].as_str()) else { continue };
        // A link to the page we are on is not the item's page.
        if !p.ends_with("@href") || p == before || !is_http(v) || bare(v) == bare(here) || out.iter().any(|(_, q)| q == p) {
            continue;
        }
        out.push((same_origin(here, v), p.to_string()));
    }
    out.sort_by_key(|(own, _)| !*own);
    out.into_iter().take(LINK_TRIES).map(|(_, p)| p).collect()
}

fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

/// A page's layout: host and path with ids and numbers generalized.
/// Two addresses on one site with the same page layout (page numbers and
/// queries may differ).
pub(crate) fn same_layout(a: &str, b: &str) -> bool {
    let origin = |u: &str| reqwest::Url::parse(u).ok().map(|u| u.origin().ascii_serialization());
    origin(a).is_some() && origin(a) == origin(b) && layout(a) == layout(b)
}

fn layout(url: &str) -> String {
    let u = url.split(['?', '#']).next().unwrap_or(url);
    let (dir, file) = u.rsplit_once('/').unwrap_or(("", u));
    let ext = file.rsplit_once('.').map(|(_, e)| e).filter(|e| e.len() <= 5).unwrap_or("");
    // Pages in one directory share a layout; ids in the path are generalized.
    let dir: String = dir.chars().map(|c| if c.is_ascii_digit() { '9' } else { c }).collect();
    format!("{dir}/*.{ext}")
}

async fn fs(sess: &mut Session, expr: &str) -> Result<J> {
    let wrapped = format!("(async () => {{ if (!window.__fs) {{ {SCRAPE_JS} }} return await ({expr}); }})()");
    sess.browser.eval(&wrapped).await
}

impl Scraper {
    /// The structural invariants the scraper's own operations maintain.
    fn validate(&self) -> Result<(), String> {
        // Item handles are u32 (`Value::Item`).
        if self.items.len() > u32::MAX as usize {
            return Err("more items than item handles".into());
        }
        if let Some(h) = self.open_item
            && h as usize >= self.items.len()
        {
            return Err(format!("the open item {h} doesn't exist ({} items)", self.items.len()));
        }
        // A page read from its fetch (or an item without a page) is only
        // ever the open item's page.
        if let Some(v) = &self.virt {
            if self.open_item.is_none() {
                return Err("a fetched page is open without an open item".into());
            }
            if v != EMPTY && !is_http(v) {
                return Err(format!("the open fetched page isn't a web address: {}", crate::trunc(v, 80)));
            }
        }
        if let Some(u) = self.fetched.iter().find(|u| !is_http(u)) {
            return Err(format!("a fetched page isn't a web address: {}", crate::trunc(u, 80)));
        }
        for it in &self.items {
            match &it.fields {
                J::Null => {}
                J::Object(o) if !o.contains_key("__link") => {}
                _ => return Err(format!("item {:?} of {:?} has malformed fields", it.key, it.list)),
            }
        }
        if let Some(it) = self.items.iter().find(|it| !valid_attrs(&it.attrs)) {
            return Err(format!("item {:?} of {:?} has a malformed attribute set", it.key, it.list));
        }
        for (sel, (fields, map, link)) in &self.maps {
            let keys: Option<HashSet<&str>> = map.as_object().map(|o| o.keys().map(String::as_str).collect());
            if keys != Some(fields.iter().map(String::as_str).collect()) {
                return Err(format!("the mapping of {sel:?} doesn't match its fields"));
            }
            if link.as_ref().is_some_and(|l| !l.ends_with("@href")) {
                return Err(format!("the link of {sel:?} isn't an @href path"));
            }
        }
        for (key, maps) in &self.doc_maps {
            if maps.is_empty() || !maps.iter().all(J::is_object) {
                return Err(format!("the page mappings for {key:?} are empty or malformed"));
            }
        }
        for (key, n) in &self.relearned {
            if !self.doc_maps.contains_key(key) || !(1..=2).contains(n) {
                return Err(format!("re-learning count {n} for {key:?} is out of bounds or has no mapping"));
            }
        }
        if let Some(l) = self.real_layouts.iter().find(|l| !self.checked_layouts.contains(*l)) {
            return Err(format!("layout {l:?} is marked browser-rendered without being checked"));
        }
        // Seen items are exactly the items produced: each seen key (scoped
        // "list@document") is an item of that list, and each item was seen.
        let mut produced: HashMap<&str, HashSet<&str>> = HashMap::new();
        for it in &self.items {
            produced.entry(it.list.as_str()).or_default().insert(it.key.as_str());
        }
        let mut seen_pairs: HashSet<(&str, &str)> = HashSet::new();
        for (scope, keys) in &self.seen {
            let owners: Vec<(&str, &HashSet<&str>)> = produced
                .iter()
                .filter(|(l, _)| scope.strip_prefix(**l).and_then(|r| r.strip_prefix('@')).is_some_and(|doc| doc.is_empty() || is_http(doc)))
                .map(|(l, ks)| (*l, ks))
                .collect();
            for k in keys {
                let mut found = false;
                for (l, ks) in &owners {
                    if ks.contains(k.as_str()) {
                        seen_pairs.insert((l, k.as_str()));
                        found = true;
                    }
                }
                if !found {
                    return Err(format!("seen item {k:?} in {scope:?} was never produced"));
                }
            }
        }
        if let Some(it) = self.items.iter().find(|it| !seen_pairs.contains(&(it.list.as_str(), it.key.as_str()))) {
            return Err(format!("item {:?} of {:?} isn't marked seen", it.key, it.list));
        }
        if !(self.cost.is_finite() && self.cost >= 0.0 && self.llm_ms.is_finite() && self.llm_ms >= 0.0) {
            return Err("invalid accounting".into());
        }
        Ok(())
    }

    /// A detail page is open (an item's, or an address opened with `open`)
    /// until `back`.
    pub(crate) fn in_detail(&self) -> bool {
        self.open_item.is_some() || !self.navs.is_empty()
    }

    /// Whether the page being read is a fetched copy (or an item's empty
    /// page), not the browser's.
    pub(crate) fn is_virtual(&self) -> bool {
        self.virt.is_some()
    }

    /// The list page the outermost open detail page was opened from: None
    /// when the browser never left it (the detail page was fetched).
    pub(crate) fn list_page(&self) -> Option<&str> {
        self.navs.first().map(|n| match n {
            Nav::Url(u) | Nav::History(u) => u.as_str(),
        })
    }

    /// The address an item's `open` would go to, when it has one. None when
    /// the item has no link to follow, or when its page is a fetched copy
    /// that leaves the browser where it is.
    pub(crate) fn item_link(&self, handle: u32) -> Option<String> {
        self.items.get(handle as usize).and_then(|it| it.link.clone())
    }

    /// The address an `open` request would navigate to, as evidence for
    /// resolving one that may have been dispatched (see `durable_program`).
    pub(crate) fn open_target(&self, item: &fab_core::script::Value) -> Option<String> {
        if let fab_core::script::Value::Str(u) = item.scalar() {
            let u = u.trim();
            if u.starts_with("http://") || u.starts_with("https://") {
                return Some(u.to_string());
            }
        }
        let handle = item.get("__item");
        let fab_core::script::Value::Item(h) = (if matches!(item, fab_core::script::Value::Item(_)) { item } else { &handle }) else { return None };
        self.item_link(*h as u32)
    }

    pub(crate) fn adopt_page(&mut self) -> Result<()> {
        anyhow::ensure!(self.open_item.is_none() && self.navs.is_empty(), "finish or cancel the open detail-page workflow before adopting another page");
        self.lists.clear();
        self.maps.clear();
        self.last_list = None;
        Ok(())
    }

    fn note(&mut self, s: String) {
        self.log.push(s);
    }

    async fn ask(&mut self, system: &str, user: String) -> Result<J> {
        let m = model();
        let llm = crate::planner::llm_for(&m)?;
        let mut last_err = String::new();
        for _ in 0..2 {
            let mut body = json!({
                "messages": [{"role": "system", "content": system}, {"role": "user", "content": &user}],
                "temperature": 0,
                "max_tokens": 1500,
                "reasoning": {"enabled": false},
            });
            let r = match llm.chat(body.clone()).await {
                Ok(r) => Ok(r),
                Err(e) if format!("{e:#}").to_lowercase().contains("reasoning") => {
                    body.as_object_mut().unwrap().remove("reasoning");
                    llm.chat(body).await
                }
                Err(e) => Err(e),
            };
            let (msg, usage, dt) = r?;
            self.turns += 1;
            self.cost += usage.cost;
            self.llm_ms += dt.as_secs_f64() * 1e3;
            let text = msg["content"].as_str().unwrap_or("");
            // The last JSON object in the reply: models sometimes answer,
            // reconsider ("Wait, …") and answer again.
            match last_object(text) {
                Some(v) => return Ok(v),
                None => last_err = format!("no JSON: {text}"),
            }
        }
        bail!("the field mapper didn't answer in JSON ({})", crate::trunc(&last_err, 200))
    }

    /// The not-yet-seen items of the list the user means by `what`, with the
    /// fields the loop wants already extracted.
    pub async fn items(&mut self, sess: &mut Session, what: &str, wants: &Wants) -> Result<Vec<Value>> {
        if self.virt.as_deref() == Some(EMPTY) {
            return Ok(vec![]);
        }
        // On an opened item's fetched page, or the live one.
        let url = json!(self.virt);
        let known = self.lists.get(what).cloned();
        let mut n = match &known {
            Some(l) => fs(sess, &format!("__fs.count({}, {}, {url})", js_str(&l.sel), l.span)).await?.as_u64().unwrap_or(0),
            None => 0,
        };
        // A known list missing from an item's fetched copy may be built in
        // the browser (the first such page taught the list there).
        if let (Some(l), 0, true) = (&known, n, self.open_item.is_some()) {
            if self.virt.as_deref().is_some_and(|v| v != EMPTY) && !self.real_layouts.is_empty() {
                if let Some((here, v)) = self.render_open(sess).await? {
                    let t = Instant::now();
                    while n == 0 && t.elapsed() < Duration::from_secs(5) {
                        n = fs(sess, &format!("__fs.count({}, {}, null)", js_str(&l.sel), l.span)).await?.as_u64().unwrap_or(0);
                        if n == 0 {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                    if n > 0 {
                        self.real_layouts.insert(layout(&v));
                        self.checked_layouts.insert(layout(&v));
                    } else {
                        self.render_close(sess, here, v).await?;
                    }
                }
            }
        }
        let url = json!(self.virt);
        // An item page opened for real may still be loading its list.
        if let (Some(l), 0, true, None) = (&known, n, self.open_item.is_some(), &self.virt) {
            let t = Instant::now();
            while n == 0 && t.elapsed() < Duration::from_secs(5) {
                tokio::time::sleep(Duration::from_millis(200)).await;
                n = fs(sess, &format!("__fs.count({}, {}, null)", js_str(&l.sel), l.span)).await?.as_u64().unwrap_or(0);
            }
        }
        // An opened item's page (a post without comments) may have none.
        if n == 0 && self.open_item.is_some() {
            let any = fs(sess, &format!("__fs.lists(1, {url})")).await?.as_array().is_some_and(|a| !a.is_empty());
            if known.is_some() || !any {
                self.note(format!("items \"{what}\": none on this page"));
                return Ok(vec![]);
            }
        }
        let list = match known {
            Some(l) if n > 0 => l,
            _ => match self.choose(sess, what, wants).await? {
                Some(l) => {
                    self.lists.insert(what.to_string(), l.clone());
                    // `choose` may have opened the page for real.
                    let url = json!(self.virt);
                    if fs(sess, &format!("__fs.count({}, {}, {url})", js_str(&l.sel), l.span)).await?.as_u64().unwrap_or(0) == 0 {
                        self.note(format!("items \"{what}\": none on this page"));
                        return Ok(vec![]);
                    }
                    l
                }
                None => {
                    self.note(format!("items \"{what}\": none on this page"));
                    return Ok(vec![]);
                }
            },
        };
        // `next page` pages the list on the live page, not one read from an
        // opened item's fetched page.
        if self.virt.is_none() {
            self.last_list = Some(list.sel.clone());
        }
        // Fields the mapping lacks (another extract of the same list): map again.
        let need: Vec<String> = wants.fields.iter().filter(|f| !self.maps.get(&list.sel).is_some_and(|(have, _, _)| have.contains(f))).cloned().collect();
        // (A list whose items open by a click has no link: asked once.)
        if !need.is_empty() || (wants.open.is_some() && self.maps.get(&list.sel).is_some_and(|m| m.2.is_none()) && !self.link_asked.contains(&list.sel)) {
            self.map_items(sess, &list, what, wants).await?;
        }
        let out = self.take(sess, &list).await?;
        self.note(format!("items \"{what}\": {} new", out.len()));
        // Leaf detail pages (opened, or read for fields the list lacks): fetch
        // them all now, in parallel.
        // (Not inside an opened page: extract reads a list there as it is.)
        let dives = self.open_item.is_none() && wants.fields.iter().any(|f| self.maps.get(&list.sel).is_some_and(|m| m.1.get(f.as_str()).is_none_or(J::is_null)));
        if (wants.leaf_open || dives) && !out.is_empty() {
            let here = sess_url(sess).await;
            let urls: Vec<String> = out
                .iter()
                .filter_map(|v| if let Value::Item(h) = v { self.items.get(*h as usize).and_then(|it| it.link.clone()) } else { None })
                .filter(|u| fetchable(u, &here) && !self.real_layouts.contains(&layout(u)))
                .collect();
            if !urls.is_empty() {
                let t = Instant::now();
                let mut r = fs(sess, &format!("__fs.fetch({}, 8)", json!(urls))).await?;
                let ok = r.as_array().map(|a| a.iter().filter(|x| x["ok"] == true).count()).unwrap_or(0);
                // Only the first ones per layout are ever read again.
                for u in &urls {
                    if self.fetched.iter().filter(|f| layout(f) == layout(u)).count() < FETCHED_PER_LAYOUT {
                        self.fetched.push(u.clone());
                    }
                }
                self.note(format!("fetched {ok}/{} detail pages in {:.0} ms", urls.len(), t.elapsed().as_secs_f64() * 1e3));
                // A link to follow whose pages none of them can be read is
                // unproven. The page already shows the items' other links:
                // take the first of those that opens a page that reads,
                // rather than asking for one the mapper already answered.
                // One try per list, and a fetch the program does anyway.
                if ok == 0 && urls.len() > 1 && self.maps.get(&list.sel).is_some_and(|m| m.2.is_some()) && self.link_retried.insert(list.sel.clone()) {
                    if let Some(again) = self.own_link(sess, &list, &urls).await? {
                        r = again;
                    }
                }
                let failed: Vec<String> = r.as_array().into_iter().flatten().filter(|x| x["ok"] != true).map(|x| format!("{} ({})", x["url"].as_str().unwrap_or(""), x["err"].as_str().unwrap_or(""))).collect();
                if !failed.is_empty() {
                    self.note(format!("{} page(s) didn't load and will be opened for real: {}", failed.len(), crate::trunc(&failed.join(", "), 300)));
                }
                // A page whose fetch holds little text may render in the
                // browser: check the thinnest one once per layout.
                let thin = r.as_array().into_iter().flatten().filter(|x| x["ok"] == true).min_by_key(|x| x["text"].as_u64().unwrap_or(0)).cloned();
                if let Some(x) = thin {
                    let (u, n) = (x["url"].as_str().unwrap_or_default().to_string(), x["text"].as_u64().unwrap_or(0));
                    self.check_rendering(sess, &u, n).await?;
                }
            }
        }
        Ok(out)
    }

    /// The not-yet-seen items of a mapped list, read in code; each is
    /// marked seen.
    async fn take(&mut self, sess: &mut Session, list: &List) -> Result<Vec<Value>> {
        // The first read of a live list waits until it stops growing: pages
        // render a few items first and the rest moments later.
        if self.virt.is_none() && !self.seen.contains_key(&self.scope(&list.sel)) {
            let count = |n: J| n.as_u64().unwrap_or(0);
            let expr = format!("__fs.count({}, {}, null)", js_str(&list.sel), list.span);
            let (t, mut last) = (Instant::now(), count(fs(sess, &expr).await?));
            while t.elapsed() < Duration::from_secs(3) {
                tokio::time::sleep(Duration::from_millis(400)).await;
                let now = count(fs(sess, &expr).await?);
                if now == last {
                    break;
                }
                last = now;
            }
        }
        let url = json!(self.virt);
        let (_, map, link) = self.maps.get(&list.sel).cloned().unwrap_or_default();
        let mut map = if map.is_null() { json!({}) } else { map };
        if let Some(l) = &link {
            map["__link"] = json!(l);
        }
        // Seen items are per list and document: the same list on two opened
        // pages holds different items.
        let scope = self.scope(&list.sel);
        let skip: Vec<String> = self.seen.get(&scope).map(|s| s.iter().cloned().collect()).unwrap_or_default();
        let got = fs(sess, &format!("__fs.items({}, {}, {}, {}, {url})", js_str(&list.sel), list.span, map, json!(skip))).await?;
        let mut out = vec![];
        let seen = self.seen.entry(scope).or_default();
        let only = self.only_level.get(&list.sel).copied();
        for it in got.as_array().into_iter().flatten() {
            if only.is_some_and(|l| it["level"].as_u64().unwrap_or(0) != l) {
                continue;
            }
            let key = it["key"].as_str().unwrap_or_default().to_string();
            let mut fields = it["fields"].clone();
            let link = fields.get("__link").and_then(J::as_str).map(str::to_string);
            if let Some(o) = fields.as_object_mut() {
                o.remove("__link");
            }
            // Nothing the list's items have (no field, no link where items
            // link): another kind of item among them (an ad), not one of them.
            let empty = fields.as_object().is_some_and(|o| !o.is_empty() && o.values().all(J::is_null));
            if empty && link.is_none() && self.maps.get(&list.sel).is_some_and(|m| m.2.is_some()) {
                continue;
            }
            seen.insert(key.clone());
            let h = u32::try_from(self.items.len()).map_err(|_| anyhow::anyhow!("too many items for item handles"))?;
            let attrs = item_attrs(&it["attrs"]);
            self.items.push(Item { list: list.sel.clone(), key, i: it["i"].as_u64().unwrap_or(0), level: it["level"].as_u64().unwrap_or(0), fields, link, attrs });
            out.push(Value::Item(h));
        }
        Ok(out)
    }

    /// Where a list's seen keys are kept: per list and document.
    fn scope(&self, sel: &str) -> String {
        format!("{sel}@{}", self.virt.clone().unwrap_or_default())
    }

    /// What a list already identified shows on the page now: its items'
    /// keys (at the level it is restricted to), and those never produced.
    /// Evidence for an interrupted `items` or `next page`: it reads the page
    /// in code and never asks the model. `what` names the list of an
    /// `items` request; `None` is the list `next page` pages.
    pub(crate) async fn sight(&mut self, sess: &mut Session, what: Option<&str>) -> Result<Sighting> {
        let sel = match what {
            Some(w) => self.lists.get(w).map(|l| l.sel.clone()),
            None => self.last_list.clone(),
        };
        let Some(sel) = sel else { bail!("the list was not identified before the interruption, so the page can't be compared with it") };
        let span = self.lists.values().find(|l| l.sel == sel).map(|l| l.span).unwrap_or(0);
        let got = fs(sess, &format!("__fs.items({}, {span}, null, [], {})", js_str(&sel), json!(self.virt))).await?;
        let seen = self.seen.get(&self.scope(&sel));
        let only = self.only_level.get(&sel).copied();
        let shown: Vec<String> = got
            .as_array()
            .into_iter()
            .flatten()
            .filter(|it| only.is_none_or(|l| it["level"].as_u64().unwrap_or(0) == l))
            .filter_map(|it| it["key"].as_str().map(str::to_string))
            .collect();
        let unseen = shown.iter().filter(|k| !seen.is_some_and(|s| s.contains(*k))).cloned().collect();
        Ok(Sighting { list: sel, shown, unseen })
    }

    /// `items` answered from the page alone, for a list identified and
    /// mapped for what the loop wants: the unseen items it shows now. Fails
    /// when answering would need the model (the list or a field unmapped)
    /// or the page shows none of the list (its end, or another page).
    pub(crate) async fn items_seen_now(&mut self, sess: &mut Session, what: &str, wants: &Wants) -> Result<(Sighting, Vec<Value>)> {
        let list = self.lists.get(what).cloned().with_context(|| format!("the list \"{what}\" was not identified before the interruption"))?;
        let mapped = self.maps.get(&list.sel).is_some_and(|(have, _, link)| {
            wants.fields.iter().all(|f| have.contains(f)) && !(wants.open.is_some() && link.is_none() && !self.link_asked.contains(&list.sel))
        });
        anyhow::ensure!(mapped, "the list \"{what}\" is not mapped for the fields the loop reads");
        let seen = self.sight(sess, Some(what)).await?;
        anyhow::ensure!(!seen.shown.is_empty(), "the page shows none of the list \"{what}\"");
        if self.virt.is_none() {
            self.last_list = Some(list.sel.clone());
        }
        let out = self.take(sess, &list).await?;
        self.note(format!("items \"{what}\": {} unseen on the page (observed)", out.len()));
        Ok((seen, out))
    }

    /// Picks the list and maps the loop's fields and link in one LLM call.
    /// The list the user means, or None when this page doesn't have it (an
    /// opened item's page may lack it: a story without comments).
    /// Opens the item page being read from its fetch in the browser.
    /// Returns (the page to come back to, the item page).
    async fn render_open(&mut self, sess: &mut Session) -> Result<Option<(String, String)>> {
        let Some(v) = self.virt.clone().filter(|v| v != EMPTY) else { return Ok(None) };
        let here = sess_url(sess).await;
        self.virt = None;
        self.navs.push(Nav::Url(here.clone()));
        goto(sess, &v).await?;
        Ok(Some((here, v)))
    }

    /// Undoes [`Self::render_open`]: back to the fetched copy.
    async fn render_close(&mut self, sess: &mut Session, here: String, v: String) -> Result<()> {
        self.navs.pop();
        goto(sess, &here).await?;
        self.virt = Some(v);
        Ok(())
    }

    async fn choose(&mut self, sess: &mut Session, what: &str, wants: &Wants) -> Result<Option<List>> {
        let found = self.choose_here(sess, what, wants).await?;
        // A fetched item page without the list may build it in the browser
        // (comments loaded by script): open it for real and look again. Pages
        // of that layout open for real from then on, if it was there.
        if found.is_some() || self.virt.as_deref().is_none_or(|v| self.real_layouts.contains(&layout(v))) {
            return Ok(found);
        }
        let Some((here, v)) = self.render_open(sess).await? else { return Ok(found) };
        let lay = layout(&v);
        self.real_layouts.insert(lay.clone());
        self.checked_layouts.insert(lay.clone());
        let found = self.choose_here(sess, what, wants).await?;
        if found.is_some() {
            self.note(format!("pages like {v} build their {what} in the browser: opening them for real"));
            return Ok(found);
        }
        // Not there either: the page has none; keep reading fetched copies.
        self.real_layouts.remove(&lay);
        self.render_close(sess, here, v).await?;
        Ok(None)
    }

    async fn choose_here(&mut self, sess: &mut Session, what: &str, wants: &Wants) -> Result<Option<List>> {
        let mut cands = J::Null;
        // On an opened item's fetched page, learn the list from the fetched
        // page of this layout where it's richest: the first item opened may
        // have none (a story without comments), and a list learned there
        // would be the menu.
        if let Some(v) = self.virt.clone().filter(|v| v != EMPTY) {
            let lay = layout(&v);
            let mut urls: Vec<String> = self.fetched.iter().filter(|u| layout(u) == lay).take(8).cloned().collect();
            if !urls.contains(&v) {
                urls.insert(0, v);
            }
            let mut best = (0u64, J::Null);
            for u in urls {
                let c = fs(sess, &format!("__fs.lists(6, {})", js_str(&u))).await?;
                let n = c.as_array().and_then(|a| a.iter().map(|x| x["n"].as_u64().unwrap_or(0)).max()).unwrap_or(0);
                if n > best.0 {
                    best = (n, c);
                }
            }
            cands = best.1;
        }
        // Lists render late on some pages: wait for one.
        let t = Instant::now();
        while cands.is_null() && t.elapsed() < Duration::from_secs(8) {
            let c = fs(sess, &format!("__fs.lists(6, {})", json!(self.virt))).await?;
            if c.as_array().is_some_and(|a| !a.is_empty()) {
                cands = c;
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        // A live page may still be rendering: choose among the lists once
        // they stop changing (a thread's first replies render before its
        // comment list).
        if self.virt.is_none() && !cands.is_null() {
            let shape = |c: &J| c.as_array().map(|a| a.iter().map(|x| (x["sel"].to_string(), x["n"].as_u64().unwrap_or(0))).collect::<Vec<_>>()).unwrap_or_default();
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(4) {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let c = fs(sess, "__fs.lists(6, null)").await?;
                let same = shape(&c) == shape(&cands);
                cands = c;
                if same {
                    break;
                }
            }
        }
        let mut arr = cands.as_array().cloned().unwrap_or_default();
        // Lists of the same items (a thread and the replies nested in one of
        // its comments): the fullest one is the list; the others are parts.
        let item = |c: &J| c["sel"].as_str().and_then(|s| s.split_once("|>")).map(|(_, i)| i.trim().to_string());
        let most: HashMap<String, u64> = arr.iter().filter_map(|c| Some((item(c)?, c["n"].as_u64().unwrap_or(0)))).fold(HashMap::new(), |mut m, (k, n)| {
            let e = m.entry(k).or_insert(0);
            *e = (*e).max(n);
            m
        });
        let mut kept = std::collections::HashSet::new();
        arr.retain(|c| match item(c) {
            Some(k) => c["n"].as_u64().unwrap_or(0) == most[&k] && kept.insert(k),
            None => true,
        });
        if arr.is_empty() {
            if self.open_item.is_some() {
                return Ok(None);
            }
            bail!("no list of items on this page ({})", self.virt.clone().unwrap_or(sess_url(sess).await));
        }
        let v = self.ask_items(what, wants, &arr).await?;
        let Some(k) = v["list"].as_u64() else {
            if self.open_item.is_some() {
                return Ok(None);
            }
            bail!("this page has no list of {what} ({})", sess_url(sess).await);
        };
        let c = arr.get(k as usize).or(arr.first()).context("no list")?;
        let list = List { sel: c["sel"].as_str().unwrap_or_default().to_string(), span: c["span"].as_u64().unwrap_or(0), pager: false };
        self.store_map(&list, wants, &v);
        self.note(format!("list \"{what}\" = {} ({} items)", list.sel, c["n"]));
        Ok(Some(list))
    }

    async fn map_items(&mut self, sess: &mut Session, list: &List, what: &str, wants: &Wants) -> Result<()> {
        let leaves = fs(sess, &format!("__fs.sampleLeaves({}, {}, {})", js_str(&list.sel), list.span, json!(self.virt))).await?;
        let mut all = wants.clone();
        if let Some((have, _, _)) = self.maps.get(&list.sel) {
            for f in have {
                if !all.fields.contains(f) {
                    all.fields.push(f.clone());
                }
            }
        }
        let cand = json!([{"sel": list.sel, "n": 0, "sample": leaves}]);
        let _ = what;
        let v = self.ask_items(what, &all, cand.as_array().unwrap()).await?;
        self.store_map(list, &all, &v);
        Ok(())
    }

    fn store_map(&mut self, list: &List, wants: &Wants, v: &J) {
        if wants.open.is_some() {
            self.link_asked.insert(list.sel.clone());
        }
        match v["only_level"].as_u64() {
            Some(l) => {
                self.only_level.insert(list.sel.clone(), l);
            }
            None => {
                self.only_level.remove(&list.sel);
            }
        }
        let v = &{
            let mut v = v.clone();
            v["fields"] = align(&wants.fields, &v["fields"]);
            v
        };
        // Only a link target: an item whose "link" is some other attribute opens by a click.
        let link = v["link"].as_str().filter(|s| s.ends_with("@href")).map(str::to_string);
        self.log.push(format!("mapped {} → {}{}", list.sel, v["fields"], link.as_ref().map(|l| format!(" link {l}")).unwrap_or_default()));
        self.maps.insert(list.sel.clone(), (wants.fields.clone(), v["fields"].clone(), link));
    }

    /// The pages a link path opens for a list's items, by item key, read in
    /// code: an item the path doesn't open has none.
    async fn item_links(&self, sess: &mut Session, list: &List, link: &str) -> Result<HashMap<String, Option<String>>> {
        let mut map = self.maps.get(&list.sel).map(|m| m.1.clone()).unwrap_or(J::Null);
        if map.is_null() {
            map = json!({});
        }
        map["__link"] = json!(link);
        let got = fs(sess, &format!("__fs.items({}, {}, {}, [], {})", js_str(&list.sel), list.span, map, json!(self.virt))).await?;
        Ok(got
            .as_array()
            .into_iter()
            .flatten()
            .map(|it| (it["key"].as_str().unwrap_or_default().to_string(), it["fields"]["__link"].as_str().map(str::to_string)))
            .collect())
    }

    /// Takes the first of the items' other links that opens a page that
    /// reads, when the link the mapper gave led nowhere. The answer is not
    /// asked again: it was already given, and it was wrong. Returns the
    /// fetch of the new links' pages, when one was taken.
    async fn own_link(&mut self, sess: &mut Session, list: &List, dead: &[String]) -> Result<Option<J>> {
        let before = self.maps.get(&list.sel).and_then(|m| m.2.clone()).unwrap_or_default();
        let here = match self.virt.clone() {
            Some(u) => u,
            None => sess_url(sess).await,
        };
        let leaves = fs(sess, &format!("__fs.sampleLeaves({}, {}, {})", js_str(&list.sel), list.span, json!(self.virt))).await?;
        for path in link_candidates(&leaves, &here, &before) {
            let links = self.item_links(sess, list, &path).await?;
            let urls: Vec<String> = links.values().flatten().filter(|u| fetchable(u, &here) && !dead.contains(u)).cloned().collect();
            let probe: Vec<&String> = urls.iter().take(LINK_PROBES).collect();
            if probe.is_empty() || !fs(sess, &format!("__fs.fetch({}, {LINK_PROBES})", json!(probe))).await?.as_array().is_some_and(|a| a.iter().any(|x| x["ok"] == true)) {
                continue;
            }
            if let Some(m) = self.maps.get_mut(&list.sel) {
                m.2 = Some(path.clone());
            }
            for it in self.items.iter_mut().filter(|x| x.list == list.sel) {
                if let Some(l) = links.get(&it.key) {
                    it.link = l.clone();
                }
            }
            // Pages that could not be read are not evidence for anything.
            self.fetched.retain(|f| !dead.contains(f));
            self.note(format!("the link to follow led nowhere readable: {path} opens a page that does"));
            let r = fs(sess, &format!("__fs.fetch({}, 8)", json!(urls))).await?;
            for u in &urls {
                if self.fetched.iter().filter(|f| layout(f) == layout(u)).count() < FETCHED_PER_LAYOUT {
                    self.fetched.push(u.clone());
                }
            }
            return Ok(Some(r));
        }
        Ok(None)
    }

    async fn ask_items(&mut self, what: &str, wants: &Wants, cands: &[J]) -> Result<J> {
        let mut u = String::new();
        if !self.request.is_empty() {
            u.push_str(&format!("The user's request: \"{}\"\n", self.request));
        }
        u.push_str(&format!("The program wants the items: \"{what}\".\nFields to extract from each item: {}.\n", json!(wants.fields)));
        let after = if wants.after.is_empty() { String::new() } else { format!(" The page it opens must hold: {}.", wants.after.join("; ")) };
        match wants.open.as_deref() {
            Some(h) if !h.trim().is_empty() => u.push_str(&format!("Each item's link to follow: \"{h}\".{after}\n")),
            Some(_) if !after.is_empty() => u.push_str(&format!("Each item's link to follow: the one whose page has what the program reads there.{after} (An item often links elsewhere too, e.g. to an external article or the author: pick the link to the page that holds this.)\n")),
            Some(_) => u.push_str("Each item's link to follow: its main link (to the item's own page).\n"),
            None => u.push_str("Each item's link to follow: its main link (to the item's own page), for fields the item doesn't show.\n"),
        }
        u.push_str("\nLists found on the page (path = value for up to 3 sample items):\n");
        for (k, c) in cands.iter().enumerate() {
            u.push_str(&format!("\nList {k} ({} items):\n", c["n"]));
            u.push_str(&table(&c["sample"], "item"));
        }
        self.ask(ITEM_SYSTEM, u).await
    }

    /// Forgets the fields and link of every item the program no longer
    /// holds (`live`) and that isn't open: they are read only through a held
    /// handle. The item stays (its list and key keep it from repeating), so a
    /// checkpoint stops growing with every item scraped.
    pub fn forget_items(&mut self, live: &std::collections::HashSet<u32>) {
        for (h, it) in self.items.iter_mut().enumerate() {
            let h = h as u32;
            if !live.contains(&h) && self.open_item != Some(h) && (!it.fields.is_null() || it.link.is_some() || !it.attrs.is_empty()) {
                it.fields = J::Null;
                it.link = None;
                it.attrs.clear();
            }
        }
    }

    /// Whether `h` names an item this scraper produced.
    pub fn has_item(&self, h: u32) -> bool {
        (h as usize) < self.items.len()
    }

    /// An item as the program sees it: its extracted fields, plus its handle.
    pub fn record(&self, h: u32) -> Value {
        let mut fs = vec![("__item".to_string(), Value::Item(h))];
        if let Some(it) = self.items.get(h as usize) {
            fs.push(("__level".to_string(), Value::Num(it.level as f64)));
            if let Some(o) = it.fields.as_object() {
                fs.extend(o.iter().map(|(k, v)| (k.clone(), Value::from_json(v))));
            }
            // The item's own attributes read as fields the mapper left unset.
            for (k, v) in &it.attrs {
                let name = fab_core::script::field_name(k);
                match fs.iter().position(|(f, _)| *f == name) {
                    Some(p) if matches!(fs[p].1, Value::Null) => fs[p].1 = Value::from_json(v),
                    Some(_) => {}
                    None => fs.push((name, Value::from_json(v))),
                }
            }
        }
        Value::Obj(fs)
    }

    /// Fields of an item, or of the page (the opened detail page when there is one).
    pub async fn extract(&mut self, sess: &mut Session, fields: &[String], from: Option<&Value>) -> Result<Value> {
        let handle = from.map(|v| v.get("__item")).filter(|v| matches!(v, Value::Item(_)));
        let mut from = handle.as_ref().or(from);
        // `extract … from item` while that item's page is open: read its page
        // for the fields its list entry doesn't show.
        if let (Some(Value::Item(h)), Some(o)) = (from, self.open_item) {
            let listed = self.items.get(*h as usize).map(|it| fields.iter().all(|f| it.fields.get(f).is_some_and(|v| !v.is_null()))).unwrap_or(false);
            if *h == o && !listed {
                from = None;
            }
        }
        if let Some(Value::Item(h)) = from {
            let it = self.items.get(*h as usize).context("unknown item")?.clone();
            let have = self.maps.get(&it.list).map(|m| m.0.clone()).unwrap_or_default();
            if fields.iter().any(|f| !have.contains(f)) {
                // Fields the loop didn't announce: map them now, then read them
                // for every item on the page.
                let list = self.lists.values().find(|l| l.sel == it.list).cloned().context("unknown list")?;
                let what = self.lists.iter().find(|(_, l)| l.sel == it.list).map(|(w, _)| w.clone()).unwrap_or_default();
                let wants = Wants { fields: fields.to_vec(), ..Default::default() };
                self.map_items(sess, &list, &what, &wants).await?;
                let (_, map, link) = self.maps.get(&list.sel).cloned().unwrap_or_default();
                let mut map = map;
                if let Some(l) = &link {
                    map["__link"] = json!(l);
                }
                let got = fs(sess, &format!("__fs.items({}, {}, {}, [], {})", js_str(&list.sel), list.span, map, json!(self.virt))).await?;
                for g in got.as_array().into_iter().flatten() {
                    let k = g["key"].as_str().unwrap_or_default();
                    for x in self.items.iter_mut().filter(|x| x.list == list.sel && x.key == k) {
                        let mut f = g["fields"].clone();
                        x.link = f.get("__link").and_then(J::as_str).map(str::to_string).or(x.link.take());
                        if let Some(o) = f.as_object_mut() {
                            o.remove("__link");
                        }
                        x.fields = f;
                    }
                }
            }
            let it = self.items.get(*h as usize).context("unknown item")?.clone();
            // Fields the list doesn't show at all: read them from the item's page.
            let map = self.maps.get(&it.list).map(|m| m.1.clone()).unwrap_or_default();
            let deep: Vec<String> = fields.iter().filter(|f| map.get(f.as_str()).is_none_or(J::is_null)).cloned().collect();
            let mut out: Vec<(String, Value)> = fields.iter().map(|f| (f.clone(), Value::from_json(&it.fields[f]))).collect();
            if !deep.is_empty() && self.open_item.is_none() && (it.link.is_some() || self.maps.get(&it.list).is_some()) {
                self.note(format!("reading {deep:?} from the item's page"));
                let leaf = true;
                self.open(sess, &Value::Item(*h), leaf).await?;
                let page = Box::pin(self.extract(sess, &deep, None)).await;
                self.back(sess).await?;
                for (k, v) in out.iter_mut() {
                    if deep.contains(k) {
                        *v = page.as_ref().map(|p| p.get(k)).unwrap_or(Value::Null);
                    }
                }
                page?;
            }
            return Ok(Value::Obj(out));
        }
        if let Some(Value::Obj(_)) = from {
            return Ok(Value::Obj(fields.iter().map(|f| (f.clone(), from.unwrap().get(f))).collect()));
        }
        if self.virt.as_deref() == Some(EMPTY) {
            return Ok(Value::Obj(fields.iter().map(|f| (f.clone(), Value::Null)).collect()));
        }
        // The page: the fetched detail page, or the live one.
        let url = match &self.virt {
            Some(u) => u.clone(),
            None => sess_url(sess).await,
        };
        let lay = layout(&url);
        let key = format!("{lay}|{}", fields.join(","));
        if !self.doc_maps.contains_key(&key) {
            let m = self.map_doc(sess, fields, &lay, true).await?;
            self.doc_maps.insert(key.clone(), vec![m]);
        }
        let target = self.virt.clone();
        let mut got = J::Null;
        for m in self.doc_maps.get(&key).cloned().unwrap_or_default() {
            got = self.read_doc(sess, target.as_deref(), &m).await?;
            if got.as_object().is_some_and(|o| o.values().any(|v| !v.is_null())) {
                break;
            }
        }
        // Nothing found with any mapping: this page may be another variant of
        // the layout. Learn it from this page (kept when it finds something).
        // (A yes/no field is legitimately null: no.)
        let tries = self.relearned.get(&key).copied().unwrap_or(0);
        if !got.as_object().is_some_and(|o| o.values().any(|v| !v.is_null())) && tries < 2 && !fields.iter().all(|f| f.starts_with("yes/no:")) {
            *self.relearned.entry(key.clone()).or_default() += 1;
            let m = self.map_doc(sess, fields, &lay, false).await?;
            let g = self.read_doc(sess, target.as_deref(), &m).await?;
            if g.as_object().is_some_and(|o| o.values().any(|v| !v.is_null())) {
                self.doc_maps.entry(key.clone()).or_default().push(m);
                got = g;
            }
        }
        Ok(Value::Obj(fields.iter().map(|f| (f.clone(), Value::from_json(&got[f]))).collect()))
    }

    /// Learns which path holds each field on pages of this layout, from the
    /// fetched pages that show the most fields (or just this page).
    async fn map_doc(&mut self, sess: &mut Session, fields: &[String], lay: &str, samples: bool) -> Result<J> {
        let target = self.virt.clone();
        let pages = match &target {
            Some(t) if samples => {
                let mut urls: Vec<String> = self.fetched.iter().filter(|u| layout(u) == lay).take(30).cloned().collect();
                if !urls.contains(t) {
                    urls.insert(0, t.clone());
                }
                fs(sess, &format!("__fs.docSamples({})", json!(urls))).await?.as_array().cloned().unwrap_or_default()
            }
            Some(t) => vec![self.page_leaves(sess, Some(t)).await?],
            None => vec![self.page_leaves(sess, None).await?],
        };
        let mut u = format!("Fields to extract from pages with this layout: {}.\n\n{} sample page(s) (path = value on each page):\n", json!(fields), pages.len());
        let pages = J::Array(pages);
        u.push_str(&table(&pages, "page"));
        let v = self.ask(DOC_SYSTEM, u.clone()).await?;
        let mut aligned = align(fields, &v["fields"]);
        // A path on none of the pages the mapper was shown reads nothing
        // anywhere: ask once more for those fields, naming the paths.
        let missing: Vec<String> = serde_json::from_value(fs(sess, &format!("__fs.unresolved({aligned}, {pages})")).await?).unwrap_or_default();
        if !missing.is_empty() {
            let named: Vec<String> = missing.iter().map(|f| format!("{f} → {}", aligned[f])).collect();
            self.note(format!("paths on no sample page, asking again: {}", named.join("; ")));
            u.push_str(&format!("\nYou answered {aligned}, but these paths are on none of the pages above: {}. Map those fields again with paths listed above, or null.\n", named.join("; ")));
            let again = align(&missing, &self.ask(DOC_SYSTEM, u).await?["fields"]);
            for f in &missing {
                aligned[f] = again[f].clone();
            }
        }
        self.note(format!("mapped page fields for {lay} → {aligned}"));
        Ok(aligned)
    }

    async fn read_doc(&mut self, sess: &mut Session, url: Option<&str>, map: &J) -> Result<J> {
        match url {
            Some(u) => fs(sess, &format!("__fs.doc({}, {})", js_str(u), map)).await,
            None => {
                // A client-rendered page may still be filling in.
                let t = Instant::now();
                loop {
                    let g = fs(sess, &format!("__fs.doc(null, {map})")).await?;
                    let any = g.as_object().is_some_and(|o| o.values().any(|v| !v.is_null()));
                    if any || t.elapsed() > Duration::from_secs(4) {
                        return Ok(g);
                    }
                    tokio::time::sleep(Duration::from_millis(120)).await;
                }
            }
        }
    }

    async fn page_leaves(&mut self, sess: &mut Session, url: Option<&str>) -> Result<J> {
        let arg = url.map(js_str).unwrap_or_else(|| "null".into());
        let t = Instant::now();
        loop {
            let v = fs(sess, &format!("__fs.docLeaves({arg})")).await?;
            let rich = v.as_array().map(|a| a.len()).unwrap_or(0) >= 3;
            if rich || url.is_some() || t.elapsed() > Duration::from_secs(5) {
                return Ok(v);
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// Follows an item's link.
    pub async fn open(&mut self, sess: &mut Session, item: &Value, leaf: bool) -> Result<()> {
        // An address: go there (and `back` returns).
        if let Value::Str(u) = item.scalar() {
            let u = u.trim().to_string();
            if u.starts_with("http://") || u.starts_with("https://") {
                // (Pushed even when already there, so `back` pairs with it.)
                let here = sess_url(sess).await;
                self.navs.push(Nav::Url(here.clone()));
                if here != u {
                    goto(sess, &u).await?;
                    self.note(format!("opened {u}"));
                }
                return Ok(());
            }
        }
        let handle = item.get("__item");
        let Value::Item(h) = (if matches!(item, Value::Item(_)) { item } else { &handle }) else { bail!("open needs an item from `items` or an address") };
        let it = self.items.get(*h as usize).context("unknown item")?.clone();
        self.open_item = Some(*h);
        let here = sess_url(sess).await;
        // The list's items have a link to follow and this one doesn't (a job
        // ad among stories has no comments link): its page is empty, rather
        // than whatever else the item links to.
        let mapped = self.maps.get(&it.list).is_some_and(|m| m.2.is_some());
        if it.link.is_none() && mapped {
            self.virt = Some(EMPTY.to_string());
            self.note("this item has no such link: nothing to read".into());
            return Ok(());
        }
        if let Some(u) = &it.link {
            if leaf && fetchable(u, &here) && !self.real_layouts.contains(&layout(u)) {
                let r = fs(sess, &format!("__fs.fetch([{}], 1)", js_str(u))).await?;
                if r[0]["ok"] == true {
                    self.check_rendering(sess, u, r[0]["text"].as_u64().unwrap_or(0)).await?;
                    if !self.real_layouts.contains(&layout(u)) {
                        self.virt = Some(u.clone());
                        return Ok(());
                    }
                }
            }
            if fetchable(u, &here) {
                self.navs.push(Nav::Url(here));
                goto(sess, u).await?;
                self.note(format!("opened {u}"));
                return Ok(());
            }
        }
        // A client-side route: click the item and come back through history.
        let path = self.maps.get(&it.list).and_then(|m| m.2.clone()).unwrap_or_default();
        let ok = fs(sess, &format!("__fs.clickItem({}, {}, {}, {})", js_str(&it.list), self.lists.values().find(|l| l.sel == it.list).map(|l| l.span).unwrap_or(0), it.i, js_str(&path))).await?;
        if ok != true {
            bail!("couldn't find the item to open");
        }
        self.navs.push(Nav::History(here.clone()));
        settle(sess).await;
        let t = Instant::now();
        while sess_url(sess).await == here && t.elapsed() < Duration::from_secs(3) {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        if sess_url(sess).await == here {
            self.navs.pop();
            bail!("clicking the item didn't open anything");
        }
        Ok(())
    }

    /// Whether pages of `u`'s layout can be read from their fetch: a thin
    /// fetch (little text) is compared once with the page as rendered in a
    /// hidden frame; pages that fill in in the browser are opened for real.
    async fn check_rendering(&mut self, sess: &mut Session, u: &str, fetched: u64) -> Result<()> {
        let lay = layout(u);
        if fetched >= 200 || self.checked_layouts.contains(&lay) || self.real_layouts.contains(&lay) {
            return Ok(());
        }
        self.checked_layouts.insert(lay.clone());
        let rendered = fs(sess, &format!("__fs.renderedText({})", js_str(u))).await?.as_i64().unwrap_or(-1);
        let real = if rendered < 0 { fetched < 40 } else { rendered as u64 > fetched * 2 + 100 };
        if real {
            self.real_layouts.insert(lay);
            self.note(format!("pages like {u} render in the browser ({fetched} chars fetched, {rendered} rendered): opening them for real"));
        }
        Ok(())
    }

    /// An item's page is open: page questions are read from it in code.
    /// The item page being read from a fetched copy, if any.
    pub fn source_url(&self) -> Option<&str> {
        self.virt.as_deref().filter(|u| *u != EMPTY)
    }

    pub fn is_open(&self) -> bool {
        self.open_item.is_some()
    }

    pub async fn back(&mut self, sess: &mut Session) -> Result<()> {
        // Back on the item's list: it is the list `next page` pages again
        // (reading the item's page made its own list the last one).
        if let Some(parent) = self.open_item.and_then(|h| self.items.get(h as usize)).map(|it| it.list.clone()) {
            self.last_list = Some(parent);
        }
        self.open_item = None;
        if self.virt.take().is_some() {
            return Ok(());
        }
        match self.navs.pop() {
            Some(Nav::Url(u)) => {
                if sess_url(sess).await != u {
                    goto(sess, &u).await?;
                }
            }
            Some(Nav::History(u)) => {
                sess.browser.eval("history.back()").await?;
                settle(sess).await;
                // Wait for the list to come back.
                if let Some(sel) = self.last_list.clone() {
                    let span = self.lists.values().find(|l| l.sel == sel).map(|l| l.span).unwrap_or(0);
                    let t = Instant::now();
                    while t.elapsed() < Duration::from_secs(6) {
                        let n = fs(sess, &format!("__fs.count({}, {span})", js_str(&sel))).await.ok().and_then(|v| v.as_u64()).unwrap_or(0);
                        if n > 0 && sess_url(sess).await == u {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            None => {}
        }
        Ok(())
    }

    /// Shows the next page or batch of the current list; false at the end.
    pub async fn next(&mut self, sess: &mut Session, what: &str) -> Result<bool> {
        // Paging an opened page read from its fetch: nothing to page when its
        // HTML has no pager; otherwise open it for real first.
        if let Some(u) = self.virt.clone() {
            if u == EMPTY || fs(sess, &format!("__fs.next({})", js_str(&u))).await?.is_null() {
                self.note(format!("next {what}: none on this page (end)"));
                return Ok(false);
            }
            self.virt = None;
            let here = sess_url(sess).await;
            self.navs.push(Nav::Url(here));
            goto(sess, &u).await?;
            self.note(format!("opened {u} (to page it)"));
        }
        let (sel, span) = match self.last_list.clone() {
            Some(s) => {
                let span = self.lists.values().find(|l| l.sel == s).map(|l| l.span).unwrap_or(0);
                (s, span)
            }
            None => (String::new(), 0),
        };
        let st = format!("__fs.state({}, {span})", if sel.is_empty() { "null".into() } else { js_str(&sel) });
        let before = fs(sess, &st).await?;
        let ctl = fs(sess, "__fs.next()").await?;
        let t = Instant::now();
        if ctl.is_null() {
            let had_pager = self.lists.values().any(|l| l.sel == sel && l.pager);
            if had_pager {
                self.note(format!("next {what}: none (end)"));
                return Ok(false);
            }
            // Infinite scroll: scroll down and wait for more items.
            fs(sess, "__fs.scrollEnd()").await?;
            let changed = wait_change(sess, &before, &sel, span, Duration::from_millis(2500)).await?;
            self.note(format!("next {what}: scrolled, {}", if changed { "more loaded" } else { "no more (end)" }));
            return Ok(changed);
        }
        for l in self.lists.values_mut() {
            if l.sel == sel {
                l.pager = true;
            }
        }
        let href = ctl["href"].as_str().map(str::to_string);
        let here = before["url"].as_str().unwrap_or_default().to_string();
        let clicked = href.as_ref().filter(|h| fetchable(h, &here)).is_none();
        match href.filter(|h| fetchable(h, &here)) {
            Some(h) => {
                goto(sess, &h).await?;
            }
            None => {
                fs(sess, "__fs.clickNext()").await?;
            }
        }
        let changed = wait_change(sess, &before, &sel, span, Duration::from_secs(8)).await?;
        self.note(format!("next {what}: \"{}\" → {} in {:.0} ms", ctl["text"].as_str().unwrap_or(""), if changed { "new page" } else { "nothing new (end)" }, t.elapsed().as_secs_f64() * 1e3));
        // A control that loaded nothing may not be the list's pager (a "See
        // more" on one post): an endless list loads more when scrolled.
        if !changed {
            // A link that went somewhere else: back to the list first.
            if !clicked && sess_url(sess).await != here {
                goto(sess, &here).await?;
                settle(sess).await;
            }
            // Twice: a list still hydrating may ignore the first scroll.
            let mut more = false;
            for _ in 0..2 {
                fs(sess, "__fs.scrollEnd()").await?;
                more = wait_change(sess, &before, &sel, span, Duration::from_millis(4000)).await?;
                if more {
                    break;
                }
            }
            if more {
                for l in self.lists.values_mut().filter(|l| l.sel == sel) {
                    l.pager = false;
                }
                self.note(format!("next {what}: scrolled, more loaded"));
                return Ok(true);
            }
        }
        Ok(changed)
    }
}

async fn wait_change(sess: &mut Session, before: &J, sel: &str, span: u64, max: Duration) -> Result<bool> {
    let t = Instant::now();
    let e = format!("__fs.state({}, {span})", if sel.is_empty() { "null".into() } else { js_str(sel) });
    while t.elapsed() < max {
        tokio::time::sleep(Duration::from_millis(80)).await;
        let Ok(now) = fs(sess, &e).await else { continue };
        let n = now["n"].as_u64().unwrap_or(0);
        let strip = |v: &J| v.as_str().unwrap_or_default().split('#').next().unwrap_or_default().to_string();
        let hash_route = |v: &J| v.as_str().is_some_and(|u| u.contains("#/") || u.contains("#!"));
        let url_moved = if hash_route(&now["url"]) || hash_route(&before["url"]) { now["url"] != before["url"] } else { strip(&now["url"]) != strip(&before["url"]) };
        let moved = url_moved || now["first"] != before["first"] || n > before["n"].as_u64().unwrap_or(0);
        if moved && (n > 0 || sel.is_empty()) {
            // Let an appended batch finish rendering.
            tokio::time::sleep(Duration::from_millis(60)).await;
            return Ok(true);
        }
    }
    Ok(false)
}

async fn sess_url(sess: &mut Session) -> String {
    sess.browser.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// Navigates for real; a throttled response (HTTP 429/5xx) is retried with
/// backoff, as fetches are.
async fn goto(sess: &mut Session, u: &str) -> Result<()> {
    for tries in 0..6u32 {
        sess.invalidate();
        sess.browser.goto(u, &sess.k).await?;
        let status = fs(sess, "__fs.status()").await.ok().and_then(|v| v.as_u64()).unwrap_or(0);
        if !(status == 429 || status >= 500) {
            return Ok(());
        }
        if tries == 5 {
            bail!("{u} answered HTTP {status} (throttled) after {} tries", tries + 1);
        }
        tokio::time::sleep(Duration::from_millis((400u64 << tries).min(8000))).await;
    }
    Ok(())
}

async fn settle(sess: &mut Session) {
    sess.invalidate();
    let doc = sess.browser.doc_id().await.unwrap_or_default();
    let _ = sess.browser.settle(&sess.k, &doc).await;
}

/// An address that loads its own document (not a hash route of this one).
/// Fetched pages kept per layout: choosing a list reads 8, checking
/// rendering 30.
const FETCHED_PER_LAYOUT: usize = 30;

fn fetchable(u: &str, here: &str) -> bool {
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return false;
    }
    let strip = |s: &str| s.split('#').next().unwrap_or(s).to_string();
    !(u.contains('#') && strip(u) == strip(here))
}

/// The mapper's answer keyed by the requested field names (models echo a
/// field as given, snake-cased, or reworded).
fn align(fields: &[String], m: &J) -> J {
    use fab_core::script::field_name;
    let Some(o) = m.as_object() else { return json!({}) };
    let words = |s: &str| field_name(s).split('_').filter(|w| w.len() > 2).map(str::to_string).collect::<HashSet<_>>();
    let mut left: Vec<&String> = o.keys().collect();
    let mut out = serde_json::Map::new();
    let mut pending = vec![];
    for f in fields {
        let hit = left.iter().position(|k| *k == f).or_else(|| left.iter().position(|k| field_name(k) == field_name(f)));
        match hit {
            Some(i) => {
                out.insert(f.clone(), o[left.remove(i)].clone());
            }
            None => pending.push(f),
        }
    }
    for f in pending {
        let fw = words(f);
        let best = left.iter().enumerate().map(|(i, k)| (i, words(k).intersection(&fw).count())).max_by_key(|x| x.1);
        let i = match best {
            Some((i, n)) if n > 0 => Some(i),
            _ if !left.is_empty() && out.len() + left.len() == fields.len() => Some(0),
            _ => None,
        };
        let v = i.map(|i| o[left.remove(i)].clone()).unwrap_or(J::Null);
        out.insert(f.clone(), v);
    }
    J::Object(out)
}

/// The last complete top-level JSON object in `text`.
fn last_object(text: &str) -> Option<J> {
    let mut last: Option<(usize, J)> = None;
    for (i, _) in text.match_indices('{') {
        // Skip braces inside the object already found.
        if last.as_ref().is_some_and(|(end, _)| i < *end) {
            continue;
        }
        let mut de = serde_json::Deserializer::from_str(&text[i..]).into_iter::<J>();
        if let Some(Ok(v)) = de.next().filter(|v| v.as_ref().is_ok_and(J::is_object)) {
            last = Some((i + de.byte_offset(), v));
        }
    }
    last.map(|(_, v)| v)
}

/// Sample leaves as a compact table: one line per path, values per sample.
fn table(samples: &J, what: &str) -> String {
    let mut order: Vec<String> = vec![];
    let mut vals: HashMap<String, Vec<String>> = HashMap::new();
    let n = samples.as_array().map(|a| a.len()).unwrap_or(0);
    for (k, s) in samples.as_array().into_iter().flatten().enumerate() {
        for pair in s.as_array().into_iter().flatten() {
            let (Some(p), Some(v)) = (pair[0].as_str(), pair[1].as_str()) else { continue };
            if !vals.contains_key(p) {
                order.push(p.to_string());
                vals.insert(p.to_string(), vec![String::new(); n]);
            }
            vals.get_mut(p).unwrap()[k] = crate::trunc(v, 90);
        }
    }
    let mut out = String::new();
    for p in order.iter().take(120) {
        let v = &vals[p];
        if n > 1 {
            out.push_str(&format!("  {p} = {}\n", v.iter().enumerate().map(|(i, x)| format!("[{what} {}] {x}", i + 1)).collect::<Vec<_>>().join(" | ")));
        } else {
            out.push_str(&format!("  {p} = {}\n", v[0]));
        }
    }
    out
}

const ITEM_SYSTEM: &str = r##"You map fields a user wants to structural paths on a web page, so code can extract them from every item.
Reply with JSON only: {"list": <index of the list holding the wanted items, or null when none of them does>, "only_level": <n> | null, "fields": {"<field>": "<path>" | {"path": "<path>", <steps>} | null}, "link": "<path ending in @href to follow>" | null}
- "item|level" is an item's nesting level in a threaded list (0 = top level, higher = a reply). When the user wants only the top-level items (e.g. "replies to the post", "top-level comments"), set "only_level": 0; otherwise null.
- Pick the list whose items are the ones the user wants (not navigation, footer, tags or ads). When none of the lists holds them (this page has none, e.g. a story without comments), answer "list": null.
- "page|…" paths are the page around the items (its heading, title, the list's heading): use them for a field that describes the whole page, such as the category of a category page.
- For each field, pick the path whose value is exactly that field for every sample item, not a longer text that contains it. Paths are relative to the item; "N|" prefixes a row of a multi-row item.
- Text that can run to several paragraphs (a comment, a review, a description): the path of the whole text block, never one of its paragraphs.
- The item element's own attributes are paths like ".@author" (web components carry their data there); prefer one when it holds exactly the field.
- Steps take part of a path's text, applied in this order: "after": "<text>" / "before": "<text>" (the text after / before its first occurrence); "part": {"sep": "<text>", "index": <n>} (the parts split at sep, index from 0, -1 for the last); "take": "number" (its first number, only for a count, score or rating: "721 points" → "721", "4.5 out of 5 stars" → "4.5"; a measure or price keeps its unit, e.g. "12 kg"); "not": "<text>" (none when the text contains it); "has": "<word>" or ["<word>", …] (the word the text contains, else none).
- Use steps only when no path holds just the value: to take a number out of its text, or one of two values sharing a text ("Northwind · Oslo": "before" or "after" " · "). Never use them to trim words off a text that is the field (a headline keeps its "World:" prefix).
- Prefer the visible text over an attribute (@datetime, @title, @aria-label) unless only an attribute holds the value, or the field is a link.
- Copy values as shown (money keeps its currency, dates their format, ids like "#2394" or "SKU-1" whole), except counts, scores and ratings: just the number ("take": "number").
- A field with several values per item (tags, labels, authors): the "…*" path, which gives them all as a list.
- A yes/no field (sponsored, on sale, overdue, in stock): a path that has a value only when the answer is yes, so it is null otherwise (with "has": "Overdue" or "not": "No comments" when the path always has a text).
- Use null when no path holds the field.
- "link": the @href path of the item's link to follow (when asked), else null; null too when the items have no link (they open when clicked).
- Answer once, with one JSON object."##;

const DOC_SYSTEM: &str = r##"You map fields a user wants to structural paths on a web page, so code can extract them from every page with this layout.
Reply with JSON only: {"fields": {"<field>": "<path>" | {"path": "<path>", <steps>} | null}}
- Prefer a "label:…" path (the value next to that label) when there is one: it still works when other fields are missing.
- Steps take part of a path's text, applied in this order: "after": "<text>" / "before": "<text>" (the text after / before its first occurrence); "part": {"sep": "<text>", "index": <n>} (the parts split at sep, index from 0, -1 for the last); "take": "number" (its first number, only for a count, score or rating: "721 points" → "721", "4.5 out of 5 stars" → "4.5"; a measure or price keeps its unit, e.g. "12 kg"); "not": "<text>" (none when the text contains it); "has": "<word>" or ["<word>", …] (the word the text contains, else none).
- Pick the path whose value is exactly the field, not a longer text containing it; use steps only to take a number out of its text or one of two values sharing a text, never to trim words off the field's own text.
- Prefer the visible text over an attribute (@datetime, @title) unless only an attribute holds the value, or the field is a link. A field with several values: the "…*" path (a list).
- Copy values as shown (money keeps its currency, dates their format), except counts, scores and ratings: just the number. Use null when the page doesn't have the field.
- A yes/no field: a path that has a value only when the answer is yes (with "has" or "not" when the path always has a text).
- When a field asks for the first of several (the first comment's author), pick the path of the first one."##;

#[cfg(test)]
mod tests {
    use super::*;

    /// A scraper as its operations leave it: an item's fetched page open.
    fn valid() -> Scraper {
        let item = |key: &str| Item { list: "ul>li".into(), key: key.into(), i: 0, level: 0, fields: json!({"title": key}), link: Some(format!("https://x.io/p/{key}")), attrs: Default::default() };
        let lay = layout("https://x.io/p/b");
        let mut s = Scraper { request: "stories".into(), ..Default::default() };
        s.lists.insert("stories".into(), List { sel: "ul>li".into(), span: 1, pager: true });
        s.maps.insert("ul>li".into(), (vec!["title".into()], json!({"title": "a"}), Some("a@href".into())));
        s.doc_maps.insert(format!("{lay}|title"), vec![json!({"title": "h1"})]);
        s.relearned.insert(format!("{lay}|title"), 2);
        s.only_level.insert("ul>li".into(), 0);
        s.link_asked.insert("ul>li".into());
        s.items = vec![item("a"), item("b"), Item { fields: J::Null, ..item("c") }];
        s.seen.insert("ul>li@".into(), ["a", "b"].map(String::from).into());
        s.seen.insert("ul>li@https://x.io/p/b".into(), ["c".to_string()].into());
        s.checked_layouts.insert(lay.clone());
        s.real_layouts.insert(lay);
        s.fetched = vec!["https://x.io/p/b".into()];
        s.open_item = Some(1);
        s.virt = Some("https://x.io/p/b".into());
        s.navs = vec![Nav::Url("https://x.io/".into()), Nav::History("https://x.io/#/list".into())];
        s.last_list = Some("ul>li".into());
        s.turns = 3;
        s.cost = 0.01;
        s.llm_ms = 12.5;
        s.log = vec!["mapped".into()];
        s
    }

    fn load(v: J) -> Result<Scraper, String> {
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    #[test]
    fn forgotten_items_keep_their_identity_only() {
        let mut s = valid();
        let open = s.open_item;
        s.forget_items(&[0u32].into_iter().collect());
        for (h, it) in s.items.iter().enumerate() {
            let kept = h == 0 || open == Some(h as u32);
            assert_eq!(!it.fields.is_null() || it.link.is_some(), kept, "item {h}");
            assert!(!it.key.is_empty());
        }
        load(serde_json::to_value(&s).unwrap()).expect("still a valid scraper");
    }

    #[test]
    fn scraper_round_trips() {
        for s in [valid(), Scraper::default(), Scraper { virt: Some(EMPTY.into()), ..valid() }] {
            // Sets serialize in hash order: compare them as sorted arrays.
            fn sorted(mut v: J) -> J {
                match &mut v {
                    J::Array(xs) => {
                        xs.iter_mut().for_each(|x| *x = sorted(x.take()));
                        xs.sort_by_key(|x| x.to_string());
                    }
                    J::Object(m) => m.values_mut().for_each(|x| *x = sorted(x.take())),
                    _ => {}
                }
                v
            }
            let v = serde_json::to_value(&s).unwrap();
            let back = load(v.clone()).unwrap();
            assert_eq!(sorted(serde_json::to_value(&back).unwrap()), sorted(v));
        }
    }

    #[test]
    fn checkpoint_before_item_attributes_still_loads() {
        let mut v = serde_json::to_value(valid()).unwrap();
        for it in v["items"].as_array_mut().unwrap() {
            it.as_object_mut().unwrap().remove("attrs");
        }
        let s = load(v).expect("a checkpoint without attrs loads");
        assert!(s.items.iter().all(|it| it.attrs.is_empty()));
    }

    #[test]
    fn item_attributes_map_store_and_read_as_fields() {
        // The mapper names the attribute as a field; the store keeps its path.
        let mut s = Scraper::default();
        let list = List { sel: "ul>li".into(), span: 1, pager: false };
        let wants = Wants { fields: vec!["pinned".into(), "author".into()], ..Default::default() };
        s.store_map(&list, &wants, &json!({"list": 0, "fields": {"pinned": ".@pinned", "author": ".@author"}}));
        assert_eq!(s.maps["ul>li"].1, json!({"pinned": ".@pinned", "author": ".@author"}));
        // The page reports an item's distinctive attributes; ill-formed ones and any beyond the bound are dropped.
        let mut reported = serde_json::Map::new();
        reported.insert("pinned".into(), json!(true));
        reported.insert("data-kind".into(), json!("sticky"));
        reported.insert("long".into(), json!("x".repeat(41)));
        reported.insert("num".into(), json!(3));
        let attrs = item_attrs(&J::Object(reported));
        assert_eq!(serde_json::to_value(&attrs).unwrap(), json!({"data-kind": "sticky", "pinned": true}));
        // An attribute is a field the mapper left unset, never over one it set.
        let item = Item { list: "ul>li".into(), key: "a".into(), i: 0, level: 0, fields: json!({"pinned": null, "author": "ann"}), link: None, attrs };
        s.items = vec![item];
        s.seen.insert("ul>li@".into(), ["a".to_string()].into());
        let back = load(serde_json::to_value(&s).unwrap()).expect("round trip");
        let rec = back.record(0);
        assert_eq!(rec.get("pinned"), Value::Bool(true));
        assert_eq!(rec.get("data_kind"), Value::Str("sticky".into()));
        assert_eq!(rec.get("author"), Value::Str("ann".into()));
    }

    #[test]
    fn scraper_rejects_broken_invariants() {
        let cases: Vec<(&str, Box<dyn Fn(&mut J)>)> = vec![
            ("unknown field", Box::new(|v| v["extra"] = json!(1))),
            ("unknown field", Box::new(|v| v["items"][0]["extra"] = json!(1))),
            ("unknown field", Box::new(|v| v["lists"]["stories"]["extra"] = json!(1))),
            ("missing field", Box::new(|v| drop(v.as_object_mut().unwrap().remove("seen")))),
            ("open item 3 doesn't exist", Box::new(|v| v["open_item"] = json!(3))),
            ("without an open item", Box::new(|v| v["open_item"] = J::Null)),
            ("isn't a web address", Box::new(|v| v["virt"] = json!("javascript:void(0)"))),
            ("isn't a web address", Box::new(|v| v["fetched"] = json!(["file:///etc/passwd"]))),
            ("malformed attribute set", Box::new(|v| v["items"][0]["attrs"] = json!({"pinned": 1}))),
            ("malformed attribute set", Box::new(|v| v["items"][0]["attrs"] = json!({"note": "x".repeat(41)}))),
            ("malformed attribute set", Box::new(|v| v["items"][0]["attrs"] = json!({"bad name": true}))),
            ("malformed attribute set", Box::new(|v| v["items"][0]["attrs"] = (0..9).map(|i| (format!("a{i}"), json!(true))).collect::<serde_json::Map<_, _>>().into())),
            ("malformed fields", Box::new(|v| v["items"][0]["fields"] = json!("x"))),
            ("malformed fields", Box::new(|v| v["items"][0]["fields"]["__link"] = json!("https://x.io/"))),
            ("doesn't match its fields", Box::new(|v| v["maps"]["ul>li"][0] = json!(["title", "price"]))),
            ("doesn't match its fields", Box::new(|v| v["maps"]["ul>li"][1] = J::Null)),
            ("isn't an @href path", Box::new(|v| v["maps"]["ul>li"][2] = json!("a@data-id"))),
            ("empty or malformed", Box::new(|v| v["doc_maps"] = json!({"k": []}))),
            ("empty or malformed", Box::new(|v| v["doc_maps"] = json!({"k": [null]}))),
            ("out of bounds or has no mapping", Box::new(|v| v["relearned"] = json!({"nope": 1}))),
            ("out of bounds or has no mapping", Box::new(|v| *v["relearned"].as_object_mut().unwrap().values_mut().next().unwrap() = json!(3))),
            ("without being checked", Box::new(|v| v["checked_layouts"] = json!([]))),
            ("was never produced", Box::new(|v| v["seen"]["ul>li@"] = json!(["a", "b", "z"]))),
            ("was never produced", Box::new(|v| v["seen"]["ol>li@"] = json!(["a"]))),
            ("was never produced", Box::new(|v| v["seen"]["ul>li@javascript:x"] = json!(["a"]))),
            ("isn't marked seen", Box::new(|v| v["seen"]["ul>li@"] = json!(["a"]))),
            ("was never produced", Box::new(|v| v["items"][2]["list"] = json!("ol>li"))),
            ("invalid accounting", Box::new(|v| v["cost"] = json!(-1.0))),
            ("invalid accounting", Box::new(|v| v["llm_ms"] = json!(-0.5))),
        ];
        for (want, break_it) in cases {
            let mut v = serde_json::to_value(valid()).unwrap();
            break_it(&mut v);
            let err = load(v).err().unwrap_or_else(|| panic!("accepted a checkpoint with: {want}"));
            assert!(err.contains(want), "expected {want:?}, got {err:?}");
        }
    }

    #[tokio::test]
    async fn opening_an_unknown_item_leaves_nothing_open() {
        use fab_core::backend::{Browser, driver::{DriverFuture, PageDriver}};
        struct Fake;
        impl PageDriver for Fake {
            fn name(&self) -> &'static str { "scrape-test" }
            fn describe(&self) -> String { "scrape test".into() }
            fn page_id(&self) -> String { "page".into() }
            fn eval<'a>(&'a mut self, _: &'a str) -> DriverFuture<'a, Result<J>> { Box::pin(async { Ok(json!("https://x.io/")) }) }
            fn goto<'a>(&'a mut self, _: &'a str) -> DriverFuture<'a, Result<()>> { Box::pin(async { bail!("unexpected navigation") }) }
            fn close(&mut self) -> DriverFuture<'_, ()> { Box::pin(async {}) }
        }
        let mut knobs = fab_core::Knobs::default();
        knobs.shape = "off".into();
        let mut sess = Session::from_browser(Browser::from_driver(Fake), knobs, fab_core::decide::Decider(fab_core::jev::Jev::without_key())).unwrap();
        let mut s = Scraper { open_item: None, virt: None, ..valid() };
        assert!(s.open(&mut sess, &Value::Item(99), true).await.is_err());
        assert!(!s.is_open());
        assert!(load(serde_json::to_value(&s).unwrap()).is_ok());
    }

    #[test]
    fn last_answer_wins() {
        let t = "{\"list\": 0, \"link\": \".@data-id\"}\n\nWait, that's not valid.\n\n{\"list\": 0, \"fields\": {\"a\": \"b\"}, \"link\": null}";
        assert_eq!(last_object(t).unwrap(), json!({"list": 0, "fields": {"a": "b"}, "link": null}));
        assert_eq!(last_object("```json\n{\"x\": {\"y\": 1}}\n```").unwrap(), json!({"x": {"y": 1}}));
    }

    #[test]
    fn aligns_reworded_fields() {
        let f = vec!["the username of the first commenter".to_string(), "price".to_string()];
        let m = json!({"the_username_of_the_first_comment": "a.user", "Price": "span.p"});
        assert_eq!(align(&f, &m), json!({"the username of the first commenter": "a.user", "price": "span.p"}));
    }

    #[test]
    fn layouts_and_links() {
        assert_eq!(layout("http://a.io/jobs/job-1007.html?x=1"), layout("http://a.io/jobs/job-1231.html"));
        assert_eq!(layout("http://a.io/news/a/world-9.html"), layout("http://a.io/news/a/sports-2.html"));
        assert_ne!(layout("http://a.io/news/a/world-9.html"), layout("http://a.io/news/world.html"));
        assert!(fetchable("http://a.io/p/1.html", "http://a.io/index.html"));
        assert!(!fetchable("http://a.io/index.html#/home/3", "http://a.io/index.html#/page/1"));
        assert!(!fetchable("javascript:void(0)", "http://a.io/"));
    }

    #[test]
    fn link_candidates_own_site_first_and_bounded() {
        let leaves = json!([
            [[".", "t"], ["a@href", "https://ext.com/x"], ["b@href", "https://x.io/p/1"], ["c@href", "https://x.io/u/1"]],
            [["a@href", "https://ext.com/y"], ["d@href", "https://x.io/#top"], ["e@href", "https://x.io/p/2"], ["f@href", "https://z.org/"], ["g@href", "https://x.io/q"], ["a", "text"]],
        ]);
        let got = link_candidates(&leaves, "https://x.io/", "c@href");
        assert_eq!(got, ["b@href", "e@href", "g@href"]);
        assert_eq!(LINK_TRIES, 3);
        let few = json!([[["a@href", "https://ext.com/x"], ["b@href", "https://x.io/p"]]]);
        assert_eq!(link_candidates(&few, "https://x.io/", "b@href"), ["a@href"]);
        assert!(link_candidates(&few, "https://x.io/", "").len() <= LINK_TRIES);
        assert!(link_candidates(&json!([]), "https://x.io/", "").is_empty());
    }
}
