//! The worker loop and the one place a trace record is built (e.1, f.2, SC-I1, SC-I4).
//!
//! A worker pops, applies, pushes and records. It never calls the reactor, never waits on a
//! `Completion` and never allocates payload memory: the only calls it makes into another
//! component are `Placement::pop`, `Placement::push`, `Kernel::apply` and `TraceSink::record`
//! (SC-I1). It holds no scheduler lock across any of them, and it does not block outside
//! `Kernel::apply`: see the note on the claim in `run_task`.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use amoru_kernel::{
    AmoruError, KernelState, Locality, Morsel, MorselFeatures, NoState, Outcome, Payload,
    PayloadSpec, PlacementStats, ProbeResult, Result, Sample, Seq, StageId, TIER_COUNT, Tier,
    TraceRecord,
};
use crossbeam::channel::Receiver;
use crossbeam::sync::Parker;

use crate::cputime::{now_ns, thread_cpu_ns};
use crate::instances;
use crate::shared::{JobOutput, JobRequest, Shared, WorkerJob};

/// The per-worker loop of e.1: `Parked` -> `Idle` -> `Running(stage, seq)` -> `Idle`.
pub(crate) fn worker_loop(
    shared: Arc<Shared>,
    worker: u16,
    jobs: Receiver<JobRequest>,
    parker: Parker,
) {
    loop {
        shared.heartbeat.touch(worker);
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        if let Ok(request) = jobs.try_recv() {
            #[cfg(test)]
            if matches!(request.job, WorkerJob::Die) {
                // SC-T19's seam: leave the loop without reporting, so the heartbeat checker
                // has a death to find and the run ends with a diagnostic rather than a hang.
                return;
            }
            let outcome = run_job(&shared, worker, request.job);
            let _ = request.reply.send(outcome);
            continue;
        }
        if !active_slot(&shared, worker) || shared.gate.load(Ordering::SeqCst) {
            idle(&shared, worker, &parker);
            continue;
        }
        let Some(stage) = crate::pick::pick(&shared, worker) else {
            idle(&shared, worker, &parker);
            continue;
        };
        match run_task(&shared, worker, stage, false) {
            Ok(_) | Err(AmoruError::Cancelled) => {}
            Err(e) => crate::policy::terminate(&shared, e),
        }
        shared.advance_closes();
    }
    shared.heartbeat.exited_clean(worker);
}

fn idle(shared: &Shared, worker: u16, parker: &Parker) {
    shared.heartbeat.parked(worker, true);
    parker.park_timeout(Duration::from_millis(1));
    shared.heartbeat.parked(worker, false);
    shared.heartbeat.touch(worker);
}

/// SC-I7: lowering `active_workers` parks workers only between tasks, and a parked worker holds
/// no morsel and no instance, because parking happens at the loop head and nowhere else.
fn active_slot(shared: &Shared, worker: u16) -> bool {
    // f.1: the pool is spawned parked and takes no task until `run`, so `init_instances` and
    // every probe run alone on the worker they were given (f.4, f.9).
    if !shared.run_state().is_live() {
        return false;
    }
    worker < shared.knobs.active_workers()
}

fn run_job(shared: &Shared, worker: u16, job: WorkerJob) -> Result<JobOutput> {
    match job {
        WorkerJob::Init { stage_ix, instance } => {
            instances::run_init(shared, worker, stage_ix, instance)
        }
        WorkerJob::Restore {
            stage_ix,
            instance,
            bytes,
        } => instances::run_restore(shared, worker, stage_ix, instance, &bytes),
        WorkerJob::Probe { stage } => {
            run_task(shared, worker, stage, true).map(|result| match result {
                Some(probe) => JobOutput::Probe(probe),
                None => JobOutput::Done,
            })
        }
        #[cfg(test)]
        WorkerJob::Die => Ok(JobOutput::Done),
    }
}

/// How long the probe waits for the morsel the source drive is pushing for it (f.9).
const PROBE_POP_TIMEOUT: Duration = Duration::from_secs(5);

