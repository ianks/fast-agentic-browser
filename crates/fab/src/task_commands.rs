//! Task control commands. This layer only reads and changes the durable
//! journal; resuming browser work is explicitly delegated to the caller.

use crate::task_store::{
    Checkpoint, EffectKind, EffectId, EffectState, OutputRecord, Resolution, Store, TaskId, TaskRecord,
    TaskState,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum TaskCommand {
    List {
        session: Option<String>,
    },
    Show {
        task: TaskId,
    },
    Output {
        task: TaskId,
        after: Option<u64>,
    },
    Cancel {
        task: TaskId,
        reason: String,
    },
    Resolve {
        task: TaskId,
        effect: EffectId,
        resolution: ResolutionCommand,
    },
    Resume {
        task: TaskId,
        #[serde(default)]
        adopt_page: bool,
    },
}

impl TaskCommand {
    /// The target to check against the authenticated session before dispatch.
    pub fn task_id(&self) -> Option<&TaskId> {
        match self {
            Self::List { .. } => None,
            Self::Show { task }
            | Self::Output { task, .. }
            | Self::Cancel { task, .. }
            | Self::Resolve { task, .. }
            | Self::Resume { task, .. } => Some(task),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResolutionCommand {
    Applied { reason: String, result: TaskReply },
    NotApplied { reason: String },
    ProgramApplied { reason: String, result: Value },
    /// Resolve an uncertain `items` or `next page` request from what the
    /// page it ran on shows now (needs the session's browser).
    Observed { reason: String },
}

/// What an applied whole call is asserted to have answered: the `end`
/// value it would have given (and optionally its report).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskReply {
    #[serde(default)]
    pub text: String,
    pub ok: bool,
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub cost: f64,
}

impl TaskReply {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.cost.is_finite() && self.cost >= 0.0,
            "reply cost must be finite and nonnegative"
        );
        Ok(())
    }
    pub fn as_value(&self) -> Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }
    /// The `end` event the call would have committed.
    pub fn end(&self) -> Result<Value> {
        let reply = crate::api::Reply { text: self.text.clone(), ok: self.ok, turns: self.turns, cost: self.cost, value: self.value.clone(), ..Default::default() };
        let mut end = serde_json::to_value(reply.end())?;
        end["t"] = serde_json::json!("end");
        Ok(end)
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum TaskCommandOutcome {
    List(Vec<TaskRecord>),
    Show(TaskRecord),
    Output(Vec<OutputRecord>),
    Cancelled(TaskRecord),
    /// The task had already finished (completed, failed or cancelled)
    /// before the cancellation could apply; nothing changed.
    AlreadyFinished(TaskRecord),
    Resolved(TaskRecord),
    /// The caller must acquire a fresh guard and run the browser task.
    Resume { record: TaskRecord, adopt_page: bool },
    /// The caller must observe the task's page and resolve `effect` from it
    /// (`durable_program::resolve_observed`); nothing changed yet.
    Observe { record: TaskRecord, effect: EffectId, reason: String },
}

impl TaskCommandOutcome {
    /// The `end` of an outcome about one task: its record as the value, or
    /// why nothing changed. Listings and output streams are built by callers.
    pub fn end(&self) -> Result<Option<crate::events::End>> {
        use crate::events::{End, ErrorCode, Failure};
        Ok(match self {
            Self::Show(t) | Self::Cancelled(t) | Self::Resolved(t) | Self::Resume { record: t, .. } => Some(End::ok(serde_json::to_value(t)?)),
            Self::AlreadyFinished(t) => Some(End {
                value: serde_json::to_value(t)?,
                ..End::fail(Failure::new(ErrorCode::InvalidArgs, format!("task {} had already finished; nothing was cancelled", t.id)))
            }),
            Self::Observe { .. } => Some(End::fail(Failure::new(ErrorCode::InvalidArgs, "resolving from the page needs the running session's browser; nothing was resolved"))),
            Self::List(_) | Self::Output(_) => None,
        })
    }
}

fn show_recovered(store: &Store, task: &TaskId) -> Result<TaskRecord> {
    let current = store.get(task)?;
    if matches!(current.state, TaskState::Running) {
        if let Some(guard) = store.try_acquire(task)? {
            return store.recover_interrupted(&guard);
        }
        // The owner still holds the lock; retain the live Running state.
        return store.get(task);
    }
    Ok(current)
}

/// Execute one task command within the authenticated session's scope.
/// `Resume` only returns a record; it never starts browser work here.
pub fn execute_in_session(
    store: &Store,
    session: &str,
    command: TaskCommand,
) -> Result<TaskCommandOutcome> {
    ensure!(!session.trim().is_empty(), "session must not be empty");
    if let Some(id) = command.task_id() {
        let task = store.get(id)?;
        ensure!(
            task.session == session,
            crate::events::invalid("task does not belong to this session")
        );
    }
    match command {
        TaskCommand::List { session: requested } => {
            ensure!(
                requested.as_deref().is_none_or(|s| s == session),
                "cannot list another session's tasks"
            );
            let records = store
                .list(Some(session))?
                .into_iter()
                .map(|record| show_recovered(store, &record.id))
                .collect::<Result<Vec<_>>>()?;
            Ok(TaskCommandOutcome::List(records))
        }
        TaskCommand::Show { task } => Ok(TaskCommandOutcome::Show(show_recovered(store, &task)?)),
        TaskCommand::Output { task, after } => {
            // Its state is read next: a dead runner's task shows as interrupted.
            show_recovered(store, &task)?;
            // `after` is a record cursor: the task's `end` always comes back.
            let all = store.output(&task, None)?;
            Ok(TaskCommandOutcome::Output(all.into_iter().filter(|o| o.value["t"] == "end" || after.is_none_or(|a| o.sequence > a)).collect()))
        }
        TaskCommand::Cancel { task, reason } => {
            ensure!(!reason.trim().is_empty(), crate::events::invalid("cancellation requires a reason"));
            let guard = store.acquire(&task)?;
            let current = store.get(&task)?;
            if matches!(current.state, TaskState::Finished { .. }) {
                return Ok(TaskCommandOutcome::AlreadyFinished(current));
            }
            Ok(TaskCommandOutcome::Cancelled(store.cancel(
                &guard,
                current.revision,
                &reason,
            )?))
        }
        TaskCommand::Resolve {
            task,
            effect,
            resolution,
        } => {
            ensure!(effect.task == task, crate::events::invalid("effect belongs to a different task"));
            let guard = store.acquire(&task)?;
            let current = store.recover_interrupted(&guard)?;
            let pending = current
                .pending_effect
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("task has no pending effect"))?;
            ensure!(pending.id == effect, crate::events::invalid("effect is not pending for this task"));
            let resolution = match resolution {
                ResolutionCommand::Applied { reason, result } => {
                    ensure!(!reason.trim().is_empty(), crate::events::invalid("resolution requires a reason"));
                    // For a planner call, `result` is still the whole call's
                    // reply: planner loops do not resume from a checkpoint.
                    ensure!(
                        matches!(pending.kind, EffectKind::ToolCall | EffectKind::PlannerCall),
                        "applied resolution requires a whole-call or planner-call effect"
                    );
                    ensure!(
                        matches!(pending.state, EffectState::Uncertain { .. }),
                        "effect was not dispatched"
                    );
                    let value = result.as_value()?;
                    Resolution::Applied {
                        assertion: reason,
                        receipt: value.clone(),
                        checkpoint: Checkpoint::new(json!({"completed": value})),
                        output: Some(result.end()?),
                    }
                }
                ResolutionCommand::ProgramApplied { reason, result } => {
                    ensure!(!reason.trim().is_empty(), crate::events::invalid("resolution requires a reason"));
                    ensure!(
                        matches!(pending.state, EffectState::Uncertain { .. }),
                        "effect was not dispatched"
                    );
                    let (checkpoint, receipt) = match pending.kind {
                        EffectKind::ProgramRequest => crate::durable_program::resolve_value(&current, &result)?,
                        EffectKind::ProgramNavigation => crate::durable_program::resolve_navigation(&current, &result)?,
                        EffectKind::ProgramReturn => crate::durable_program::resolve_return(&current, &result)?,
                        EffectKind::ToolCall | EffectKind::PlannerCall => anyhow::bail!("not a program effect"),
                    };
                    Resolution::Applied { assertion: reason, receipt, checkpoint, output: None }
                }
                ResolutionCommand::Observed { reason } => {
                    ensure!(!reason.trim().is_empty(), crate::events::invalid("resolution requires a reason"));
                    ensure!(pending.kind == EffectKind::ProgramRequest, "only a program request can be resolved from the page");
                    ensure!(matches!(pending.state, EffectState::Uncertain { .. }), "effect was not dispatched");
                    return Ok(TaskCommandOutcome::Observe { record: current.clone(), effect, reason });
                }
                ResolutionCommand::NotApplied { reason } => {
                    ensure!(!reason.trim().is_empty(), crate::events::invalid("resolution requires a reason"));
                    Resolution::NotApplied { assertion: reason }
                }
            };
            Ok(TaskCommandOutcome::Resolved(store.resolve(
                &guard,
                current.revision,
                &effect,
                resolution,
            )?))
        }
        TaskCommand::Resume { task, adopt_page } => {
            let guard = store.acquire(&task)?;
            let current = store.recover_interrupted(&guard)?;
            ensure!(
                matches!(
                    current.state,
                    TaskState::Queued | TaskState::Paused { .. } | TaskState::Interrupted { .. }
                ),
                "task cannot resume from its current state"
            );
            ensure!(
                current.pending_effect.is_none(),
                "pending effect requires reconciliation before resume"
            );
            Ok(TaskCommandOutcome::Resume { record: current, adopt_page })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TestDb(PathBuf);
    impl TestDb {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("fab-task-command-{}", TaskId::new().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn store(&self) -> Result<Store> {
            Store::open(self.0.join("tasks.sqlite3"))
        }
    }
    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn commands_enforce_session_scope_and_resume_is_a_handoff() -> Result<()> {
        let db = TestDb::new();
        let store = db.store()?;
        let task = store.submit("session-a", &json!({"goal":"read"}), None)?;
        assert!(
            execute_in_session(
                &store,
                "session-b",
                TaskCommand::Show {
                    task: task.id.clone()
                }
            )
            .is_err()
        );
        assert!(
            execute_in_session(
                &store,
                "session-b",
                TaskCommand::List {
                    session: Some("session-a".into())
                }
            )
            .is_err()
        );
        let out = execute_in_session(
            &store,
            "session-a",
            TaskCommand::Resume {
                task: task.id.clone(), adopt_page: false,
            },
        )?;
        assert!(matches!(
            out,
            TaskCommandOutcome::Resume { record: TaskRecord { state: TaskState::Queued, .. }, adopt_page: false }
        ));
        assert!(matches!(store.get(&task.id)?.state, TaskState::Queued));
        Ok(())
    }

    #[test]
    fn cancelling_a_finished_task_changes_nothing() -> Result<()> {
        let db = TestDb::new();
        let store = db.store()?;
        let task = store.submit("session", &json!({"goal": "read"}), None)?;
        let guard = store.acquire(&task.id)?;
        let running = store.resume(&guard, task.revision)?;
        let done = store.finish(&guard, running.revision, crate::task_store::TaskOutcome::Completed(json!({"ok": true})))?;
        drop(guard);
        let out = execute_in_session(&store, "session", TaskCommand::Cancel { task: task.id.clone(), reason: "too late".into() })?;
        let TaskCommandOutcome::AlreadyFinished(record) = &out else { panic!("expected already finished") };
        assert_eq!(record.revision, done.revision);
        assert!(!out.end()?.unwrap().ok);
        Ok(())
    }

    #[test]
    fn applied_resolution_derives_checkpoint_and_outbox() -> Result<()> {
        let db = TestDb::new();
        let store = db.store()?;
        let task = store.submit("session", &json!({"goal":"click"}), None)?;
        let guard = store.acquire(&task.id)?;
        let running = store.resume(&guard, task.revision)?;
        let prepared = store.begin_effect(
            &guard,
            running.revision,
            EffectKind::ToolCall,
            &json!({"tool":"do","args":{"step":"click"}}),
        )?;
        let dispatched = store.mark_dispatched(&guard, &prepared)?;
        let interrupted = store.interrupt(&guard, dispatched.revision(), "lost reply")?;
        drop(guard);
        let reply = TaskReply {
            text: "done".into(),
            ok: true,
            value: json!("answer"),
            turns: 2,
            cost: 0.1,
        };
        let out = execute_in_session(
            &store,
            "session",
            TaskCommand::Resolve {
                task: task.id.clone(),
                effect: dispatched.id().clone(),
                resolution: ResolutionCommand::Applied {
                    reason: "verified result".into(),
                    result: reply.clone(),
                },
            },
        )?;
        let TaskCommandOutcome::Resolved(resolved) = out else {
            panic!("expected resolved task")
        };
        assert_eq!(resolved.revision, interrupted.revision + 1);
        assert_eq!(
            resolved.checkpoint.unwrap().payload(),
            &json!({"completed": reply})
        );
        let end = &store.output(&task.id, None)?[0].value;
        assert_eq!((&end["t"], &end["ok"], &end["value"]), (&json!("end"), &json!(true), &json!("answer")));
        Ok(())
    }
}
