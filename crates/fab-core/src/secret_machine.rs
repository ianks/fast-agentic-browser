//! Durable orchestration for a single secret fill. This module contains no
//! secret material: the executor keeps values behind opaque handles and checks
//! the target's site policy before resolving either handle.
//!
//! Persist `Checkpoint` after `issue` and before sending its request. On
//! restart, call `Machine::restore`; an in-flight save becomes reconciliation,
//! so a lost save reply can never cause another generation or blind save.

use crate::domain::TaskId;
use serde::{Deserialize, Serialize};

macro_rules! reference {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub struct $name(TaskId);
        impl $name {
            pub fn new() -> Result<Self, getrandom::Error> {
                Ok(Self(TaskId::new()?))
            }
            pub fn id(&self) -> &TaskId {
                &self.0
            }
        }
    };
}

reference!(/// A durable locator for public workflow metadata held by the executor.
IntentRef);
reference!(/// A durable locator for an observed field; it is not live page authority.
TargetRef);
reference!(/// A locator for secret material held outside the checkpoint. It carries no
/// value and does not authorize typing on its own.
SecretHandle);
reference!(/// A confirmed password-manager item, distinct from generated material.
SavedRef);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Intent {
    Existing {
        lookup: IntentRef,
        target: TargetRef,
    },
    GeneratedLogin {
        generation: IntentRef,
        save: IntentRef,
        target: TargetRef,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationId { epoch: [u8; 16], sequence: u64 }
impl OperationId {
    pub fn value(self) -> u64 {
        self.sequence
    }
}

/// A request names opaque resources only. The executor obtains values from a
/// vault or transient registry and must enforce the origin/field policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Lookup {
        op: OperationId,
        lookup: IntentRef,
    },
    Generate {
        op: OperationId,
        generation: IntentRef,
    },
    Save {
        op: OperationId,
        save: IntentRef,
        generated: SecretHandle,
    },
    ReconcileSave {
        op: OperationId,
        save: IntentRef,
        generated: SecretHandle,
    },
    Type {
        op: OperationId,
        target: TargetRef,
        source: TypeSource,
    },
}

