//! Durable references to generated-password workflows, so a daemon restart
//! cannot lose a save whose outcome is unknown (INTENT I08, I09).
//!
//! One small JSON file, `<state dir>/secret-workflows.json`, mode 0600: per
//! site, the origin, the username the login was saved under, the workflow's
//! [`Checkpoint`] (a random epoch, a phase, opaque references) and the
//! [`Verifier`] that proves which login is fab's own. Never the password:
//! after a restart its material comes only from the password store, and the
//! verifier only answers "is this the value I generated". Rewritten whole
//! (temporary file, fsync, rename) under an exclusive lock file, so the
//! daemons and `fab secrets reset` never lose each other's updates and
//! readers never see a torn file.
//!
//! A plain file rather than the task journal's SQLite database: the vault
//! lives in `fab-core`, which has no SQLite, and the CLI's `reset` reads it
//! without opening the journal or taking its task locks.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::secrets::verifier::Verifier;
use crate::secret_machine::{Checkpoint, Machine};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub site: String,
    pub origin: String,
    pub username: String,
    pub workflow: Checkpoint,
    /// Proof that a login in the password manager is the value fab generated
    /// ([`Verifier`]): a keyed tag, never the password and never anything from
    /// which it can be recovered. Reconciliation after a restart accepts a
    /// login only when its password matches this, so a login that was already
    /// saved for the same site and account is not taken for fab's own.
    ///
    /// Absent in a record written before verifiers existed: such a workflow
    /// cannot prove a save and stays paused until it is reset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier: Option<Verifier>,
}

impl Record {
    /// The live workflow; an in-flight save restores as reconciliation.
    pub fn machine(&self) -> Result<Machine> {
        Machine::restore(self.workflow.clone()).with_context(|| format!("the generated-password workflow recorded for {} is unreadable", self.site))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    workflows: Vec<Record>,
}

#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Journal { path: path.into() }
    }

    /// `<state dir>/secret-workflows.json` (see [`crate::paths::state_dir`]).
    pub fn in_state_dir() -> Option<Self> {
        crate::paths::state_dir().map(|d| Journal::new(d.join("secret-workflows.json")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every recorded workflow. A missing file is empty; an unreadable one is
    /// an error, never silently empty (that would permit regeneration).
    pub fn all(&self) -> Result<Vec<Record>> {
        Ok(self.read()?.workflows)
    }

    pub fn get(&self, site: &str) -> Result<Option<Record>> {
        Ok(self.all()?.into_iter().find(|r| r.site == site))
    }

    /// Records (or advances) the site's workflow. Another workflow's record
    /// (another epoch, e.g. from a second process) is never overwritten:
    /// its save may be in flight and would lose its reconciliation.
    pub fn put(&self, record: Record) -> Result<()> {
        let mut out = Ok(());
        self.update(|f| {
            let epoch = record.workflow.epoch();
            if f.workflows.iter().any(|r| r.site == record.site && r.workflow.epoch() != epoch) {
                out = Err(anyhow::anyhow!("another process holds a generated-password workflow for {}; run the step again to continue it", record.site));
                return;
            }
            f.workflows.retain(|r| r.site != record.site);
            f.workflows.push(record);
        })?;
        out
    }

    /// Forgets the site's workflow, only if it is still `epoch`'s.
    pub fn remove(&self, site: &str, epoch: [u8; 16]) -> Result<()> {
        self.update(|f| f.workflows.retain(|r| !(r.site == site && r.workflow.epoch() == epoch)))
    }

    /// Clears the site's workflow if it is paused or saved; one whose save
    /// outcome is still unknown must be reconciled first. Returns what was
    /// cleared.
    pub fn reset(&self, site: &str) -> Result<Option<Record>> {
        let mut out = Ok(None);
        self.update(|f| {
            let Some(i) = f.workflows.iter().position(|r| r.site == site) else { return };
            let reconcile = f.workflows[i].machine().map(|m| m.needs_reconcile() || m.in_flight());
            match reconcile {
                Ok(true) => {
                    out = Err(anyhow::anyhow!(
                        "the generated password for {site} may have been saved: run the step again to reconcile it with the password manager before resetting"
                    ))
                }
                // Paused, saved, or unreadable: the user asked to start over.
                _ => out = Ok(Some(f.workflows.remove(i))),
            }
        })?;
        out
    }

    fn read(&self) -> Result<File> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => serde_json::from_str(&s).with_context(|| format!("{} is unreadable", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    /// Read-modify-write under an exclusive lock, replaced atomically.
    fn update(&self, change: impl FnOnce(&mut File)) -> Result<()> {
        let dir = self.path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let lock = private(&mut std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)).open(self.path.with_extension("lock"))?;
        lock.lock()?;
        let mut f = self.read()?;
        change(&mut f);
        let tmp = self.path.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        {
            let mut out = private(&mut std::fs::OpenOptions::new().write(true).create(true).truncate(true)).open(&tmp)?;
            out.write_all(serde_json::to_string_pretty(&f)?.as_bytes())?;
            out.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        #[cfg(unix)]
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}

/// Owner-only on Unix.
fn private(o: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o
}
