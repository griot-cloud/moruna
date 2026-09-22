//! The stateful instance pools: eager creation, affinity, retirement and the resume path
//! (f.4, f.13, SC-I6).

use std::sync::atomic::Ordering;

use amoru_kernel::{AmoruError, DeviceId, InitCtx, KernelState, Result};

use crate::shared::{JobOutput, Shared, WorkerJob};

/// How long `checkpoint_states` waits for one busy instance before it gives up (f.12).
const ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Which device this instance gets: round robin over the devices the host has, for a kernel
/// that says it allocates device memory (f.4). Without a device there is none.
fn device_for(shared: &Shared, index: usize, instance: usize) -> Option<DeviceId> {
    if !shared.stages[index].kernel.hints().uses_device_memory {
        return None;
    }
    Some(DeviceId(instance as u8))
}

/// `Kernel::init` for every instance of every stateful stage, in stage order, each on the worker
/// that will own it (f.4). The first `Err` stops the loop and is returned as it is, with nothing
/// read and nothing written (SC-T18).
pub(crate) fn init_instances(shared: &Shared) -> Result<()> {
    shared.gate.store(true, Ordering::SeqCst);
    let outcome = init_all(shared);
    shared.gate.store(false, Ordering::SeqCst);
    shared.unpark_all();
    outcome
}

fn init_all(shared: &Shared) -> Result<()> {
    let workers = shared.cfg.workers_max as usize;
    for index in 0..shared.stages.len() {
        let Some(max) = shared.stages[index].pool.as_ref().map(|pool| pool.max) else {
            continue;
        };
        let started = std::time::Instant::now();
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
        tracing::info!(
            target: "sched.init_instances",
            stage = shared.stages[index].stage,
            instances = max,
            duration_us = started.elapsed().as_micros() as u64,
            "instance pool filled"
        );
    }
    Ok(())
}

/// The body of `WorkerJob::Init`, run on the owning worker (f.4).
pub(crate) fn run_init(
    shared: &Shared,
    worker: u16,
    stage_ix: usize,
    instance: usize,
) -> Result<JobOutput> {
    let ctx = InitCtx {
        instance,
        device: device_for(shared, stage_ix, instance),
        alloc: shared.alloc.clone(),
    };
    let state = shared.stages[stage_ix].kernel.init(&ctx)?;
    store(shared, stage_ix, instance, worker, state);
    Ok(JobOutput::Done)
}

/// The body of `WorkerJob::Restore`, run on the owning worker (f.13).
pub(crate) fn run_restore(
    shared: &Shared,
    worker: u16,
    stage_ix: usize,
    instance: usize,
    bytes: &[u8],
) -> Result<JobOutput> {
    let ctx = InitCtx {
        instance,
        device: device_for(shared, stage_ix, instance),
        alloc: shared.alloc.clone(),
    };
    let state = shared.stages[stage_ix].kernel.restore(&ctx, bytes)?;
    store(shared, stage_ix, instance, worker, state);
    Ok(JobOutput::Done)
}

fn store(
    shared: &Shared,
    stage_ix: usize,
    instance: usize,
    worker: u16,
    state: Box<dyn KernelState>,
) {
    let Some(pool) = shared.stages[stage_ix].pool.as_ref() else {
        return;
    };
    let mut slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = slots.get_mut(instance) {
        slot.state = Some(state);
        slot.owner = Some(worker);
        slot.in_use = false;
        slot.retired = false;
    }
}

/// An instance held by one worker for the length of one task (SC-I6).
pub(crate) struct Held {
    pub(crate) index: usize,
    pub(crate) state: Box<dyn KernelState>,
}

