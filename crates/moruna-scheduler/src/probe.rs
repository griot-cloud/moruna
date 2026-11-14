//! The probe protocol (f.9, `Prober::probe`).
//!
//! Every worker but one is held off for the whole call, so the peak the sampler reports is the
//! probe's own. A stage's input is the head of one of its input queues: one that already holds a
//! morsel (an upstream stage's probe output, or a source read pushed to every queue its source
//! gives), or else one read the source feeding one of its ports issues on the probing thread's
//! behalf (the drive-side helper of b), so no thread but a drive ever waits on a completion
//! (SC-I1). The controller probes the stages in order, which is an order in which every queue
//! goes forward, so in a chain stage 1 reads and every later stage takes the previous stage's
//! output. The output is pushed downstream as a normal morsel: nothing is wasted.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crossbeam::channel::unbounded;
use moruna_kernel::{MorunaError, ProbeResult, Result, StageId};

use crate::shared::{HelperRequest, JobOutput, Shared, WorkerJob};

/// Why a stage had nothing to probe with when its sources still have rows (f.9); the controller
/// names it in the note it leaves (RC f.2).
const NOTHING_REACHED: &str = "no morsel reached it";

/// The probing worker for a stage that keeps no instances: worker 0, the one left running while
/// the rest are held off. A stage that keeps instances is probed on its first instance's home.
const PROBING_WORKER: u16 = 0;

/// The worker that probes stage `stage_ix` (f.9): the home of its first instance when it keeps
/// instances, so the probe takes the instance a task would; worker 0 otherwise. Every stage was
/// probed on worker 0, which may not borrow an instance whose home is another active worker
/// (f.4, SC-I6): with instances dealt across workers, every stateful stage after the first
/// failed its probe with "no morsel to probe with" (2026-11-14, found once RB2 probed small
/// datasets too).
fn probing_worker(shared: &Shared, stage_ix: usize) -> u16 {
    match shared.stages[stage_ix].pool {
        Some(_) => crate::instances::home(shared, stage_ix, 0),
        None => PROBING_WORKER,
    }
}

/// `Prober::probe(stage, bytes)` (f.9).
pub(crate) fn probe(shared: &Shared, stage: StageId, bytes: u64) -> Result<ProbeResult> {
    if stage == 0 || stage as usize > shared.stages.len() {
        // h: a chain with no kernel has no stage to probe, and the controller does not ask.
        return Err(MorunaError::Plan(format!(
            "stage {stage} cannot be probed: the chain has {} kernel stages",
            shared.stages.len()
        )));
    }
    if shared
        .control_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(MorunaError::Plan(
            "another scheduler control operation is active".into(),
        ));
    }
    shared.probing.store(true, Ordering::SeqCst);
    shared.close_gate();
    // Every worker that was inside `apply` finishes first; after this nobody but the probing
    // worker runs anything, because `pick` returns None while the gate is up and a worker that
    // picked before it went up either claimed its slot before (and is waited for here) or is
    // refused the claim (`slots`).
    wait_for_quiet(shared);
    let outcome = probe_inner(shared, stage, bytes);
    shared.open_gate();
    shared.probing.store(false, Ordering::SeqCst);
    shared.control_active.store(false, Ordering::SeqCst);
    shared.unpark_all();
    outcome
}

fn probe_inner(shared: &Shared, stage: StageId, bytes: u64) -> Result<ProbeResult> {
    let inputs = &shared.stages[usize::from(stage) - 1].inputs;
    let held = inputs
        .iter()
        .position(|&q| shared.queue_count(q) != 0)
        .map(|port| port as u16);
    let fed_by_source = inputs.iter().enumerate().find_map(|(port, &q)| {
        shared
            .topology
            .source_of(q)
            .map(|source| (port as u16, source))
    });
    let port = match (held, fed_by_source) {
        (Some(port), _) => port,
        (None, Some((port, source))) => {
            read_for(shared, source, bytes)?;
            port
        }
        // A stage no source feeds, with nothing waiting for it, in a run whose sources have
        // nothing left to read: it has nothing to probe with, as a stage a source feeds does
        // (the controller seeds it from hints, 11 f.14). 0.6.3 called it "no morsel to probe
        // with", which the controller ended the run on, so a job whose source had nothing to
        // read failed at its first stage after a join.
        (None, None) if crate::source_drive::nothing_left(shared) => {
            return Err(MorunaError::Plan(
                "the source plan is exhausted; there is nothing to probe with".into(),
            ));
        }
        // Nothing is waiting and no source feeds it, so nothing can arrive while the others are
        // held off: the stage before it was probed and its morsel was skipped (RC f.2 seeds it).
        (None, None) => {
            return Err(MorunaError::Plan(format!(
                "stage {stage} has nothing to probe with: {NOTHING_REACHED}"
            )));
        }
    };
    let worker = probing_worker(shared, usize::from(stage) - 1);
    tracing::info!(target: "sched.probe", stage, port, bytes, worker, "probing");
    match shared.run_on_worker(worker, WorkerJob::Probe { stage, port })? {
        JobOutput::Probe(result) => Ok(result),
        // Nothing reached the stage: the morsel the stage before it was probed with was skipped
        // under the error policy, or, at a join, one of its inputs was. Like a plan with nothing
        // left to read, this is not a failure of the run, and the controller seeds the stage
        // from hints (RC f.2). It ended the run with "no morsel to probe with" until 2026-11-14,
        // so a bad first batch under `skip` failed a job it should only have dropped a batch of.
        JobOutput::Done => Err(MorunaError::Plan(format!(
            "stage {stage} has nothing to probe with: {NOTHING_REACHED}"
        ))),
    }
}

/// f.9: source `source`'s drive answers helper requests in `Init` too, which is when the
/// controller probes; it issues the read, waits on it and pushes the morsel into every queue the
/// source gives to, with the next sequence number.
fn read_for(shared: &Shared, source: u16, bytes: u64) -> Result<()> {
    let Some(end) = shared.sources.get(usize::from(source)) else {
        return Err(MorunaError::Plan(format!("the run has no source {source}")));
    };
    {
        let (reply, rx) = unbounded();
        end.helper_tx
            .send(HelperRequest { bytes, reply })
            .map_err(|_| MorunaError::Plan("the source drive is not running".into()))?;
        if let Some(unparker) = end
            .unparker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            unparker.unpark();
        }
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(result) => result?,
            Err(_) => {
                return Err(MorunaError::Plan(
                    "the source drive did not answer the probe's read request".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Wait until no worker is inside `apply`, bounded so a long kernel cannot wedge the call.
fn wait_for_quiet(shared: &Shared) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while shared.knobs.slots.busy() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}
