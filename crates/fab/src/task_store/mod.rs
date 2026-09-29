//! Same-machine task journal. Browser effects are write-ahead recorded; a lost
//! response is uncertainty, never permission to repeat an action.
//!
//! JSON at this boundary must already be sanitized by the application. The
//! store deliberately cannot serialize live capabilities or resolve secrets.

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Mutex,
};

const SCHEMA: i64 = 1;
const CHECKPOINT_SCHEMA: u32 = 1;
const ENGINE: &str = env!("CARGO_PKG_VERSION");

pub use fab_core::domain::TaskId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectId {
    pub task: TaskId,
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "CheckpointDto")]
pub struct Checkpoint {
    schema: u32,
    engine_version: String,
    payload: Value,
}
#[derive(Deserialize)]
struct CheckpointDto {
    schema: u32,
    engine_version: String,
    payload: Value,
}
impl TryFrom<CheckpointDto> for Checkpoint {
    type Error = String;
    fn try_from(d: CheckpointDto) -> std::result::Result<Self, String> {
        if d.schema != CHECKPOINT_SCHEMA || d.engine_version != ENGINE {
            return Err(format!(
                "unsupported checkpoint schema {} / engine {}",
                d.schema, d.engine_version
            ));
        }
        Ok(Self {
            schema: d.schema,
            engine_version: d.engine_version,
            payload: d.payload,
        })
    }
}
impl Checkpoint {
    pub fn new(payload: Value) -> Self {
        Self {
            schema: CHECKPOINT_SCHEMA,
            engine_version: ENGINE.into(),
            payload,
        }
    }
    pub fn payload(&self) -> &Value {
        &self.payload
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Interrupted { reason: String },
    Paused { reason: String },
    Finished { outcome: TaskOutcome },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "result", rename_all = "snake_case")]
