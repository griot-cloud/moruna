//! The gate's seven functions as native Moruna functions (`images/gate/README.md`), each a
//! program on `moruna-function` (system design 5A): the job's port (`moruna_gate_functions::batch`,
//! a `RecordBatch` in and one out) called on each batch the data plane posts, read in place, and
//! its output written back into windows, cut by rows when it takes more than one.
//!
//! A batch the function refuses ends the task with exit code 7 (`moruna_function::code`) and the
//! reason in its log, as a Python task that raised ended its stage on 0.6.6.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use moruna_function::{Emit, FunctionError, RecordBatch};

/// A job's function, as `moruna_gate_functions::batch` has it.
pub type Job = fn(&RecordBatch) -> moruna_gate_functions::error::Result<RecordBatch>;

/// Answers `batch` with `job`'s output.
pub fn answer(job: Job, batch: RecordBatch, out: &mut Emit) -> Result<(), FunctionError> {
    let output = job(&batch).map_err(|e| FunctionError::new(e.to_string()))?;
    out.batch(&output)
}

/// The task: `job` on every batch of its rings, until the engine posts no more. Its exit code,
/// for `main` to return.
pub fn serve(job: Job) -> ExitCode {
    moruna_function::serve(move |batch, out| answer(job, batch, out))
}
