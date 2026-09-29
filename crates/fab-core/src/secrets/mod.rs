//! Secrets from the user's password manager, typed without any model ever
//! seeing them. Agents write `{{plain words}}` where a value goes
//! (`{{password}}`, `{{Visa card number}}`, `{{Stripe test secret key}}`);
//! the words are resolved against the stores' metadata, and the value is
//! fetched only when [`crate::backend::Browser::fill`] types it into a field.
//!
//! Rules:
//! - a login fills only on the sites it is saved for;
//! - any other item (cards, identities, keys) needs the user's approval per
//!   site, asked outside the model (a system dialog, or `fab secrets allow`);
//! - concealed values are masked on the page listing and scrubbed from
//!   everything fab returns ([`redact`]).

pub mod bw;
pub mod helper;
pub mod keychain;
pub mod names;
pub mod op;
pub mod redact;
pub mod store;
pub mod verifier;
pub mod workflows;

pub use names::{has_placeholder, placeholders};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

use names::{CardField, IdField, LoginField, Want};
use store::{Full, Item, Kind, Store};

/// Where a value is about to go (from `__ub.target(i)`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Target {
    /// The page's origin.
    pub origin: String,
    /// The origin of the document the field is in (a frame's).
    #[serde(default)]
    pub frame: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default, rename = "type")]
    pub ty: String,
    /// The field's `autocomplete` attribute.
    #[serde(default)]
    pub ac: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub ph: String,
    #[serde(default)]
    pub editable: bool,
}

impl Target {
    /// A target for lookups that type nothing (`fab secrets test`).
    pub fn at(origin: &str) -> Self {
        Target { origin: origin.to_string(), editable: true, ..Default::default() }
    }

    fn says(&self, s: &str) -> bool {
        self.label.to_lowercase().contains(s) || self.ph.to_lowercase().contains(s)
    }
}

/// A resolved text: what to type, whether it must stay hidden, and where
/// each value came from (for the reply).
pub struct Filled {
    pub value: Zeroizing<String>,
    pub concealed: bool,
    pub sources: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    secrets: SecretsConfig,
}

/// `[secrets]` in `~/.config/fab/config.toml`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct SecretsConfig {
    /// Stores to use, in order: 1password, bitwarden, keychain, or a helper's name.
    /// Default: every one that is installed.
    pub stores: Option<Vec<String>>,
    /// Where `save` puts new logins (default: the first store that can).
    pub save_to: Option<String>,
    pub op_account: Option<String>,
    pub op_vault: Option<String>,
    /// How to ask the user for approval: "dialog" (default) or "never" (refuse; use `fab secrets allow`).
    pub prompt: Option<String>,
}

pub fn config_path() -> std::path::PathBuf {
    crate::paths::config_dir().join("config.toml")
}

