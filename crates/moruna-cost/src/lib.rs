//! Moruna's cost model (SDD 24; `architecture/adaptive-runtime.md` sections 3 to 7).
//!
//! What one task applied to one batch takes from what a run is given (worker time, memory, disk
//! traffic) and what it gives, predicted as distributions and learned from what units measured.
//! The planner asks before a run and the scheduler asks during one, the same model the same
//! question; they differ only in what they know.
//!
//! - [`Inputs`]: what is known of a unit (section 3), every field optional and versioned.
//! - [`CostKey`], [`Estimate`], [`UnitCost`], [`Outcome`]: the identity, the answer, the
//!   measurement (section 4).
//! - [`CostModel`]: the port a model implements, and [`CostModels`], the registry a setting
//!   chooses from; [`InUse`] runs one model deciding and one in shadow (section 5).
//! - [`LogLinear`]: the default model (section 5.3) and its fallbacks (section 5.4).
//! - [`ObservationStore`]: the raw history, and [`LocalStore`], its local-directory adapter
//!   (section 7).

#![deny(missing_docs)]

mod error;
mod estimate;
mod exact;
mod inputs;
mod key;
pub mod loglinear;
mod model;
mod registry;
mod store;

pub use error::{CostError, Result};
pub use estimate::{Basis, Estimate, Origin, Outcome, Target, Unit, UnitCost};
pub use inputs::{Capacity, Column, Data, INPUTS_VERSION, Inputs, Job, Kernel, Layout, Resources};
pub use key::{CostKey, Digest};
pub use loglinear::{Defaults, LogLinear, LogLinearAdapter};
pub use model::{CostModel, FitReport, ModelId};
pub use registry::{
    Comparison, CostModels, CostSetting, InUse, ModelAdapter, Prediction, Readiness, Score, compare,
};
pub use store::{
    History, LocalStore, Moments, Observation, ObservationSource, ObservationStore, Predicted,
    RETENTION_PER_KEY, Retention, Summary,
};
