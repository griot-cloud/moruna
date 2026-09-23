//! The run state machine, `run`, `run_resumed`, `apply_resume_point` and `shutdown`
//! (e.2, f.7, f.10, f.13, SC-I9).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use moruna_kernel::{CancelToken, MorunaError, Result, ResumePoint, ResumePolicy, Sink, StageId};

use crate::checkpoint;
use crate::shared::{DRIVE_DRIVING, DRIVE_STOP, Exit, RunState, Shared, WorkerJob};
use crate::source_drive;

/// `run` and `run_resumed` (d.1, f.13). Both start the drives and the checkpoint thread, enter
/// `Running` and block until the completion condition holds or the run terminated or was
/// cancelled. Every run that entered `Running` returns `Ok(RunOutcome)` (SC-I9).
pub(crate) fn run(
    shared: &Arc<Shared>,
    cancel: CancelToken,
    resumed: bool,
) -> Result<crate::RunOutcome> {
    if shared.run_state() != RunState::Init {
        return Err(MorunaError::Plan(format!(
            "run was called in state {:?}; it is only valid once, from Init",
            shared.run_state()
        )));
    }
    if resumed && !shared.resumed.load(Ordering::SeqCst) {
        return Err(MorunaError::Resume(
            "run_resumed was called without apply_resume_point".into(),
        ));
    }
    *shared.token.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel);
    shared.set_run_state(RunState::Running);
    shared.source_mode.set(DRIVE_DRIVING);
    shared.sink_mode.set(DRIVE_DRIVING);
    checkpoint::start(shared);
    shared.unpark_all();

    // The facade's runtime thread waits here. It polls rather than blocking on the condvar
    // alone, because a cancel is a flag on a token the surface owns and not an event of ours.
    loop {
        if let Some(exit) = shared.take_exit() {
            return Ok(finalize(shared, exit));
        }
        if shared.is_cancelled() {
            crate::policy::cancel(shared);
        }
        if shared.source_exhausted.load(Ordering::SeqCst) && shared.run_state() == RunState::Running
        {
            shared.set_run_state(RunState::Draining);
        }
        shared.advance_closes();
        let guard = shared.exit.lock().unwrap_or_else(|e| e.into_inner());
        let _ = shared
            .exit_signal
            .wait_timeout(guard, Duration::from_millis(2));
    }
}

/// The exit sequence every ending shares (preamble 4.3, e.2, f.10).
fn finalize(shared: &Arc<Shared>, exit: Exit) -> crate::RunOutcome {
    match &exit {
        Exit::Completed(_) => shared.set_run_state(RunState::Finishing),
        Exit::Terminated(_) => shared.set_run_state(RunState::Terminating),
        Exit::Cancelled => shared.set_run_state(RunState::Cancelling),
    }
    // The source drive stops issuing at once; the workers finish the task they are on.
    shared.source_mode.set(DRIVE_STOP);
    shared.stopping.store(true, Ordering::SeqCst);
    shared.unpark_all();
    // The engine cancels its in-flight moves and releases tier accounting; this also releases
    // every worker waiting in `pop_blocking` (preamble 4.3).
    shared.placement.shutdown();
    join_workers(shared);
    shared.sink_mode.set(DRIVE_STOP);
    shared.unpark_all();
    join_drives(shared);

    // f.12: on termination and cancellation the scheduler writes the final manifest itself,
    // from the thread that drives the exit, before the checkpoint thread is stopped, and on
    // completion as well when `checkpoint.keep` is set. Without that last case a run shorter
    // than one `checkpoint.interval_ms` had no tick, so `checkpoint.keep` kept an empty
    // directory and 12 f.7's promise that the report names a manifest was false.
    checkpoint::stop(shared);
    let final_manifest = shared.checkpoint_enabled()
        && (!matches!(exit, Exit::Completed(_)) || shared.cfg.checkpoint_keep);
    if final_manifest {
        let _ = checkpoint::checkpoint_now(shared);
    }
    let manifest = shared
        .last_manifest
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let _ = shared.trace.flush();

    match exit {
        Exit::Completed(sink) => {
            shared.set_run_state(RunState::Completed);
            crate::RunOutcome::Completed { sink }
        }
        Exit::Terminated(diagnostic) => {
            shared.set_run_state(RunState::Terminated);
            crate::RunOutcome::Terminated {
                diagnostic,
                manifest,
            }
        }
        Exit::Cancelled => {
            shared.set_run_state(RunState::Cancelled);
            crate::RunOutcome::Cancelled { manifest }
        }
    }
}

/// f.10: the source drive lets its in-flight reads resolve and the sink drive lets its
/// in-flight writes complete, then both threads end.
fn join_drives(shared: &Shared) {
    for slot in [&shared.source_handle, &shared.sink_handle] {
        let handle = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            shared.unpark_all();
            let _ = handle.join();
        }
    }
}

fn join_workers(shared: &Shared) {
    let handles: Vec<_> = {
        let mut slots = shared
            .worker_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        slots.iter_mut().filter_map(|slot| slot.take()).collect()
    };
    for handle in handles {
        shared.unpark_all();
        let _ = handle.join();
    }
}