fn allow_path() -> std::path::PathBuf {
    crate::paths::config_dir().join("secrets-allow.toml")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AllowFile {
    #[serde(default)]
    allow: Vec<Allow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Allow {
    /// `<store>:<item id>`.
    pub item: String,
    pub title: String,
    /// The site (registrable domain, or host:port for local servers).
    pub site: String,
}

/// What the password manager can say about the login for the site and
/// username a generated password belongs to. Only [`Candidate::Ours`] is
/// evidence; the rest keep the site paused (INTENT I08, I09).
enum Candidate {
    /// The saved login holds the generated value.
    Ours(Zeroizing<String>),
    /// No login is saved for the site and username.
    Absent,
    /// A login is saved, but it holds a different password: not fab's.
    Different { unreadable: Vec<String> },
    /// A login is saved, but these stores did not return its password, so fab
    /// cannot prove what it holds.
    Unreadable(String),
    /// The workflow has nothing to compare the saved password against.
    NoVerifier,
}

/// A save that was never dispatched to any store: nothing can have been
/// written, so there is nothing to reconcile.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct SaveNotSent(pub(crate) String);

/// A generated password and its workflow. `value` is None for a workflow
/// restored after a restart: its material then comes only from the store.
struct GeneratedCredential {
    value: Option<Zeroizing<String>>,
    workflow: crate::secret_machine::Machine,
    /// The account it is saved under, set when the save is first issued.
    username: Option<String>,
    /// Proof that a saved login is the value fab generated, recorded with the
    /// workflow so a restart can compare (INTENT I09).
    verifier: Option<verifier::Verifier>,
    /// Recorded in the vault's [`workflows::Journal`].
    persisted: bool,
}

impl GeneratedCredential {
    fn new() -> Result<Self> {
        use crate::secret_machine::*;
        let mut workflow = Machine::new(Intent::GeneratedLogin { generation: IntentRef::new()?, save: IntentRef::new()?, target: TargetRef::new()? })?;
        let operation = workflow.issue()?.context("generation is not ready")?;
        let value = generate();
        // Made at generation, so it is on record before the save is dispatched.
        let verifier = Some(verifier::Verifier::of(&value).context("no verifier for a generated password")?);
        workflow.complete(Completion::Generate { op: operation.op(), result: GenerateResult::Ready(SecretHandle::new()?) })?;
        Ok(GeneratedCredential { value: Some(value), workflow, username: None, verifier, persisted: false })
    }

    fn restored(r: workflows::Record) -> Result<Self> {
        Ok(GeneratedCredential { value: None, workflow: r.machine()?, username: Some(r.username), verifier: r.verifier, persisted: true })
    }
}

/// `fab secrets reset`: forgets the site's paused (or saved) generated-password
/// workflow in fab's state directory, so the next signup generates anew. A
/// running daemon notices on its next use of the site. The password store is
/// never touched.
pub fn reset_generated(origin: &str) -> Result<Option<workflows::Record>> {
    let journal = workflows::Journal::in_state_dir().context("no state directory (set HOME or FAB_STATE_DIR)")?;
    journal.reset(&site_of(origin))
}

#[derive(Default)]
struct State {
    items: Option<(Instant, Arc<Vec<Item>>)>,
    warnings: Vec<String>,
    /// The login chosen per site (`login` with a username, or the only match).
    chosen: HashMap<String, (usize, String)>,
    /// The card / identity chosen this session, so later fields use the same one.
    chosen_kind: HashMap<&'static str, (usize, String)>,
    /// Generated passwords, per site.
    generated: HashMap<String, GeneratedCredential>,
    fulls: HashMap<(usize, String), Arc<Full>>,
    /// Approved once: (item key, site).
    once: HashSet<(String, String)>,
    /// Sources of the values typed since the last `take_log`.
    log: Vec<String>,
}

pub struct Vault {
    pub stores: Vec<Store>,
    pub cfg: SecretsConfig,
    /// Where generated-password workflows are recorded (None: memory only).
    journal: Option<workflows::Journal>,
    state: tokio::sync::Mutex<State>,
}

static VAULT: OnceLock<Vault> = OnceLock::new();

/// The process-wide vault (stores detected on first use).
pub fn vault() -> &'static Vault {
    VAULT.get_or_init(Vault::detect)
}

/// How long listed metadata is reused.
const LIST_TTL: Duration = Duration::from_secs(300);

impl Vault {
    pub fn detect() -> Self {
        let cfg: SecretsConfig = std::fs::read_to_string(config_path()).ok().and_then(|s| toml::from_str::<FileConfig>(&s).ok()).map(|c| c.secrets).unwrap_or_default();
        let helpers = helper::discover();
        let mut stores = vec![];
        let want: Vec<String> = match std::env::var("FAB_SECRETS").ok().filter(|s| !s.is_empty()) {
            Some(s) if s == "none" => vec![],
            Some(s) => s.split(',').map(|x| x.trim().to_lowercase()).collect(),
            None => cfg.stores.clone().unwrap_or_else(|| {
                let mut d = vec!["1password".to_string(), "bitwarden".into(), "keychain".into()];
                d.extend(helpers.iter().map(|h| h.name.clone()));
                d
            }),
        };
        let mut helpers = helpers;
        for w in want {
            match w.as_str() {
                "1password" | "op" if store::on_path("op") => stores.push(Store::OnePassword(op::Op { account: cfg.op_account.clone(), vault: cfg.op_vault.clone() })),
                "bitwarden" | "bw" if store::on_path("bw") => stores.push(Store::Bitwarden(bw::Bw)),
                "keychain" if cfg!(target_os = "macos") => stores.push(Store::Keychain(keychain::Keychain)),
                name => {
                    if let Some(i) = helpers.iter().position(|h| h.name == name) {
                        stores.push(Store::Helper(helpers.remove(i)));
                    }
                }
            }
        }
        Vault::new(stores, cfg, workflows::Journal::in_state_dir())
    }

    /// A vault recording generated-password workflows in `journal`; the ones
    /// an earlier process left there are restored, without material.
    pub fn new(stores: Vec<Store>, cfg: SecretsConfig, journal: Option<workflows::Journal>) -> Self {
        let mut state = State::default();
        // An unreadable record is skipped here and refused by `sync` on use.
        for r in journal.as_ref().and_then(|j| j.all().ok()).unwrap_or_default() {
            let site = r.site.clone();
            if let Ok(g) = GeneratedCredential::restored(r) {
                state.generated.insert(site, g);
            }
        }
        Vault { stores, cfg, journal, state: tokio::sync::Mutex::new(state) }
    }

    /// Brings the site's workflow in line with the journal: another process
    /// may have reset it (`fab secrets reset`) or recorded a newer one.
    fn sync(&self, st: &mut State, site: &str) -> Result<()> {
        let Some(journal) = &self.journal else { return Ok(()) };
        let recorded = journal.get(site)?;
        let mine = st.generated.get(site).map(|g| (g.workflow.checkpoint().epoch(), g.persisted));
        match (mine, recorded) {
            (Some((epoch, _)), Some(r)) if epoch == r.workflow.epoch() => {}
            (_, Some(r)) => {
                st.generated.insert(site.into(), GeneratedCredential::restored(r)?);
            }
            (Some((_, true)), None) => {
                st.generated.remove(site);
            }
            _ => {}
        }
        Ok(())
    }

    /// Records the site's workflow (once it has an account to be saved under).
    fn record(&self, st: &mut State, site: &str, origin: &str) -> Result<()> {
        let Some(journal) = &self.journal else { return Ok(()) };
        let Some(g) = st.generated.get_mut(site) else { return Ok(()) };
        let Some(username) = g.username.clone() else { return Ok(()) };
        journal.put(workflows::Record { site: site.into(), origin: origin.into(), username, workflow: g.workflow.checkpoint(), verifier: g.verifier.clone() })?;
        g.persisted = true;
        Ok(())
    }

    /// Forgets a workflow whose save was never sent: nothing can need reconciling.
    fn forget(&self, st: &mut State, site: &str) -> Result<()> {
        let Some(journal) = &self.journal else { return Ok(()) };
        let Some(g) = st.generated.get_mut(site) else { return Ok(()) };
        journal.remove(site, g.workflow.checkpoint().epoch())?;
        g.persisted = false;
        Ok(())
    }

    fn store_names(&self) -> String {
        if self.stores.is_empty() {
            return "no password manager (install the 1Password or Bitwarden CLI, or add a fab-secret-<name> helper)".into();
        }
        self.stores.iter().map(Store::name).collect::<Vec<_>>().join(", ")
    }

    /// Every store's items (metadata), listed concurrently and cached.
    pub async fn items(&self) -> Result<Arc<Vec<Item>>> {
        {
            let st = self.state.lock().await;
            if let Some((t, items)) = &st.items {
                if t.elapsed() < LIST_TTL {
                    return Ok(items.clone());
                }
            }
        }
        let lists = futures_util::future::join_all(self.stores.iter().map(|s| s.list())).await;
        let mut all = vec![];
        let mut warnings = vec![];
        for (i, r) in lists.into_iter().enumerate() {
            match r {
                Ok(items) => all.extend(items.into_iter().map(|mut it| {
                    it.store = i;
                    it
                })),
                Err(e) => warnings.push(format!("{}: {e:#}", self.stores[i].name())),
            }
        }
        if all.is_empty() && !warnings.is_empty() {
            bail!("could not read the password manager: {}", warnings.join("; "));
        }
        let all = Arc::new(all);
        let mut st = self.state.lock().await;
        st.items = Some((Instant::now(), all.clone()));
        st.warnings = warnings;
        Ok(all)
    }

    /// Stores that failed to list at the last refresh.
    pub async fn warnings(&self) -> Vec<String> {
        self.state.lock().await.warnings.clone()
    }

    async fn full(&self, it: &Item) -> Result<Option<Arc<Full>>> {
        let key = (it.store, it.id.clone());
        if let Some(f) = self.state.lock().await.fulls.get(&key) {
            return Ok(Some(f.clone()));
        }
        let Some(f) = self.stores[it.store].full(it).await? else { return Ok(None) };
        let f = Arc::new(f);
        self.state.lock().await.fulls.insert(key, f.clone());
        Ok(Some(f))
    }

    /// Whether a login has a one-time code (TOTP) saved with it.
    pub async fn has_otp(&self, it: &Item) -> bool {
        match self.full(it).await {
            Ok(Some(f)) => f.totp,
            Ok(None) => it.labels.iter().any(|l| l == "one-time-code"),
            Err(_) => false,
        }
    }

    /// The saved logins for the page's site.
    pub async fn logins_for(&self, origin: &str) -> Result<Vec<Item>> {
        let items = self.items().await?;
        Ok(items.iter().filter(|it| it.kind == Kind::Login && it.urls.iter().any(|u| login_matches(u, origin))).cloned().collect())
    }

    /// Picks the login `login` will use on `origin` (by username, or the only one).
    pub async fn choose_login(&self, origin: &str, username: Option<&str>) -> Result<Item> {
        self.choose_login_by(origin, username, &[]).await
    }

    async fn choose_login_by(&self, origin: &str, username: Option<&str>, hints: &[String]) -> Result<Item> {
        let site = site_of(origin);
        let mut cands = self.logins_for(origin).await?;
        if cands.is_empty() {
            bail!("{}", self.no_login(origin).await);
        }
        if let Some(u) = username.map(str::trim).filter(|u| !u.is_empty()) {
            let lu = u.to_lowercase();
            let by_user: Vec<Item> = cands.iter().filter(|it| it.username.as_deref().is_some_and(|x| x.to_lowercase() == lu)).cloned().collect();
            cands = if by_user.is_empty() { cands.into_iter().filter(|it| it.title.to_lowercase().contains(&lu)).collect() } else { by_user };
            if cands.is_empty() {
                bail!("no saved login for {site} with username {u}; saved: {}", list_logins(&self.logins_for(origin).await?));
            }
        } else if let Some((s, id)) = self.state.lock().await.chosen.get(&site).cloned() {
            if let Some(it) = cands.iter().find(|it| it.store == s && it.id == id) {
                return Ok(it.clone());
            }
        }
        if cands.len() > 1 && !hints.is_empty() {
            // "{{work password}}": words that pick one of the site's logins.
            let hit = |it: &Item| {
                let hay = format!("{} {}", it.title, it.username.clone().unwrap_or_default()).to_lowercase();
                hints.iter().filter(|h| hay.contains(h.as_str())).count()
            };
            let best = cands.iter().map(hit).max().unwrap_or(0);
            if best > 0 {
                cands.retain(|it| hit(it) == best);
            }
        }
        if cands.len() > 1 {
            // The same account saved twice (1Password and the Keychain): the first store wins.
            let users: HashSet<String> = cands.iter().map(|it| it.username.clone().unwrap_or_default().to_lowercase()).collect();
            if users.len() > 1 {
                bail!("several saved logins for {site}: {}. Say which with `username`", list_logins(&cands));
            }
        }
        let it = cands.remove(0);
        self.state.lock().await.chosen.insert(site, (it.store, it.id.clone()));
        Ok(it)
    }

    async fn no_login(&self, origin: &str) -> String {
        let site = site_of(origin);
        let word = site.split('.').next().unwrap_or_default().to_string();
        let near: Vec<String> = match self.items().await {
            Ok(items) => items
                .iter()
                .filter(|it| it.kind == Kind::Login && word.len() > 2 && it.title.to_lowercase().contains(&word))
                .take(3)
                .map(|it| format!("\"{}\" ({})", it.title, it.urls.first().map(String::as_str).unwrap_or("no site")))
                .collect(),
            Err(e) => return format!("{e:#}"),
        };
        let mut m = format!("no saved login for {site} in {}. If the value is saved under another name, put that name in: {{{{<item name> password}}}}", self.store_names());
        if !near.is_empty() {
            m.push_str(&format!("; saved for other sites: {} (fab fills a login only on the site it is saved for)", near.join(", ")));
        }
        m
    }

    /// Resolves every `{{…}}` in `text` for typing into `t`.
    pub async fn substitute(&self, text: &str, t: &Target) -> Result<Filled> {
        if !t.editable {
            bail!("{{{{…}}}} values can only be typed into a text field");
        }
        if !t.frame.is_empty() && site_of(&t.frame) != site_of(&t.origin) {
            bail!("refusing to type a secret into a frame from another site ({})", t.frame);
        }
        let mut out = Zeroizing::new(String::new());
        let mut concealed = false;
        let mut sources = vec![];
        let mut last = 0;
        for (range, name) in placeholders(text) {
            out.push_str(&text[last..range.start]);
            let (v, c, src) = self.resolve(name, t).await?;
            out.push_str(&v);
            concealed |= c;
            sources.push(format!("{{{{{name}}}}} ← {src}"));
            last = range.end;
        }
        out.push_str(&text[last..]);
        if concealed {
            redact::add(&out);
        }
        self.state.lock().await.log.extend(sources.iter().cloned());
        Ok(Filled { value: out, concealed, sources })
    }

    /// Where the values typed since the last call came from.
    pub async fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().await.log)
    }

    /// One placeholder → (value, concealed, source).
    async fn resolve(&self, raw: &str, t: &Target) -> Result<(Zeroizing<String>, bool, String)> {
        let name = names::parse(raw);
        let site = site_of(&t.origin);
        if site.is_empty() {
            bail!("no site to fill {{{{{raw}}}}} on (open a page first)");
        }
        let (v, concealed, src) = match name.want {
            // Typed only once saved: resolving never generates (see `save_generated_login`).
            Want::NewPassword => (
                self.generated_for(&t.origin).await.context("the generated password must be saved before typing; request signup with a quoted username first")?,
                true,
                "a new generated password".to_string(),
            ),
            Want::Login(f) => self.login_value(raw, f, &name.hints, t).await?,
            Want::Email => {
                let login = self.logins_for(&t.origin).await.ok().filter(|c| !c.is_empty());
                match login {
                    Some(_) if self.login_username_is_email(t).await => self.login_value(raw, LoginField::Username, &name.hints, t).await?,
                    _ => self.kind_value(raw, name.want, &name.hints, t).await?,
                }
            }
            Want::Card(_) | Want::Identity(_) => self.kind_value(raw, name.want, &name.hints, t).await?,
            Want::Custom => self.custom_value(raw, &name.words, t).await?,
        };
        if concealed {
            redact::add(&v);
        }
        Ok((v, concealed, src))
    }

    async fn login_username_is_email(&self, t: &Target) -> bool {
        match self.choose_login(&t.origin, None).await {
            Ok(it) => it.username.as_deref().is_some_and(|u| u.contains('@')),
            Err(_) => false,
        }
    }

    /// Only a confirmed save makes generated material available for typing.
    pub async fn generated_for(&self, origin: &str) -> Option<Zeroizing<String>> {
        let site = site_of(origin);
        let mut st = self.state.lock().await;
        self.sync(&mut st, &site).ok()?;
        st.generated.get(&site).filter(|g| g.workflow.has_saved_value()).and_then(|g| g.value.clone())
    }

    pub async fn substitute_for_input(&self, text: &str, target: &Target) -> Result<Filled> {
        if placeholders(text).iter().any(|(_, name)| names::parse(name).want == Want::NewPassword) {
            anyhow::ensure!(self.generated_for(&target.origin).await.is_some(), "the generated password must be saved before typing; request signup with a quoted username first");
        }
        self.substitute(text, target).await
    }

    pub async fn save_generated_login(&self, origin: &str, username: &str) -> Result<String> {
        use crate::secret_machine::*;
        let site = site_of(origin);
        let reset = format!("check the password manager, then start over with `fab secrets reset --url {origin}`");
        let (operation, password, username, verifier) = {
            let mut st = self.state.lock().await;
            self.sync(&mut st, &site)?;
            if !st.generated.contains_key(&site) {
                st.generated.insert(site.clone(), GeneratedCredential::new()?);
            }
            let g = st.generated.get_mut(&site).unwrap();
            if g.workflow.in_flight() {
                bail!("a save for {site} is already in progress; wait for it to finish (nothing was regenerated or typed)");
            }
            if let Some(u) = g.username.as_deref().filter(|u| !u.eq_ignore_ascii_case(username)) {
                bail!("the password generated for {site} is for {u}; to use another account, {reset}");
            }
            if g.workflow.has_saved_value() && g.value.is_some() {
                return Ok("the login saved earlier for this site".into());
            }
            let username = g.username.clone().unwrap_or_else(|| username.to_string());
            // Saved before a restart: nothing to issue, the store has the material.
            let operation = match g.workflow.has_saved_value() {
                true => None,
                false => Some(g.workflow.issue()?.with_context(|| format!("the generated password save for {site} is unconfirmed; nothing was regenerated or typed: {reset}"))?),
            };
            if let Some(Request::Save { op, .. }) = &operation {
                let op = *op;
                g.username = Some(username.clone());
                // Recorded before it is sent, so a restart reconciles instead of losing it.
                let recorded = match g.value.is_some() {
                    true => self.record(&mut st, &site, origin),
                    false => Err(anyhow::anyhow!("the generated password is no longer available")),
                };
                if let Err(e) = recorded {
                    st.generated.get_mut(&site).unwrap().workflow.complete(Completion::Save { op, result: SaveResult::NotSent })?;
                    return Err(e.context("nothing was saved"));
                }
            }
            let password = st.generated.get(&site).and_then(|g| g.value.clone());
            let verifier = st.generated.get(&site).and_then(|g| g.verifier.clone());
            (operation, password, username, verifier)
        };
        if let Some(p) = &password {
            redact::add(p);
        }
        match operation {
            None => {
                let value = self
                    .saved_password(origin, &username)
                    .await
                    .with_context(|| format!("the login saved for {username} on {site} is no longer in the password manager; {reset}"))?;
                redact::add(&value);
                let mut st = self.state.lock().await;
                let g = st.generated.get_mut(&site).context("the generated password workflow was reset")?;
                g.value = Some(value);
                Ok("the login saved earlier for this site".into())
            }
            Some(Request::Save { op, .. }) => {
                let password = password.context("the generated password is no longer available")?;
                let result = self.save_login(origin, &username, &password).await;
                let completion = match &result {
                    Ok(_) => SaveResult::Saved(SavedRef::new()?),
                    Err(e) if e.downcast_ref::<SaveNotSent>().is_some() => SaveResult::NotSent,
                    Err(_) => SaveResult::LostResponse,
                };
                let sent = completion != SaveResult::NotSent;
                let mut st = self.state.lock().await;
                st.generated.get_mut(&site).context("the generated password workflow was reset")?.workflow.complete(Completion::Save { op, result: completion })?;
                // A failed update leaves the dispatched record: a restart reconciles.
                let kept = if sent { self.record(&mut st, &site, origin) } else { self.forget(&mut st, &site) };
                if let Err(e) = kept {
                    tracing::warn!("generated-password workflow for {site} not updated: {e:#}");
                }
                result
            }
            Some(Request::ReconcileSave { op, .. }) => {
                // Any failure to read the store leaves the save unconfirmed; an
                // early `?` here would strand the workflow awaiting this reply.
                let found = self.saved_candidate(origin, &username, password.as_ref(), verifier.as_ref()).await;
                // What the pause says, so the user can tell "not saved" from
                // "not provable" (which is what a store that only lists logins
                // gives).
                let (confirmed, why) = match found {
                    Candidate::Ours(v) => (Some(v), String::new()),
                    Candidate::Absent => (None, format!("no login for {username} is saved on {site} in {}", self.store_names())),
                    Candidate::Different { unreadable } => {
                        let also = match unreadable.is_empty() {
                            true => String::new(),
                            false => format!("; {} did not return its password to compare", unreadable.join(", ")),
                        };
                        (
                            None,
                            format!("the login saved for {username} on {site} holds a different password, so it is not the one fab generated{also}"),
                        )
                    }
                    Candidate::Unreadable(who) => {
                        (None, format!("{who} has a login for {username} on {site} but did not return its password, so the save cannot be proved"))
                    }
                    Candidate::NoVerifier => {
                        (None, format!("the recorded workflow for {site} has no verifier, so its password cannot be compared with the one fab generated"))
                    }
                };
                let result = if confirmed.is_some() { ReconcileResult::Found(SavedRef::new()?) } else { ReconcileResult::Unavailable };
                let mut st = self.state.lock().await;
                let g = st.generated.get_mut(&site).context("the generated password workflow was reset")?;
                g.workflow.complete(Completion::ReconcileSave { op, result })?;
                let ok = confirmed.is_some();
                if let Some(v) = confirmed {
                    redact::add(&v);
                    g.value = Some(v);
                }
                if let Err(e) = self.record(&mut st, &site, origin) {
                    tracing::warn!("generated-password workflow for {site} not updated: {e:#}");
                }
                anyhow::ensure!(ok, "the password save is unconfirmed ({why}); no password was regenerated or typed: {reset}");
                Ok("the reconciled password-manager login".into())
            }
            _ => anyhow::bail!("generated password workflow is not ready to save"),
        }
    }

    /// The password saved for exactly `username` on `origin`, read fresh from
    /// the stores: the listing and full-item caches are dropped first. Used
    /// for a save the store already confirmed, where the account's login is
    /// the one fab saved.
    async fn saved_password(&self, origin: &str, username: &str) -> Option<Zeroizing<String>> {
        {
            let mut st = self.state.lock().await;
            st.items = None;
            st.fulls.clear();
        }
        let item = self.choose_login(origin, Some(username)).await.ok()?;
        if !item.username.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(username)) {
            return None;
        }
        self.stored_password(&item).await
    }

    /// The password of one saved login, if its store will hand it over.
    async fn stored_password(&self, it: &Item) -> Option<Zeroizing<String>> {
        let full = self.full(it).await.ok()?;
        self.item_field(it, &full, "password").await.filter(|v| !v.is_empty()).map(Zeroizing::new)
    }

    /// Reconciliation is evidence, not assumption (INTENT I09): among the
    /// logins saved for `origin` under exactly `username` — read fresh, caches
    /// dropped — only one whose password is proved to be the generated value
    /// counts. A login that does not match is never typed, and a store that
    /// will not return a password proves nothing, so both leave the site
    /// paused rather than "saved".
    async fn saved_candidate(&self, origin: &str, username: &str, known: Option<&Zeroizing<String>>, verifier: Option<&verifier::Verifier>) -> Candidate {
        // Nothing to compare against (a record written before verifiers
        // existed): never assume the save happened.
        if known.is_none() && verifier.is_none() {
            return Candidate::NoVerifier;
        }
        {
            let mut st = self.state.lock().await;
            st.items = None;
            st.fulls.clear();
        }
        let mut cands = self.logins_for(origin).await.unwrap_or_default();
        cands.retain(|it| it.username.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(username)));
        if cands.is_empty() {
            return Candidate::Absent;
        }
        let mut different = false;
        let mut silent = vec![];
        for it in cands {
            let Some(v) = self.stored_password(&it).await else {
                silent.push(self.stores[it.store].name());
                continue;
            };
            // Positive evidence only: this process's own value, or the
            // verifier recorded for the workflow.
            if known.is_some_and(|k| *k == v) || verifier.is_some_and(|w| w.matches(&v)) {
                return Candidate::Ours(v);
            }
            different = true;
        }
        match (different, silent.is_empty()) {
            (true, true) => Candidate::Different { unreadable: vec![] },
            (true, false) => Candidate::Different { unreadable: silent },
            (false, _) => Candidate::Unreadable(silent.join(", ")),
        }
    }

    async fn login_value(&self, raw: &str, f: LoginField, hints: &[String], t: &Target) -> Result<(Zeroizing<String>, bool, String)> {
        let it = match self.choose_login_by(&t.origin, None, hints).await {
            Ok(it) => it,
            Err(e) => {
                // Just signed up: the generated password is this site's password.
                if f == LoginField::Password {
                    if let Some(g) = self.generated_for(&t.origin).await {
                        return Ok((g, true, "the password generated for this site".into()));
                    }
                }
                // Not this site's login but a named item ("{{Home Wi-Fi password}}"):
                // found by name, and typed only with the user's approval.
                if !hints.is_empty() {
                    let words: Vec<String> = names::parse(raw).words;
                    if let Ok(found) = Box::pin(self.custom_value(raw, &words, t)).await {
                        return Ok(found);
                    }
                }
                return Err(e.context(format!("{{{{{raw}}}}}")));
            }
        };
        let store = &self.stores[it.store];
        let src = |what: &str| format!("{} \"{}\" › {what}", store.name(), it.title);
        match f {
            LoginField::Otp => {
                let code = store.otp(&it).await.with_context(|| format!("{} \"{}\" has no one-time code", store.name(), it.title))?;
                Ok((code, true, src("one-time code")))
            }
            LoginField::Username => {
                if let Some(u) = it.username.clone().filter(|u| !u.is_empty()) {
                    return Ok((Zeroizing::new(u), false, src("username")));
                }
                let v = self.field_value(&it, "username", "username").await?.with_context(|| format!("{} \"{}\" has no username", store.name(), it.title))?;
                Ok((v, false, src("username")))
            }
            LoginField::Password => {
                let v = self.field_value(&it, "current-password", "password").await?.with_context(|| format!("{} \"{}\" has no password", store.name(), it.title))?;
                Ok((v, true, src("password")))
            }
        }
    }

    /// A field by standard name (or label, for helpers).
    async fn field_value(&self, it: &Item, token: &str, label: &str) -> Result<Option<Zeroizing<String>>> {
        match self.full(it).await? {
            Some(f) => Ok(f.get(token).map(|x| x.value.clone())),
            None => self.stores[it.store].field(it, token, label).await,
        }
    }

    /// A card or identity field.
    async fn kind_value(&self, raw: &str, want: Want, hints: &[String], t: &Target) -> Result<(Zeroizing<String>, bool, String)> {
        let kind = match want {
            Want::Card(_) => Kind::Card,
            _ => Kind::Identity,
        };
        let it = self.pick(kind, hints, raw).await?;
        self.approve(&it, raw, &t.origin).await?;
        let store = &self.stores[it.store];
        let full = self.full(&it).await?;
        let val = |tok: &'static str| self.item_field(&it, &full, tok);
        use {CardField as C, IdField as I};
        let v: Option<String> = match want {
            Want::Card(C::Exp) | Want::Card(C::ExpMonth) | Want::Card(C::ExpYear) => {
                let (m, y) = match val("cc-exp").await.and_then(|e| store::month_year(&e)) {
                    Some(my) => my,
                    None => {
                        let m: Option<u32> = val("cc-exp-month").await.and_then(|m| m.trim().parse().ok());
                        let y: Option<u32> = val("cc-exp-year").await.and_then(|y| y.trim().parse().ok()).map(|y: u32| if y < 100 { 2000 + y } else { y });
                        match (m, y) {
                            (Some(m), Some(y)) => (m, y),
                            _ => bail!("{} \"{}\" has no expiry date", store.name(), it.title),
                        }
                    }
                };
                let long = t.says("yyyy") || t.ac == "cc-exp" && t.says("20");
                Some(match want {
                    Want::Card(C::ExpMonth) => format!("{m:02}"),
                    Want::Card(C::ExpYear) if t.says("yy") && !t.says("yyyy") => format!("{:02}", y % 100),
                    Want::Card(C::ExpYear) => y.to_string(),
                    _ if long => format!("{m:02}/{y}"),
                    _ => format!("{m:02}/{:02}", y % 100),
                })
            }
            Want::Card(f) => {
                let tok = match f {
                    C::Number => "cc-number",
                    C::Csc => "cc-csc",
                    C::Name => "cc-name",
                    _ => "cc-type",
                };
                val(tok).await
            }
            Want::Identity(I::Name) => match val("name").await {
                Some(n) => Some(n),
                None => {
                    let parts: Vec<String> = [val("given-name").await, val("family-name").await].into_iter().flatten().collect();
                    (!parts.is_empty()).then(|| parts.join(" "))
                }
            },
            Want::Identity(f @ (I::Street | I::City | I::Region | I::Postal | I::Country)) => {
                let tok = Want::Identity(f).token();
                match val(tok).await {
                    Some(v) => Some(v),
                    None => val("street-address").await.and_then(|a| address_part(&a, f)),
                }
            }
            Want::Email => val("email").await,
            other => val(other.token()).await,
        };
        let v = v.filter(|v| !v.is_empty()).with_context(|| format!("{} \"{}\" has no {}", store.name(), it.title, raw))?;
        let concealed = want.concealed();
        let src = format!("{} \"{}\" › {raw}", store.name(), it.title);
        let key = kind.word();
        self.state.lock().await.chosen_kind.insert(key, (it.store, it.id.clone()));
        Ok((Zeroizing::new(v), concealed, src))
    }

    async fn item_field(&self, it: &Item, full: &Option<Arc<Full>>, tok: &str) -> Option<String> {
        match full {
            Some(f) => f.get(tok).map(|x| x.value.to_string()),
            None => self.stores[it.store].field(it, tok, "").await.ok().flatten().map(|v| v.to_string()),
        }
    }

    /// The card or identity the words name (or the only one, or the one used before).
    async fn pick(&self, kind: Kind, hints: &[String], raw: &str) -> Result<Item> {
        let items = self.items().await?;
        let cands: Vec<&Item> = items.iter().filter(|it| it.kind == kind).collect();
        if cands.is_empty() {
            bail!("no saved {} in {} for {{{{{raw}}}}}", kind.word(), self.store_names());
        }
        const GENERIC: &[&str] = &["card", "credit", "debit", "shipping", "billing", "my", "default", "address", "saved"];
        let hints: Vec<&String> = hints.iter().filter(|h| !GENERIC.contains(&h.as_str())).collect();
        let score = |it: &Item| {
            let hay = format!("{} {}", it.title, it.note.clone().unwrap_or_default()).to_lowercase();
            hints.iter().filter(|h| hay.contains(h.as_str())).count()
        };
        let best = cands.iter().map(|it| score(it)).max().unwrap_or(0);
        let mut top: Vec<&Item> = if best > 0 { cands.iter().copied().filter(|it| score(it) == best).collect() } else { cands.clone() };
        if top.len() > 1 {
            if let Some((s, id)) = self.state.lock().await.chosen_kind.get(kind.word()).cloned() {
                if let Some(it) = top.iter().find(|it| it.store == s && it.id == id) {
                    return Ok((*it).clone());
                }
            }
            top.truncate(6);
            let opts: Vec<String> = top.iter().map(|it| format!("\"{}\"{}", it.title, it.note.as_ref().map(|n| format!(" ({n})")).unwrap_or_default())).collect();
            bail!("several saved {}s match {{{{{raw}}}}}: {}. Name one, e.g. {{{{{} {raw}}}}}", kind.word(), opts.join(", "), top[0].title);
        }
        Ok(top[0].clone())
    }

    /// Any other item, named by title and field label ("Stripe test secret key").
    async fn custom_value(&self, raw: &str, words: &[String], t: &Target) -> Result<(Zeroizing<String>, bool, String)> {
        let items = self.items().await?;
        let (it, label) = self.find_custom(&items, raw, words).await?;
        let store = &self.stores[it.store];
        let own_login = it.kind == Kind::Login && it.urls.iter().any(|u| login_matches(u, &t.origin));
        if !own_login {
            self.approve(&it, raw, &t.origin).await?;
        }
        let (v, concealed) = match self.full(&it).await? {
            Some(f) => {
                let fl = f.fields.iter().find(|x| x.label == label).with_context(|| format!("{} \"{}\" has no field {label}", store.name(), it.title))?;
                (fl.value.clone(), fl.concealed)
            }
            None => (store.field(&it, "", &label).await?.with_context(|| format!("{} \"{}\" has no field {label}", store.name(), it.title))?, true),
        };
        Ok((v, concealed, format!("{} \"{}\" › {label}", store.name(), it.title)))
    }

    /// (item, field label) for custom words; errors list the near misses.
    async fn find_custom(&self, items: &[Item], raw: &str, words: &[String]) -> Result<(Item, String)> {
        let toks = |s: &str| -> Vec<String> { s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect() };
        let title_score = |it: &Item| {
            let tw = toks(&it.title);
            words.iter().filter(|w| tw.contains(w)).count()
        };
        let mut ranked: Vec<(&Item, usize)> = items.iter().map(|it| (it, title_score(it))).filter(|(_, s)| *s > 0).collect();
        ranked.sort_by_key(|(it, s)| (std::cmp::Reverse(*s), it.title.len()));
        ranked.truncate(5);
        if ranked.is_empty() {
            bail!("nothing in {} is named like {{{{{raw}}}}}: name the item as it is titled there (e.g. {{{{Stripe secret key}}}})", self.store_names());
        }
        // (item, label, score, concealed)
        let mut best: Vec<(Item, String, usize)> = vec![];
        for (it, ts) in ranked {
            let labels: Vec<(String, bool)> = match self.full(it).await {
                Ok(Some(f)) => f.fields.iter().map(|x| (x.label.clone(), x.concealed)).collect(),
                Ok(None) => it.labels.iter().map(|l| (l.clone(), true)).collect(),
                Err(_) => continue,
            };
            let title_words = toks(&it.title);
            let rest: Vec<&String> = words.iter().filter(|w| !title_words.contains(w)).collect();
            for (label, concealed) in &labels {
                let lw = toks(label);
                let ls = rest.iter().filter(|w| lw.contains(w)).count();
                // With nothing left to match, the item's one concealed field (an API credential).
                let only_secret = rest.is_empty() && *concealed && labels.iter().filter(|(_, c)| *c).count() == 1;
                if ls > 0 || only_secret {
                    best.push((it.clone(), label.clone(), ts * 10 + ls * 10 + usize::from(*concealed)));
                }
            }
        }
        best.sort_by_key(|(_, _, s)| std::cmp::Reverse(*s));
        match best.as_slice() {
            [] => bail!("found items named like {{{{{raw}}}}} but no matching field; name the field too (e.g. {{{{<item> <field label>}}}})"),
            [one] => Ok((one.0.clone(), one.1.clone())),
            [a, b, ..] if a.2 > b.2 => Ok((a.0.clone(), a.1.clone())),
            many => {
                let opts: Vec<String> = many.iter().take(5).map(|(it, l, _)| format!("\"{}\" › {l}", it.title)).collect();
                bail!("{{{{{raw}}}}} could be: {}. Use more of the item's title or field label", opts.join(", "))
            }
        }
    }

    /// Checks (or asks for) the user's approval to type this item on this site.
    async fn approve(&self, it: &Item, what: &str, origin: &str) -> Result<()> {
        let site = site_of(origin);
        let store = &self.stores[it.store];
        let key = format!("{}:{}", store.key(), it.id);
        if self.state.lock().await.once.contains(&(key.clone(), site.clone())) || allowed(&key, &site) {
            return Ok(());
        }
        let desc = format!("{} \"{}\"", store.name(), it.title);
        let how = format!("fab secrets allow \"{what}\" --url {origin}");
        let prompt = std::env::var("FAB_SECRETS_PROMPT").ok().or_else(|| self.cfg.prompt.clone()).unwrap_or_else(|| "dialog".into());
        if prompt == "never" {
            bail!("{desc} isn't approved for {site}: the user can allow it with `{how}`");
        }
        match ask_user(&format!("fab wants to type {{{{{what}}}}} from your {desc} into {site}."), &site).await {
            Answer::Always => {
                add_allow(Allow { item: key, title: it.title.clone(), site })?;
                Ok(())
            }
            Answer::Once => {
                self.state.lock().await.once.insert((key, site));
                Ok(())
            }
            Answer::Denied => bail!("the user declined to type {desc} into {site}"),
            Answer::Unavailable(why) => bail!("{desc} isn't approved for {site} ({why}): the user can allow it with `{how}`"),
        }
    }

    /// Resolves a name for a step on `origin` before anything is typed: any
    /// approval is asked now, and a name that can't be resolved fails the step
    /// up front. Returns the value when it is plain (a name, an address), which
    /// can go into the step as ordinary text; None for a concealed one, which is
    /// typed only at the moment of filling.
    pub async fn prepare(&self, raw: &str, origin: &str) -> Result<(Option<Zeroizing<String>>, String)> {
        let (v, concealed, src) = self.resolve(raw, &Target::at(origin)).await?;
        Ok(((!concealed).then_some(v), src))
    }

    /// For `fab secrets allow/test`: the item a name refers to, without its value.
    pub async fn locate(&self, raw: &str, origin: &str) -> Result<(Item, String)> {
        let name = names::parse(raw.trim().trim_start_matches("{{").trim_end_matches("}}"));
        let desc = |it: &Item| format!("{} \"{}\"", self.stores[it.store].name(), it.title);
        match name.want {
            Want::NewPassword => bail!("{{{{new password}}}} is generated, not stored"),
            Want::Login(_) => {
                let it = self.choose_login(origin, None).await?;
                let d = desc(&it);
                Ok((it, d))
            }
            Want::Card(_) => {
                let it = self.pick(Kind::Card, &name.hints, raw).await?;
                let d = desc(&it);
                Ok((it, d))
            }
            Want::Identity(_) | Want::Email => {
                let it = self.pick(Kind::Identity, &name.hints, raw).await?;
                let d = desc(&it);
                Ok((it, d))
            }
            Want::Custom => {
                let items = self.items().await?;
                let (it, label) = self.find_custom(&items, raw, &name.words).await?;
                let d = format!("{} › {label}", desc(&it));
                Ok((it, d))
            }
        }
    }

    /// Resolves a name as if typing it on `origin`, for `fab secrets test`
    /// (asks for approval like a fill would).
    pub async fn test(&self, raw: &str, origin: &str) -> Result<(usize, bool, String)> {
        let inner = raw.trim().trim_start_matches("{{").trim_end_matches("}}").trim();
        let (v, c, src) = self.resolve(inner, &Target::at(origin)).await?;
        Ok((v.chars().count(), c, src))
    }

    /// Records an approval without asking (the user ran `fab secrets allow`).
    pub async fn allow(&self, raw: &str, origin: &str) -> Result<String> {
        let (it, desc) = self.locate(raw, origin).await?;
        let site = site_of(origin);
        add_allow(Allow { item: format!("{}:{}", self.stores[it.store].key(), it.id), title: it.title.clone(), site: site.clone() })?;
        Ok(format!("allowed {desc} on {site}"))
    }

    /// Saves a new login in the configured (or first capable) store.
    pub async fn save_login(&self, origin: &str, username: &str, password: &str) -> Result<String> {
        let site = site_of(origin);
        let order: Vec<usize> = match &self.cfg.save_to {
            Some(k) => self.stores.iter().position(|s| s.key() == *k || s.name().to_lowercase() == k.to_lowercase()).into_iter().collect(),
            None => (0..self.stores.len()).collect(),
        };
        if order.is_empty() {
            return Err(SaveNotSent(format!("nowhere to save the login: {}", self.store_names())).into());
        }
        let mut unsent = None;
        for i in order {
            let s = &self.stores[i];
            if !s.status().await.ready {
                continue;
            }
            match s.save_login(origin, &site, username, password).await {
                Ok(where_) => {
                    let mut st = self.state.lock().await;
                    st.items = None;
                    return Ok(where_);
                }
                // Nothing reached this store: another one may still take it.
                Err(e) if e.downcast_ref::<SaveNotSent>().is_some() => unsent = Some(e),
                Err(e) => return Err(e.context(format!("{} save response is uncertain; reconcile before another save", s.name()))),
            }
        }
        Err(unsent.unwrap_or_else(|| SaveNotSent("could not save the login: no store is ready".into()).into()))
    }

    /// One line per store: name, whether it's ready, and why not.
    pub async fn status(&self) -> Vec<(String, bool, String)> {
        let st = futures_util::future::join_all(self.stores.iter().map(|s| s.status())).await;
        self.stores.iter().zip(st).map(|(s, st)| (s.name(), st.ready, st.note)).collect()
    }
}

