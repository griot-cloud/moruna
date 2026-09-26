//! The checkpoint thread and `checkpoint_now` (f.12).
//!
//! A dedicated thread, not a worker and not a reactor thread, because `KernelState::checkpoint`
//! may run kernel code. It never holds the stage table lock across the placement call: that is
//! the deadlock the lock order of preamble 4.2 was written to prevent, so `checkpoint_now`
//! gathers what it needs through the instance pool locks, drops every one of them, and only
//! then calls `Placement::checkpoint`, which writes the manifest on this thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use moruna_kernel::{CheckpointExtras, MorunaError, Result, ResumePolicy, Sink, StageId};

use crate::instances;
use crate::shared::Shared;

/// How many manifest writes may fail in a row before the run ends (f.12, placement h).
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// The thread f.12 starts on entering `Running` and stops on leaving `Running` or `Draining`.
pub(crate) fn thread(shared: Arc<Shared>) {
    let interval = Duration::from_millis(shared.cfg.checkpoint_interval_ms);
    let mut failures = 0u32;
    loop {
        let deadline = Instant::now() + interval;
        while Instant::now() < deadline {
            if shared.checkpoint_stop.load(Ordering::SeqCst) {
                return;
            }
            // MH 4.3 `checkpoint`: a request ends the wait at once, so a host about to destroy
            // the machine gets a manifest now rather than at the end of the interval.
            if requested(&shared) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1).min(interval));
        }
        if shared.checkpoint_stop.load(Ordering::SeqCst) {
            return;
        }
        crate::heartbeat::check(&shared);
        if !shared.run_state().is_live() {
            continue;
        }
        // Read before the write starts, so a request that arrives during it is served by the
        // next one and never by a manifest whose snapshot is older than the request.
        let serving = shared.checkpoint_requested.load(Ordering::SeqCst);
        match checkpoint_now(&shared) {
            Ok(_) => {
                failures = 0;
                shared
                    .checkpoint_served
                    .fetch_max(serving, Ordering::SeqCst);
            }
            Err(MorunaError::Cancelled) => return,
            Err(e) => {
                failures += 1;
                tracing::debug!(target: "sched.checkpoint", failures, error = %e, "a manifest write failed");
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    crate::policy::terminate(&shared, e);
                    return;
                }
            }
        }
    }
}

/// True when an on-demand manifest has been asked for and not yet written.
fn requested(shared: &Shared) -> bool {
    shared.checkpoint_requested.load(Ordering::SeqCst)
        > shared.checkpoint_served.load(Ordering::SeqCst)
}

/// Ask for a manifest now and return the request's number (MH 4.3 `checkpoint`). The write
/// happens on the checkpoint thread, as every other one does (f.12), so the lock order and the
/// "never a worker" rule are unchanged; `None` when this run writes no manifests.
pub(crate) fn request(shared: &Shared) -> Option<u64> {
    if !shared.checkpoint_enabled() {
        return None;
    }
    Some(shared.checkpoint_requested.fetch_add(1, Ordering::SeqCst) + 1)
}

