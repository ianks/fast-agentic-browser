//! Labelled decision cases: a stored observation, the instruction, and the
//! action a passing run took there. Used to evaluate decision engines offline
//! (live Jev, no browser).

use serde::{Deserialize, Serialize};

use crate::decide::{FillValue, Plan};
use crate::snapshot::{El, Snapshot};

/// Identifies an element independently of its per-snapshot id, so labels
/// survive re-recording.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fp {
    pub role: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
}

impl Fp {
    pub fn of(e: &El) -> Self {
        Self { role: e.r.clone(), name: e.n.clone(), ctx: e.c.clone(), t: e.t.clone() }
    }

    pub fn matches(&self, e: &El) -> bool {
        *self == Fp::of(e)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Gold {
    /// Field writes: text/select values, "on"/"off" for checkboxes and radios.
    #[serde(default)]
    pub writes: Vec<(Fp, String)>,
    #[serde(default)]
    pub click: Option<Fp>,
    #[serde(default)]
    pub enter: bool,
    /// The instruction was already complete at this observation.
    #[serde(default)]
    pub done: bool,
}

impl Gold {
    pub fn from_plan(plan: &Plan, snap: &Snapshot) -> Self {
        let el = |i: usize| snap.els.iter().find(|e| e.i == i);
        let mut writes: Vec<(Fp, String)> = plan
            .fills
            .iter()
            .filter_map(|f| {
                let e = el(f.el)?;
                let v = match &f.value {
                    FillValue::Text(v) | FillValue::Select(v) => v.clone(),
                    FillValue::Check(on) => if *on { "on" } else { "off" }.to_string(),
                    FillValue::Radio(_) => "on".to_string(),
                };
                Some((Fp::of(e), v))
            })
            .collect();
        writes.sort_by(|a, b| (&a.0.role, &a.0.name, &a.1).cmp(&(&b.0.role, &b.0.name, &b.1)));
        Self { writes, click: plan.click.and_then(el).map(Fp::of), enter: plan.enter, done: false }
    }

    pub fn done() -> Self {
        Self { done: true, ..Default::default() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionCase {
    pub id: String,
    pub scenario: String,
    /// "scripted", "agent", "hand", "paraphrase".
    pub source: String,
    pub instr: String,
    pub history: Vec<String>,
    pub snap: Snapshot,
    pub gold: Gold,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Score {
    pub click_ok: bool,
    pub writes_ok: bool,
    pub done_ok: bool,
    /// Did something the gold didn't (wrong click, wrong/extra value) — harmful.
    /// Omissions (a deferred click, a missing write, stopping early) cost a
    /// round or a retry instead.
    pub commission: bool,
}

impl Score {
    pub fn all(&self) -> bool {
        self.click_ok && self.writes_ok && self.done_ok
    }
}

/// Compares a predicted plan against the gold label.
pub fn score(pred: &Gold, gold: &Gold) -> Score {
    if gold.done || pred.done {
        let ok = gold.done == pred.done;
        // Acting when the step was already done is a commission; stopping early is not.
        let commission = gold.done && !pred.done && (pred.click.is_some() || !pred.writes.is_empty());
        return Score { click_ok: ok, writes_ok: ok, done_ok: ok, commission };
    }
    let wrong_click = pred.click.is_some() && pred.click != gold.click;
    let extra_writes = pred.writes.iter().any(|w| !gold.writes.contains(w));
    Score {
        click_ok: pred.click == gold.click && pred.enter == gold.enter,
        writes_ok: pred.writes == gold.writes,
        done_ok: true,
        commission: wrong_click || extra_writes,
    }
}