fn list_logins(items: &[Item]) -> String {
    items.iter().take(6).map(|it| format!("\"{}\" ({})", it.title, it.username.as_deref().unwrap_or("no username"))).collect::<Vec<_>>().join(", ")
}

/// A strong password: 20 characters from letters, digits and symbols, with
/// at least one of each.
fn generate() -> Zeroizing<String> {
    const LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const DIGIT: &[u8] = b"23456789";
    const SYMBOL: &[u8] = b"-_.!@#%+=";
    let all: Vec<u8> = [LOWER, UPPER, DIGIT, SYMBOL].concat();
    loop {
        let mut buf = Zeroizing::new([0u8; 64]);
        getrandom::fill(&mut buf[..]).expect("no system randomness");
        // Rejection sampling keeps every character equally likely.
        let limit = 256 - 256 % all.len();
        let pw: String = buf.iter().filter(|b| (**b as usize) < limit).take(20).map(|b| all[*b as usize % all.len()] as char).collect();
        let has = |set: &[u8]| pw.bytes().any(|c| set.contains(&c));
        if pw.len() == 20 && has(LOWER) && has(UPPER) && has(DIGIT) && has(SYMBOL) {
            return Zeroizing::new(pw);
        }
    }
}

/// One part of an address stored as one line ("1 Main St, Springfield, IL, 62704, us").
fn address_part(a: &str, f: IdField) -> Option<String> {
    let parts: Vec<&str> = a.split([',', '\n']).map(str::trim).filter(|p| !p.is_empty()).collect();
    let (street, city, region, postal, country) = match parts.as_slice() {
        [s, c, r, p, co, ..] => (*s, *c, *r, *p, *co),
        [s, c, rp, co] => {
            // "IL 62704" or "0153 Oslo": the part with digits is the postal code.
            let toks: Vec<&str> = rp.split_whitespace().collect();
            let p = toks.iter().rev().find(|t| t.chars().any(|c| c.is_ascii_digit())).copied().unwrap_or("");
            let r = toks.iter().filter(|t| **t != p).copied().collect::<Vec<_>>().join(" ");
            return address_pick(f, s, c, &r, p, co);
        }
        [s, c, p] => (*s, *c, "", *p, ""),
        [s] => (*s, "", "", "", ""),
        _ => return None,
    };
    address_pick(f, street, city, region, postal, country)
}

