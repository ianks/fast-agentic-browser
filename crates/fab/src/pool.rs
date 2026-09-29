//! A pool of pages (tabs) in one browser, so concurrent requests to a session
//! run side by side instead of queueing behind each other. Every page is a
//! `Session` of its own (snapshot, decisions, live feed) on its own tab; the
//! browser, its profile, cookies and logins are shared. One browser per
//! profile is a hard limit (Firefox's profile lock, Chrome's SingletonLock),
//! so tabs are the unit of concurrency.
//!
//! Leases and affinity: a request leases one page for its whole run and gives
//! it back when done. Each request names its client (FAB_CLIENT; plain CLI
//! commands are all client ""), and a client's next
//! request goes back to the page its last one finished on, so sequential steps
//! continue where they left off. A request whose client's page is busy (the
//! same client asking twice at once) or was reclaimed gets another page,
//! opened at the address the client was last on ([`Lease::fork`]).
//!
//! Choice, in order: the client's own idle page (never waits in line); an
//! idle page nobody used; a new page while under `max`; the least recently
//! used idle page no longer held for its client; else wait in line (FIFO)
//! up to `wait`, failing at once when `queue` requests already wait.
//! The hold matters only when the pool is full: a client's next step
//! usually comes within seconds, and taking its page in between would lose
//! what is on it (a half-filled form), which opening its address elsewhere
//! can't bring back. It lasts 3× the client's usual gap between requests
//! (1 s to `hold`), or `hold` for a client not seen twice yet.
//! Pages idle longer than `idle` close, down to `min`; the first page (the
//! browser's own tab, "home") always stays. Before a lease, a page must
//! answer a script within a few seconds, or it is renewed on a fresh tab (a
//! crashed or hung renderer), or replaced. A lease dropped without
//! [`Lease::release`] (the request timed out, failed hard or was cancelled)
//! discards its page, except home, which is kept and re-checked.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Opens, checks and closes pages; the pool decides when.
pub trait Manager: Send + Sync + 'static {
    type Page: Send + 'static;
    fn open(&self) -> impl Future<Output = Result<Self::Page>> + Send;
    /// Readies an idle page for a lease: checks it answers, repairing it in
    /// place if it can (true: it was repaired). Err: unusable.
    fn ready(&self, p: &mut Self::Page) -> impl Future<Output = Result<bool>> + Send;
    fn close(&self, p: Self::Page) -> impl Future<Output = ()> + Send;
    /// Closes the browser, with the home page when it came back.
    fn shutdown(&self, home: Option<Self::Page>) -> impl Future<Output = ()> + Send;
    /// Where the page is (its address), remembered as its client's place.
    fn place(&self, p: &Self::Page) -> Option<String>;
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Pages kept open when idle (home included).
    pub min: usize,
    /// At most this many pages; 1 serializes requests as before pooling.
    pub max: usize,
    /// A page idle this long closes (down to `min`).
    pub idle: Duration,
    /// How long a request waits for a page when all are busy.
    pub wait: Duration,
    /// A full pool keeps a page for its client this long after its request.
    pub hold: Duration,
    /// Requests allowed to wait at once; more fail at once.
    pub queue: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self { min: 1, max: 4, idle: Duration::from_secs(60), wait: Duration::from_secs(300), hold: Duration::from_secs(10), queue: 64 }
    }
}

impl Config {
    /// `FAB_POOL_MAX`, `FAB_POOL_MIN`, `FAB_POOL_IDLE` (s), `FAB_POOL_WAIT`
    /// (s), `FAB_POOL_HOLD` (s), `FAB_POOL_QUEUE`.
    pub fn from_env() -> Self {
        let d = Self::default();
        let n = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok());
        let max = n("FAB_POOL_MAX").map_or(d.max, |v| v.max(1) as usize);
        Self {
            max,
            min: n("FAB_POOL_MIN").map_or(d.min, |v| (v as usize).clamp(1, max)),
            idle: n("FAB_POOL_IDLE").map_or(d.idle, Duration::from_secs),
            wait: n("FAB_POOL_WAIT").map_or(d.wait, Duration::from_secs),
            hold: n("FAB_POOL_HOLD").map_or(d.hold, Duration::from_secs),
            queue: n("FAB_POOL_QUEUE").map_or(d.queue, |v| v as usize),
        }
    }
}

struct Slot<P> {
    id: u64,
    /// None while leased.
    page: Option<P>,
    home: bool,
    /// Who used it last.
    client: Option<String>,
    /// When it was last given back.
    used: Instant,
    /// Kept for `client` until then (when the pool is full).
    until: Instant,
}

/// What the pool knows of a client from its last request.
struct Client {
    page: u64,
    place: Option<String>,
    at: Instant,
    /// Its usual gap between requests (moving average).
    gap: Option<Duration>,
}

#[derive(Default, Debug, Clone, serde::Serialize)]
pub struct Stats {
    pub opened: u64,
    pub closed: u64,
    /// Pages found dead before a lease, renewed or replaced.
    pub replaced: u64,
    /// Leases dropped without a release (their page discarded).
    pub discarded: u64,
    pub peak: usize,
    /// Requests refused (queue full or waited too long).
    pub rejected: u64,
}

