//! Validated domain identities, evidence and observation-bound actions.
//!
//! An identity identifies evidence; it does not establish that a live page has
//! not changed. All execution boundaries must check the observation again.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Debug, thiserror::Error)]
#[error("invalid task identity: expected 32 lowercase hexadecimal characters")]
pub struct IdentityError;

/// Stable identity of one durable execution, independent of a transport call.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TaskId(String);

impl TaskId {
    pub fn new() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes.iter().map(|b| format!("{b:02x}")).collect()))
    }

    pub fn parse(value: &str) -> Result<Self, IdentityError> {
        if value.len() == 32 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            Ok(Self(value.to_owned()))
        } else {
            Err(IdentityError)
        }
    }

    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}
impl FromStr for TaskId {
    type Err = IdentityError;
    fn from_str(s: &str) -> Result<Self, Self::Err> { Self::parse(s) }
}
impl TryFrom<String> for TaskId {
    type Error = IdentityError;
    fn try_from(s: String) -> Result<Self, Self::Error> { Self::parse(&s) }
}
impl From<TaskId> for String {
    fn from(id: TaskId) -> Self { id.0 }
}

/// A finite probability. Policy thresholds that disable a rule are separate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Probability(f64);
impl Probability { pub const ZERO: Self = Self(0.0); pub const ONE: Self = Self(1.0); }

#[derive(Debug, thiserror::Error)]
#[error("probability must be finite and between zero and one")]
pub struct ProbabilityError;

impl TryFrom<f64> for Probability {
    type Error = ProbabilityError;
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if value.is_finite() && (0.0..=1.0).contains(&value) { Ok(Self(value)) } else { Err(ProbabilityError) }
    }
}
impl From<Probability> for f64 {
    fn from(p: Probability) -> Self { p.0 }
}

/// Page identity within a browser runtime, distinct from a DOM document.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PageId(TaskId);
impl PageId {
    pub fn new() -> Result<Self, getrandom::Error> { Ok(Self(TaskId::new()?)) }
    pub fn as_str(&self) -> &str { self.0.as_str() }
}

/// A process-local stamp. It intentionally cannot deserialize into live evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationStamp {
    page: PageId,
    incarnation: u64,
    document: String,
    revision: u64,
}