impl Request {
    pub fn op(&self) -> OperationId {
        match self {
            Self::Lookup { op, .. }
            | Self::Generate { op, .. }
            | Self::Save { op, .. }
            | Self::ReconcileSave { op, .. }
            | Self::Type { op, .. } => *op,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeSource {
    Existing(SecretHandle),
    Saved(SavedRef),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LookupResult {
    Found(SecretHandle),
    Unavailable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerateResult {
    Ready(SecretHandle),
    Unavailable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SaveResult {
    Saved(SavedRef),
    /// The save was never dispatched to any store, so nothing can have been
    /// written: the same generated value may be saved again (never regenerated).
    NotSent,
    /// The save was dispatched but its outcome is unknown (an error or no
    /// reply after sending): only reconciliation may follow.
    LostResponse,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconcileResult {
    Found(SavedRef),
    Absent,
    Unavailable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeResult {
    Acknowledged,
    NotSent,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Completion {
    Lookup {
        op: OperationId,
        result: LookupResult,
    },
    Generate {
        op: OperationId,
        result: GenerateResult,
    },
    Save {
        op: OperationId,
        result: SaveResult,
    },
    ReconcileSave {
        op: OperationId,
        result: ReconcileResult,
    },
    Type {
        op: OperationId,
        result: TypeResult,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseReason {
    LookupUnavailable,
    GenerationUnconfirmed,
    SaveUnconfirmed,
    TypeUnconfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    ReadyLookup,
    AwaitLookup(OperationId),
    ReadyGenerate,
    AwaitGenerate(OperationId),
    ReadySave(SecretHandle),
    AwaitSave {
        op: OperationId,
        generated: SecretHandle,
    },
    ReadyReconcile(SecretHandle),
    AwaitReconcile {
        op: OperationId,
        generated: SecretHandle,
    },
    ReadyType(TypeSourceCheckpoint),
    AwaitType(OperationId),
    Paused(PauseReason),
    Done,
}

/// The only serialized representation of a type source. A generated value
/// cannot enter this enum until save or reconciliation returns a `SavedRef`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum TypeSourceCheckpoint {
    Existing(SecretHandle),
    Saved(SavedRef),
}
impl From<TypeSourceCheckpoint> for TypeSource {
    fn from(value: TypeSourceCheckpoint) -> Self {
        match value {
            TypeSourceCheckpoint::Existing(x) => Self::Existing(x),
            TypeSourceCheckpoint::Saved(x) => Self::Saved(x),
        }
    }
}

/// Safe to persist: no password, token, or generated text is represented.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    intent: Intent,
    phase: Phase,
    next_op: u64,
    epoch: [u8; 16],
}

impl Checkpoint {
    /// The workflow's random epoch: checkpoints with the same epoch are the
    /// same workflow, possibly at different phases.
    pub fn epoch(&self) -> [u8; 16] {
        self.epoch
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error("completion does not match the pending secret request")]
    UnexpectedCompletion,
    #[error("secret operation id exhausted")]
    OperationIdExhausted,
    #[error("checkpoint phase is incompatible with the secret intent")]
    InvalidCheckpoint,
    #[error("system randomness is unavailable")]
    Random,
}

pub struct Machine {
    state: Checkpoint,
}

impl Machine {
    pub fn new(intent: Intent) -> Result<Self, TransitionError> {
        let mut epoch = [0u8; 16];
        getrandom::fill(&mut epoch).map_err(|_| TransitionError::Random)?;
        let phase = match intent {
            Intent::Existing { .. } => Phase::ReadyLookup,
            Intent::GeneratedLogin { .. } => Phase::ReadyGenerate,
        };
        Ok(Self {
            state: Checkpoint { intent, phase, next_op: 1, epoch },
        })
    }

    pub fn has_saved_value(&self) -> bool {
        matches!(self.state.phase, Phase::ReadyType(TypeSourceCheckpoint::Saved(_)))
    }

    pub fn checkpoint(&self) -> Checkpoint {
        self.state.clone()
    }

    /// Never replay an uncertain save. A lost generation or typing reply also
    /// pauses because the executor cannot prove what happened from this state.
    pub fn restore(mut checkpoint: Checkpoint) -> Result<Self, TransitionError> {
        let compatible = match (&checkpoint.intent, &checkpoint.phase) {
            (
                Intent::Existing { .. },
                Phase::ReadyGenerate
                | Phase::AwaitGenerate(_)
                | Phase::ReadySave(_)
                | Phase::AwaitSave { .. }
                | Phase::ReadyReconcile(_)
                | Phase::AwaitReconcile { .. }
                | Phase::ReadyType(TypeSourceCheckpoint::Saved(_)),
            ) => false,
            (
                Intent::GeneratedLogin { .. },
                Phase::ReadyLookup
                | Phase::AwaitLookup(_)
                | Phase::ReadyType(TypeSourceCheckpoint::Existing(_)),
            ) => false,
            _ => true,
        };
        let outstanding_op = match &checkpoint.phase {
            Phase::AwaitLookup(op) | Phase::AwaitGenerate(op) | Phase::AwaitType(op) => Some(*op),
            Phase::AwaitSave { op, .. } | Phase::AwaitReconcile { op, .. } => Some(*op),
            _ => None,
        };
        if !compatible
            || checkpoint.next_op == 0
            || outstanding_op.is_some_and(|op| op.epoch != checkpoint.epoch || op.sequence == 0 || op.sequence >= checkpoint.next_op)
        {
            return Err(TransitionError::InvalidCheckpoint);
        }
        checkpoint.phase = match checkpoint.phase {
            Phase::AwaitLookup(_) => Phase::ReadyLookup,
            Phase::AwaitGenerate(_) => Phase::Paused(PauseReason::GenerationUnconfirmed),
            Phase::AwaitSave { generated, .. } => Phase::ReadyReconcile(generated),
            Phase::AwaitReconcile { generated, .. } => Phase::ReadyReconcile(generated),
            Phase::AwaitType(_) => Phase::Paused(PauseReason::TypeUnconfirmed),
            other => other,
        };
        Ok(Self { state: checkpoint })
    }

    pub fn paused(&self) -> Option<PauseReason> {
        match self.state.phase {
            Phase::Paused(reason) => Some(reason),
            _ => None,
        }
    }
    pub fn done(&self) -> bool {
        self.state.phase == Phase::Done
    }
    /// A request was issued and its completion has not arrived.
    pub fn in_flight(&self) -> bool {
        matches!(
            self.state.phase,
            Phase::AwaitLookup(_) | Phase::AwaitGenerate(_) | Phase::AwaitSave { .. } | Phase::AwaitReconcile { .. } | Phase::AwaitType(_)
        )
    }
    /// A save was dispatched with an unknown outcome: only reconciliation may follow.
    pub fn needs_reconcile(&self) -> bool {
        matches!(self.state.phase, Phase::ReadyReconcile(_))
    }

    /// Marks the effect in flight before returning it. Persist `checkpoint()`
    /// before executing the request. Calling `issue` again while awaiting a
    /// completion never dispatches the effect twice.
    pub fn issue(&mut self) -> Result<Option<Request>, TransitionError> {
        let op = OperationId { epoch: self.state.epoch, sequence: self.state.next_op };
        let request = match (&self.state.intent, &self.state.phase) {
            (Intent::Existing { lookup, .. }, Phase::ReadyLookup) => Request::Lookup {
                op,
                lookup: lookup.clone(),
            },
            (Intent::GeneratedLogin { generation, .. }, Phase::ReadyGenerate) => {
                Request::Generate {
                    op,
                    generation: generation.clone(),
                }
            }
            (Intent::GeneratedLogin { save, .. }, Phase::ReadySave(secret)) => Request::Save {
                op,
                save: save.clone(),
                generated: secret.clone(),
            },
            (Intent::GeneratedLogin { save, .. }, Phase::ReadyReconcile(secret)) => {
                Request::ReconcileSave {
                    op,
                    save: save.clone(),
                    generated: secret.clone(),
                }
            }
            (Intent::Existing { target, .. }, Phase::ReadyType(source))
            | (Intent::GeneratedLogin { target, .. }, Phase::ReadyType(source)) => Request::Type {
                op,
                target: target.clone(),
                source: source.clone().into(),
            },
            _ => return Ok(None),
        };
        self.state.next_op = self
            .state
            .next_op
            .checked_add(1)
            .ok_or(TransitionError::OperationIdExhausted)?;
        self.state.phase = match &request {
            Request::Lookup { .. } => Phase::AwaitLookup(op),
            Request::Generate { .. } => Phase::AwaitGenerate(op),
            Request::Save { generated, .. } => Phase::AwaitSave {
                op,
                generated: generated.clone(),
            },
            Request::ReconcileSave { generated, .. } => Phase::AwaitReconcile {
                op,
                generated: generated.clone(),
            },
            Request::Type { .. } => Phase::AwaitType(op),
        };
        Ok(Some(request))
    }

    pub fn complete(&mut self, completion: Completion) -> Result<(), TransitionError> {
        let next = match (&self.state.phase, completion) {
            (Phase::AwaitLookup(expected), Completion::Lookup { op, result })
                if *expected == op =>
            {
                match result {
                    LookupResult::Found(secret) => {
                        Phase::ReadyType(TypeSourceCheckpoint::Existing(secret))
                    }
                    LookupResult::Unavailable => Phase::Paused(PauseReason::LookupUnavailable),
                }
            }
            (Phase::AwaitGenerate(expected), Completion::Generate { op, result })
                if *expected == op =>
            {
                match result {
                    GenerateResult::Ready(secret) => Phase::ReadySave(secret),
                    GenerateResult::Unavailable => {
                        Phase::Paused(PauseReason::GenerationUnconfirmed)
                    }
                }
            }
            (
                Phase::AwaitSave {
                    op: expected,
                    generated,
                },
                Completion::Save { op, result },
            ) if *expected == op => match result {
                SaveResult::Saved(saved) => Phase::ReadyType(TypeSourceCheckpoint::Saved(saved)),
                SaveResult::NotSent => Phase::ReadySave(generated.clone()),
                SaveResult::LostResponse => Phase::ReadyReconcile(generated.clone()),
            },
            (
                Phase::AwaitReconcile { op: expected, .. },
                Completion::ReconcileSave { op, result },
            ) if *expected == op => match result {
                ReconcileResult::Found(saved) => {
                    Phase::ReadyType(TypeSourceCheckpoint::Saved(saved))
                }
                ReconcileResult::Absent | ReconcileResult::Unavailable => {
                    Phase::Paused(PauseReason::SaveUnconfirmed)
                }
            },
            (Phase::AwaitType(expected), Completion::Type { op, result }) if *expected == op => {
                match result {
                    TypeResult::Acknowledged => Phase::Done,
                    TypeResult::NotSent | TypeResult::Uncertain => {
                        Phase::Paused(PauseReason::TypeUnconfirmed)
                    }
                }
            }
            _ => return Err(TransitionError::UnexpectedCompletion),
        };
        self.state.phase = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs() -> (IntentRef, IntentRef, TargetRef) {
        (
            IntentRef::new().unwrap(),
            IntentRef::new().unwrap(),
            TargetRef::new().unwrap(),
        )
    }
    fn generated() -> Machine {
        let (generation, save, target) = refs();
        Machine::new(Intent::GeneratedLogin {
            generation,
            save,
            target,
        }).unwrap()
    }

    #[test]
    fn generated_value_is_saved_before_typing() {
        let mut machine = generated();
        let generate = machine.issue().unwrap().unwrap();
        assert!(matches!(generate, Request::Generate { .. }));
        let secret = SecretHandle::new().unwrap();
        machine
            .complete(Completion::Generate {
                op: generate.op(),
                result: GenerateResult::Ready(secret.clone()),
            })
            .unwrap();
        let save = machine.issue().unwrap().unwrap();
        assert!(matches!(&save, Request::Save { generated, .. } if *generated == secret));
        assert!(machine.issue().unwrap().is_none());
        let saved = SavedRef::new().unwrap();
        machine
            .complete(Completion::Save {
                op: save.op(),
                result: SaveResult::Saved(saved.clone()),
            })
            .unwrap();
        let typing = machine.issue().unwrap().unwrap();
        assert!(
            matches!(&typing, Request::Type { source: TypeSource::Saved(x), .. } if *x == saved)
        );
        machine
            .complete(Completion::Type {
                op: typing.op(),
                result: TypeResult::Acknowledged,
            })
            .unwrap();
        assert!(machine.done());
    }

    #[test]
    fn lost_save_reply_reconciles_without_regenerating() {
        let mut machine = generated();
        let generate = machine.issue().unwrap().unwrap();
        machine
            .complete(Completion::Generate {
                op: generate.op(),
                result: GenerateResult::Ready(SecretHandle::new().unwrap()),
            })
            .unwrap();
        let save = machine.issue().unwrap().unwrap();
        let json = serde_json::to_string(&machine.checkpoint()).unwrap();
        let mut restored = Machine::restore(serde_json::from_str(&json).unwrap()).unwrap();
        let reconcile = restored.issue().unwrap().unwrap();
        assert!(matches!(&reconcile, Request::ReconcileSave { .. }));
        assert_ne!(save.op(), reconcile.op());
        assert!(
            restored
                .complete(Completion::Save {
                    op: save.op(),
                    result: SaveResult::Saved(SavedRef::new().unwrap())
                })
                .is_err()
        );
        restored
            .complete(Completion::ReconcileSave {
                op: reconcile.op(),
                result: ReconcileResult::Absent,
            })
            .unwrap();
        assert_eq!(restored.paused(), Some(PauseReason::SaveUnconfirmed));
        assert!(restored.issue().unwrap().is_none());
    }

    #[test]
    fn checkpoint_contains_only_opaque_references() {
        let mut machine = generated();
        let request = machine.issue().unwrap().unwrap();
        machine
            .complete(Completion::Generate {
                op: request.op(),
                result: GenerateResult::Ready(SecretHandle::new().unwrap()),
            })
            .unwrap();
        let checkpoint = serde_json::to_string(&machine.checkpoint()).unwrap();
        assert!(!checkpoint.contains("correct horse battery staple"));
        assert!(!checkpoint.contains("password"));
        assert!(!checkpoint.contains("value"));
        let restored = Machine::restore(serde_json::from_str(&checkpoint).unwrap()).unwrap();
        assert!(matches!(restored.state.phase, Phase::ReadySave(_)));
        let mut with_value: serde_json::Value = serde_json::from_str(&checkpoint).unwrap();
        with_value["password"] = "correct horse battery staple".into();
        assert!(serde_json::from_value::<Checkpoint>(with_value).is_err());
    }

    #[test]
    fn failed_save_reconciles_to_confirmed_item() {
        let mut machine = generated();
        let generate = machine.issue().unwrap().unwrap();
        machine
            .complete(Completion::Generate {
                op: generate.op(),
                result: GenerateResult::Ready(SecretHandle::new().unwrap()),
            })
            .unwrap();
        let save = machine.issue().unwrap().unwrap();
        machine
            .complete(Completion::Save {
                op: save.op(),
                result: SaveResult::LostResponse,
            })
            .unwrap();
        let reconcile = machine.issue().unwrap().unwrap();
        assert!(matches!(&reconcile, Request::ReconcileSave { .. }));
        let saved = SavedRef::new().unwrap();
        machine
            .complete(Completion::ReconcileSave {
                op: reconcile.op(),
                result: ReconcileResult::Found(saved.clone()),
            })
            .unwrap();
        assert!(
            matches!(machine.issue().unwrap(), Some(Request::Type { source: TypeSource::Saved(x), .. }) if x == saved)
        );
    }

    fn at_ready_save(machine: &mut Machine) -> SecretHandle {
        let generate = machine.issue().unwrap().unwrap();
        let secret = SecretHandle::new().unwrap();
        machine
            .complete(Completion::Generate { op: generate.op(), result: GenerateResult::Ready(secret.clone()) })
            .unwrap();
        secret
    }

    #[test]
    fn completion_from_another_workflow_is_rejected() {
        let (mut a, mut b) = (generated(), generated());
        let op_a = a.issue().unwrap().unwrap().op();
        let op_b = b.issue().unwrap().unwrap().op();
        // Same sequence number, different random epoch.
        assert_eq!(op_a.value(), op_b.value());
        assert_ne!(op_a, op_b);
        let (before_a, before_b) = (a.checkpoint(), b.checkpoint());
        let crossed = |op| Completion::Generate { op, result: GenerateResult::Ready(SecretHandle::new().unwrap()) };
        assert_eq!(a.complete(crossed(op_b)), Err(TransitionError::UnexpectedCompletion));
        assert_eq!(b.complete(crossed(op_a)), Err(TransitionError::UnexpectedCompletion));
        assert_eq!((a.checkpoint(), b.checkpoint()), (before_a, before_b));
        // Neither advanced: each still awaits (and accepts) its own reply only.
        assert!(a.issue().unwrap().is_none() && b.issue().unwrap().is_none());
        a.complete(crossed(op_a)).unwrap();
        b.complete(crossed(op_b)).unwrap();
        assert!(matches!(a.issue().unwrap(), Some(Request::Save { .. })));

        // Also at the save step: a foreign save reply cannot confirm this save.
        let save_a = a.checkpoint();
        let save_b_op = b.issue().unwrap().unwrap().op();
        let foreign = Completion::Save { op: save_b_op, result: SaveResult::Saved(SavedRef::new().unwrap()) };
        assert_eq!(a.complete(foreign), Err(TransitionError::UnexpectedCompletion));
        assert_eq!(a.checkpoint(), save_a);
        assert!(!a.has_saved_value());

        // A checkpoint whose in-flight op carries another epoch is refused.
        let mut forged = a.checkpoint();
        forged.phase = match forged.phase {
            Phase::AwaitSave { generated, .. } => Phase::AwaitSave { op: save_b_op, generated },
            other => panic!("unexpected phase {other:?}"),
        };
        assert_eq!(Machine::restore(forged).err(), Some(TransitionError::InvalidCheckpoint));
    }

    #[test]
    fn stale_and_duplicate_completions_are_rejected() {
        let mut machine = generated();
        let generate = machine.issue().unwrap().unwrap();
        let reply = Completion::Generate { op: generate.op(), result: GenerateResult::Ready(SecretHandle::new().unwrap()) };
        machine.complete(reply.clone()).unwrap();
        let after_generate = machine.checkpoint();
        // Duplicate delivery of the same reply.
        assert_eq!(machine.complete(reply.clone()), Err(TransitionError::UnexpectedCompletion));
        assert_eq!(machine.checkpoint(), after_generate);

        let save = machine.issue().unwrap().unwrap();
        let awaiting = machine.checkpoint();
        // A stale op id (the generation's) on a save reply.
        let stale = Completion::Save { op: generate.op(), result: SaveResult::Saved(SavedRef::new().unwrap()) };
        assert_eq!(machine.complete(stale), Err(TransitionError::UnexpectedCompletion));
        // The right op id on the wrong kind of reply.
        let wrong_kind = Completion::ReconcileSave { op: save.op(), result: ReconcileResult::Found(SavedRef::new().unwrap()) };
        assert_eq!(machine.complete(wrong_kind), Err(TransitionError::UnexpectedCompletion));
        // A future op id.
        let future = OperationId { epoch: awaiting.epoch, sequence: save.op().value() + 1 };
        assert_eq!(
            machine.complete(Completion::Save { op: future, result: SaveResult::Saved(SavedRef::new().unwrap()) }),
            Err(TransitionError::UnexpectedCompletion)
        );
        assert_eq!(machine.checkpoint(), awaiting);

        machine.complete(Completion::Save { op: save.op(), result: SaveResult::LostResponse }).unwrap();
        let reconciling = machine.checkpoint();
        // The lost reply arriving late (or twice) cannot skip reconciliation.
        for result in [SaveResult::Saved(SavedRef::new().unwrap()), SaveResult::LostResponse] {
            assert_eq!(machine.complete(Completion::Save { op: save.op(), result }), Err(TransitionError::UnexpectedCompletion));
        }
        assert_eq!(machine.checkpoint(), reconciling);
        assert!(!machine.has_saved_value());
        assert!(matches!(machine.issue().unwrap(), Some(Request::ReconcileSave { .. })));
    }

    #[test]
    fn unsent_save_retries_the_same_value_without_reconciling() {
        let mut machine = generated();
        let secret = at_ready_save(&mut machine);
        let save = machine.issue().unwrap().unwrap();
        machine.complete(Completion::Save { op: save.op(), result: SaveResult::NotSent }).unwrap();
        assert!(!machine.has_saved_value());
        let again = machine.issue().unwrap().unwrap();
        assert!(matches!(&again, Request::Save { generated, .. } if *generated == secret));
        assert_ne!(again.op(), save.op());
    }

    /// Every phase serializes to references and counters only, and restores.
    #[test]
    fn every_phase_checkpoint_is_opaque_and_restorable() {
        fn check(machine: &Machine, seen: &mut Vec<String>) {
            let json = serde_json::to_string(&machine.checkpoint()).unwrap();
            for word in ["password", "value", "secret\"", "Zeroizing"] {
                assert!(!json.contains(word), "{word} in {json}");
            }
            Machine::restore(serde_json::from_str(&json).unwrap()).unwrap();
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            let phase = match &v["phase"] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Object(o) => o.keys().next().unwrap().clone(),
                other => panic!("{other}"),
            };
            seen.push(phase);
        }
        let mut seen = vec![];
        // Generated: every phase, including reconciliation and each pause.
        let mut m = generated();
        check(&m, &mut seen);
        let g = m.issue().unwrap().unwrap();
        check(&m, &mut seen);
        m.complete(Completion::Generate { op: g.op(), result: GenerateResult::Ready(SecretHandle::new().unwrap()) }).unwrap();
        check(&m, &mut seen);
        let s = m.issue().unwrap().unwrap();
        check(&m, &mut seen);
        m.complete(Completion::Save { op: s.op(), result: SaveResult::LostResponse }).unwrap();
        check(&m, &mut seen);
        let r = m.issue().unwrap().unwrap();
        check(&m, &mut seen);
        m.complete(Completion::ReconcileSave { op: r.op(), result: ReconcileResult::Found(SavedRef::new().unwrap()) }).unwrap();
        check(&m, &mut seen);
        let t = m.issue().unwrap().unwrap();
        check(&m, &mut seen);
        m.complete(Completion::Type { op: t.op(), result: TypeResult::Acknowledged }).unwrap();
        check(&m, &mut seen);
        let mut p = generated();
        let g = p.issue().unwrap().unwrap();
        p.complete(Completion::Generate { op: g.op(), result: GenerateResult::Unavailable }).unwrap();
        check(&p, &mut seen);
        // Existing: lookup phases.
        let (lookup, _, target) = refs();
        let mut e = Machine::new(Intent::Existing { lookup, target }).unwrap();
        check(&e, &mut seen);
        e.issue().unwrap().unwrap();
        check(&e, &mut seen);
        seen.sort();
        seen.dedup();
        let all = ["AwaitGenerate", "AwaitLookup", "AwaitReconcile", "AwaitSave", "AwaitType", "Done", "Paused", "ReadyGenerate", "ReadyLookup", "ReadyReconcile", "ReadySave", "ReadyType"];
        assert_eq!(seen, all);
    }

    #[test]
    fn existing_reference_can_type_without_save() {
        let (lookup, _, target) = refs();
        let mut machine = Machine::new(Intent::Existing { lookup, target }).unwrap();
        let request = machine.issue().unwrap().unwrap();
        machine
            .complete(Completion::Lookup {
                op: request.op(),
                result: LookupResult::Found(SecretHandle::new().unwrap()),
            })
            .unwrap();
        assert!(matches!(
            machine.issue().unwrap(),
            Some(Request::Type {
                source: TypeSource::Existing(_),
                ..
            })
        ));
    }
}