fn address_pick(f: IdField, street: &str, city: &str, region: &str, postal: &str, country: &str) -> Option<String> {
    let v = match f {
        IdField::Street => street,
        IdField::City => city,
        IdField::Region => region,
        IdField::Postal => postal,
        IdField::Country => country,
        _ => return None,
    };
    (!v.is_empty()).then(|| v.to_string())
}

// ---------- sites ----------

/// (scheme, host, port) of a URL or origin.
fn parts(url: &str) -> Option<(String, String, Option<u16>)> {
    let url = url.trim();
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_lowercase(), r),
        None => ("https".to_string(), url),
    };
    let hostport = rest.split(['/', '?', '#']).next()?.rsplit('@').next()?;
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => match p.parse::<u16>() {
            Ok(p) => (h.to_string(), Some(p)),
            Err(_) => (hostport.to_string(), None),
        },
        _ => (hostport.to_string(), None),
    };
    let host = host.trim_end_matches('.').to_lowercase();
    (!host.is_empty()).then_some((scheme, host, port))
}

fn is_local(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host.ends_with(".test") || host.ends_with(".local") || host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') || !host.contains('.')
}

/// Hosting suffixes where every subdomain is someone else's site.
const SHARED: &[&str] = &[
    "github.io", "gitlab.io", "netlify.app", "vercel.app", "herokuapp.com", "pages.dev", "workers.dev", "web.app", "firebaseapp.com", "azurewebsites.net", "cloudfront.net", "appspot.com",
    "blogspot.com", "ngrok.io", "ngrok-free.app", "fly.dev", "onrender.com", "glitch.me", "replit.app", "surge.sh", "s3.amazonaws.com", "amplifyapp.com", "myshopify.com",
];