struct State<P> {
    slots: Vec<Slot<P>>,
    opening: usize,
    /// Tickets of waiting requests, oldest first.
    line: VecDeque<u64>,
    ticket: u64,
    next: u64,
    closed: bool,
    /// Shut down: pages coming back are closed.
    done: bool,
    /// Each client's page and address after its last request.
    clients: HashMap<String, Client>,
    stats: Stats,
}

pub struct Pool<M: Manager> {
    mgr: M,
    cfg: Config,
    st: Mutex<State<M::Page>>,
    changed: Notify,
}

enum Pick<P> {
    Page(u64, P, bool),
    Open(u64),
    /// Nothing yet; a held page frees up at this time, if any.
    Wait(Option<Instant>),
}

impl<M: Manager> Pool<M> {
    /// A pool around `home`, the page the browser opened with.
    pub fn new(mgr: M, mut cfg: Config, home: M::Page) -> Arc<Self> {
        cfg.max = cfg.max.max(1);
        cfg.min = cfg.min.clamp(1, cfg.max);
        let now = Instant::now();
        let slot = Slot { id: 0, page: Some(home), home: true, client: None, used: now, until: now };
        let stats = Stats { peak: 1, ..Default::default() };
        let st = State { slots: vec![slot], opening: 0, line: VecDeque::new(), ticket: 0, next: 1, closed: false, done: false, clients: HashMap::new(), stats };
        Arc::new(Self { mgr, cfg, st: Mutex::new(st), changed: Notify::new() })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Pages, busy pages, waiting requests and counters.
    pub fn stats(&self) -> Value {
        let st = self.st.lock().unwrap();
        let busy = st.slots.iter().filter(|s| s.page.is_none()).count();
        let mut v = json!(st.stats);
        v["pages"] = json!(st.slots.len());
        v["busy"] = json!(busy);
        v["opening"] = json!(st.opening);
        v["waiting"] = json!(st.line.len());
        v["max"] = json!(self.cfg.max);
        v
    }

    /// What a request can have now, if anything. `ticket`: its place in line.
    fn choose(&self, st: &mut State<M::Page>, client: &str, ticket: Option<u64>) -> Pick<M::Page> {
        let now = Instant::now();
        let free = |s: &&Slot<M::Page>| s.page.is_some();
        let held = |s: &Slot<M::Page>| s.client.is_some() && now < s.until;
        let own = st.slots.iter().filter(free).filter(|s| s.client.as_deref() == Some(client)).max_by_key(|s| s.used).map(|s| s.id);
        let id = match own {
            Some(id) => id,
            None => {
                let unheld = st.slots.iter().filter(free).filter(|s| !held(s)).count();
                let room = self.cfg.max.saturating_sub(st.slots.len() + st.opening);
                // FIFO: the first `unheld + room` in line may go; newcomers after them.
                let pos = match ticket {
                    Some(t) => st.line.iter().position(|x| *x == t).unwrap_or(0),
                    None => st.line.len(),
                };
                let next_free = || st.slots.iter().filter(free).filter(|s| held(s)).map(|s| s.until).min();
                if pos >= unheld + room {
                    return Pick::Wait(next_free());
                }
                let fresh = st.slots.iter().filter(free).find(|s| s.client.is_none()).map(|s| s.id);
                let lru = st.slots.iter().filter(free).filter(|s| !held(s)).min_by_key(|s| s.used).map(|s| s.id);
                match fresh {
                    Some(id) => id,
                    None if room > 0 => {
                        st.opening += 1;
                        let id = st.next;
                        st.next += 1;
                        return Pick::Open(id);
                    }
                    None => match lru {
                        Some(id) => id,
                        None => return Pick::Wait(next_free()),
                    },
                }
            }
        };
        let s = st.slots.iter_mut().find(|s| s.id == id).unwrap();
        Pick::Page(id, s.page.take().unwrap(), s.home)
    }

    /// Leases a page for `client`, waiting in line when all are busy.
    pub async fn lease(self: &Arc<Self>, client: &str) -> Result<Lease<M>> {
        let deadline = tokio::time::Instant::now() + self.cfg.wait;
        let mut line: Option<InLine<M>> = None;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let pick = {
                let mut st = self.st.lock().unwrap();
                if st.closed {
                    bail!("the session is closing");
                }
                let pick = self.choose(&mut st, client, line.as_ref().map(|l| l.ticket));
                if !matches!(pick, Pick::Wait(_)) {
                    if let Some(l) = line.take() {
                        st.line.retain(|t| *t != l.ticket);
                        std::mem::forget(l);
                    }
                } else if line.is_none() {
                    if st.line.len() >= self.cfg.queue {
                        st.stats.rejected += 1;
                        bail!("all {} pages are busy and {} requests are waiting: try again later (or raise FAB_POOL_MAX)", st.slots.len(), st.line.len());
                    }
                    st.ticket += 1;
                    let t = st.ticket;
                    st.line.push_back(t);
                    line = Some(InLine { pool: self.clone(), ticket: t });
                }
                pick
            };
            match pick {
                Pick::Page(id, page, home) => {
                    let mut lease = self.lease_of(id, page, home, client);
                    match self.mgr.ready(lease.page.as_mut().unwrap()).await {
                        Ok(repaired) => {
                            if repaired {
                                self.st.lock().unwrap().stats.replaced += 1;
                            }
                            return Ok(lease);
                        }
                        Err(e) => {
                            self.st.lock().unwrap().stats.replaced += 1;
                            // Home stays (re-checked next time); another is discarded.
                            if home {
                                return Err(e.context("the session's page is not responding"));
                            }
                            tracing::warn!("pool: page {id} is dead ({e:#}); replacing it");
                            drop(lease);
                        }
                    }
                }
                Pick::Open(id) => {
                    let opening = Opening { pool: self.clone() };
                    let page = self.mgr.open().await?;
                    // Counted as opening until it is in, never twice or neither.
                    std::mem::forget(opening);
                    let mut st = self.st.lock().unwrap();
                    st.opening -= 1;
                    let now = Instant::now();
                    st.slots.push(Slot { id, page: None, home: false, client: Some(client.to_string()), used: now, until: now });
                    st.stats.opened += 1;
                    st.stats.peak = st.stats.peak.max(st.slots.len());
                    drop(st);
                    let mut lease = self.lease_of(id, page, false, client);
                    lease.fork = self.place_of(client, id);
                    return Ok(lease);
                }
                Pick::Wait(free_at) => {
                    let until = free_at.map_or(deadline, |t| deadline.min(tokio::time::Instant::from_std(t)));
                    if tokio::time::timeout_at(until, changed).await.is_err() && until == deadline {
                        let mut st = self.st.lock().unwrap();
                        st.stats.rejected += 1;
                        let busy = st.slots.len();
                        bail!("all {busy} pages stayed busy for {} s: try again later (or raise FAB_POOL_MAX)", self.cfg.wait.as_secs());
                    }
                }
            }
        }
    }