/// Request a manifest and wait until one started after the request is on disk (MH 4.7): the
/// entry point the host protocol's `checkpoint` message calls through the facade. Refuses when
/// the run writes no manifests, and gives up after `timeout` naming why: a run that is not in
/// `Running` has no checkpoint thread to serve it, and one whose writes fail has said so on
/// its own already (f.12).
pub(crate) fn checkpoint_and_wait(shared: &Shared, timeout: Duration) -> Result<PathBuf> {
    let Some(ticket) = request(shared) else {
        return Err(MorunaError::Resume(
            "checkpointing is off for this run".into(),
        ));
    };
    let deadline = Instant::now() + timeout;
    while shared.checkpoint_served.load(Ordering::SeqCst) < ticket {
        if Instant::now() >= deadline {
            return Err(MorunaError::Resume(format!(
                "no manifest was written within {} ms of the request; the run is {:?}",
                timeout.as_millis(),
                shared.run_state()
            )));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    shared
        .last_manifest
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| MorunaError::Resume("the manifest path was not recorded".into()))
}

/// Gather the pieces the placement engine does not own and write the manifest (f.12).
///
/// No lock of the scheduler's is held when `Placement::checkpoint` is called: the instance
/// pools are locked and released one instance at a time inside `checkpoint_states`, and the
/// stage table lock is never taken here at all.
pub(crate) fn checkpoint_now(shared: &Shared) -> Result<PathBuf> {
    if !shared.checkpoint_enabled() {
        return Err(MorunaError::Resume(
            "checkpointing is off for this run".into(),
        ));
    }
    let started = Instant::now();
    let mut kernel_states: Vec<(StageId, usize, Vec<u8>)> = Vec::new();
    for (index, entry) in shared.stages.iter().enumerate() {
        if entry.kernel.hints().resume != ResumePolicy::Checkpoint {
            continue;
        }
        // f.12: a `Checkpoint` kernel whose `checkpoint` returns `Ok(None)` is a kernel bug;
        // the run terminates with `Resume` naming the stage.
        let states = match instances::checkpoint_states(shared, index) {
            Ok(states) => states,
            Err(e) => {
                crate::policy::terminate(shared, e);
                return Err(MorunaError::Resume(format!(
                    "stage {} could not be checkpointed",
                    entry.stage
                )));
            }
        };
        for (instance, bytes) in states {
            kernel_states.push((entry.stage, instance, bytes));
        }
    }
    // The watermark is read before the sink's state, so the state holds at least every file
    // at or below it: a file the sink commits in between is in the state and above the
    // watermark, which resume deletes and recomputes. Read the other way round, a file
    // committed in between was in neither, and its morsels were lost (MH 4.7, H5).
    let committed_seq = shared
        .committed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reached;
    let sink_state = {
        let sink = shared.sink.read().unwrap_or_else(|e| e.into_inner());
        sink.checkpoint()?
    };
    let (source_cursor, issued) = crate::source_drive::cursor(shared);
    let extras = CheckpointExtras {
        kernel_states,
        sink_state,
        committed_seq,
        source_cursor,
        issued,
    };
    // Nothing of this component is locked here, which is what preamble 4.2 requires.
    let path = shared.placement.checkpoint(&extras)?;
    let took = started.elapsed().as_micros() as u64;
    shared.checkpoints.fetch_add(1, Ordering::SeqCst);
    shared.last_checkpoint_us.store(took, Ordering::SeqCst);
    *shared
        .last_manifest
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(path.clone());
    tracing::debug!(
        target: "sched.checkpoint",
        committed_seq = committed_seq.unwrap_or_default(),
        split_index = source_cursor.split_index,
        row_offset = source_cursor.row_offset,
        duration_us = took,
        "manifest written"
    );
    Ok(path)
}

/// Start the thread on entering `Running` (f.12).
pub(crate) fn start(shared: &Arc<Shared>) {
    if !shared.checkpoint_enabled() {
        return;
    }
    shared.checkpoint_stop.store(false, Ordering::SeqCst);
    let mut slot = shared
        .checkpoint_handle
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return;
    }
    let thread_shared = Arc::clone(shared);
    match std::thread::Builder::new()
        .name("moruna-checkpoint".into())
        .spawn(move || thread(thread_shared))
    {
        Ok(handle) => *slot = Some(handle),
        Err(e) => {
            drop(slot);
            crate::policy::terminate(
                shared,
                MorunaError::Resume(format!("the checkpoint thread could not be started: {e}")),
            );
        }
    }
}

/// Stop and join the thread on leaving `Running` or `Draining` (f.12, f.10).
pub(crate) fn stop(shared: &Shared) {
    shared.checkpoint_stop.store(true, Ordering::SeqCst);
    let handle = shared
        .checkpoint_handle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}
