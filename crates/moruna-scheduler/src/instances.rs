//! The stateful instance pools: eager creation, affinity, retirement and the resume path
//! (f.4, f.13, SC-I6).

use std::sync::atomic::Ordering;

use moruna_kernel::{DeviceId, InitCtx, KernelState, MorunaError, Result};

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
    shared.close_gate();
    let outcome = init_all(shared);
    shared.open_gate();
    shared.unpark_all();
    outcome
}

/// The worker that is instance `instance` of stage `stage_ix`'s home (f.4): the run's stateful
/// instances are dealt to the workers in turn, across stages, not each stage's from worker 0.
/// Counting each stage's instances from 0 gave every single-instance stage the same home,
/// worker 0, so stateful stages on separate branches ran one at a time (0.6.3: six branches of
/// a 0.5 s stateful function took 3.5 s, not 0.5 s).
pub(crate) fn home(shared: &Shared, stage_ix: usize, instance: usize) -> u16 {
    let pools = shared
        .stages
        .iter()
        .map(|stage| stage.pool.as_ref().map(|pool| pool.max));
    dealt_home(pools, stage_ix, instance, shared.cfg.workers_max as usize)
}

/// `home` over the stages' pool sizes (`None` for a stateless stage) and the workers' count.
fn dealt_home(
    pools: impl Iterator<Item = Option<usize>>,
    stage_ix: usize,
    instance: usize,
    workers: usize,
) -> u16 {
    let before: usize = pools.take(stage_ix).flatten().sum();
    ((before + instance) % workers.max(1)) as u16
}