/// Acquire an instance for `stage_ix`: the one this worker last used when it is free, else any
/// free one, else `None` and the worker re-picks (f.4).
pub(crate) fn acquire(shared: &Shared, stage_ix: usize, worker: u16) -> Result<Option<Held>> {
    let Some(pool) = shared.stages[stage_ix].pool.as_ref() else {
        return Ok(None);
    };
    let mut slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
    let mine = slots
        .iter()
        .position(|slot| !slot.in_use && slot.owner == Some(worker));
    let chosen = match mine {
        Some(index) => index,
        None => match slots.iter().position(|slot| !slot.in_use) {
            Some(index) => index,
            None => return Ok(None),
        },
    };
    let Some(slot) = slots.get_mut(chosen) else {
        return Ok(None);
    };
    slot.in_use = true;
    slot.owner = Some(worker);
    let retired = slot.retired;
    let taken = slot.state.take();
    drop(slots);

    // f.4: a retired slot is rebuilt by `kernel.init` on the next acquire, by the worker that
    // acquires it; that is the only `init` after `init_instances`.
    let state = match (taken, retired) {
        (Some(state), false) => state,
        (_, _) => {
            let ctx = InitCtx {
                instance: chosen,
                device: device_for(shared, stage_ix, chosen),
                alloc: shared.alloc.clone(),
            };
            match shared.stages[stage_ix].kernel.init(&ctx) {
                Ok(state) => state,
                Err(e) => {
                    release_empty(shared, stage_ix, chosen);
                    return Err(e);
                }
            }
        }
    };
    Ok(Some(Held {
        index: chosen,
        state,
    }))
}

/// Give the instance back, retiring it when the kernel errored or panicked (f.4).
pub(crate) fn release(shared: &Shared, stage_ix: usize, held: Held, retire: bool) {
    let Some(pool) = shared.stages[stage_ix].pool.as_ref() else {
        return;
    };
    let mut slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = slots.get_mut(held.index) {
        slot.in_use = false;
        slot.retired = retire;
        slot.state = if retire { None } else { Some(held.state) };
    }
}

fn release_empty(shared: &Shared, stage_ix: usize, index: usize) {
    let Some(pool) = shared.stages[stage_ix].pool.as_ref() else {
        return;
    };
    let mut slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = slots.get_mut(index) {
        slot.in_use = false;
        slot.retired = true;
        slot.state = None;
    }
}

/// Instances alive for `StageStats::instances_live` (j).
pub(crate) fn live(shared: &Shared, stage_ix: usize) -> u16 {
    let Some(pool) = shared.stages[stage_ix].pool.as_ref() else {
        return 0;
    };
    let slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
    slots.iter().filter(|slot| !slot.retired).count() as u16
}

/// `KernelState::checkpoint` for every instance of one `Checkpoint` stage, each instance
/// acquired for the call like a task so no `apply` runs on it concurrently (f.12, SC-I6).
///
/// The caller holds no lock of the scheduler's own while this runs, and none at all once it
/// returns: the manifest write that follows must not be made under the stage table lock
/// (preamble 4.2).
pub(crate) fn checkpoint_states(shared: &Shared, stage_ix: usize) -> Result<Vec<(usize, Vec<u8>)>> {
    let Some(pool) = shared.stages[stage_ix].pool.as_ref() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(pool.max);
    // f.12: each instance is acquired for the call like a task, so no `apply` runs on it while
    // it is being saved. Waiting is what makes that true; the bound is there so a kernel that
    // never returns costs a failed manifest write rather than a wedged checkpoint thread.
    let deadline = std::time::Instant::now() + ACQUIRE_TIMEOUT;
    for instance in 0..pool.max {
        let mut taken = loop {
            let mut slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
            let Some(slot) = slots.get_mut(instance) else {
                break None;
            };
            if slot.retired {
                // Nothing to save: the slot is rebuilt by `init` on the next acquire (f.4).
                break None;
            }
            if !slot.in_use {
                slot.in_use = true;
                break slot.state.take();
            }
            drop(slots);
            if std::time::Instant::now() >= deadline {
                return Err(AmoruError::Resume(format!(
                    "stage {} instance {instance} was busy for the whole checkpoint",
                    shared.stages[stage_ix].stage
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        let held = taken.is_some();
        let result = match taken.as_mut() {
            Some(state) => state.checkpoint(),
            None => Ok(None),
        };
        if held {
            let mut slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = slots.get_mut(instance) {
                slot.in_use = false;
                slot.state = taken;
            }
        }
        if !held {
            continue;
        }
        match result? {
            Some(bytes) => out.push((instance, bytes)),
            None => {
                return Err(AmoruError::Resume(format!(
                    "stage {} declares Checkpoint but instance {instance} saved nothing",
                    shared.stages[stage_ix].stage
                )));
            }
        }
    }
    Ok(out)
}