    /// Where `client` was, when that was on another page than `id`.
    fn place_of(&self, client: &str, id: u64) -> Option<String> {
        let st = self.st.lock().unwrap();
        st.clients.get(client).filter(|c| c.page != id).and_then(|c| c.place.clone())
    }

    fn lease_of(self: &Arc<Self>, id: u64, page: M::Page, home: bool, client: &str) -> Lease<M> {
        let fork = {
            let mut st = self.st.lock().unwrap();
            let was = st.slots.iter().find(|s| s.id == id).and_then(|s| s.client.clone());
            let c = st.clients.get_mut(client);
            // The page last served someone else: carry the client over.
            let fork = c.as_ref().filter(|c| c.page != id && was.as_deref() != Some(client)).and_then(|c| c.place.clone());
            if let Some(c) = c {
                let g = c.at.elapsed();
                c.gap = Some(c.gap.map_or(g, |p| (p + g) / 2));
            }
            fork
        };
        Lease { pool: self.clone(), id, page: Some(page), home, ok: false, client: client.to_string(), fork }
    }

    fn give_back(self: &Arc<Self>, id: u64, page: M::Page, ok: bool, home: bool, client: &str) {
        let mut st = self.st.lock().unwrap();
        let i = st.slots.iter().position(|s| s.id == id);
        if st.done || i.is_none() || (!ok && !home) {
            if let Some(i) = i {
                st.slots.remove(i);
            }
            if !ok {
                st.stats.discarded += 1;
            }
            st.stats.closed += 1;
            drop(st);
            self.changed.notify_waiters();
            if !home {
                let pool = self.clone();
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    rt.spawn(async move { pool.mgr.close(page).await });
                }
            }
            return;
        }
        if ok {
            let place = self.mgr.place(&page);
            let gap = st.clients.get(client).and_then(|c| c.gap);
            st.clients.insert(client.to_string(), Client { page: id, place, at: Instant::now(), gap });
            if st.clients.len() > 256
                && let Some(old) = st.clients.iter().min_by_key(|(_, c)| c.at).map(|(k, _)| k.clone())
            {
                st.clients.remove(&old);
            }
        } else {
            st.stats.discarded += 1;
        }
        let hold = match st.clients.get(client).and_then(|c| c.gap) {
            Some(g) if ok => (g * 3).clamp(Duration::from_secs(1).min(self.cfg.hold), self.cfg.hold),
            _ => self.cfg.hold,
        };
        let s = &mut st.slots[i.unwrap()];
        s.page = Some(page);
        s.client = Some(client.to_string());
        s.used = Instant::now();
        s.until = s.used + hold;
        drop(st);
        self.changed.notify_waiters();
    }

    /// Closes pages idle longer than the idle interval, down to `min`.
    pub async fn reap(&self) -> usize {
        let pages: Vec<M::Page> = {
            let mut st = self.st.lock().unwrap();
            if st.closed {
                return 0;
            }
            let now = Instant::now();
            let mut idle: Vec<(Instant, u64)> = st.slots.iter().filter(|s| !s.home && s.page.is_some() && s.until <= now && s.used.elapsed() >= self.cfg.idle).map(|s| (s.used, s.id)).collect();
            idle.sort();
            let extra = st.slots.len().saturating_sub(self.cfg.min);
            let ids: Vec<u64> = idle.into_iter().take(extra).map(|(_, id)| id).collect();
            let mut out = vec![];
            st.slots.retain_mut(|s| {
                if ids.contains(&s.id) {
                    out.push(s.page.take().unwrap());
                    false
                } else {
                    true
                }
            });
            st.stats.closed += out.len() as u64;
            out
        };
        let n = pages.len();
        for p in pages {
            self.mgr.close(p).await;
        }
        n
    }

    /// Opens pages up to `min` (after the browser starts).
    pub async fn warm(&self) {
        loop {
            let id = {
                let mut st = self.st.lock().unwrap();
                if st.closed || st.slots.len() + st.opening >= self.cfg.min {
                    return;
                }
                st.opening += 1;
                st.next += 1;
                st.next - 1
            };
            let r = self.mgr.open().await;
            let mut st = self.st.lock().unwrap();
            st.opening -= 1;
            match r {
                Ok(p) => {
                    let now = Instant::now();
                    st.slots.push(Slot { id, page: Some(p), home: false, client: None, used: now, until: now });
                    st.stats.opened += 1;
                    st.stats.peak = st.stats.peak.max(st.slots.len());
                    drop(st);
                    self.changed.notify_waiters();
                }
                Err(e) => {
                    tracing::warn!("pool: could not open a page: {e:#}");
                    return;
                }
            }
        }
    }

    /// Stops leasing, waits up to `grace` for leased pages to come back,
    /// closes every page and then the browser.
    pub async fn shutdown(&self, grace: Duration) {
        self.st.lock().unwrap().closed = true;
        self.changed.notify_waiters();
        let until = tokio::time::Instant::now() + grace;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.st.lock().unwrap().slots.iter().all(|s| s.page.is_some()) {
                break;
            }
            if tokio::time::timeout_at(until, changed).await.is_err() {
                break;
            }
        }
        let (home, rest) = {
            let mut st = self.st.lock().unwrap();
            st.done = true;
            let mut home = None;
            let mut rest = vec![];
            for mut s in st.slots.drain(..) {
                match (s.home, s.page.take()) {
                    (true, p) => home = p,
                    (false, Some(p)) => rest.push(p),
                    (false, None) => {}
                }
            }
            (home, rest)
        };
        for p in rest {
            self.mgr.close(p).await;
        }
        self.mgr.shutdown(home).await;
    }
}

