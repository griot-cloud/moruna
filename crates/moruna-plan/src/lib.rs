//! Moruna's plan of a job (SDD 26): what runs when and how, parallel or in sequence, before it
//! runs (`architecture/adaptive-runtime.md` sections 4, 8 and 11).
//!
//! - [`Plan`], the plan document: its [`Step`]s in the order they start, each a task of the job
//!   or a fused group, with its dependencies, its group, its [`Resources`] and its estimated
//!   [`UnitCost`]; the job's [`Totals`]. FlatBuffers on the wire ([`Plan::from_flatbuffer`]
//!   verifies untrusted bytes), JSON for people ([`Plan::to_json`]).
//! - [`Planner`], the port a planner implements, and [`Planners`], the registry a setting
//!   chooses from; [`Levels`], the first planner, and [`Simulated`], which plays the job's units
//!   in `moruna-cost`'s simulator with a cost model ([`default_planner`] chooses it when one is
//!   given).
//! - [`check_plan`], the assertion every plan passes before it is shown or run, and
//!   [`explain`], which checks a job, plans it and checks the plan.
//!
//! The crate sits above `moruna-job` and `moruna-cost` and beside the runtime: an embedder plans
//! in its own process without Arrow or the engines, asking a `moruna-cost` model
//! ([`PlanInputs::with_model`]).

#![deny(missing_docs)]

mod check;
mod costing;
mod explain;
mod flatbuffer;
mod levels;
mod plan;
mod planner;
mod simulated;

/// The Rust flatc generates from `schema/plan.fbs` (`tools/flatc/generate.sh`), committed and
/// checked in CI against what the schema generates.
#[allow(missing_docs, clippy::all, clippy::pedantic, unsafe_op_in_unsafe_fn, deprecated)]
#[rustfmt::skip]
mod generated {
    include!("generated/plan_generated.rs");
}

/// The plan document's FlatBuffers tables as flatc generates them from `schema/plan.fbs`. Bytes
/// from anyone are read with [`Plan::from_flatbuffer`], which verifies them first.
pub use generated::moruna::plan::fb;

pub use check::{PlanRefusal, PlanRefusalCode, check_plan, job_ceiling};
pub use costing::cost_key;
pub use explain::{explain, plan_with};
pub use flatbuffer::{PLAN_IDENTIFIER, PlanBytesError, is_plan_flatbuffer};
pub use levels::Levels;
pub use plan::{
    Basis, Estimate, PLAN_VERSION, Plan, PlannerId, Range, Resources, Step, Target, Totals, Unit,
    UnitCost, is_step_id, job_digest, step_id, weakest,
};
pub use planner::{
    ColumnStats, DEFAULT_PLANNER, DISK_BYTES_PER_SECOND, PlanError, PlanInputs, Planner, Planners,
    TableStats, budget_resources, default_planner,
};
pub use simulated::{Simulated, Simulation};
