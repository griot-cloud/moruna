//! The error policy and the one way a run is ended by the runtime (f.8, SC-I8, G-I8).

use std::sync::atomic::Ordering;

use amoru_kernel::{AmoruError, ErrorPolicy, Outcome, Seq, Sink};

use crate::shared::{Exit, RunState, Shared};

/// `apply_policy` of f.8. Returns the outcome the trace record carries.
///
/// `Terminate` ends the run with the diagnostic. `Skip` drops the morsel and tells the sink so,
/// which is what keeps a skipped sequence number from holding the commit watermark back (f.11).
/// `Budget(n)` skips errors 1 to n-1 and terminates on the n-th, so `Budget(1)` equals
/// `Terminate`.
pub(crate) fn apply_policy(
    shared: &Shared,
    stage_ix: usize,
    seq: Seq,
    diagnostic: AmoruError,
) -> Outcome {
    let seen = shared.errors_total.fetch_add(1, Ordering::SeqCst) + 1;
    let terminate_now = match shared.cfg.error_policy {
        ErrorPolicy::Terminate => true,
        ErrorPolicy::Skip => false,
        ErrorPolicy::Budget(n) => seen >= n,
    };
    if terminate_now {
        tracing::error!(target: "sched.policy", stage = stage_ix + 1, seq, "the run ends on this error");
        terminate(shared, diagnostic);
        return Outcome::Error;
    }
    tracing::warn!(target: "sched.policy", stage = stage_ix + 1, seq, "morsel skipped");
    if let Some(entry) = shared.stages.get(stage_ix) {
        entry.skipped.fetch_add(1, Ordering::SeqCst);
    }
    {
        let sink = shared.sink.read().unwrap_or_else(|e| e.into_inner());
        sink.skip(seq);
    }
    crate::sink_drive::update_watermark(shared);
    Outcome::Skipped
}

/// End the run with a diagnostic: the kernel error path, `Knobs::terminate` from the
/// controller's thread, and the heartbeat checker's dead worker all arrive here (f.8, f.14).
/// The first diagnostic wins; later ones are dropped, so the report names the first cause.
pub(crate) fn terminate(shared: &Shared, diagnostic: AmoruError) {
    // f.10: a terminate that loses the claim arrives after the sink drive has already finished
    // the run, so there is nothing left to end.
    if !shared.claim_exit() {
        return;
    }
    if shared.run_state().is_live() || shared.run_state() == RunState::Init {
        shared.set_run_state(RunState::Terminating);
    }
    if shared.publish_exit(Exit::Terminated(diagnostic)) {
        shared.unpark_all();
    }
}

/// The surface's cancel, observed between tasks (f.10).
pub(crate) fn cancel(shared: &Shared) {
    // A cancel that arrives after the last morsel was committed is a no-op: the run is done.
    if !shared.claim_exit() {
        return;
    }
    if shared.run_state().is_live() {
        shared.set_run_state(RunState::Cancelling);
    }
    if shared.publish_exit(Exit::Cancelled) {
        tracing::info!(target: "sched.cancel", "the run was cancelled");
        shared.unpark_all();
    }
}
