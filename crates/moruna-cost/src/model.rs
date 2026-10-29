//! The model port (design section 5.1; SDD 24 d.2).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::estimate::{Outcome, UnitCost};
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_display() {
        assert_eq!(ModelId::new("loglinear", 1).to_string(), "`loglinear@1`");
    }
}