fn init_all(shared: &Shared) -> Result<()> {
    for index in 0..shared.stages.len() {
        let Some(max) = shared.stages[index].pool.as_ref().map(|pool| pool.max) else {
            continue;
        };
        let started = std::time::Instant::now();
        for instance in 0..max {
            let worker = home(shared, index, instance);
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
    let _working = shared.work_for(shared.stages[stage_ix].stage);
    let state = shared.stages[stage_ix].kernel.init(&ctx)?;
    store(shared, stage_ix, instance, worker, state);
    Ok(JobOutput::Done)
}

/// The body of `WorkerJob::Finish` (f.6, contracts d.7): `Kernel::finish` for every live
/// instance of one stateful stage, in instance order, once every input of the stage has ended
/// and its last `apply` has returned; what each gives is pushed into the stage's outputs,
/// numbered from the stage's finish base (contracts d.16), and then the outputs close, so the
/// stages and sinks downstream take it like any other morsel. A failure ends the run, naming the
/// stage; the outputs close either way, so nothing waits for them.
pub(crate) fn run_finish(shared: &Shared, worker: u16, stage_ix: usize) -> Result<JobOutput> {
    if let Err(e) = finish_stage(shared, worker, stage_ix) {
        crate::policy::terminate(shared, e);
    }
    shared.close_outputs(stage_ix);
    shared.advance_closes();
    shared.unpark_all();
    Ok(JobOutput::Done)
}

fn finish_stage(shared: &Shared, worker: u16, stage_ix: usize) -> Result<()> {
    let entry = &shared.stages[stage_ix];
    let stage = entry.stage;
    let Some(pool) = entry.pool.as_ref() else {
        let _ = entry.finished.set(0);
        return Ok(());
    };
    // Every input has ended, so the numbers they carried are known: the finish base is one past
    // the highest (d.16).
    let base = shared.produced().base(stage).ok_or_else(|| {
        MorunaError::Plan(format!(
            "stage {stage}'s inputs ended and the numbers they carried are not known"
        ))
    })?;
    // f.12: no manifest is taken while the instances are finishing, and none after one gave a
    // batch; the checkpoint thread waits for this lock rather than for an instance.
    let _manifest = shared
        .manifest_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // The checkpoint barrier waits for busy slots, so the finish holds one like a task.
    let _slot = shared.knobs.slots.claim();
    let _working = shared.work_for(stage);
    let mut given: u64 = 0;
    for instance in 0..pool.max {
        let taken = {
            let mut slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
            match slots.get_mut(instance) {
                Some(slot) if slot.in_use => {
                    return Err(MorunaError::Plan(format!(
                        "stage {stage} instance {instance} was in use when the stage's input \
                         ended"
                    )));
                }
                Some(slot) if !slot.retired => {
                    slot.in_use = true;
                    slot.state.take()
                }
                _ => None,
            }
        };
        let Some(mut state) = taken else {
            continue;
        };
        let s0 = shared.sampler.sample()?;
        let q_before = crate::worker::tier_totals(&shared.placement_stats());
        let t0 = crate::cputime::now_ns();
        let c0 = crate::cputime::thread_cpu_ns();
        let kernel = entry.kernel.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            kernel.finish(state.as_mut())
        }));
        let c1 = crate::cputime::thread_cpu_ns();
        let t1 = crate::cputime::now_ns();
        let alloc = moruna_kernel::take_kernel_alloc();
        let retire = !matches!(outcome, Ok(Ok(_)));
        {
            let mut slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = slots.get_mut(instance) {
                slot.in_use = false;
                slot.retired = retire;
                slot.state = if retire { None } else { Some(state) };
            }
        }
        let seq = base.saturating_add(given);
        let payload = match outcome {
            Ok(Ok(Some(payload))) => payload,
            Ok(Ok(None)) => continue,
            Ok(Err(MorunaError::Kernel { msg, features, .. })) => {
                return Err(MorunaError::Kernel {
                    stage,
                    seq,
                    msg,
                    features,
                });
            }
            Ok(Err(other)) => return Err(other),
            Err(panic) => {
                return Err(MorunaError::Kernel {
                    stage,
                    seq,
                    msg: format!(
                        "panic in finish: {}",
                        crate::worker::panic_message(panic.as_ref())
                    ),
                    features: None,
                });
            }
        };
        // f.12: from here on no manifest is written; this batch has no origin to read it again
        // from.
        shared.finish_gave.store(true, Ordering::SeqCst);
        let rows = payload.rows();
        let bytes = payload.bytes();
        let tier = payload.tier().index() as u8;
        let morsel = moruna_kernel::Morsel::new(seq, stage, payload, FINISHED);
        crate::decide::admit_push(shared, &entry.outputs, morsel, None)?;
        given += 1;
        entry.rows_out.fetch_add(rows, Ordering::SeqCst);
        entry.bytes_out.fetch_add(bytes, Ordering::SeqCst);
        entry.tasks.fetch_add(1, Ordering::SeqCst);
        entry
            .busy_ns
            .fetch_add(t1.saturating_sub(t0), Ordering::SeqCst);
        for &q in &entry.outputs {
            shared.fed(q);
        }
        let s1 = shared.sampler.sample()?;
        let q_after = crate::worker::tier_totals(&shared.placement_stats());
        // SC-I4: the batch is a morsel of this stage, and leaves one record like every other.
        let record = crate::worker::build_record(
            shared,
            crate::worker::RecordInput {
                seq,
                stage,
                worker,
                instance,
                t0,
                t1,
                rows_in: 0,
                bytes_in: 0,
                rows_out: rows,
                bytes_out: bytes,
                tier_in: tier,
                tier_out: tier,
                features: &moruna_kernel::MorselFeatures::default(),
                s0,
                s1,
                cpu_ns: c1.saturating_sub(c0),
                q_before,
                q_after,
                miss_wait_us: 0,
                state_bytes: 0,
                outcome: moruna_kernel::Outcome::Ok,
                error: None,
                alloc,
            },
        );
        crate::worker::emit(shared, record);
    }
    // Before the outputs close: a sink that sees them closed knows every number they carried.
    let _ = entry.finished.set(given);
    Ok(())
}

/// The origin of a batch a stage's `finish` gave (contracts d.5): no split's rows, because it
/// was read from none. Its queues are a stage's, which the placement engine never recomputes
/// from an origin, and no manifest is written once one exists (f.12).
const FINISHED: moruna_kernel::Origin = moruna_kernel::Origin {
    split: moruna_kernel::SplitId::MAX,
    row_start: 0,
    row_end: 0,
    node: moruna_kernel::LOCAL_NODE,
};

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
    let _working = shared.work_for(shared.stages[stage_ix].stage);
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

/// Whether worker `w` may be handed a slot whose home is `owner` (f.4, SC-I6).
///
/// A slot's home is the worker `init_instances` built it on, and it stays that worker's for the
/// run. Another worker may borrow it only when its home cannot come for it: a slot with no home
/// yet, or one whose home is outside the active set the controller set. Anything looser and
/// eight workers share four instances round robin, which is the model thrash D1 accepted
/// instance pools to avoid.
fn borrowable(shared: &Shared, owner: Option<u16>) -> bool {
    match owner {
        None => true,
        Some(home) => home >= shared.knobs.active_workers(),
    }
}