impl ObservationStamp {
    pub fn capture(page: PageId, incarnation: u64, snapshot: &crate::snapshot::Snapshot) -> Result<Self, TargetError> {
        if snapshot.doc_id.is_empty() { return Err(TargetError::MissingDocument); }
        Ok(Self { page, incarnation, document: snapshot.doc_id.clone(), revision: snapshot.version })
    }
    pub fn document(&self) -> &str { &self.document }
    pub fn revision(&self) -> u64 { self.revision }
    pub fn page(&self) -> &PageId { &self.page }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TargetError {
    #[error("the observation has no document identity")]
    MissingDocument,
    #[error("element e{0} is absent from the observation")]
    Missing(usize),
    #[error("element e{0} does not support the requested operation")]
    WrongKind(usize),
    #[error("element e{0} is disabled")]
    Disabled(usize),
    #[error("the target belongs to a different observation")]
    Stale,
    #[error("a batch cannot both click and press Enter")]
    ConflictingActivation,
    #[error("the action batch contains no work")]
    EmptyBatch,
}

#[derive(Clone, Debug)]
struct BoundElement {
    key: usize,
    observation: ObservationStamp,
}

macro_rules! target {
    ($name:ident, $accepts:expr) => {
        #[derive(Clone, Debug)]
        pub struct $name(BoundElement);
        impl $name {
            pub fn resolve(snapshot: &crate::snapshot::Snapshot, stamp: &ObservationStamp, key: usize) -> Result<Self, TargetError> {
                if stamp.document != snapshot.doc_id || stamp.revision != snapshot.version { return Err(TargetError::Stale); }
                let el = snapshot.els.iter().find(|el| el.i == key).ok_or(TargetError::Missing(key))?;
                if el.has_flag("disabled") { return Err(TargetError::Disabled(key)); }
                if !($accepts)(el.kind()) { return Err(TargetError::WrongKind(key)); }
                Ok(Self(BoundElement { key, observation: stamp.clone() }))
            }
            pub fn key(&self) -> usize { self.0.key }
            pub fn observation(&self) -> &ObservationStamp { &self.0.observation }
            pub fn check(&self, current: &ObservationStamp) -> Result<(), TargetError> {
                if &self.0.observation == current { Ok(()) } else { Err(TargetError::Stale) }
            }
        }
    }
}

use crate::snapshot::Kind;
target!(TextTarget, |k| k == Kind::Text);
target!(SelectTarget, |k| k == Kind::Select);
target!(CheckTarget, |k| k == Kind::Check);
target!(RadioTarget, |k| k == Kind::Radio);
target!(ClickTarget, |k| k != Kind::Other);

/// A text instruction preserves secret placeholders until the secret workflow.
#[derive(Clone, Debug)]
pub struct TextInput(TextSource);
#[derive(Clone, Debug)]
enum TextSource { Literal(String), SecretTemplate(String) }
impl TextInput {
    pub fn parse(text: String) -> Self {
        Self(if crate::secrets::has_placeholder(&text) { TextSource::SecretTemplate(text) } else { TextSource::Literal(text) })
    }
    pub fn as_str(&self) -> &str {
        match &self.0 { TextSource::Literal(s) | TextSource::SecretTemplate(s) => s }
    }
}

#[derive(Clone, Debug)]
pub enum FieldEdit {
    Text { target: TextTarget, value: TextInput },
    Select { target: SelectTarget, option: String },
    Check { target: CheckTarget, checked: bool },
    Radio { target: RadioTarget },
}

#[derive(Clone, Debug)]
pub enum EnterDestination {
    Focused,
    Element(TextTarget),
}

#[derive(Clone, Debug)]
pub enum Activation {
    Click(ClickTarget),
    Enter(EnterDestination),
}

/// A batch always contains work and never both a click and an Enter activation.
///
/// ```compile_fail
/// use fab_core::domain::ActionBatch;
/// let invalid = ActionBatch { edits: vec![], activation: None };
/// ```
#[derive(Clone, Debug)]
pub struct ActionBatch {
    observation: ObservationStamp,
    edits: Vec<FieldEdit>,
    activation: Option<Activation>,
}
impl ActionBatch {
    pub fn new(observation: ObservationStamp, edits: Vec<FieldEdit>, activation: Option<Activation>) -> Option<Self> {
        if edits.is_empty() && activation.is_none() { return None; }
        for edit in &edits {
            let stamp = match edit {
                FieldEdit::Text { target, .. } => target.observation(),
                FieldEdit::Select { target, .. } => target.observation(),
                FieldEdit::Check { target, .. } => target.observation(),
                FieldEdit::Radio { target } => target.observation(),
            };
            if stamp != &observation { return None; }
        }
        let target_stamp = match &activation {
            Some(Activation::Click(target)) => Some(target.observation()),
            Some(Activation::Enter(EnterDestination::Element(target))) => Some(target.observation()),
            _ => None,
        };
        if target_stamp.is_some_and(|stamp| stamp != &observation) { return None; }
        Some(Self { observation, edits, activation })
    }
    pub fn observation(&self) -> &ObservationStamp { &self.observation }
    /// Validate an untrusted decision before the first browser effect.
    pub fn resolve(plan: &crate::decide::Plan, snapshot: &crate::snapshot::Snapshot, stamp: &ObservationStamp) -> Result<Self, TargetError> {
        use crate::decide::FillValue;
        if plan.click.is_some() && plan.enter { return Err(TargetError::ConflictingActivation); }
        let mut edits = Vec::with_capacity(plan.fills.len());
        let mut last_text = None;
        for fill in &plan.fills {
            edits.push(match &fill.value {
                FillValue::Text(value) => {
                    let target = TextTarget::resolve(snapshot, stamp, fill.el)?;
                    last_text = Some(target.clone());
                    FieldEdit::Text { target, value: TextInput::parse(value.clone()) }
                }
                FillValue::Select(option) => FieldEdit::Select { target: SelectTarget::resolve(snapshot, stamp, fill.el)?, option: option.clone() },
                FillValue::Check(checked) => FieldEdit::Check { target: CheckTarget::resolve(snapshot, stamp, fill.el)?, checked: *checked },
                FillValue::Radio(key) => {
                    if *key != fill.el { return Err(TargetError::WrongKind(fill.el)); }
                    FieldEdit::Radio { target: RadioTarget::resolve(snapshot, stamp, fill.el)? }
                }
            });
        }
        let activation = if let Some(key) = plan.click {
            Some(Activation::Click(ClickTarget::resolve(snapshot, stamp, key)?))
        } else if plan.enter {
            Some(Activation::Enter(last_text.map(EnterDestination::Element).unwrap_or(EnterDestination::Focused)))
        } else { None };
        Self::new(stamp.clone(), edits, activation).ok_or(TargetError::EmptyBatch)
    }
    pub fn edits(&self) -> &[FieldEdit] { &self.edits }
    pub fn activation(&self) -> Option<&Activation> { self.activation.as_ref() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_validate_on_decode() {
        let id = TaskId::new().unwrap();
        assert_eq!(serde_json::from_str::<TaskId>(&serde_json::to_string(&id).unwrap()).unwrap(), id);
        assert!(serde_json::from_str::<TaskId>(r#""../../tasks""#).is_err());
    }
    #[test]
    fn probability_rejects_invalid_evidence() {
        for p in [f64::NAN, f64::INFINITY, -0.1, 1.1] { assert!(Probability::try_from(p).is_err()); }
        assert!(Probability::try_from(0.0).is_ok());
        assert!(Probability::try_from(1.0).is_ok());
    }
    #[test]
    fn empty_action_is_not_executable() {
        let stamp = ObservationStamp { page: PageId::new().unwrap(), incarnation: 0, document: "doc".into(), revision: 1 };
        assert!(ActionBatch::new(stamp, vec![], None).is_none());
    }
    #[test]
    fn batches_cannot_mix_observations() {
        let snapshot: crate::snapshot::Snapshot = serde_json::from_value(serde_json::json!({"docId":"doc","version":1,"url":"https://test.invalid","title":"","texts":[],"els":[{"i":1,"r":"textbox","n":"name"}]})).unwrap();
        let first = ObservationStamp::capture(PageId::new().unwrap(), 0, &snapshot).unwrap();
        let other = ObservationStamp::capture(PageId::new().unwrap(), 0, &snapshot).unwrap();
        let target = TextTarget::resolve(&snapshot, &other, 1).unwrap();
        assert!(ActionBatch::new(first, vec![FieldEdit::Text { target, value: TextInput::parse("Ada".into()) }], None).is_none());
    }
}