/// A place in line; leaving it (cancelled, timed out) lets the next one go.
struct InLine<M: Manager> {
    pool: Arc<Pool<M>>,
    ticket: u64,
}

impl<M: Manager> Drop for InLine<M> {
    fn drop(&mut self) {
        self.pool.st.lock().unwrap().line.retain(|t| *t != self.ticket);
        self.pool.changed.notify_waiters();
    }
}

/// A page being opened; counts toward `max` until it is in the pool (or,
/// dropped, when opening failed or was cancelled).
struct Opening<M: Manager> {
    pool: Arc<Pool<M>>,
}

impl<M: Manager> Drop for Opening<M> {
    fn drop(&mut self) {
        self.pool.st.lock().unwrap().opening -= 1;
        self.pool.changed.notify_waiters();
    }
}

/// One page, held for one request. Give it back with [`Lease::release`];
/// dropped otherwise, its page is discarded (home: kept and re-checked).
pub struct Lease<M: Manager> {
    pool: Arc<Pool<M>>,
    id: u64,
    page: Option<M::Page>,
    ok: bool,
    client: String,
    /// The page is the browser's first tab.
    pub home: bool,
    /// The client's address on its previous page, when this lease is on
    /// another one: open it to continue there.
    pub fork: Option<String>,
}

impl<M: Manager> Lease<M> {
    /// Gives the page back in good order.
    pub fn release(mut self) {
        self.ok = true;
    }

    pub fn id(&self) -> u64 {
        self.id
    }
}

impl<M: Manager> Deref for Lease<M> {
    type Target = M::Page;
    fn deref(&self) -> &M::Page {
        self.page.as_ref().unwrap()
    }
}

impl<M: Manager> DerefMut for Lease<M> {
    fn deref_mut(&mut self) -> &mut M::Page {
        self.page.as_mut().unwrap()
    }
}

impl<M: Manager> Drop for Lease<M> {
    fn drop(&mut self) {
        if let Some(p) = self.page.take() {
            self.pool.give_back(self.id, p, self.ok, self.home, &self.client);
        }
    }
}

/// Pages as `fab_core::Session`s on tabs of one browser.
pub struct Tabs {
    pub pages: fab_core::Pages,
}

impl Manager for Tabs {
    type Page = fab_core::Session;

    async fn open(&self) -> Result<fab_core::Session> {
        self.pages.open().await
    }

    async fn ready(&self, s: &mut fab_core::Session) -> Result<bool> {
        let within = Duration::from_secs(3);
        if s.browser.healthy(within).await {
            return Ok(false);
        }
        tracing::warn!("pool: a page stopped answering; moving it to a fresh tab");
        s.renew().await?;
        if !s.browser.healthy(within).await {
            bail!("a fresh tab did not answer either");
        }
        Ok(true)
    }

    async fn close(&self, mut s: fab_core::Session) {
        s.flush_shapes();
        s.close().await;
    }

    async fn shutdown(&self, home: Option<fab_core::Session>) {
        match home {
            Some(mut h) => {
                h.flush_shapes();
                h.close().await;
            }
            None => self.pages.shutdown().await,
        }
    }

    fn place(&self, s: &fab_core::Session) -> Option<String> {
        s.last_snapshot().map(|s| s.url.clone()).filter(|u| u.starts_with("http") || u.starts_with("file:"))
    }
}

pub type Browser = Pool<Tabs>;

