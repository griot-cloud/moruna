//! The probe protocol (f.9, `Prober::probe`).
//!
//! Every worker but one is held off for the whole call, so the peak the sampler reports is the
//! probe's own. The input for stage 1 is one read the source drive issues on the probing
//! thread's behalf (the drive-side helper of b), so no thread but a drive ever waits on a
//! completion (SC-I1). For a later stage it is the head of the previous queue, which is that
//! stage's probe output. The output is pushed downstream as a normal morsel: nothing is wasted.

use std::sync::atomic::Ordering;
use std::time::Duration;

use amoru_kernel::{AmoruError, ProbeResult, Result, StageId};
use crossbeam::channel::unbounded;

use crate::shared::{HelperRequest, JobOutput, Shared, WorkerJob};

/// The probing worker: worker 0, the one left running while the rest are held off.
const PROBING_WORKER: u16 = 0;

/// `Prober::probe(stage, bytes)` (f.9).
pub(crate) fn probe(shared: &Shared, stage: StageId, bytes: u64) -> Result<ProbeResult> {
    if stage == 0 || stage as usize > shared.stages.len() {
        // h: a chain with no kernel has no stage to probe, and the controller does not ask.
        return Err(AmoruError::Plan(format!(
            "stage {stage} cannot be probed: the chain has {} kernel stages",
            shared.stages.len()
        )));
    }
    shared.probing.store(true, Ordering::SeqCst);
    shared.gate.store(true, Ordering::SeqCst);
    // Every worker that was inside `apply` finishes first; after this nobody but the probing
    // worker runs anything, because `pick` returns None while the gate is up.
    wait_for_quiet(shared);
    let outcome = probe_inner(shared, stage, bytes);
    shared.gate.store(false, Ordering::SeqCst);
    shared.probing.store(false, Ordering::SeqCst);
    shared.unpark_all();
    outcome
}

fn probe_inner(shared: &Shared, stage: StageId, bytes: u64) -> Result<ProbeResult> {
    if stage == 1 {
        // f.9: the drive answers helper requests in `Init` too, which is when the controller
        // probes; it issues the read, waits on it and pushes the morsel to Q0 with the next
        // sequence number.
        let (reply, rx) = unbounded();
        shared
            .helper_tx
            .send(HelperRequest { bytes, reply })
            .map_err(|_| AmoruError::Plan("the source drive is not running".into()))?;
        if let Some(unparker) = shared
            .source_unparker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            unparker.unpark();
        }
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(result) => result?,
            Err(_) => {
                return Err(AmoruError::Plan(
                    "the source drive did not answer the probe's read request".into(),
                ));
            }
        }
    }
    tracing::info!(target: "sched.probe", stage, bytes, "probing");
    match shared.run_on_worker(PROBING_WORKER, WorkerJob::Probe { stage })? {
        JobOutput::Probe(result) => Ok(result),
        JobOutput::Done => Err(AmoruError::Plan(format!(
            "stage {stage} had no morsel to probe with"
        ))),
    }
}

/// Wait until no worker is inside `apply`, bounded so a long kernel cannot wedge the call.
fn wait_for_quiet(shared: &Shared) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while shared.workers_busy.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}
