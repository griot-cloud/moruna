//! `moruna-function`: a native Moruna function in Rust (`architecture/moruna-os-system.md`, 5A).
//!
//! A function is a program the data plane feeds Arrow batches. [`serve`] takes each input batch,
//! read in place from the record the engine posted (the engine's record check first, then
//! arrow-ipc over buffers that are slices of the record, no copy), and hands it to the function
//! with an [`Emit`]; [`Emit::batch`] writes an output batch into a window, and may be called any
//! number of times. A returned [`FunctionError`] ends the task with its message in the task's log,
//! and [`log`] writes a line there. A function with something to write once the input has ended
//! (an aggregate) is a [`Function`], whose [`Function::end`] is called after the last batch.
//!
//! ```no_run
//! use std::process::ExitCode;
//!
//! use moruna_function::{Emit, FunctionError, RecordBatch};
//!
//! fn main() -> ExitCode {
//!     moruna_function::serve(|batch: RecordBatch, out: &mut Emit| -> Result<(), FunctionError> {
//!         out.batch(&batch)
//!     })
//! }
//! ```
//!
//! **A batch is the function's only during its call.** Its buffers are the engine's memory, which
//! the engine writes the next batches into once the task releases it. A function that keeps any
//! part of its input batch (the batch, a column or a buffer of it) after its call returns ends the
//! task with an error naming it, and the batch is never released. A function copies what it keeps.
//!
//! The same program builds for the core (`target_os = "moruna"`, item 5.0), where [`serve`] runs
//! on the task's rings, and for a host, where it runs on the data plane in memory that
//! [`host::Plane`] drives, so a function is tested on a laptop as it runs on a node. Nothing else
//! differs between the two.

// On the core the task's rings are std's own moruna-sys's (`std::os::moruna`), unstable to its
// users (`rustc_private`), as Hermit's `hermit_abi` is.
#![cfg_attr(target_os = "moruna", feature(rustc_private))]

mod emit;
mod error;
mod lane;
mod record;
mod serve;
/// The task a call at a time, which the C library (`kernel/function/c`, system design 5B) is
/// the Arrow C Data Interface over. Not part of this crate's interface (5A): a function in Rust
/// calls [`serve`].
#[doc(hidden)]
pub mod task;

#[cfg(not(any(target_os = "moruna", moruna_function_sys)))]
pub mod host;
#[cfg(not(any(target_os = "moruna", moruna_function_sys)))]
use host as platform;

#[cfg(any(target_os = "moruna", moruna_function_sys))]
mod sys;
#[cfg(any(target_os = "moruna", moruna_function_sys))]
use sys as platform;

pub use arrow_array::RecordBatch;
pub use emit::Emit;
pub use error::FunctionError;
pub use serve::{Function, serve};

/// A task's exit codes, the ones the gate's tasks have always ended with. [`serve`] says the code
/// on the task's rings, where the engine reads it, and returns it for `main` to return.
pub mod code {
    /// Every batch was answered and the engine posts no more.
    pub const DONE: i64 = 0;
    /// The task was not started on the data plane.
    pub const NO_PLANE: i64 = 2;
    /// The task's plane holds no rings.
    pub const NO_RINGS: i64 = 3;
    /// The rings broke: a word the engine wrote could not be true, or the output ring was full.
    pub const RINGS: i64 = 5;
    /// The function refused a batch (it returned an error, kept its input batch, or emitted a
    /// batch no record can carry), or the input record failed the record check.
    pub const REFUSED: i64 = 7;
    /// One row of an output batch takes more than a window.
    pub const WINDOW_SMALL: i64 = 8;
}

/// Writes `line` to the task's log, a record a line (a line with newlines is several), through
/// std's standard error. On the core std owns the task's log (5A0, item 5.0): it sends standard
/// error there, and to the console when the task has no log. On a host, inside
/// [`host::Plane::run`], the lines are the run's [`host::Outcome::log`], as the core's std makes
/// them the task's; anywhere else they go to standard error.
pub fn log(line: impl std::fmt::Display) {
    platform::log_line(&line.to_string());
}
