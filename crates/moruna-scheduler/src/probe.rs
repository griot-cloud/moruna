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

/// The probing worker: worker 0, the one left running while the rest are held off.
const PROBING_WORKER: u16 = 0;

/// `Prober::probe(stage, bytes)` (f.9).
pub(crate) fn probe(shared: &Shared, stage: StageId, bytes: u64) -> Result<ProbeResult> {
    if stage == 0 || stage as usize > shared.stages.len() {
        // h: a chain with no kernel has no stage to probe, and the controller does not ask.
        return Err(MorunaError::Plan(format!(
            "stage {stage} cannot be probed: the chain has {} kernel stages",
            shared.stages.len()
        )));
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
        (None, None) => 0,
    };
    tracing::info!(target: "sched.probe", stage, port, bytes, "probing");
    match shared.run_on_worker(PROBING_WORKER, WorkerJob::Probe { stage, port })? {
        JobOutput::Probe(result) => Ok(result),
        JobOutput::Done => Err(MorunaError::Plan(format!(
            "stage {stage} had no morsel to probe with"
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