/// Resume, step one (f.13). In `Init`, in this order: refuse when checkpointing is off, resume
/// the sink instead of opening it, set the cursor and the sequence counter, set the watermark,
/// rebuild the instance pools, and keep `to_recompute` for `run_resumed`. Nothing is read and
/// nothing is written.
pub(crate) fn apply_resume_point(shared: &Arc<Shared>, point: ResumePoint) -> Result<()> {
    if shared.run_state() != RunState::Init {
        return Err(MorunaError::Resume(
            "apply_resume_point is only valid before the run starts".into(),
        ));
    }
    if !shared.checkpoint_enabled() {
        // f.13: refuse before touching the sink. The message names the sink when a
        // non-resumable sink is the reason, and the staging directory otherwise.
        let sink_is_the_reason = {
            let sink = shared.sink.read().unwrap_or_else(|e| e.into_inner());
            sink.checkpoint().ok().flatten().is_none()
        };
        let reason = if sink_is_the_reason {
            "the sink does not track commits, so this run could not write its own manifests"
        } else if !shared.source.repeatable() {
            "the source is not repeatable, so this run could not write its own manifests"
        } else {
            "the staging directory is not available, so this run could not write its own manifests"
        };
        return Err(MorunaError::Resume(reason.into()));
    }

    let committed = point.extras.committed_seq;
    let state = point.extras.sink_state.clone().unwrap_or_default();
    {
        let schema = shared.schemas[0].clone();
        let mut sink = shared.sink.write().unwrap_or_else(|e| e.into_inner());
        sink.resume(&schema, &state, committed)?;
    }
    source_drive::set_cursor(shared, point.extras.source_cursor);
    shared
        .committed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .resumed_at(committed);

    restore_instances(shared, &point)?;

    *shared
        .to_recompute
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = point.to_recompute.clone();
    shared.resumed.store(true, Ordering::SeqCst);
    tracing::info!(
        target: "sched.resume",
        committed_seq = committed.unwrap_or_default(),
        to_recompute = point.to_recompute.len(),
        split_index = point.extras.source_cursor.split_index,
        row_offset = point.extras.source_cursor.row_offset,
        "resuming"
    );
    Ok(())
}

/// f.13: a `Checkpoint` stage's instances come back through `Kernel::restore` in instance order
/// on the owning worker; a `Reinit` stage runs the `init_instances` loop of f.4 for that stage.
/// `Forbid` was refused by `Placement::restore` before the scheduler was asked.
fn restore_instances(shared: &Arc<Shared>, point: &ResumePoint) -> Result<()> {
    shared.gate.store(true, Ordering::SeqCst);
    let outcome = restore_all(shared, point);
    shared.gate.store(false, Ordering::SeqCst);
    shared.unpark_all();
    outcome
}

fn restore_all(shared: &Arc<Shared>, point: &ResumePoint) -> Result<()> {
    let workers = shared.cfg.workers_max as usize;
    for index in 0..shared.stages.len() {
        let Some(max) = shared.stages[index].pool.as_ref().map(|pool| pool.max) else {
            continue;
        };
        let stage: StageId = shared.stages[index].stage;
        match shared.stages[index].kernel.hints().resume {
            ResumePolicy::Checkpoint => {
                for instance in 0..max {
                    let bytes = point
                        .extras
                        .kernel_states
                        .iter()
                        .find(|(at, slot, _)| *at == stage && *slot == instance)
                        .map(|(_, _, bytes)| bytes.clone())
                        .ok_or_else(|| {
                            MorunaError::Resume(format!(
                                "the manifest holds no state for stage {stage} instance {instance}"
                            ))
                        })?;
                    let worker = (instance % workers) as u16;
                    shared.run_on_worker(
                        worker,
                        WorkerJob::Restore {
                            stage_ix: index,
                            instance,
                            bytes,
                        },
                    )?;
                }
            }
            ResumePolicy::Reinit => {
                for instance in 0..max {
                    let worker = (instance % workers) as u16;
                    shared.run_on_worker(
                        worker,
                        WorkerJob::Init {
                            stage_ix: index,
                            instance,
                        },
                    )?;
                }
            }
            ResumePolicy::Forbid => {
                return Err(MorunaError::Resume(format!("stage {stage} forbids resume")));
            }
        }
    }
    Ok(())
}

/// f.10: set the cancel flag, unpark and join the workers, stop the drives and the checkpoint
/// thread. Idempotent; called from `Drop`, so dropping a scheduler that never ran costs a few
/// joins.
pub(crate) fn shutdown(shared: &Arc<Shared>) {
    if shared.shutdown_done.swap(true, Ordering::SeqCst) {
        return;
    }
    shared.cancel.store(true, Ordering::SeqCst);
    shared.stopping.store(true, Ordering::SeqCst);
    // The run has exited by the time `shutdown` returns, whether it ever started or not, so a
    // knob written afterwards is a no-op (f.15, RC h).
    if !matches!(
        shared.run_state(),
        RunState::Completed | RunState::Terminated | RunState::Cancelled
    ) {
        shared.set_run_state(RunState::Cancelled);
    }
    shared.source_mode.set(DRIVE_STOP);
    shared.sink_mode.set(DRIVE_STOP);
    // A worker or a drive may be inside `pop_blocking`; the engine's shutdown is what releases
    // it, and it is idempotent (contracts d.10).
    shared.placement.shutdown();
    shared.unpark_all();
    checkpoint::stop(shared);
    join_workers(shared);
    join_drives(shared);
    let _ = shared.trace.flush();
}
