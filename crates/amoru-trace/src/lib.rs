//! Amoru component 4, the trace writer and the run report.
//!
//! Design: `architecture/sdd/04-trace.md`. The writer takes `TraceRecord` values from any
//! thread through a bounded channel, batches them into Arrow chunks, keeps the chunks in
//! memory up to `trace.memory_limit`, overflows the rest to a file in the staging directory
//! and writes the whole trace as an Arrow IPC file when a path was given. The report is a
//! pure function of the trace plus the discovered limits (G-I4, TR-I3) and is computed only
//! in [`report`].
//!
//! The crate implements the contracts' `TraceSink` (write side, the scheduler) and
//! `TraceTail` (read side, the controller), so neither consumer names a type from here.

// Every fallible function here returns the contracts' `AmoruError` (contracts d.14), which is
// large because CT-I10 makes its variants carry the morsel. Boxing it would change the shape of
// the contract's `Result`, so the lint is allowed for the crate rather than worked around.
#![allow(clippy::result_large_err)]

pub mod render;
pub mod report;
pub mod view;
pub mod writer;

pub use report::{DeviceSummary, ExitReason, LimitsSummary, RunMeta, RunReport, StageReport};
pub use view::TraceView;
pub use writer::{TraceConfig, TraceWriter};

/// The contracts' result type; every error in this crate is an `AmoruError` value.
pub type Result<T> = core::result::Result<T, amoru_kernel::AmoruError>;

/// Records per in-memory chunk (b, f.1).
pub const CHUNK_ROWS: usize = 4096;

/// How long the writer thread waits for more records before finalising what it has (f.1).
pub const DRAIN_INTERVAL_MS: u64 = 100;

/// A `RunId` as the 32 lowercase hex characters the trace file names and the report use
/// (contracts d.1, b).
pub(crate) fn run_id_hex(id: amoru_kernel::RunId) -> String {
    use core::fmt::Write as _;
    let mut s = String::with_capacity(32);
    for byte in id.0 {
        // Writing to a String cannot fail; the result is discarded deliberately.
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Lock a mutex, taking the value back when a previous holder panicked. The trace writer
/// must not itself panic on a poisoned lock: losing the trace loses the diagnostic (G-I8).
pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