pub enum TaskOutcome {
    Completed(Value),
    Failed(String),
    Cancelled(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EffectState {
    Prepared,
    Dispatched,
    Confirmed { receipt: Receipt },
    Uncertain { reason: String },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Receipt {
    Applied {
        value: Value,
        assertion: Option<String>,
    },
    NotApplied {
        assertion: String,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    ToolCall,
    ProgramRequest,
    ProgramNavigation,
    /// A run's return from an open detail page to its list page, so an
    /// adopted page can continue the list: navigation only, never a mutation.
    ProgramReturn,
    /// One tool call of a `do`/`step` planner loop (`{tool, args}`), after
    /// the whole-call effect confirmed the work before the planner.
    PlannerCall,
}
impl EffectKind {
    /// Effects whose continuation is a saved program, reconciled with
    /// program evidence rather than a whole-call reply.
    pub fn is_program(self) -> bool {
        !matches!(self, Self::ToolCall | Self::PlannerCall)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectRecord {
    pub id: EffectId,
    pub kind: EffectKind,
    pub request: Value,
    pub state: EffectState,
    /// The task's next output position when the effect began: outputs past
    /// it were committed while the effect ran.
    #[serde(default)]
    pub outputs_from: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub session: String,
    pub revision: u64,
    pub state: TaskState,
    pub request: Value,
    pub checkpoint: Option<Checkpoint>,
    pub pending_effect: Option<EffectRecord>,
    next_effect: u64,
    next_output: u64,
}

impl TaskRecord {
    /// The outbox position the next output takes.
    pub fn next_output(&self) -> u64 {
        self.next_output
    }
}

/// Nonserializable correlation token. A transition invalidates older tokens.
#[derive(Debug, Clone)]
pub struct PendingEffect {
    id: EffectId,
    revision: u64,
}
impl PendingEffect {
    pub fn id(&self) -> &EffectId {
        &self.id
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputRecord {
    pub task: TaskId,
    pub sequence: u64,
    pub value: Value,
}

pub enum Resolution {
    Applied {
        assertion: String,
        receipt: Value,
        checkpoint: Checkpoint,
        output: Option<Value>,
    },
    NotApplied {
        assertion: String,
    },
}

/// The OS releases this lock after a crash. No timeout permits a second owner.
pub struct TaskGuard {
    task: TaskId,
    database: PathBuf,
    _file: File,
}
impl TaskGuard {
    pub fn task_id(&self) -> &TaskId {
        &self.task
    }
}

/// One process owns a session runtime until this guard is dropped or the
/// process exits. A task lock still protects each journal transition.
pub struct SessionGuard {
    session: String,
    database: PathBuf,
    _file: File,
}
impl SessionGuard {
    pub fn session(&self) -> &str {
        &self.session
    }
    pub fn database(&self) -> &Path {
        &self.database
    }
}

pub struct Store {
    database: PathBuf,
    conn: Mutex<Connection>,
}

impl Store {
    pub fn default_path() -> Result<PathBuf> {
        let root = fab_core::paths::state_dir()
            .context("HOME or FAB_STATE_DIR is required for durable tasks")?;
        Ok(root.join("tasks.sqlite3"))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        ensure!(
            rusqlite::version_number() >= 3_051_003,
            "SQLite 3.51.3 or newer is required for durable WAL storage"
        );
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        drop(options.open(path)?);
        let database = std::fs::canonicalize(path)?;
        let mut conn = Connection::open(&database)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= SCHEMA,
            "task database schema {version} is newer than supported {SCHEMA}"
        );
        let journal: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        if journal != "wal" {
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
          id TEXT PRIMARY KEY, session TEXT NOT NULL, request_id TEXT,
          revision INTEGER NOT NULL, body TEXT NOT NULL,
          UNIQUE(session, request_id));
          CREATE TABLE IF NOT EXISTS effects (
          task TEXT NOT NULL REFERENCES tasks(id), sequence INTEGER NOT NULL,
          body TEXT NOT NULL, PRIMARY KEY(task, sequence));
          CREATE TABLE IF NOT EXISTS outputs (
          task TEXT NOT NULL REFERENCES tasks(id), sequence INTEGER NOT NULL,
          value TEXT NOT NULL, PRIMARY KEY(task, sequence));",
        )?;
        tx.pragma_update(None, "user_version", SCHEMA)?;
        tx.commit()?;
        Ok(Self {
            database,
            conn: Mutex::new(conn),
        })
    }

    pub fn submit(
        &self,
        session: &str,
        request: &Value,
        request_id: Option<&str>,
    ) -> Result<TaskRecord> {
        ensure!(!session.trim().is_empty(), "session must not be empty");
        ensure!(
            request_id.is_none_or(|id| !id.trim().is_empty()),
            "request id must not be empty"
        );
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("task database mutex poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(key) = request_id {
            let body: Option<String> = tx
                .query_row(
                    "SELECT body FROM tasks WHERE session=?1 AND request_id=?2",
                    params![session, key],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(body) = body {
                let task = decode(&body)?;
                ensure!(
                    task.request == *request,
                    crate::events::invalid("request id already belongs to a different request")
                );
                return Ok(task);
            }
        }
        let task = TaskRecord {
            id: TaskId::new().map_err(|e| anyhow::anyhow!("task identity entropy: {e}"))?,
            session: session.into(),
            revision: 0,
            state: TaskState::Queued,
            request: request.clone(),
            checkpoint: None,
            pending_effect: None,
            next_effect: 0,
            next_output: 0,
        };
        tx.execute(
            "INSERT INTO tasks(id, session, request_id, revision, body) VALUES(?1,?2,?3,0,?4)",
            params![
                task.id.as_str(),
                session,
                request_id,
                serde_json::to_string(&task)?
            ],
        )?;
        tx.commit()?;
        Ok(task)
    }

    pub fn get(&self, task: &TaskId) -> Result<TaskRecord> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("task database mutex poisoned"))?;
        read_task(&conn, task)
    }

    pub fn list(&self, session: Option<&str>) -> Result<Vec<TaskRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("task database mutex poisoned"))?;
        let mut stmt =
            conn.prepare("SELECT body FROM tasks WHERE (?1 IS NULL OR session=?1) ORDER BY rowid")?;
        stmt.query_map([session], |r| r.get::<_, String>(0))?
            .map(|r| decode(&r?))
            .collect()
    }

    pub fn acquire(&self, task: &TaskId) -> Result<TaskGuard> {
        self.try_acquire(task)?
            .ok_or_else(|| crate::events::invalid(format!("task {task} is still running; read it with `fab tasks output {task}` when it ends")).into())
    }

    /// Distinguish a live owner's lock from I/O failures. A `None` result
    /// means another process currently owns this task.
    pub fn try_acquire(&self, task: &TaskId) -> Result<Option<TaskGuard>> {
        self.get(task)?;
        let directory = self.database.with_extension("locks");
        std::fs::create_dir_all(&directory)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(directory.join(task.as_str()))?;
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        Ok(Some(TaskGuard {
            task: task.clone(),
            database: self.database.clone(),
            _file: file,
        }))
    }

    /// Hold this guard for the lifetime of a browser session runner. The
    /// hexadecimal name is an injective encoding of the scope bytes, so two
    /// distinct scopes never share a lock file or escape the lock directory.
    pub fn acquire_session(&self, session: &str) -> Result<SessionGuard> {
        ensure!(!session.trim().is_empty(), "session must not be empty");
        ensure!(
            session.len() <= 120,
            "session scope is too long for a lock name"
        );
        let directory = self.database.with_extension("session-locks");
        std::fs::create_dir_all(&directory)?;
        let mut name = String::with_capacity(session.len() * 2);
        for byte in session.bytes() {
            use std::fmt::Write;
            write!(&mut name, "{byte:02x}")?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(directory.join(name))?;
        file.try_lock_exclusive()
            .context("session is already owned by another runner")?;
        Ok(SessionGuard {
            session: session.into(),
            database: self.database.clone(),
            _file: file,
        })
    }

    fn update<T>(
        &self,
        guard: &TaskGuard,
        revision: u64,
        f: impl FnOnce(&rusqlite::Transaction<'_>, &mut TaskRecord) -> Result<T>,
    ) -> Result<(TaskRecord, T)> {
        ensure!(
            guard.database == self.database,
            "task guard belongs to a different database"
        );
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("task database mutex poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut task = read_task(&tx, &guard.task)?;
        ensure!(
            task.revision == revision,
            "stale task revision: expected {revision}, found {}",
            task.revision
        );
        let result = f(&tx, &mut task)?;
        task.revision = revision.checked_add(1).context("task revision exhausted")?;
        ensure!(task.revision <= i64::MAX as u64, "task revision exhausted");
        ensure!(
            tx.execute(
                "UPDATE tasks SET revision=?1, body=?2 WHERE id=?3 AND revision=?4",
                params![
                    task.revision as i64,
                    serde_json::to_string(&task)?,
                    task.id.as_str(),
                    revision as i64
                ]
            )? == 1,
            "concurrent task transition"
        );
        tx.commit()?;
        Ok((task, result))
    }

    pub fn resume(&self, guard: &TaskGuard, revision: u64) -> Result<TaskRecord> {
        Ok(self
            .update(guard, revision, |_, task| {
                ensure!(
                    matches!(
                        task.state,
                        TaskState::Queued
                            | TaskState::Interrupted { .. }
                            | TaskState::Paused { .. }
                    ),
                    "task cannot resume from {:?}",
                    task.state
                );
                ensure!(
                    task.pending_effect.is_none(),
                    "pending effect requires reconciliation before resume"
                );
                task.state = TaskState::Running;
                Ok(())
            })?
            .0)
    }

    pub fn begin_effect(
        &self,
        guard: &TaskGuard,
        revision: u64,
        kind: EffectKind,
        request: &Value,
    ) -> Result<PendingEffect> {
        let (task, id) = self.update(guard, revision, |tx, task| {
            ensure!(
                matches!(task.state, TaskState::Running),
                "effects require a running task"
            );
            ensure!(
                task.pending_effect.is_none(),
                "task already has a pending effect"
            );
            let id = EffectId {
                task: task.id.clone(),
                sequence: task.next_effect,
            };
            task.next_effect = task
                .next_effect
                .checked_add(1)
                .context("effect sequence exhausted")?;
            let effect = EffectRecord {
                id: id.clone(),
                kind,
                request: request.clone(),
                state: EffectState::Prepared,
                outputs_from: task.next_output,
            };
            write_effect(tx, &effect)?;
            task.pending_effect = Some(effect);
            Ok(id)
        })?;
        Ok(PendingEffect {
            id,
            revision: task.revision,
        })
    }

    pub fn mark_dispatched(
        &self,
        guard: &TaskGuard,
        pending: &PendingEffect,
    ) -> Result<PendingEffect> {
        let (task, ()) = self.update(guard, pending.revision, |tx, task| {
            ensure!(
                matches!(task.state, TaskState::Running),
                "dispatch requires a running task"
            );
            let effect = expected_effect(task, &pending.id)?;
            ensure!(
                effect.state == EffectState::Prepared,
                "effect is not prepared"
            );
            effect.state = EffectState::Dispatched;
            write_effect(tx, effect)
        })?;
        Ok(PendingEffect {
            id: pending.id.clone(),
            revision: task.revision,
        })
    }

    pub fn complete_effect(
        &self,
        guard: &TaskGuard,
        pending: &PendingEffect,
        receipt: &Value,
        checkpoint: Checkpoint,
        output: Option<Value>,
    ) -> Result<TaskRecord> {
        Ok(self
            .update(guard, pending.revision, |tx, task| {
                ensure!(
                    matches!(task.state, TaskState::Running),
                    "completion requires a running task"
                );
                let effect = expected_effect(task, &pending.id)?;
                ensure!(
                    effect.state == EffectState::Dispatched,
                    "effect was not dispatched"
                );
                effect.state = EffectState::Confirmed {
                    receipt: Receipt::Applied {
                        value: receipt.clone(),
                        assertion: None,
                    },
                };
                write_effect(tx, effect)?;
                task.pending_effect = None;
                task.checkpoint = Some(checkpoint);
                append_output(tx, task, output)
            })?
            .0)
    }

    /// Commits an output while `pending` is in flight: a whole-call effect
    /// streams its records as it runs. Returns the output's position and the
    /// effect at the task's new revision.
    pub fn append_output(
        &self,
        guard: &TaskGuard,
        pending: &PendingEffect,
        value: Value,
    ) -> Result<(PendingEffect, u64)> {
        let (task, seq) = self.update(guard, pending.revision, |tx, task| {
            ensure!(
                matches!(task.state, TaskState::Running),
                "output requires a running task"
            );
            ensure!(
                expected_effect(task, &pending.id)?.state == EffectState::Dispatched,
                "effect was not dispatched"
            );
            let seq = task.next_output;
            append_output(tx, task, Some(value))?;
            Ok(seq)
        })?;
        Ok((
            PendingEffect {
                id: pending.id.clone(),
                revision: task.revision,
            },
            seq,
        ))
    }

    /// Pure computation/output advances are committed in the same transaction.
    pub fn checkpoint(
        &self,
        guard: &TaskGuard,
        revision: u64,
        checkpoint: Checkpoint,
        output: Option<Value>,
    ) -> Result<TaskRecord> {
        Ok(self
            .update(guard, revision, |tx, task| {
                ensure!(
                    matches!(task.state, TaskState::Running) && task.pending_effect.is_none(),
                    "checkpoint requires an idle running task"
                );
                task.checkpoint = Some(checkpoint);
                append_output(tx, task, output)
            })?
            .0)
    }

    pub fn interrupt(&self, guard: &TaskGuard, revision: u64, reason: &str) -> Result<TaskRecord> {
        self.stop(
            guard,
            revision,
            TaskState::Interrupted {
                reason: reason.into(),
            },
            reason,
        )
    }
    pub fn pause(&self, guard: &TaskGuard, revision: u64, reason: &str) -> Result<TaskRecord> {
        self.stop(
            guard,
            revision,
            TaskState::Paused {
                reason: reason.into(),
            },
            reason,
        )
    }
    pub fn cancel(&self, guard: &TaskGuard, revision: u64, reason: &str) -> Result<TaskRecord> {
        self.stop(
            guard,
            revision,
            TaskState::Finished {
                outcome: TaskOutcome::Cancelled(reason.into()),
            },
            reason,
        )
    }
    fn stop(
        &self,
        guard: &TaskGuard,
        revision: u64,
        state: TaskState,
        reason: &str,
    ) -> Result<TaskRecord> {
        Ok(self
            .update(guard, revision, |tx, task| {
                ensure!(
                    !matches!(task.state, TaskState::Finished { .. }),
                    "task is already finished"
                );
                // Cancellation is terminal, but the effect remains in the
                // journal until evidence resolves whether it happened.
                if let Some(effect) = &mut task.pending_effect {
                    if effect.state == EffectState::Dispatched {
                        effect.state = EffectState::Uncertain {
                            reason: reason.into(),
                        };
                        write_effect(tx, effect)?;
                    }
                }
                task.state = state;
                Ok(())
            })?
            .0)
    }

    /// Called only after the previous owner is gone, as proven by `guard`.
    /// Does not resume, touch a browser, or dispatch an effect.
    pub fn recover_interrupted(&self, guard: &TaskGuard) -> Result<TaskRecord> {
        let mut task = self.get(&guard.task)?;
        if matches!(task.state, TaskState::Running) {
            task = self.interrupt(guard, task.revision, "previous runner stopped")?;
        }
        if let Some(effect) = &task.pending_effect {
            if effect.state == EffectState::Prepared && matches!(task.state, TaskState::Interrupted { .. } | TaskState::Paused { .. }) {
                return self.resolve(guard, task.revision, &effect.id, Resolution::NotApplied { assertion: "the durable dispatch marker was never committed".into() });
            }
        }
        Ok(task)
    }

    pub fn resolve(
        &self,
        guard: &TaskGuard,
        revision: u64,
        effect_id: &EffectId,
        resolution: Resolution,
    ) -> Result<TaskRecord> {
        Ok(self
            .update(guard, revision, |tx, task| {
                let was_cancelled = matches!(
                    task.state,
                    TaskState::Finished {
                        outcome: TaskOutcome::Cancelled(_)
                    }
                );
                ensure!(
                    matches!(
                        task.state,
                        TaskState::Paused { .. } | TaskState::Interrupted { .. }
                    ) || was_cancelled,
                    "resolution requires a paused, interrupted, or cancelled task"
                );
                let next_output = task.next_output;
                let effect = expected_effect(task, effect_id)?;
                let was_prepared = effect.state == EffectState::Prepared;
                ensure!(
                    was_prepared || matches!(effect.state, EffectState::Uncertain { .. }),
                    "effect is not awaiting reconciliation"
                );
                let (receipt, checkpoint, output) = match resolution {
                    Resolution::Applied {
                        assertion,
                        receipt,
                        checkpoint,
                        output,
                    } => {
                        ensure!(
                            !was_prepared,
                            "an undispatched effect cannot be resolved as applied"
                        );
                        ensure!(
                            !assertion.trim().is_empty(),
                            "resolution requires evidence or an explicit assertion"
                        );
                        (
                            Receipt::Applied {
                                value: receipt,
                                assertion: Some(assertion),
                            },
                            Some(checkpoint),
                            output,
                        )
                    }
                    Resolution::NotApplied { assertion } => {
                        ensure!(
                            !assertion.trim().is_empty(),
                            "resolution requires evidence or an explicit assertion"
                        );
                        // Records it committed prove it ran, at least in part.
                        ensure!(
                            next_output <= effect.outputs_from,
                            crate::events::invalid(format!("records were committed while this effect ran (outputs {}..{next_output}), so it ran at least in part; resolve it applied", effect.outputs_from))
                        );
                        (Receipt::NotApplied { assertion }, None, None)
                    }
                };
                effect.state = EffectState::Confirmed { receipt };
                write_effect(tx, effect)?;
                task.pending_effect = None;
                if let Some(cp) = checkpoint {
                    task.checkpoint = Some(cp);
                }
                append_output(tx, task, output)?;
                // Queued means never started: a reconciled task waits for an
                // explicit resume, so a duplicate submission cannot continue it.
                if !was_cancelled {
                    task.state = TaskState::Paused {
                        reason: "effect reconciled; resume explicitly to continue".into(),
                    };
                }
                Ok(())
            })?
            .0)
    }

    pub fn finish(
        &self,
        guard: &TaskGuard,
        revision: u64,
        outcome: TaskOutcome,
    ) -> Result<TaskRecord> {
        Ok(self
            .update(guard, revision, |_, task| {
                ensure!(
                    matches!(task.state, TaskState::Running) && task.pending_effect.is_none(),
                    "finish requires an idle running task"
                );
                task.state = TaskState::Finished { outcome };
                Ok(())
            })?
            .0)
    }

    /// `after` is an exclusive cursor. Delivery acknowledgments are not stored.
    pub fn output(&self, task: &TaskId, after: Option<u64>) -> Result<Vec<OutputRecord>> {
        self.get(task)?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("task database mutex poisoned"))?;
        let mut stmt = conn.prepare("SELECT sequence,value FROM outputs WHERE task=?1 AND (?2 IS NULL OR sequence>?2) ORDER BY sequence")?;
        let after = after.map(|cursor| i64::try_from(cursor).unwrap_or(i64::MAX));
        stmt.query_map(params![task.as_str(), after], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (sequence, value) = row?;
            Ok(OutputRecord {
                task: task.clone(),
                sequence: u64::try_from(sequence).context("negative output sequence")?,
                value: serde_json::from_str(&value)?,
            })
        })
        .collect()
    }

    pub fn effects(&self, task: &TaskId) -> Result<Vec<EffectRecord>> {
        self.get(task)?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("task database mutex poisoned"))?;
        let mut stmt = conn.prepare("SELECT body FROM effects WHERE task=?1 ORDER BY sequence")?;
        stmt.query_map([task.as_str()], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect()
    }
}

fn decode(body: &str) -> Result<TaskRecord> {
    let task: TaskRecord =
        serde_json::from_str(body).context("invalid or incompatible task checkpoint")?;
    ensure!(!task.session.trim().is_empty(), "task has an empty session");
    ensure!(
        task.revision <= i64::MAX as u64,
        "task revision exceeds SQLite range"
    );
    ensure!(
        task.next_effect <= i64::MAX as u64 + 1,
        "effect sequence exceeds SQLite range"
    );
    ensure!(
        task.next_output <= i64::MAX as u64 + 1,
        "output sequence exceeds SQLite range"
    );
    if let Some(effect) = &task.pending_effect {
        ensure!(
            effect.id.task == task.id
                && effect.id.sequence.checked_add(1) == Some(task.next_effect),
            "invalid pending effect identity"
        );
        ensure!(
            matches!(
                (&task.state, &effect.state),
                (
                    TaskState::Running,
                    EffectState::Prepared | EffectState::Dispatched
                ) | (
                    TaskState::Paused { .. } | TaskState::Interrupted { .. },
                    EffectState::Prepared | EffectState::Uncertain { .. }
                ) | (
                    TaskState::Finished {
                        outcome: TaskOutcome::Cancelled(_)
                    },
                    EffectState::Prepared | EffectState::Uncertain { .. }
                )
            ),
            "pending effect is incompatible with task state"
        );
        ensure!(
            effect.kind != EffectKind::PlannerCall || effect.request["tool"].is_string(),
            "planner call effect without a tool"
        );
    }
    Ok(task)
}
fn read_task(conn: &Connection, task: &TaskId) -> Result<TaskRecord> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT revision, body FROM tasks WHERE id=?1",
            [task.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (revision, body) = row.ok_or_else(|| crate::events::invalid("task not found"))?;
    let revision = u64::try_from(revision).context("negative task revision")?;
    let record = decode(&body)?;
    ensure!(
        record.id == *task && record.revision == revision,
        "task journal identity or revision mismatch"
    );
    Ok(record)
}
fn expected_effect<'a>(task: &'a mut TaskRecord, id: &EffectId) -> Result<&'a mut EffectRecord> {
    ensure!(id.task == task.id, "completion belongs to a different task");
    let effect = task
        .pending_effect
        .as_mut()
        .context("task has no pending effect")?;
    ensure!(effect.id == *id, "completion belongs to a different effect");
    Ok(effect)
}
fn write_effect(tx: &rusqlite::Transaction<'_>, effect: &EffectRecord) -> Result<()> {
    let sequence = i64::try_from(effect.id.sequence).context("effect sequence exhausted")?;
    tx.execute("INSERT INTO effects(task,sequence,body) VALUES(?1,?2,?3) ON CONFLICT(task,sequence) DO UPDATE SET body=excluded.body", params![effect.id.task.as_str(), sequence, serde_json::to_string(effect)?])?;
    Ok(())
}
fn append_output(
    tx: &rusqlite::Transaction<'_>,
    task: &mut TaskRecord,
    output: Option<Value>,
) -> Result<()> {
    if let Some(value) = output {
        ensure!(
            task.next_output <= i64::MAX as u64,
            "output sequence exhausted"
        );
        tx.execute(
            "INSERT INTO outputs(task,sequence,value) VALUES(?1,?2,?3)",
            params![
                task.id.as_str(),
                task.next_output as i64,
                serde_json::to_string(&value)?
            ],
        )?;
        task.next_output = task
            .next_output
            .checked_add(1)
            .context("output sequence exhausted")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::Command;

    struct TestDb(PathBuf);
    impl TestDb {
        fn new() -> Self {
            let id = TaskId::new().unwrap();
            let path = std::env::temp_dir().join(format!("fab-task-store-{id}"));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("tasks.sqlite3")
        }
    }
    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn idempotent_submission_and_durable_outbox() -> Result<()> {
        let db = TestDb::new();
        let store = Store::open(db.path())?;
        let task = store.submit(
            "session",
            &json!({"url":"https://example.test"}),
            Some("request-1"),
        )?;
        let again = store.submit("session", &task.request, Some("request-1"))?;
        assert_eq!(task.id, again.id);
        assert!(
            store
                .submit("session", &json!({"url":"other"}), Some("request-1"))
                .is_err()
        );
        assert_eq!(store.list(Some("other"))?.len(), 0);
        assert_eq!(store.list(Some("session"))?.len(), 1);

        let guard = store.acquire(&task.id)?;
        assert!(store.acquire(&task.id).is_err());
        let running = store.resume(&guard, task.revision)?;
        let cp = Checkpoint::new(json!({"step": 1}));
        let advanced =
            store.checkpoint(&guard, running.revision, cp, Some(json!({"text":"hello"})))?;
        assert!(
            store
                .checkpoint(&guard, running.revision, Checkpoint::new(json!({})), None)
                .is_err()
        );
        assert_eq!(store.output(&task.id, None)?.len(), 1);
        assert!(store.output(&task.id, Some(0))?.is_empty());
        store.finish(
            &guard,
            advanced.revision,
            TaskOutcome::Completed(json!({"ok":true})),
        )?;
        drop(guard);
        drop(store);
        let reopened = Store::open(db.path())?;
        assert!(matches!(
            reopened.get(&task.id)?.state,
            TaskState::Finished { .. }
        ));
        assert_eq!(
            reopened.output(&task.id, None)?[0].value,
            json!({"text":"hello"})
        );
        Ok(())
    }

    #[test]
    fn uncertain_effect_requires_reconciliation_and_cannot_replay() -> Result<()> {
        let db = TestDb::new();
        let store = Store::open(db.path())?;
        let task = store.submit("session", &json!({"goal":"submit"}), None)?;
        let guard = store.acquire(&task.id)?;
        let running = store.resume(&guard, task.revision)?;
        let prepared = store.begin_effect(
            &guard,
            running.revision,
            EffectKind::ToolCall,
            &json!({"target":"submit"}),
        )?;
        assert!(
            store
                .begin_effect(&guard, prepared.revision(), EffectKind::ToolCall, &json!({}))
                .is_err()
        );
        let dispatched = store.mark_dispatched(&guard, &prepared)?;
        assert!(store.mark_dispatched(&guard, &prepared).is_err());
        drop(guard);
        drop(store);

        let reopened = Store::open(db.path())?;
        let guard = reopened.acquire(&task.id)?;
        let interrupted = reopened.recover_interrupted(&guard)?;
        assert!(matches!(interrupted.state, TaskState::Interrupted { .. }));
        assert!(matches!(
            interrupted.pending_effect.as_ref().unwrap().state,
            EffectState::Uncertain { .. }
        ));
        assert!(reopened.resume(&guard, interrupted.revision).is_err());
        assert!(
            reopened
                .resolve(
                    &guard,
                    interrupted.revision,
                    dispatched.id(),
                    Resolution::NotApplied {
                        assertion: "".into()
                    }
                )
                .is_err()
        );
        let resolved = reopened.resolve(
            &guard,
            interrupted.revision,
            dispatched.id(),
            Resolution::Applied {
                assertion: "checked destination page".into(),
                receipt: json!({"success":true}),
                checkpoint: Checkpoint::new(json!({"step":2})),
                output: Some(json!({"done":true})),
            },
        )?;
        assert!(matches!(resolved.state, TaskState::Paused { .. }), "reconciled work waits for an explicit resume");
        assert!(resolved.pending_effect.is_none());
        assert_eq!(reopened.output(&task.id, None)?.len(), 1);
        assert!(matches!(
            reopened.effects(&task.id)?[0].state,
            EffectState::Confirmed { .. }
        ));
        assert!(
            reopened
                .resolve(
                    &guard,
                    resolved.revision,
                    dispatched.id(),
                    Resolution::NotApplied {
                        assertion: "no".into()
                    }
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn concurrent_duplicate_submissions_admit_one_task() -> Result<()> {
        let db = TestDb::new();
        Store::open(db.path())?;
        let path = db.path();
        let ids = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let path = path.clone();
                    scope.spawn(move || -> Result<TaskId> {
                        let store = Store::open(&path)?;
                        Ok(store.submit("session", &json!({"goal": "pay"}), Some("once"))?.id)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Result<Vec<_>>>()
        })?;
        assert!(ids.iter().all(|id| id == &ids[0]));
        assert_eq!(Store::open(&path)?.list(Some("session"))?.len(), 1);
        Ok(())
    }

    #[test]
    fn prepared_effect_can_only_be_resolved_not_applied() -> Result<()> {
        let db = TestDb::new();
        let store = Store::open(db.path())?;
        let task = store.submit("session", &json!("request"), None)?;
        let guard = store.acquire(&task.id)?;
        let running = store.resume(&guard, task.revision)?;
        let pending = store.begin_effect(
            &guard,
            running.revision,
            EffectKind::ProgramNavigation,
            &json!("https://example.test"),
        )?;
        let paused = store.pause(&guard, pending.revision(), "shutdown")?;
        assert!(
            store
                .resolve(
                    &guard,
                    paused.revision,
                    pending.id(),
                    Resolution::Applied {
                        assertion: "claimed".into(),
                        receipt: json!(null),
                        checkpoint: Checkpoint::new(json!(null)),
                        output: None,
                    }
                )
                .is_err()
        );
        let queued = store.resolve(
            &guard,
            paused.revision,
            pending.id(),
            Resolution::NotApplied {
                assertion: "never dispatched".into(),
            },
        )?;
        assert!(matches!(queued.state, TaskState::Paused { .. }));
        assert_eq!(store.effects(&task.id)?.len(), 1);
        Ok(())
    }

    #[test]
    fn cancellation_keeps_uncertainty_and_resolution_stays_terminal() -> Result<()> {
        let db = TestDb::new();
        let store = Store::open(db.path())?;
        let task = store.submit("session", &json!({"goal":"send"}), None)?;
        let guard = store.acquire(&task.id)?;
        let running = store.resume(&guard, task.revision)?;
        let prepared = store.begin_effect(&guard, running.revision, EffectKind::ToolCall, &json!({"id":1}))?;
        let dispatched = store.mark_dispatched(&guard, &prepared)?;
        let cancelled = store.cancel(&guard, dispatched.revision(), "user cancelled")?;
        assert!(matches!(
            cancelled.state,
            TaskState::Finished {
                outcome: TaskOutcome::Cancelled(_)
            }
        ));
        assert!(matches!(
            cancelled.pending_effect.as_ref().unwrap().state,
            EffectState::Uncertain { .. }
        ));
        assert!(store.resume(&guard, cancelled.revision).is_err());
        drop(guard);
        drop(store);

        let reopened = Store::open(db.path())?;
        let guard = reopened.acquire(&task.id)?;
        let recovered = reopened.recover_interrupted(&guard)?;
        assert!(matches!(
            recovered.state,
            TaskState::Finished {
                outcome: TaskOutcome::Cancelled(_)
            }
        ));
        let resolved = reopened.resolve(
            &guard,
            recovered.revision,
            dispatched.id(),
            Resolution::Applied {
                assertion: "server confirmed the send".into(),
                receipt: json!({"sent":true}),
                checkpoint: Checkpoint::new(json!({"completed":true})),
                output: Some(json!({"sent":true})),
            },
        )?;
        assert!(matches!(
            resolved.state,
            TaskState::Finished {
                outcome: TaskOutcome::Cancelled(_)
            }
        ));
        assert!(resolved.pending_effect.is_none());
        assert!(matches!(
            reopened.effects(&task.id)?[0].state,
            EffectState::Confirmed {
                receipt: Receipt::Applied { .. }
            }
        ));
        assert_eq!(
            reopened.output(&task.id, None)?[0].value,
            json!({"sent":true})
        );
        assert!(reopened.resume(&guard, resolved.revision).is_err());
        Ok(())
    }

    #[test]
    fn newer_schema_is_rejected_without_migration() -> Result<()> {
        let db = TestDb::new();
        let conn = Connection::open(db.path())?;
        conn.pragma_update(None, "user_version", SCHEMA + 1)?;
        drop(conn);
        assert!(Store::open(db.path()).is_err());
        Ok(())
    }

    #[test]
    fn checkpoint_rejects_incompatible_engine() {
        let invalid = json!({"schema":CHECKPOINT_SCHEMA,"engine_version":"future","payload":{}});
        assert!(serde_json::from_value::<Checkpoint>(invalid).is_err());
    }

    #[test]
    fn session_locks_are_scoped_and_released() -> Result<()> {
        let db = TestDb::new();
        let first = Store::open(db.path())?;
        let second = Store::open(db.path())?;
        let a = first.acquire_session("a/b")?;
        assert_eq!(a.session(), "a/b");
        assert!(second.acquire_session("a/b").is_err());
        let b = second.acquire_session("a-b")?;
        assert_eq!(b.session(), "a-b");
        drop(a);
        let again = second.acquire_session("a/b")?;
        drop(again);
        drop(b);
        Ok(())
    }

    #[test]
    fn decode_rejects_impossible_effect_states() -> Result<()> {
        let db = TestDb::new();
        let store = Store::open(db.path())?;
        let task = store.submit("session", &json!("request"), None)?;
        let mut broken = task.clone();
        broken.next_effect = 1;
        broken.pending_effect = Some(EffectRecord {
            id: EffectId {
                task: task.id.clone(),
                sequence: 0,
            },
            kind: EffectKind::ToolCall,
            request: json!({}),
            state: EffectState::Prepared,
            outputs_from: 0,
        });
        assert!(decode(&serde_json::to_string(&broken)?).is_err()); // queued + pending
        broken.state = TaskState::Finished {
            outcome: TaskOutcome::Completed(json!(true)),
        };
        assert!(decode(&serde_json::to_string(&broken)?).is_err());
        broken.state = TaskState::Running;
        broken.pending_effect.as_mut().unwrap().state = EffectState::Uncertain {
            reason: "lost".into(),
        };
        assert!(decode(&serde_json::to_string(&broken)?).is_err());
        broken.state = TaskState::Interrupted {
            reason: "lost".into(),
        };
        broken.pending_effect.as_mut().unwrap().state = EffectState::Dispatched;
        assert!(decode(&serde_json::to_string(&broken)?).is_err());
        broken.pending_effect.as_mut().unwrap().state = EffectState::Confirmed {
            receipt: Receipt::NotApplied {
                assertion: "no".into(),
            },
        };
        assert!(decode(&serde_json::to_string(&broken)?).is_err());
        broken.state = TaskState::Running;
        let effect = broken.pending_effect.as_mut().unwrap();
        (effect.kind, effect.state) = (EffectKind::PlannerCall, EffectState::Dispatched);
        assert!(decode(&serde_json::to_string(&broken)?).is_err(), "a planner call names its tool");
        broken.pending_effect.as_mut().unwrap().request = json!({"tool": "read", "args": {}});
        decode(&serde_json::to_string(&broken)?)?;
        Ok(())
    }

    #[test]
    fn abrupt_process_exit_releases_os_locks() -> Result<()> {
        let db = TestDb::new();
        let store = Store::open(db.path())?;
        let task = store.submit("session", &json!("request"), None)?;
        let child_test = format!("{}::crash_child", module_path!().split_once("::").unwrap().1);
        let status = Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg(child_test)
            .env("FAB_STORE_CRASH_DB", db.path())
            .status()?;
        assert_eq!(status.code(), Some(17));
        let _session = store.acquire_session("session")?;
        let _task = store.acquire(&task.id)?;
        Ok(())
    }

    #[test]
    fn crash_child() {
        let Ok(path) = std::env::var("FAB_STORE_CRASH_DB") else {
            return;
        };
        let store = Store::open(path).unwrap();
        let task = store.list(Some("session")).unwrap().remove(0);
        let _session = store.acquire_session("session").unwrap();
        let _task = store.acquire(&task.id).unwrap();
        // Exit bypasses Rust destructors, like a killed runner.
        std::process::exit(17);
    }
}