/// The site a URL belongs to: its registrable domain (github.com for
/// gist.github.com), or host:port for local servers.
pub fn site_of(url: &str) -> String {
    let Some((_, host, port)) = parts(url) else { return String::new() };
    if is_local(&host) {
        return match port {
            Some(p) => format!("{host}:{p}"),
            None => host,
        };
    }
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    let take = if SHARED.iter().any(|s| host.ends_with(&format!(".{s}")) || host == *s) {
        SHARED.iter().find(|s| host.ends_with(&format!(".{s}"))).map(|s| s.split('.').count() + 1).unwrap_or(n)
    } else if n >= 3 && labels[n - 1].len() == 2 && matches!(labels[n - 2], "co" | "com" | "org" | "net" | "ac" | "gov" | "edu" | "ne" | "or" | "gv" | "go") {
        3
    } else {
        2
    };
    labels[n.saturating_sub(take)..].join(".")
}

/// Whether a login saved for `item_url` may be typed on `page_origin`:
/// the same site, and never from https down to http.
pub fn login_matches(item_url: &str, page_origin: &str) -> bool {
    let (Some((is, _, _)), Some((ps, ph, _))) = (parts(item_url), parts(page_origin)) else { return false };
    if site_of(item_url) != site_of(page_origin) || site_of(page_origin).is_empty() {
        return false;
    }
    !(is == "https" && ps == "http" && !is_local(&ph))
}