/// A pool around a started session: at most one page when the backend has
/// only one.
pub fn around(home: fab_core::Session, mut cfg: Config) -> Arc<Browser> {
    let pages = home.pages();
    if !pages.multi() {
        cfg.max = 1;
        cfg.min = 1;
    }
    Pool::new(Tabs { pages }, cfg, home)
}

/// What one pooled call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition { Completed, Interrupted }

pub struct Done {
    pub disposition: Disposition,
    pub reply: crate::api::Reply,
    /// Ran on the home page: its address and title.
    pub home: Option<(String, String)>,
    /// The engine committed an action that repeated work would duplicate
    /// (a submit, a purchase, a typed-and-sent form). Read-only work and a
    /// navigation are not commits: repeating those changes nothing.
    pub acted: bool,
}

/// Runs one tool call on a page leased for `client`, within `limit`.
pub async fn call(pool: &Arc<Browser>, ctx: &crate::api::Ctx, client: &str, tool: &str, args: &Value, live: Option<Arc<dyn Fn(String) + Send + Sync>>, limit: Duration) -> Done {
    if let Err(failure) = crate::task_runtime::validate(tool, args) {
        return Done { disposition: Disposition::Completed, reply: crate::api::Reply::failure(failure), home: None, acted: false };
    }
    let mut lease = match pool.lease(client).await {
        Ok(l) => l,
        Err(e) => return Done { disposition: Disposition::Completed, reply: crate::api::Reply::fail(crate::events::ErrorCode::Internal, format!("error: {e:#}")), home: None, acted: false },
    };
    let mut ctx = ctx.clone();
    // Continuing the client's work on another page: open where it was, unless
    // the request opens a page itself.
    if let Some(u) = lease.fork.clone() {
        let opens = args["url"].as_str().is_some_and(|u| !u.trim().is_empty()) || args["step"].as_str().and_then(crate::api::leading_url).is_some();
        if !opens && ctx.program_journal.is_none() && matches!(tool, "do" | "run" | "step") {
            let _ = lease.goto(&u).await;
        }
    }
    // What the call started with, so a caller can tell a call that acted from
    // one that failed before it did.
    let commits0 = lease.commits;
    let r = tokio::time::timeout(limit, crate::api::call(&mut lease, &mut ctx, tool, args, live)).await;
    let acted = lease.commits != commits0;
    let reply = match r {
        Ok(r) => r,
        Err(_) => {
            // Stopped midway: nothing of the request stays on the page (home
            // is kept, a page otherwise discarded as the lease drops).
            lease.live = None;
            lease.gate = None;
            lease.invalidate();
            return Done { disposition: Disposition::Interrupted, reply: crate::api::Reply::fail(crate::events::ErrorCode::Interrupted, format!("error: the request ran over {} s and was stopped", limit.as_secs())), home: None, acted: true };
        }
    };
    lease.flush_shapes();
    let home = lease.home.then(|| lease.last_snapshot().map(|s| (s.url.clone(), s.title.clone()))).flatten();
    lease.release();
    Done { disposition: Disposition::Completed, reply, home, acted }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// A fake page: `dead` pages fail `ready`, then renew (or not).
    struct Fake {
        id: u64,
        dead: Arc<AtomicBool>,
        url: Option<String>,
    }

    #[derive(Default)]
    struct Fakes {
        opened: AtomicU64,
        closed: Arc<Mutex<Vec<u64>>>,
        /// Dead pages can't be repaired in place.
        broken: AtomicBool,
        fail_open: AtomicBool,
        open_ms: u64,
        shut: AtomicBool,
    }

    impl Manager for Arc<Fakes> {
        type Page = Fake;
        async fn open(&self) -> Result<Fake> {
            tokio::time::sleep(Duration::from_millis(self.open_ms)).await;
            if self.fail_open.load(Ordering::SeqCst) {
                bail!("no")
            }
            Ok(Fake { id: self.opened.fetch_add(1, Ordering::SeqCst) + 1, dead: Default::default(), url: None })
        }
        async fn ready(&self, p: &mut Fake) -> Result<bool> {
            if p.dead.load(Ordering::SeqCst) {
                if self.broken.load(Ordering::SeqCst) {
                    bail!("dead");
                }
                p.dead.store(false, Ordering::SeqCst);
                return Ok(true);
            }
            Ok(false)
        }
        async fn close(&self, p: Fake) {
            self.closed.lock().unwrap().push(p.id);
        }
        async fn shutdown(&self, _home: Option<Fake>) {
            self.shut.store(true, Ordering::SeqCst);
        }
        fn place(&self, p: &Fake) -> Option<String> {
            p.url.clone()
        }
    }

    fn pool(cfg: Config) -> (Arc<Pool<Arc<Fakes>>>, Arc<Fakes>) {
        let m = Arc::new(Fakes::default());
        let home = Fake { id: 0, dead: Default::default(), url: None };
        (Pool::new(m.clone(), cfg, home), m)
    }

    fn cfg(max: usize) -> Config {
        Config { min: 1, max, idle: Duration::from_millis(50), wait: Duration::from_millis(300), hold: Duration::ZERO, queue: 8 }
    }

    fn n(p: &Pool<Arc<Fakes>>, k: &str) -> u64 {
        p.stats()[k].as_u64().unwrap()
    }

    #[tokio::test]
    async fn sequential_requests_reuse_the_home_page() {
        let (p, m) = pool(cfg(4));
        for _ in 0..3 {
            let l = p.lease("").await.unwrap();
            assert!(l.home);
            assert_eq!(l.id, 0);
            l.release();
        }
        assert_eq!(m.opened.load(Ordering::SeqCst), 0);
        assert_eq!(n(&p, "pages"), 1);
    }

    #[tokio::test]
    async fn concurrent_requests_scale_up_to_max_then_wait() {
        let (p, m) = pool(cfg(3));
        let a = p.lease("").await.unwrap();
        let b = p.lease("").await.unwrap();
        let c = p.lease("x").await.unwrap();
        assert_eq!(m.opened.load(Ordering::SeqCst), 2);
        assert_eq!(n(&p, "pages"), 3);
        assert_eq!(n(&p, "busy"), 3);
        // Saturated: the fourth waits and gets the first page given back.
        let p2 = p.clone();
        let d = tokio::spawn(async move { p2.lease("y").await.map(|l| l.id) });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(n(&p, "waiting"), 1);
        let bid = b.id;
        b.release();
        assert_eq!(d.await.unwrap().unwrap(), bid);
        drop((a, c));
        assert_eq!(n(&p, "peak"), 3);
    }

    #[tokio::test]
    async fn many_concurrent_requests_never_exceed_max() {
        let m = Arc::new(Fakes { open_ms: 3, ..Default::default() });
        let p = Pool::new(m.clone(), Config { wait: Duration::from_secs(10), queue: 100, ..cfg(5) }, Fake { id: 0, dead: Default::default(), url: None });
        let busy = Arc::new(AtomicU64::new(0));
        let most = Arc::new(AtomicU64::new(0));
        let hs: Vec<_> = (0..60)
            .map(|i| {
                let (p, busy, most) = (p.clone(), busy.clone(), most.clone());
                tokio::spawn(async move {
                    let l = p.lease(&(i % 7).to_string()).await.unwrap();
                    let b = busy.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(b, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    busy.fetch_sub(1, Ordering::SeqCst);
                    if i % 5 == 0 { drop(l) } else { l.release() }
                })
            })
            .collect();
        for h in hs {
            h.await.unwrap();
        }
        assert!(most.load(Ordering::SeqCst) <= 5);
        assert!(n(&p, "peak") <= 5, "{}", p.stats());
        assert_eq!(n(&p, "busy"), 0);
        assert_eq!(n(&p, "waiting"), 0);
        assert_eq!(n(&p, "opening"), 0);
    }

    #[tokio::test]
    async fn waiting_times_out_and_a_full_queue_fails_fast() {
        let (p, _) = pool(Config { queue: 1, ..cfg(1) });
        let _a = p.lease("").await.unwrap();
        let p2 = p.clone();
        let w = tokio::spawn(async move { p2.lease("").await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let t0 = Instant::now();
        let e = p.lease("").await.err().unwrap().to_string();
        assert!(e.contains("waiting"), "{e}");
        assert!(t0.elapsed() < Duration::from_millis(50));
        let e = w.await.unwrap().err().unwrap().to_string();
        assert!(e.contains("stayed busy"), "{e}");
        assert_eq!(n(&p, "waiting"), 0);
        assert_eq!(n(&p, "rejected"), 2);
    }

    #[tokio::test]
    async fn waiters_are_served_in_order() {
        let (p, _) = pool(Config { wait: Duration::from_secs(5), ..cfg(1) });
        let a = p.lease("").await.unwrap();
        let order = Arc::new(Mutex::new(vec![]));
        let mut hs = vec![];
        for i in 0..4 {
            let (p, order) = (p.clone(), order.clone());
            hs.push(tokio::spawn(async move {
                let l = p.lease("").await.unwrap();
                order.lock().unwrap().push(i);
                tokio::time::sleep(Duration::from_millis(5)).await;
                l.release();
            }));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        a.release();
        for h in hs {
            h.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn clients_keep_their_page_and_fork_when_it_is_busy() {
        let (p, _) = pool(cfg(4));
        let mut a = p.lease("a").await.unwrap();
        assert_eq!(a.id, 0);
        a.url = Some("http://x/a".into());
        let aid = a.id;
        a.release();
        let mut b = p.lease("b").await.unwrap();
        assert_ne!(b.id, aid, "b gets a new page rather than a's");
        b.url = Some("http://x/b".into());
        let bid = b.id;
        b.release();
        let a1 = p.lease("a").await.unwrap();
        assert_eq!(a1.id, aid);
        assert!(a1.fork.is_none());
        // a asks again while its page is busy: another page, at a's address.
        let a2 = p.lease("a").await.unwrap();
        assert_ne!(a2.id, aid);
        assert_ne!(a2.id, bid, "a fresh page before b's");
        assert_eq!(a2.fork.as_deref(), Some("http://x/a"));
        drop((a1, a2));
    }

    #[tokio::test]
    async fn at_max_an_idle_page_is_reused_before_waiting() {
        let (p, _) = pool(cfg(2));
        let a = p.lease("a").await.unwrap();
        let b = p.lease("b").await.unwrap();
        a.release();
        b.release();
        let c = p.lease("c").await.unwrap();
        assert_eq!(c.id, 0, "least recently used");
    }

    #[tokio::test]
    async fn a_full_pool_holds_pages_for_their_clients() {
        let (p, _) = pool(Config { hold: Duration::from_millis(100), wait: Duration::from_secs(2), ..cfg(2) });
        let a = p.lease("a").await.unwrap();
        let b = p.lease("b").await.unwrap();
        let (aid, bid) = (a.id, b.id);
        a.release();
        b.release();
        // c waits for a hold to lapse rather than take a's page at once...
        let p2 = p.clone();
        let t0 = Instant::now();
        let c = tokio::spawn(async move { p2.lease("c").await.map(|l| (l.id, t0.elapsed())) });
        tokio::time::sleep(Duration::from_millis(30)).await;
        // ...while a and b come back to their own pages without waiting.
        let a2 = p.lease("a").await.unwrap();
        assert_eq!(a2.id, aid);
        let (cid, waited) = c.await.unwrap().unwrap();
        assert_eq!(cid, bid, "b's hold lapsed first");
        assert!(waited >= Duration::from_millis(90), "{waited:?}");
    }

    #[tokio::test]
    async fn a_client_with_quick_steps_is_held_briefly() {
        let (p, _) = pool(Config { hold: Duration::from_secs(10), wait: Duration::from_secs(5), ..cfg(1) });
        // Two quick requests: a's gap is tiny, so its hold is the 1 s floor.
        p.lease("a").await.unwrap().release();
        p.lease("a").await.unwrap().release();
        let t0 = Instant::now();
        let b = p.lease("b").await.unwrap();
        let w = t0.elapsed();
        assert!(w >= Duration::from_millis(900) && w < Duration::from_secs(2), "{w:?}");
        b.release();
    }

    #[tokio::test]
    async fn idle_pages_scale_down_but_home_stays() {
        let (p, m) = pool(cfg(4));
        let mut ls = vec![];
        for i in 0..4 {
            ls.push(p.lease(&i.to_string()).await.unwrap());
        }
        assert_eq!(n(&p, "pages"), 4);
        for l in ls {
            l.release();
        }
        assert_eq!(p.reap().await, 0, "not idle long enough");
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(p.reap().await, 3);
        assert_eq!(n(&p, "pages"), 1);
        assert_eq!(m.closed.lock().unwrap().len(), 3);
        assert!(p.lease("0").await.unwrap().home);
    }

    #[tokio::test]
    async fn min_pages_are_kept_warm() {
        let (p, m) = pool(Config { min: 3, ..cfg(4) });
        p.warm().await;
        assert_eq!(n(&p, "pages"), 3);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(p.reap().await, 0);
        assert_eq!(m.opened.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn dead_pages_are_repaired_or_replaced() {
        let (p, m) = pool(cfg(4));
        let home = p.lease("a").await.unwrap();
        let b = p.lease("b").await.unwrap();
        let (dead_home, dead_b, bid, bpage) = (home.dead.clone(), b.dead.clone(), b.id, b.page.as_ref().unwrap().id);
        b.release();
        // Repaired in place (a fresh tab).
        dead_b.store(true, Ordering::SeqCst);
        let l = p.lease("b").await.unwrap();
        assert_eq!(l.id, bid);
        assert_eq!(n(&p, "replaced"), 1);
        l.release();
        // Beyond repair: replaced by a new page, the dead one closed.
        m.broken.store(true, Ordering::SeqCst);
        dead_b.store(true, Ordering::SeqCst);
        let l = p.lease("b").await.unwrap();
        assert_ne!(l.id, bid);
        assert!(!l.home);
        assert_eq!(n(&p, "replaced"), 2);
        l.release();
        tokio::task::yield_now().await;
        assert_eq!(*m.closed.lock().unwrap(), vec![bpage]);
        assert_eq!(n(&p, "pages"), 2);
        // Home beyond repair: its client's request fails, home stays.
        home.release();
        dead_home.store(true, Ordering::SeqCst);
        assert!(p.lease("a").await.is_err());
        assert_eq!(n(&p, "pages"), 2);
        m.broken.store(false, Ordering::SeqCst);
        assert!(p.lease("a").await.unwrap().home);
    }

    #[tokio::test]
    async fn a_dropped_lease_discards_its_page() {
        let (p, m) = pool(cfg(4));
        let _h = p.lease("").await.unwrap();
        let l = p.lease("").await.unwrap();
        let id = l.id;
        drop(l);
        tokio::task::yield_now().await;
        assert_eq!(n(&p, "discarded"), 1);
        assert_eq!(n(&p, "peak"), 2);
        assert_eq!(n(&p, "pages"), 1);
        assert!(m.closed.lock().unwrap().contains(&(id)));
    }

    #[tokio::test]
    async fn a_dropped_home_lease_keeps_home() {
        let (p, _) = pool(cfg(4));
        drop(p.lease("").await.unwrap());
        assert_eq!(n(&p, "pages"), 1);
        assert!(p.lease("").await.unwrap().home);
    }

    #[tokio::test]
    async fn cancelled_requests_leak_nothing() {
        let (p, _) = pool(Config { wait: Duration::from_secs(5), ..cfg(2) });
        // Cancelled while waiting in line.
        let a = p.lease("").await.unwrap();
        let b = p.lease("").await.unwrap();
        let r = tokio::time::timeout(Duration::from_millis(20), p.lease("")).await;
        assert!(r.is_err());
        assert_eq!(n(&p, "waiting"), 0);
        a.release();
        b.release();
        // Cancelled while a page opens.
        let m2 = Arc::new(Fakes { open_ms: 100, ..Default::default() });
        let p2 = Pool::new(m2, cfg(2), Fake { id: 0, dead: Default::default(), url: None });
        let _h = p2.lease("").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(20), p2.lease("")).await.is_err());
        assert_eq!(n(&p2, "opening"), 0);
        // Its slot is free again: the next request can open one.
        assert!(tokio::time::timeout(Duration::from_millis(300), p2.lease("")).await.unwrap().is_ok());
        // A task that panics holding a lease gives the page back.
        let p3 = p.clone();
        let t = tokio::spawn(async move {
            let _l = p3.lease("").await.unwrap();
            panic!("boom");
        });
        assert!(t.await.is_err());
        assert_eq!(n(&p, "busy"), 0);
    }

    #[tokio::test]
    async fn a_failed_open_frees_its_slot() {
        let (p, m) = pool(cfg(2));
        let _h = p.lease("").await.unwrap();
        m.fail_open.store(true, Ordering::SeqCst);
        assert!(p.lease("").await.is_err());
        assert_eq!(n(&p, "opening"), 0);
        m.fail_open.store(false, Ordering::SeqCst);
        assert!(p.lease("").await.is_ok());
    }

    #[tokio::test]
    async fn shutdown_waits_for_leases_then_closes_everything() {
        let (p, m) = pool(cfg(3));
        let a = p.lease("").await.unwrap();
        let b = p.lease("").await.unwrap();
        b.release();
        let p2 = p.clone();
        let s = tokio::spawn(async move { p2.shutdown(Duration::from_secs(2)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(p.lease("").await.is_err(), "no leases while closing");
        assert!(!m.shut.load(Ordering::SeqCst));
        a.release();
        s.await.unwrap();
        assert!(m.shut.load(Ordering::SeqCst));
        assert_eq!(m.closed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_stuck_lease_does_not_block_shutdown() {
        let (p, m) = pool(cfg(3));
        let _home = p.lease("").await.unwrap();
        let other = p.lease("").await.unwrap();
        p.shutdown(Duration::from_millis(30)).await;
        assert!(m.shut.load(Ordering::SeqCst));
        drop(other);
        tokio::task::yield_now().await;
        assert!(m.closed.lock().unwrap().contains(&1), "a page coming back late is closed");
    }
}

/// Against a real browser: `FAB_LIVE_BROWSER=firefox|chrome cargo test -p
/// fast-agentic-browser --lib pool::live -- --ignored --test-threads 1`.
#[cfg(test)]
mod live {
    use super::*;

    async fn start() -> Arc<Browser> {
        let mut k = fab_core::Knobs::default();
        k.headful = false;
        k.browser = std::env::var("FAB_LIVE_BROWSER").unwrap_or_else(|_| "chrome".into());
        let s = fab_core::Session::new(k).await.unwrap();
        around(s, Config { max: 3, hold: Duration::ZERO, ..Default::default() })
    }

    async fn url(l: &mut Lease<Tabs>) -> String {
        l.browser.eval("location.href").await.unwrap().as_str().unwrap_or_default().to_string()
    }

    #[tokio::test]
    #[ignore]
    async fn pages_are_separate_tabs_and_survive_hangs_and_closes() {
        let p = start().await;
        let mut a = p.lease("a").await.unwrap();
        let mut b = p.lease("b").await.unwrap();
        a.goto("data:text/html,<title>A</title>a").await.unwrap();
        b.goto("data:text/html,<title>B</title>b").await.unwrap();
        assert!(url(&mut a).await.contains("A"));
        assert!(url(&mut b).await.contains("B"));
        let bid = b.id();
        a.release();
        // A hung renderer: b's next lease finds it unresponsive and renews it.
        let _ = tokio::time::timeout(Duration::from_millis(300), b.browser.eval("(()=>{for(;;){}})()")).await;
        b.release();
        let t0 = Instant::now();
        let mut b = p.lease("b").await.unwrap();
        eprintln!("hung page renewed in {:?}", t0.elapsed());
        assert_eq!(b.id(), bid);
        assert_eq!(url(&mut b).await, "about:blank");
        b.goto("data:text/html,<title>B2</title>b").await.unwrap();
        // The tab closed from outside: the page moves to a fresh tab of its own.
        let is_cdp = b.browser.name() == "cdp";
        b.browser.close().await;
        b.release();
        let mut b = p.lease("b").await.unwrap();
        assert_eq!(url(&mut b).await, "about:blank");
        if is_cdp {
            // Chromium's diagnostic URL crashes only this isolated renderer.
            let _ = tokio::time::timeout(Duration::from_millis(500), b.goto("chrome://crash")).await;
            b.release();
            let mut b = p.lease("b").await.unwrap();
            assert_eq!(url(&mut b).await, "about:blank");
            b.release();
        } else { b.release(); }
        // Home was never disturbed.
        let mut a = p.lease("a").await.unwrap();
        assert!(a.home);
        assert!(url(&mut a).await.contains("A"));
        a.release();
        let st = p.stats();
        eprintln!("{st}");
        assert!(st["replaced"].as_u64().unwrap() >= 1);
        p.shutdown(Duration::from_secs(2)).await;
    }
}