/// True when `worker` may serve this stage: the stage is stateless, or it owns a free instance,
/// or a free instance's home cannot come for it (f.3's admissibility for a stateful stage).
pub(crate) fn admissible(shared: &Shared, stage_ix: usize, worker: u16) -> bool {
    let Some(pool) = shared.stages[stage_ix].pool.as_ref() else {
        return true;
    };
    let slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
    slots
        .iter()
        .any(|slot| !slot.in_use && (slot.owner == Some(worker) || borrowable(shared, slot.owner)))
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
    // f.4: prefer the instance this worker last held; else one whose home cannot come for it.
    let mine = slots
        .iter()
        .position(|slot| !slot.in_use && slot.owner == Some(worker));
    let chosen = match mine {
        Some(index) => index,
        None => match slots
            .iter()
            .position(|slot| !slot.in_use && borrowable(shared, slot.owner))
        {
            Some(index) => index,
            None => return Ok(None),
        },
    };
    let Some(slot) = slots.get_mut(chosen) else {
        return Ok(None);
    };
    slot.in_use = true;
    // A slot with no home yet takes this worker as its home; a borrow never moves one. Rewriting
    // the home on a borrow is what made an instance wander: one borrow moved it for good and the
    // worker that had been warming it lost the instance for the rest of the run (SC-I6, SC-T6).
    if slot.owner.is_none() {
        slot.owner = Some(worker);
    }
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
        let taken = loop {
            let mut slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
            let Some(slot) = slots.get_mut(instance) else {
                break Taken::Nothing;
            };
            if slot.retired {
                // Nothing to save: the slot is rebuilt by `init` on the next acquire (f.4).
                break Taken::Nothing;
            }
            if !slot.in_use {
                break match slot.state.take() {
                    Some(state) => {
                        slot.in_use = true;
                        Taken::Held(state)
                    }
                    None => Taken::Nothing,
                };
            }
            drop(slots);
            // MH 4.10: an instance waiting on a port may wait for minutes; its checkpoint is its
            // state as that `apply` began, which a resume restores before applying the morsel
            // again. Waiting it out would fail the manifest.
            if let Some(bytes) = shared.stages[stage_ix].kernel.waiting_state(instance) {
                break Taken::Waiting(bytes);
            }
            if std::time::Instant::now() >= deadline {
                return Err(MorunaError::Resume(format!(
                    "stage {} instance {instance} was busy for the whole checkpoint",
                    shared.stages[stage_ix].stage
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        let mut state = match taken {
            Taken::Nothing => continue,
            Taken::Waiting(bytes) => {
                out.push((instance, bytes));
                continue;
            }
            Taken::Held(state) => state,
        };
        let result = state.checkpoint();
        {
            let mut slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = slots.get_mut(instance) {
                slot.in_use = false;
                slot.state = Some(state);
            }
        }
        match result? {
            Some(bytes) => out.push((instance, bytes)),
            None => {
                return Err(MorunaError::Resume(format!(
                    "stage {} declares Checkpoint but instance {instance} saved nothing",
                    shared.stages[stage_ix].stage
                )));
            }
        }
    }
    Ok(out)
}

/// What the checkpoint found in one slot.
enum Taken {
    /// Nothing to save: no slot, a retired one, or one with no state.
    Nothing,
    /// The instance, acquired for the call.
    Held(Box<dyn KernelState>),
    /// The state of an instance waiting on a port, as its `apply` began (MH 4.10); its worker
    /// still holds it.
    Waiting(Vec<u8>),
}

#[cfg(test)]
mod tests {
    use super::dealt_home;

    #[test]
    fn single_instance_stages_on_separate_branches_get_separate_homes() {
        // A source, six stateful stages of one instance each, a join, a sink; eight workers.
        let pools = || {
            [
                None,
                Some(1),
                Some(1),
                Some(1),
                Some(1),
                Some(1),
                Some(1),
                None,
                None,
            ]
        };
        let homes: Vec<u16> = (1..=6)
            .map(|stage| dealt_home(pools().into_iter(), stage, 0, 8))
            .collect();
        assert_eq!(homes, [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_stages_instances_and_the_next_stages_continue_the_deal() {
        let pools = || [Some(3), None, Some(2)];
        let first: Vec<u16> = (0..3)
            .map(|i| dealt_home(pools().into_iter(), 0, i, 4))
            .collect();
        let second: Vec<u16> = (0..2)
            .map(|i| dealt_home(pools().into_iter(), 2, i, 4))
            .collect();
        assert_eq!((first, second), (vec![0, 1, 2], vec![3, 0]));
    }
}
