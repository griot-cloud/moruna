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

/// The control claim is an atomic lease, never a mutex held across kernel code or IO.
/// Original drive owners drain and acknowledge; timeout releases the gate without taking
/// ownership of any unfinished future or reactor completion.
struct Barrier<'a>(&'a Shared);
impl<'a> Barrier<'a> {
    fn enter(shared: &'a Shared) -> Result<Self> {
        if shared
            .control_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(MorunaError::Resume(
                "another scheduler control operation is active".into(),
            ));
        }
        let barrier = Self(shared);
        shared.close_gate();
        for end in &shared.sources {
            end.checkpoint_paused.store(false, Ordering::SeqCst);
        }
        for end in &shared.sinks {
            end.checkpoint_paused.store(false, Ordering::SeqCst);
        }
        shared.checkpoint_pause.store(true, Ordering::SeqCst);
        shared.unpark_all();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if shared.is_cancelled() || shared.exit_claimed.load(Ordering::SeqCst) {
                return Err(MorunaError::Cancelled);
            }
            let sources = shared
                .sources
                .iter()
                .all(|end| end.checkpoint_paused.load(Ordering::SeqCst));
            let sinks = shared.sinks.iter().all(|end| {
                end.drained.load(Ordering::SeqCst) || end.checkpoint_paused.load(Ordering::SeqCst)
            });
            if shared.knobs.slots.busy() == 0 && sources && sinks {
                return Ok(barrier);
            }
            if Instant::now() >= deadline {
                return Err(MorunaError::Resume(
                    "checkpoint barrier could not drain admitted work within 30 seconds".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for Barrier<'_> {
    fn drop(&mut self) {
        self.0.checkpoint_pause.store(false, Ordering::SeqCst);
        self.0.open_gate();
        self.0.control_active.store(false, Ordering::SeqCst);
        self.0.unpark_all();
    }
}

/// Why no manifest is written once a stage's finish has given a batch (f.12).
const FINISH_GAVE: &str = "a stage's finish gave batches made at the end of its input, which no \
                           manifest can record; the run's last manifest is the one before them";

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
        if shared.finish_gave.load(Ordering::SeqCst) {
            // f.12: a stage's finish gave batches no manifest can record; the last manifest
            // stays the one a resume starts from.
            continue;
        }
        match checkpoint_now(&shared) {
            Err(_) if shared.finish_gave.load(Ordering::SeqCst) => continue,
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
/// the run writes no manifests, and at once when the run is not live: only a live run has a
/// checkpoint thread to serve it. Gives up after `timeout` naming why; a run whose writes fail
/// has said so on its own already (f.12).
///
/// The refusal comes before the request is counted. Counted first, as it was, a request made
/// before `run` timed out and told its caller no manifest was written for it, and then stayed
/// outstanding: the checkpoint thread served it the moment it started, writing a manifest
/// nobody was waiting for, and the next caller's count was one off (SC-T21).
pub(crate) fn checkpoint_and_wait(shared: &Shared, timeout: Duration) -> Result<PathBuf> {
    if shared.checkpoint_enabled() && !shared.run_state().is_live() {
        return Err(MorunaError::Resume(format!(
            "no manifest can be written now: the run is {:?}, and only a running run has a \
             checkpoint thread",
            shared.run_state()
        )));
    }
    let Some(ticket) = request(shared) else {
        return Err(MorunaError::Resume(
            "checkpointing is off for this run".into(),
        ));
    };
    let deadline = Instant::now() + timeout;
    while shared.checkpoint_served.load(Ordering::SeqCst) < ticket {
        if shared.finish_gave.load(Ordering::SeqCst) {
            return Err(MorunaError::Resume(FINISH_GAVE.into()));
        }
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
    // f.6, f.12: never while a stage's instances finish, and never after one gave a batch.
    let _manifest = shared
        .manifest_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if shared.finish_gave.load(Ordering::SeqCst) {
        return Err(MorunaError::Resume(FINISH_GAVE.into()));
    }
    let started = Instant::now();
    let preparation = shared
        .manifest_preparation
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // Admission happens while the drives/workers can still release their working sets.
    let prepared_hook = preparation.map(|prepare| prepare()).transpose()?;
    let _barrier = if prepared_hook.is_some() && !shared.checkpoint_quiet.load(Ordering::SeqCst) {
        Some(Barrier::enter(shared)?)
    } else {
        None
    };
    if prepared_hook.is_some() {
        // Portable publication cannot replay a consumed, uncommitted sink input
        // after restoring stateful kernels. Commit its actual buffered output
        // while all writers are quiesced, then publish that genuine watermark
        // before either the sink state or placement lineage is snapshotted.
        for end in &shared.sinks {
            {
                let sink = end.sink.read().unwrap_or_else(|e| e.into_inner());
                sink.checkpoint_flush()?;
            }
            crate::sink_drive::update_watermark(shared, end);
        }
    }
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
    // Each watermark is read before its sink's state, so the state holds at least every file
    // at or below it: a file the sink commits in between is in the state and above the
    // watermark, which resume deletes and recomputes. Read the other way round, a file
    // committed in between was in neither, and its morsels were lost (MH 4.7, H5).
    let mut committed = Vec::with_capacity(shared.sinks.len());
    let mut sink_states = Vec::with_capacity(shared.sinks.len());
    for end in &shared.sinks {
        committed.push(
            end.committed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reached,
        );
        let sink = end.sink.read().unwrap_or_else(|e| e.into_inner());
        sink_states.push(sink.checkpoint()?);
    }
    let mut source_cursors = Vec::with_capacity(shared.sources.len());
    let mut issued = Vec::with_capacity(shared.sources.len());
    for end in &shared.sources {
        let (cursor, reads) = crate::source_drive::cursor(end);
        source_cursors.push(cursor);
        issued.push(reads);
    }
    let extras = CheckpointExtras {
        kernel_states,
        sink_states,
        committed: committed.clone(),
        source_cursors: source_cursors.clone(),
        issued,
    };
    // Nothing of this component is locked here, which is what preamble 4.2 requires.
    let path = shared.placement.checkpoint(&extras)?;
    let hook = shared
        .manifest_hook
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(hook) = prepared_hook.or(hook) {
        hook(&path)?;
    }
    let took = started.elapsed().as_micros() as u64;
    shared.checkpoints.fetch_add(1, Ordering::SeqCst);
    shared.last_checkpoint_us.store(took, Ordering::SeqCst);
    *shared
        .last_manifest
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(path.clone());
    tracing::debug!(
        target: "sched.checkpoint",
        committed = ?committed,
        cursors = ?source_cursors,
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

#[cfg(test)]
mod portable_barrier_tests {
    use super::*;
    use crate::tests::common::{RigBuilder, manifest_lock};
    use moruna_testkit::{FakePlacement, FakeSink};
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn admission_precedes_pause_and_publication_observes_original_drive_acknowledgements() {
        let _serial = manifest_lock();
        let rig = RigBuilder::new()
            .cfg(|cfg| cfg.checkpoint_enabled = true)
            .sink(FakeSink::new().resumable(true))
            .placement(FakePlacement::new().with_manifest_store())
            .stages(1)
            .go();
        let shared = Arc::downgrade(&rig.scheduler.shared);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        rig.scheduler.set_manifest_preparation(Arc::new(move || {
            let shared = shared.upgrade().unwrap();
            assert!(!shared.checkpoint_pause.load(Ordering::SeqCst));
            assert!(!shared.control_active.load(Ordering::SeqCst));
            assert_eq!(observed.fetch_add(1, Ordering::SeqCst), 0);
            let observed = observed.clone();
            Ok(Arc::new(move |_| {
                assert!(shared.checkpoint_pause.load(Ordering::SeqCst));
                assert!(shared.control_active.load(Ordering::SeqCst));
                assert_eq!(shared.knobs.slots.busy(), 0);
                assert!(
                    shared
                        .sources
                        .iter()
                        .all(|end| end.checkpoint_paused.load(Ordering::SeqCst))
                );
                assert!(
                    shared
                        .sinks
                        .iter()
                        .all(|end| end.checkpoint_paused.load(Ordering::SeqCst))
                );
                assert_eq!(observed.fetch_add(1, Ordering::SeqCst), 1);
                Err(MorunaError::Resume("test publication refused".into()))
            }))
        }));
        let error = checkpoint_now(&rig.scheduler.shared).unwrap_err();
        assert!(
            matches!(error, MorunaError::Resume(reason) if reason == "test publication refused")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!rig.scheduler.shared.checkpoint_pause.load(Ordering::SeqCst));
        assert!(!rig.scheduler.shared.control_active.load(Ordering::SeqCst));
        assert!(rig.scheduler.shared.last_manifest.lock().unwrap().is_none());
    }

    #[test]
    fn admission_refusal_never_closes_worker_or_drive_gates() {
        let rig = RigBuilder::new()
            .cfg(|cfg| cfg.checkpoint_enabled = true)
            .stages(1)
            .go();
        rig.scheduler.set_manifest_preparation(Arc::new(|| {
            Err(MorunaError::Resume("no charged reservation".into()))
        }));
        assert!(checkpoint_now(&rig.scheduler.shared).is_err());
        assert!(!rig.scheduler.shared.checkpoint_pause.load(Ordering::SeqCst));
        assert!(!rig.scheduler.shared.control_active.load(Ordering::SeqCst));
        assert_eq!(rig.scheduler.shared.checkpoints.load(Ordering::SeqCst), 0);
    }
}
