//! The model port (design section 5.1; SDD 24 d.2).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::estimate::{Outcome, Target, UnitCost};
use crate::inputs::Inputs;
use crate::key::CostKey;
use crate::store::ObservationSource;

/// A model's name and version. Stored beside everything it fits; a change of either is a
/// retrain, and fitted state saved under one id is never loaded by another (CM-I3).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelId {
    /// The name the registry knows it by and a setting chooses it by.
    pub name: String,
    /// The version of its fitted state's meaning.
    pub version: u32,
}

impl ModelId {
    /// An id.
    pub fn new(name: impl Into<String>, version: u32) -> ModelId {
        ModelId {
            name: name.into(),
            version,
        }
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}@{}`", self.name, self.version)
    }
}

/// What a fit read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FitReport {
    /// Keys with at least one observation.
    pub keys: u64,
    /// Observations read.
    pub observations: u64,
    /// Records the source skipped as torn.
    pub torn: u64,
    /// Older records the source holds only as a summary.
    pub summarised: u64,
}

/// A cost model: one adapter behind this trait, named by configuration (decision 3).
///
/// `predict` is called inside the scheduler's decision loop and must be cheap; `observe` once
/// per finished unit; `fit`, `save` and `load` when a model is opened or closed.
pub trait CostModel: Send + Sync {
    /// Name and version, stored beside everything it fits; a change of either is a retrain.
    fn id(&self) -> ModelId;
    /// The prediction for one unit from what is known now; missing inputs widen the estimate.
    fn predict(&self, key: &CostKey, inputs: &Inputs) -> UnitCost;
    /// One measured unit. A model may learn online, or ignore it until `fit`.
    fn observe(&mut self, key: &CostKey, inputs: &Inputs, outcome: &Outcome);
    /// Fit from the store's raw observations: how a new model learns the history. What the
    /// model held before is replaced.
    fn fit(&mut self, history: &dyn ObservationSource) -> Result<FitReport>;
    /// Its fitted state, versioned by `id`.
    fn save(&self) -> Result<Vec<u8>>;
    /// Fitted state back; refused when another id saved it.
    fn load(&mut self, bytes: &[u8]) -> Result<()>;

    /// The online learner marks `key` drifting, or settled again (design 9.6, SDD 24 f.11): its
    /// outcomes left its predictions' spread for several units, or have come back inside it. A
    /// model that forgets raises its forgetting while a key drifts, so the rest of the run is
    /// predicted from what the run measures now; a model that does not forget ignores it, which
    /// the provided answer does. The spread is widened by the learner, for every model.
    fn set_drifting(&mut self, key: &CostKey, drifting: bool) {
        let _ = (key, drifting);
    }

    /// A target's cost near a batch of `rows` rows as a straight line in rows: a fixed part per
    /// batch and a part per row, at the median and at the 95th percentile (SDD 24 f.7). What
    /// morsel sizing weighs (`architecture/adaptive-runtime.md` G4). The provided answer asks
    /// [`CostModel::predict`] twice, at `rows` and at twice `rows` with the same bytes per row,
    /// so every model answers it; a model that holds its overhead apart may answer from it.
    fn marginal(&self, key: &CostKey, inputs: &Inputs, target: Target, rows: u64) -> Marginal {
        let rows = rows.max(1);
        let width = inputs.bytes_per_row();
        let at = |r: u64| {
            let mut i = inputs.clone();
            i.data.rows = Some(r);
            i.data.bytes = width.map(|w| (w * r as f64).round() as u64);
            *self.predict(key, &i).get(target)
        };
        let one = at(rows);
        let two = at(rows.saturating_mul(2));
        let line = |y1: f64, y2: f64| {
            let per_row = ((y2 - y1) / rows as f64).max(0.0);
            Line {
                fixed: (y1 - per_row * rows as f64).max(0.0),
                per_row,
            }
        };
        Marginal {
            p50: line(one.p50, two.p50),
            p95: line(one.p95, two.p95),
        }
    }
}

/// A cost as a straight line in a batch's rows: `fixed + per_row · rows`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Line {
    /// The part every batch pays, whatever its size.
    pub fixed: f64,
    /// The part each row adds.
    pub per_row: f64,
}

impl Line {
    /// The line's value at `rows`.
    pub fn at(&self, rows: f64) -> f64 {
        self.fixed + self.per_row * rows
    }
}

/// A target's cost near a batch size, at the median and at the 95th percentile.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Marginal {
    /// The median's line.
    pub p50: Line,
    /// The 95th percentile's line.
    pub p95: Line,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_display() {
        assert_eq!(ModelId::new("loglinear", 1).to_string(), "`loglinear@1`");
    }
}
