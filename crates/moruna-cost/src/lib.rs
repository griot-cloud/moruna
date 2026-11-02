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
//! - [`Online`]: the models a run uses, learning from every unit it finishes, judging drift and
//!   writing what it learned to the store off the run's hot path (section 9.6).
//! - [`sim`]: the discrete-event simulator a job's finish and the resources it holds are found
//!   by (section 8), and the [`sim::Policy`] port it plays; [`morsel`], the batch size a task is
//!   given (G4); [`Rng`], the seeded draws its Monte Carlo runs take.

#![deny(missing_docs)]

mod error;
mod estimate;
mod exact;
mod inputs;
mod key;
pub mod loglinear;
mod model;
pub mod morsel;
pub mod online;
mod registry;
mod sample;
pub mod sim;
mod store;

pub use error::{CostError, Result};
pub use estimate::{Basis, Estimate, Origin, Outcome, Target, Unit, UnitCost};
pub use inputs::{Capacity, Column, Data, INPUTS_VERSION, Inputs, Job, Kernel, Layout, Resources};
pub use key::{CostKey, Digest};
pub use loglinear::{Defaults, LogLinear, LogLinearAdapter};
pub use model::{CostModel, FitReport, Line, Marginal, ModelId};
pub use morsel::{Bound, Morsel, morsel};
pub use online::{Drift, DriftEvent, LearningReport, Online};
pub use registry::{
    Comparison, CostModels, CostSetting, InUse, ModelAdapter, Prediction, Readiness, Score, compare,
};
pub use sample::{Rng, Z95, normal_quantile};
pub use store::{
    History, LocalStore, Moments, Observation, ObservationSource, ObservationStore, Predicted,
    RETENTION_PER_KEY, Retention, Summary, decode_observations, encode_observation, key_partition,
    model_state_name, trim_history,
};