// ---------- approvals ----------

fn allowed(item: &str, site: &str) -> bool {
    read_allow().allow.iter().any(|a| a.item == item && a.site == site)
}

fn read_allow() -> AllowFile {
    std::fs::read_to_string(allow_path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
}

fn add_allow(a: Allow) -> Result<()> {
    let mut f = read_allow();
    if !f.allow.iter().any(|x| x.item == a.item && x.site == a.site) {
        f.allow.push(a);
    }
    let dir = crate::paths::config_dir();
    std::fs::create_dir_all(&dir)?;
    let p = allow_path();
    std::fs::write(&p, toml::to_string(&f)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Every recorded approval.
pub fn approvals() -> Vec<Allow> {
    read_allow().allow
}

/// Removes approvals for a site (and an item title, when given); returns how many.
pub fn revoke(site: &str, title: Option<&str>) -> Result<usize> {
    let mut f = read_allow();
    let n = f.allow.len();
    f.allow.retain(|a| !(a.site == site && title.is_none_or(|t| a.title.eq_ignore_ascii_case(t))));
    let removed = n - f.allow.len();
    if removed > 0 {
        std::fs::write(allow_path(), toml::to_string(&f)?)?;
    }
    Ok(removed)
}

enum Answer {
    Always,
    Once,
    Denied,
    Unavailable(String),
}

/// Asks the user directly (never the model): a macOS dialog, or zenity on Linux.
async fn ask_user(msg: &str, site: &str) -> Answer {
    let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    if cfg!(target_os = "macos") {
        let script = format!(
            "display dialog \"{}\" with title \"fab\" buttons {{\"Deny\", \"Allow once\", \"Always allow on {}\"}} default button \"Deny\" cancel button \"Deny\" giving up after 120 with icon caution",
            q(msg),
            q(site)
        );
        let out = tokio::process::Command::new("osascript").arg("-e").arg(&script).stdin(std::process::Stdio::null()).output().await;
        return match out {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout);
                if s.contains("gave up:true") {
                    Answer::Unavailable("no answer within 2 minutes".into())
                } else if s.contains("Always allow") {
                    Answer::Always
                } else if s.contains("Allow once") {
                    Answer::Once
                } else if o.status.success() {
                    Answer::Denied
                } else {
                    let err = String::from_utf8_lossy(&o.stderr);
                    if err.contains("-128") { Answer::Denied } else { Answer::Unavailable(format!("no dialog: {}", err.trim())) }
                }
            }
            Err(e) => Answer::Unavailable(format!("no dialog: {e}")),
        };
    }
    if store::on_path("zenity") && std::env::var_os("DISPLAY").or_else(|| std::env::var_os("WAYLAND_DISPLAY")).is_some() {
        let out = tokio::process::Command::new("zenity").args(["--question", "--title=fab", "--ok-label=Allow once", "--cancel-label=Deny", "--timeout=120", &format!("--text={msg}")]).output().await;
        return match out {
            Ok(o) if o.status.success() => Answer::Once,
            Ok(o) if o.status.code() == Some(5) => Answer::Unavailable("no answer within 2 minutes".into()),
            Ok(_) => Answer::Denied,
            Err(e) => Answer::Unavailable(format!("no dialog: {e}")),
        };
    }
    Answer::Unavailable("no way to ask the user here".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites() {
        assert_eq!(site_of("https://gist.github.com/x"), "github.com");
        assert_eq!(site_of("https://www.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(site_of("http://localhost:3000/login"), "localhost:3000");
        assert_eq!(site_of("http://127.0.0.1:8080"), "127.0.0.1:8080");
        assert_eq!(site_of("https://ada.github.io/app"), "ada.github.io");
        assert_eq!(site_of("console.aws.amazon.com"), "amazon.com");
        assert!(login_matches("https://github.com/login", "https://gist.github.com"));
        assert!(!login_matches("https://github.com", "https://github.com.evil.io"));
        assert!(!login_matches("https://github.com", "http://github.com"));
        assert!(!login_matches("https://ada.github.io", "https://eve.github.io"));
        assert!(login_matches("localhost:3000", "http://localhost:3000"));
        assert!(!login_matches("http://localhost:3000", "http://localhost:4000"));
    }

    #[test]
    fn generated_passwords() {
        let a = generate();
        let b = generate();
        assert_eq!(a.len(), 20);
        assert_ne!(*a, *b);
    }

    /// A fake password store: a `fab-secret-<name>` helper script in its own
    /// temporary directory. It logs every call, and `store` either persists the
    /// login and exits `store_exit`, or (with `persist: false`) fails without it.
    #[cfg(unix)]
    struct FakeStore {
        dir: std::path::PathBuf,
        name: String,
    }

    #[cfg(unix)]
    impl FakeStore {
        fn new(name: &str, persist: bool, store_exit: i32) -> Self {
            Self::with_delay(name, persist, store_exit, 0.0)
        }

        /// A store whose `store` takes `delay` seconds.
        fn with_delay(name: &str, persist: bool, store_exit: i32, delay: f64) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let mut tag = [0u8; 8];
            getrandom::fill(&mut tag).unwrap();
            let dir = std::env::temp_dir().join(format!("fab-secret-test-{name}-{}", tag.iter().map(|b| format!("{b:02x}")).collect::<String>()));
            std::fs::create_dir_all(&dir).unwrap();
            let persist = if persist { r#"cat > "$D/stored""# } else { "cat > /dev/null" };
            let script = format!(
                r#"#!/bin/sh
D="$(dirname "$0")"
echo "$1" >> "$D/calls"
case "$1" in
  store) sleep {delay}; {persist}; exit {store_exit} ;;
  list) [ -f "$D/stored" ] || exit 1; echo id=generated; echo kind=login; grep '^url=' "$D/stored"; grep '^username=' "$D/stored"; echo field=password; exit 0 ;;
  get) [ -f "$D/stored" ] || exit 1; printf 'value=%s\n' "$(sed -n 's/^password=//p' "$D/stored")"; exit 0 ;;
esac
exit 1
"#
            );
            let path = dir.join(format!("fab-secret-{name}"));
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            FakeStore { dir, name: name.into() }
        }

        fn store(&self) -> Store {
            Store::Helper(helper::Helper { name: self.name.clone(), path: self.dir.join(format!("fab-secret-{}", self.name)) })
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default().lines().map(str::to_string).collect()
        }

        fn saves(&self) -> usize {
            self.calls().iter().filter(|c| *c == "store").count()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    fn vault_with(stores: &[&FakeStore], save_to: Option<&str>) -> Vault {
        Vault::new(stores.iter().map(|s| s.store()).collect(), SecretsConfig { save_to: save_to.map(str::to_string), ..Default::default() }, None)
    }

    /// A vault recording its workflows in `journal`: building a second one
    /// on the same journal is a daemon restart.
    #[cfg(unix)]
    fn vault_journaled(stores: &[&FakeStore], journal: &std::path::Path) -> Vault {
        Vault::new(stores.iter().map(|s| s.store()).collect(), SecretsConfig::default(), Some(workflows::Journal::new(journal)))
    }

    const ORIGIN: &str = "http://localhost:4173";

    fn field() -> Target {
        Target { ty: "password".into(), ..Target::at(ORIGIN) }
    }

    /// The generated value and its workflow checkpoint (as JSON), which must
    /// never contain it.
    #[cfg(unix)]
    async fn generated_state(vault: &Vault) -> (Zeroizing<String>, String) {
        let st = vault.state.lock().await;
        let g = &st.generated[&site_of(ORIGIN)];
        let json = serde_json::to_string(&g.workflow.checkpoint()).unwrap();
        let value = g.value.clone().expect("no generated value in memory");
        assert!(!json.contains(value.as_str()), "checkpoint holds the generated password");
        (value, json)
    }

    /// A journal file in its own temporary directory.
    #[cfg(unix)]
    struct TempJournal(std::path::PathBuf);

    #[cfg(unix)]
    impl TempJournal {
        fn new() -> Self {
            let mut tag = [0u8; 8];
            getrandom::fill(&mut tag).unwrap();
            let dir = std::env::temp_dir().join(format!("fab-workflows-test-{}", tag.iter().map(|b| format!("{b:02x}")).collect::<String>()));
            std::fs::create_dir_all(&dir).unwrap();
            TempJournal(dir)
        }

        fn path(&self) -> std::path::PathBuf {
            self.0.join("secret-workflows.json")
        }

        fn text(&self) -> String {
            std::fs::read_to_string(self.path()).unwrap_or_default()
        }

        /// The recorded bytes hold no part of `secret` (no 6-character run).
        fn assert_opaque(&self, secret: &str) {
            let text = self.text();
            assert!(!text.is_empty(), "nothing recorded");
            for w in secret.as_bytes().windows(6) {
                assert!(!text.contains(std::str::from_utf8(w).unwrap()), "{text}");
            }
            assert!(!text.contains("password\""), "{text}");
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(self.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[cfg(unix)]
    impl Drop for TempJournal {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_reconciles_a_lost_save_from_the_store() {
        let journal = TempJournal::new();
        // The store writes the login, then the reply is lost (exit 3).
        let store = FakeStore::new("restart", true, 3);
        let value = {
            let vault = vault_journaled(&[&store], &journal.path());
            assert!(vault.save_generated_login(ORIGIN, "ada@example.com").await.is_err());
            let (value, json) = generated_state(&vault).await;
            assert!(json.contains("ReadyReconcile"), "{json}");
            value
        };
        journal.assert_opaque(&value);
        assert!(journal.text().contains("ada@example.com") && journal.text().contains("ReadyReconcile"), "{}", journal.text());

        // The daemon restarts: the workflow comes back without its material.
        let vault = vault_journaled(&[&store], &journal.path());
        refuses_to_type(&vault).await;
        let err = vault.save_generated_login(ORIGIN, "grace@example.com").await.unwrap_err();
        assert!(format!("{err:#}").contains("is for ada@example.com"), "{err:#}");
        vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap();
        assert_eq!(store.saves(), 1, "reconciliation saved again");
        let (after, json) = generated_state(&vault).await;
        assert_eq!(*after, *value, "not the stored password");
        assert!(json.contains("ReadyType"), "{json}");
        let filled = vault.substitute_for_input("{{new password}}", &field()).await.unwrap();
        assert_eq!(*filled.value, *value);
        journal.assert_opaque(&value);

        // Another restart after the confirmed save: the store's login again.
        let vault = vault_journaled(&[&store], &journal.path());
        assert!(vault.generated_for(ORIGIN).await.is_none());
        vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap();
        assert_eq!(*vault.generated_for(ORIGIN).await.unwrap(), *value);
        assert_eq!(store.saves(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_without_the_saved_login_stays_paused_until_reset() {
        let journal = TempJournal::new();
        // Dispatched, failed, and nothing written.
        let store = FakeStore::new("absent", false, 3);
        let value = {
            let vault = vault_journaled(&[&store], &journal.path());
            assert!(vault.save_generated_login(ORIGIN, "ada@example.com").await.is_err());
            generated_state(&vault).await.0
        };
        journal.assert_opaque(&value);
        // A save whose outcome is unknown can't be reset before it is reconciled.
        let cli = workflows::Journal::new(journal.path());
        assert!(format!("{:#}", cli.reset(&site_of(ORIGIN)).unwrap_err()).contains("reconcile"));

        let vault = vault_journaled(&[&store], &journal.path());
        let err = vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap_err();
        assert!(format!("{err:#}").contains("unconfirmed"), "{err:#}");
        refuses_to_type(&vault).await;
        assert!(journal.text().contains("SaveUnconfirmed"), "{}", journal.text());

        // Paused across another restart too: never regenerated, never saved again.
        let vault = vault_journaled(&[&store], &journal.path());
        let err = vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap_err();
        assert!(format!("{err:#}").contains("fab secrets reset"), "{err:#}");
        refuses_to_type(&vault).await;
        assert_eq!(store.saves(), 1);

        // `fab secrets reset` (another process) clears it; the running daemon
        // notices and the next signup generates and saves anew.
        let cleared = cli.reset(&site_of(ORIGIN)).unwrap().expect("nothing cleared");
        assert_eq!(cleared.username, "ada@example.com");
        assert!(cli.get(&site_of(ORIGIN)).unwrap().is_none());
        assert!(cli.reset(&site_of(ORIGIN)).unwrap().is_none());
        let _ = vault.save_generated_login(ORIGIN, "ada@example.com").await;
        assert_eq!(store.saves(), 2);
        let (fresh, _) = generated_state(&vault).await;
        assert_ne!(*fresh, *value);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_concurrent_save_for_the_site_is_refused_as_in_progress() {
        let journal = TempJournal::new();
        let store = FakeStore::with_delay("slow", true, 0, 0.5);
        let vault = vault_journaled(&[&store], &journal.path());
        let second = async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            vault.save_generated_login(ORIGIN, "ada@example.com").await
        };
        let (first, second) = tokio::join!(vault.save_generated_login(ORIGIN, "ada@example.com"), second);
        first.unwrap();
        let err = second.unwrap_err();
        assert!(format!("{err:#}").contains("already in progress"), "{err:#}");
        assert_eq!(store.saves(), 1);
        let value = vault.generated_for(ORIGIN).await.unwrap();
        journal.assert_opaque(&value);
    }

    /// Reconciliation reads the store fresh, not a full item cached before.
    #[cfg(unix)]
    #[tokio::test]
    async fn reconciliation_ignores_cached_items() {
        let store = FakeStore::new("cached", true, 3);
        let vault = vault_with(&[&store], None);
        assert!(vault.save_generated_login(ORIGIN, "ada@example.com").await.is_err());
        // A stale copy of the item, as an earlier fill would have cached it.
        let stale = Full { fields: vec![], totp: false };
        vault.state.lock().await.fulls.insert((0, "generated".into()), Arc::new(stale));
        vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap();
        assert!(vault.generated_for(ORIGIN).await.is_some());
    }

    #[cfg(unix)]
    async fn refuses_to_type(vault: &Vault) {
        assert!(vault.generated_for(ORIGIN).await.is_none());
        let err = vault.substitute_for_input("{{new password}}", &field()).await.err().expect("typed an unsaved password");
        assert!(format!("{err:#}").contains("must be saved before typing"), "{err:#}");
        // The lower-level path refuses too, and neither generates anew.
        assert!(vault.substitute("{{new password}}", &field()).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dispatched_save_failure_reconciles_and_never_types() {
        let failing = FakeStore::new("failing", false, 3);
        let second = FakeStore::new("second", true, 0);
        let vault = vault_with(&[&failing, &second], None);
        let err = vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap_err();
        assert!(format!("{err:#}").contains("uncertain"), "{err:#}");
        assert_eq!(failing.saves(), 1);
        assert_eq!(second.saves(), 0, "a second store was tried after a dispatched save");
        let (value, json) = generated_state(&vault).await;
        assert!(json.contains("ReadyReconcile"), "{json}");
        refuses_to_type(&vault).await;

        // Retrying reconciles (nothing found): no regeneration, no second save.
        let err = vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap_err();
        assert!(format!("{err:#}").contains("unconfirmed"), "{err:#}");
        let (after, json) = generated_state(&vault).await;
        assert_eq!(*after, *value, "the password was regenerated");
        assert!(json.contains("SaveUnconfirmed"), "{json}");
        refuses_to_type(&vault).await;
        assert!(vault.save_generated_login(ORIGIN, "ada@example.com").await.is_err());
        assert_eq!((failing.saves(), second.saves()), (1, 0));
        assert_eq!(*generated_state(&vault).await.0, *value);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsent_save_fails_without_typing_or_trying_another_store() {
        let a = FakeStore::new("first", true, 0);
        let b = FakeStore::new("other", true, 0);
        // `save_to` names no store: the save is never dispatched anywhere.
        let vault = vault_with(&[&a, &b], Some("missing"));
        let err = vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap_err();
        assert!(format!("{err:#}").contains("nowhere to save"), "{err:#}");
        assert_eq!((a.saves(), b.saves()), (0, 0));
        let (value, json) = generated_state(&vault).await;
        assert!(json.contains("ReadySave"), "{json}");
        refuses_to_type(&vault).await;

        // Nothing was sent, so the same value can be saved once a store is set.
        let vault = Vault { cfg: SecretsConfig { save_to: Some("other".into()), ..Default::default() }, ..vault };
        vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap();
        assert_eq!((a.saves(), b.saves()), (0, 1));
        assert_eq!(*vault.generated_for(ORIGIN).await.unwrap(), *value);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconciliation_finds_the_saved_login_then_types_it() {
        // The store writes the login, then the reply is lost (exit 3).
        let store = FakeStore::new("lossy", true, 3);
        let vault = vault_with(&[&store], None);
        assert!(vault.save_generated_login(ORIGIN, "ada@example.com").await.is_err());
        let (value, json) = generated_state(&vault).await;
        assert!(json.contains("ReadyReconcile"), "{json}");
        refuses_to_type(&vault).await;

        vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap();
        assert_eq!(store.saves(), 1, "reconciliation saved again");
        let (after, json) = generated_state(&vault).await;
        assert_eq!(*after, *value);
        assert!(json.contains("ReadyType") && json.contains("Saved"), "{json}");
        assert_eq!(*vault.generated_for(ORIGIN).await.unwrap(), *value);
        let filled = vault.substitute_for_input("{{new password}}", &field()).await.unwrap();
        assert_eq!(*filled.value, *value);
        assert!(filled.concealed);
        // Asking again reuses the confirmed login: no save, no regeneration.
        vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap();
        assert_eq!(store.saves(), 1);
    }

    /// The durable workflow for a real generated value: every phase's
    /// checkpoint is scanned for the value's bytes.
    #[cfg(unix)]
    #[tokio::test]
    async fn workflow_checkpoints_never_hold_the_generated_value() {
        use crate::secret_machine::*;
        let store = FakeStore::new("scan", true, 0);
        let vault = vault_with(&[&store], Some("missing"));
        let _ = vault.save_generated_login(ORIGIN, "ada@example.com").await;
        let (value, json) = generated_state(&vault).await;
        let scan = |m: &Machine| {
            let json = serde_json::to_string(&m.checkpoint()).unwrap();
            assert!(!json.contains(value.as_str()), "{json}");
            // Nor any 6-character run of it.
            for w in value.as_bytes().windows(6) {
                assert!(!json.contains(std::str::from_utf8(w).unwrap()), "{json}");
            }
            Machine::restore(serde_json::from_str(&json).unwrap()).unwrap();
        };
        let mut m = Machine::restore(serde_json::from_str(&json).unwrap()).unwrap();
        scan(&m);
        let save = m.issue().unwrap().unwrap();
        scan(&m);
        m.complete(Completion::Save { op: save.op(), result: SaveResult::LostResponse }).unwrap();
        scan(&m);
        let reconcile = m.issue().unwrap().unwrap();
        scan(&m);
        m.complete(Completion::ReconcileSave { op: reconcile.op(), result: ReconcileResult::Found(SavedRef::new().unwrap()) }).unwrap();
        scan(&m);
        let typing = m.issue().unwrap().unwrap();
        scan(&m);
        m.complete(Completion::Type { op: typing.op(), result: TypeResult::Acknowledged }).unwrap();
        scan(&m);
        assert!(m.done());
        // And the vault's own workflow through a real save.
        let vault = Vault { cfg: SecretsConfig::default(), ..vault };
        vault.save_generated_login(ORIGIN, "ada@example.com").await.unwrap();
        let (after, json) = generated_state(&vault).await;
        assert_eq!(*after, *value);
        assert!(json.contains("ReadyType"), "{json}");
    }

    #[test]
    fn addresses() {
        let a = "1 Main St, Springfield, IL, 62704, us";
        assert_eq!(address_part(a, IdField::City).as_deref(), Some("Springfield"));
        assert_eq!(address_part(a, IdField::Postal).as_deref(), Some("62704"));
        assert_eq!(address_part("5 Kirkegata, Oslo, 0153 Oslo, no", IdField::Postal).as_deref(), Some("0153"));
        assert_eq!(address_part("1 Main St, Springfield, IL 62704, us", IdField::Region).as_deref(), Some("IL"));
    }
}