/// Take the head of the input queue without blocking, for the reason `run_task` gives. A probe
/// retries for a bounded time, because its input is a morsel another thread has just been asked
/// to produce and the call must be deterministic (f.9).
fn claim_and_pop(
    shared: &Shared,
    stage: StageId,
    want: PayloadSpec,
    probing: bool,
) -> Result<Option<(Morsel, u64)>> {
    let started = std::time::Instant::now();
    loop {
        if let Some(morsel) = shared.placement.pop(stage - 1, want, Locality::Any)? {
            // PL-I9's figure is the microseconds the caller waited on a move. A worker no longer
            // waits at all, so the only wait this can report is a probe's.
            let waited = if probing {
                started.elapsed().as_micros() as u64
            } else {
                0
            };
            return Ok(Some((morsel, waited)));
        }
        if !probing || started.elapsed() >= PROBE_POP_TIMEOUT {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// What `apply` did, before the policy sees it.
enum Applied {
    Produced(Payload),
    Failed(AmoruError),
}

/// One task: f.2's body. With `probing` it is also f.9's body, which is the same work with a
/// peak reset in front, a sample taken before the pop and a `Probe` outcome on the record.
pub(crate) fn run_task(
    shared: &Shared,
    worker: u16,
    stage: StageId,
    probing: bool,
) -> Result<Option<ProbeResult>> {
    let stage_ix = match (stage as usize).checked_sub(1) {
        Some(index) if index < shared.stages.len() => index,
        _ => {
            return Err(AmoruError::Plan(format!(
                "stage {stage} is not a kernel stage"
            )));
        }
    };
    let entry = &shared.stages[stage_ix];

    // f.9: the probe resets the peak and samples before it pops, so the peak it reports is its
    // own; a normal task samples immediately before `apply` (f.2).
    let mut s0 = Sample::default();
    if probing {
        shared.sampler.reset_peak();
        s0 = shared.sampler.sample();
    }

    // f.7: the claim on the stage is taken before the pop, not after it. The close cascade closes
    // a queue when its producer's input is closed and empty and the producer has no task running,
    // and a claim taken after the pop leaves a window in which both look true while this worker
    // already holds a morsel: the cascade would then close the output queue under a morsel about
    // to be pushed into it and the run would complete with that morsel stranded (SC-I9, SC-T9).
    // The claim therefore brackets the pop, so the pop must not block. That is why this is the
    // contract's non-blocking `pop`: f.3 only ever offers a stage whose head is already resident,
    // and a `None` here is the same race f.4 describes, answered by re-picking. Reported to the
    // PM as a finding on f.2, which names `pop_blocking`.
    entry.running.fetch_add(1, Ordering::SeqCst);
    let popped = claim_and_pop(shared, stage, entry.spec, probing);
    let Some((morsel, wait_us)) = popped.inspect_err(|_| {
        entry.running.fetch_sub(1, Ordering::SeqCst);
    })?
    else {
        entry.running.fetch_sub(1, Ordering::SeqCst);
        return Ok(None);
    };

    let held = match instances::acquire(shared, stage_ix, worker) {
        Ok(Some(held)) => Some(held),
        Ok(None) if entry.pool.is_some() => {
            // f.4: the stage was admissible only if a slot was free, so this is a race, not a
            // policy. The morsel goes back and the worker re-picks.
            let _ = shared.placement.push(stage - 1, morsel);
            entry.running.fetch_sub(1, Ordering::SeqCst);
            return Ok(None);
        }
        Ok(None) => None,
        Err(e) => {
            let _ = shared.placement.push(stage - 1, morsel);
            entry.running.fetch_sub(1, Ordering::SeqCst);
            return Err(e);
        }
    };

    let Morsel {
        seq,
        stage: in_stage,
        payload,
        bytes: bytes_in,
        origin,
        features,
    } = morsel;
    let rows_in = features.rows;
    let tier_in = payload.tier().index() as u8;

    shared.workers_busy.fetch_add(1, Ordering::SeqCst);
    shared.heartbeat.start_task(worker, stage, seq);
    let q_before = tier_totals(&shared.placement_stats());

    let (instance_index, mut state_box) = match held {
        Some(held) => (held.index, Some(held.state)),
        None => (usize::MAX, None),
    };
    let mut no_state = NoState;

    if !probing {
        s0 = shared.sampler.sample();
    }
    let t0 = now_ns();
    let c0 = thread_cpu_ns();
    let raw = {
        let state: &mut dyn KernelState = match state_box.as_mut() {
            Some(state) => state.as_mut(),
            None => &mut no_state,
        };
        let kernel = entry.kernel.clone();
        std::panic::catch_unwind(AssertUnwindSafe(move || kernel.apply(state, payload)))
    };
    let c1 = thread_cpu_ns();
    let t1 = now_ns();
    let s1 = shared.sampler.sample();
    shared.heartbeat.end_task(worker);

    // f.2: read the footprint while the instance is still held.
    let state_bytes = state_box
        .as_ref()
        .and_then(|state| state.footprint())
        .unwrap_or(0);

    let applied = match raw {
        Ok(Ok(out)) => Applied::Produced(out),
        Ok(Err(e)) => Applied::Failed(e),
        // l: a panicking kernel's state is retired, and the panic becomes a `Kernel` error
        // whose message starts with `panic:` (f.8).
        Err(panic) => Applied::Failed(AmoruError::Kernel {
            stage,
            seq,
            msg: format!("panic: {}", panic_message(panic.as_ref())),
            features: Some(features.clone()),
        }),
    };
    let retire = matches!(applied, Applied::Failed(_));
    if let Some(state) = state_box.take() {
        instances::release(
            shared,
            stage_ix,
            instances::Held {
                index: instance_index,
                state,
            },
            retire,
        );
    }

    let mut rows_out = 0;
    let mut bytes_out = 0;
    let mut tier_out = tier_in;
    let mut error = None;
    let mut failure = None;
    let mut push_error = None;
    match applied {
        Applied::Produced(out) => {
            rows_out = out.rows();
            bytes_out = out.bytes();
            tier_out = out.tier().index() as u8;
            let out_morsel = Morsel::new(seq, in_stage.saturating_add(1), out, origin);
            if let Err(e) = shared.placement.push(stage, out_morsel) {
                push_error = Some(e);
            } else {
                entry.tasks.fetch_add(1, Ordering::SeqCst);
                entry
                    .busy_ns
                    .fetch_add(t1.saturating_sub(t0), Ordering::SeqCst);
            }
        }
        Applied::Failed(e) => {
            error = Some(e.to_string());
            failure = Some(enrich(e, stage, seq, &features));
        }
    }

    let q_after = tier_totals(&shared.placement_stats());
    let mut record = build_record(
        shared,
        RecordInput {
            seq,
            stage,
            worker,
            instance: instance_index,
            t0,
            t1,
            rows_in,
            bytes_in,
            rows_out,
            bytes_out,
            tier_in,
            tier_out,
            features: &features,
            s0,
            s1,
            cpu_ns: c1.saturating_sub(c0),
            q_before,
            q_after,
            miss_wait_us: wait_us,
            state_bytes,
            outcome: if failure.is_some() {
                Outcome::Error
            } else {
                Outcome::Ok
            },
            error,
        },
    );

    // f.8: a device out of memory is the one error that does not reach the policy first. Its
    // record goes to the hook alone, so the trace still sees exactly one record per morsel and
    // stage (SC-I4, TR-I2), and the controller shrinks the stage's device footprint inside it.
    let device_oom = matches!(
        failure,
        Some(AmoruError::Alloc {
            tier: Tier::Device(_),
            ..
        })
    );
    if device_oom && let Some(hook) = shared.record_hook() {
        hook(&record);
    }

    if let Some(e) = failure {
        entry.errors.fetch_add(1, Ordering::SeqCst);
        record.outcome = crate::policy::apply_policy(shared, stage_ix, seq, e);
    }
    if probing {
        record.outcome = Outcome::Probe;
    }

    entry.running.fetch_sub(1, Ordering::SeqCst);
    shared.workers_busy.fetch_sub(1, Ordering::SeqCst);
    emit(shared, record);

    if let Some(e) = push_error {
        return Err(e);
    }
    if probing {
        return Ok(Some(ProbeResult {
            bytes_in,
            rows_in,
            peak_delta: s1.peak_anon_bytes.saturating_sub(s0.anon_bytes),
            dev_peak_delta: s1.device_used[0].saturating_sub(s0.device_used[0]),
            wall_ns: t1.saturating_sub(t0),
            cpu_ns: c1.saturating_sub(c0),
        }));
    }
    Ok(None)
}

/// CT-I10: the diagnostic names the morsel, its stage and its features.
fn enrich(e: AmoruError, stage: StageId, seq: Seq, features: &MorselFeatures) -> AmoruError {
    match e {
        AmoruError::Kernel { msg, .. } => AmoruError::Kernel {
            stage,
            seq,
            msg,
            features: Some(features.clone()),
        },
        other => other,
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = panic.downcast_ref::<&'static str>() {
        return (*text).to_string();
    }
    if let Some(text) = panic.downcast_ref::<String>() {
        return text.clone();
    }
    "a kernel panicked".to_string()
}

/// One record to the trace, then the controller's hook on the same thread (f.2).
pub(crate) fn emit(shared: &Shared, record: TraceRecord) {
    shared.trace.record(record.clone());
    if let Some(hook) = shared.record_hook() {
        hook(&record);
    }
}

/// Bytes per tier over every queue, for `q_bytes_before` and `q_bytes_after` (d.13). The trace
/// field names queue bytes and the run report reads the pair as a delta, so the totals over
/// every queue are what makes `staging_bytes_delta` the change one task caused.
pub(crate) fn tier_totals(stats: &PlacementStats) -> Vec<u64> {
    let mut totals = vec![0u64; TIER_COUNT];
    for queue in &stats.queues {
        for (index, bytes) in queue.bytes_by_tier.iter().enumerate() {
            if let Some(slot) = totals.get_mut(index) {
                *slot += bytes;
            }
        }
    }
    totals
}

/// Everything one record is built from; gathered by the caller so the builder has no policy.
pub(crate) struct RecordInput<'a> {
    pub(crate) seq: Seq,
    pub(crate) stage: StageId,
    pub(crate) worker: u16,
    pub(crate) instance: usize,
    pub(crate) t0: u64,
    pub(crate) t1: u64,
    pub(crate) rows_in: u64,
    pub(crate) bytes_in: u64,
    pub(crate) rows_out: u64,
    pub(crate) bytes_out: u64,
    pub(crate) tier_in: u8,
    pub(crate) tier_out: u8,
    pub(crate) features: &'a MorselFeatures,
    pub(crate) s0: Sample,
    pub(crate) s1: Sample,
    pub(crate) cpu_ns: u64,
    pub(crate) q_before: Vec<u64>,
    pub(crate) q_after: Vec<u64>,
    pub(crate) miss_wait_us: u64,
    pub(crate) state_bytes: u64,
    pub(crate) outcome: Outcome,
    pub(crate) error: Option<String>,
}

/// The only builder of a `TraceRecord` in this crate (l, anti-patterns); the probe reaches it
/// through `run_task`.
pub(crate) fn build_record(shared: &Shared, input: RecordInput<'_>) -> TraceRecord {
    let disk = amoru_kernel::TierKind::Disk.index();
    let staging_delta = input.q_after.get(disk).copied().unwrap_or(0) as i64
        - input.q_before.get(disk).copied().unwrap_or(0) as i64;
    TraceRecord {
        seq: input.seq,
        stage: input.stage,
        worker: input.worker,
        instance: u16::try_from(input.instance).unwrap_or(u16::MAX),
        t_start_ns: input.t0,
        t_end_ns: input.t1,
        rows_in: input.rows_in,
        bytes_in: input.bytes_in,
        rows_out: input.rows_out,
        bytes_out: input.bytes_out,
        tier_in: input.tier_in,
        tier_out: input.tier_out,
        feat_mean_string_len: input.features.mean_string_len.unwrap_or(0.0),
        feat_null_ratio: input.features.null_ratio.unwrap_or(0.0),
        feat_column_bytes: input.features.column_bytes.clone(),
        knob_morsel_target: shared.knobs.morsel_target(input.stage),
        knob_active_workers: shared.knobs.active_workers(),
        knob_read_ahead: shared.knobs.read_ahead(),
        mem_anon_before: input.s0.anon_bytes,
        mem_anon_peak: input.s0.anon_bytes.max(input.s1.anon_bytes),
        dev_mem_peak: input.s1.device_used[0],
        cpu_time_us: input.cpu_ns / 1_000,
        throttled_delta_us: input.s1.throttled_us.saturating_sub(input.s0.throttled_us),
        q_bytes_before: input.q_before,
        q_bytes_after: input.q_after,
        staging_bytes_delta: staging_delta,
        placement_miss_wait_us: input.miss_wait_us,
        state_bytes: input.state_bytes,
        // The sizer is the controller's choice and no knob carries it to the scheduler; the
        // rule sizer is the default of the whole configuration table.
        sizer: 0,
        outcome: input.outcome,
        error: input.error,
    }
}
