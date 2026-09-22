//! The state every thread of the component shares: the stage table, the instance pools, the
//! run state machine, the knob cell and the thread handles (e.2, e.3, g).

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use amoru_kernel::{
    Allocator, AmoruError, CancelToken, Kernel, KernelKind, KernelState, PayloadSpec, Placement,
    PlacementStats, RecordHook, Result, Sampler, Seq, Sink, SinkSummary, Source, SourceSchema,
    Split, StageId, TraceSink,
};
use amoru_sinks::SinkHandle;
use crossbeam::channel::{Receiver, Sender, unbounded};
use crossbeam::sync::{Parker, Unparker};

use crate::heartbeat::HeartbeatTable;
use crate::knobs::KnobState;
use crate::pipeline::{Pipeline, SchedulerConfig, validate_chain};

/// Where the run is (e.2). Stored as an atomic code so any thread can read it without a lock.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum RunState {
    Init,
    Running,
    Draining,
    Finishing,
    Completed,
    Terminating,
    Terminated,
    Cancelling,
    Cancelled,
}

impl RunState {
    pub(crate) fn code(self) -> u8 {
        match self {
            RunState::Init => 0,
            RunState::Running => 1,
            RunState::Draining => 2,
            RunState::Finishing => 3,
            RunState::Completed => 4,
            RunState::Terminating => 5,
            RunState::Terminated => 6,
            RunState::Cancelling => 7,
            RunState::Cancelled => 8,
        }
    }

    pub(crate) fn from_code(code: u8) -> RunState {
        match code {
            1 => RunState::Running,
            2 => RunState::Draining,
            3 => RunState::Finishing,
            4 => RunState::Completed,
            5 => RunState::Terminating,
            6 => RunState::Terminated,
            7 => RunState::Cancelling,
            8 => RunState::Cancelled,
            _ => RunState::Init,
        }
    }

    /// True while workers may take tasks and the drives may drive.
    pub(crate) fn is_live(self) -> bool {
        matches!(self, RunState::Running | RunState::Draining)
    }
}

/// How a run ended, as the thread that noticed it reports the fact to `run` (e.2).
pub(crate) enum Exit {
    Completed(SinkSummary),
    Terminated(AmoruError),
    Cancelled,
}

/// One stateful instance and who owns it (e.3).
pub(crate) struct Slot {
    pub(crate) state: Option<Box<dyn KernelState>>,
    pub(crate) owner: Option<u16>,
    pub(crate) in_use: bool,
    pub(crate) retired: bool,
}

/// Exactly `max` instances of one stateful stage, all built by `init_instances` (e.3, f.4).
pub(crate) struct InstancePool {
    pub(crate) slots: Mutex<Vec<Slot>>,
    pub(crate) max: usize,
}

/// One kernel stage: the kernel, its instance pool and its counters (e.3).
pub(crate) struct StageEntry {
    pub(crate) stage: StageId,
    pub(crate) kernel: Arc<dyn Kernel>,
    pub(crate) spec: PayloadSpec,
    pub(crate) pool: Option<InstancePool>,
    pub(crate) out_bytes: AtomicU64,
    pub(crate) tasks: AtomicU64,
    pub(crate) busy_ns: AtomicU64,
    pub(crate) errors: AtomicU32,
    pub(crate) skipped: AtomicU32,
    pub(crate) running: AtomicU32,
}

/// A job the scheduler asks one named worker to run, because the SDD says the call must happen
/// on the worker that will own its result (f.4) or on the one probing worker (f.9).
pub(crate) enum WorkerJob {
    /// `Kernel::init` for one instance of one stage (f.4).
    Init { stage_ix: usize, instance: usize },
    /// `Kernel::restore` for one instance of one stage (f.13).
    Restore {
        stage_ix: usize,
        instance: usize,
        bytes: Vec<u8>,
    },
    /// One probe of one stage (f.9).
    Probe { stage: StageId },
    /// The SC-T19 seam: leave the loop without reporting (f.14).
    #[cfg(test)]
    Die,
}

/// What a worker job produced.
pub(crate) enum JobOutput {
    Done,
    Probe(amoru_kernel::ProbeResult),
}

pub(crate) struct JobRequest {
    pub(crate) job: WorkerJob,
    pub(crate) reply: Sender<Result<JobOutput>>,
}

/// A request the source drive services on behalf of another thread (b, f.9), so the requesting
/// thread never waits on a reactor completion itself.
pub(crate) struct HelperRequest {
    /// About this many payload bytes at the cursor.
    pub(crate) bytes: u64,
    pub(crate) reply: Sender<Result<()>>,
}

/// Where the source drive is and what it has issued (f.5).
pub(crate) struct Cursor {
    pub(crate) split_index: u32,
    pub(crate) row_offset: u64,
    pub(crate) next_seq: Seq,
}

/// The two drive threads are idle until `run` starts them driving (f.1).
pub(crate) const DRIVE_IDLE: u8 = 0;
pub(crate) const DRIVE_DRIVING: u8 = 1;
pub(crate) const DRIVE_STOP: u8 = 2;

/// Everything the workers, the drives, the checkpoint thread and the controller's calls share.
pub(crate) struct Shared {
    pub(crate) cfg: SchedulerConfig,
    pub(crate) source: Arc<dyn Source>,
    pub(crate) sink: RwLock<SinkHandle>,
    pub(crate) sink_spec: PayloadSpec,
    pub(crate) placement: Arc<dyn Placement>,
    pub(crate) alloc: Arc<dyn Allocator>,
    pub(crate) trace: Arc<dyn TraceSink>,
    pub(crate) sampler: Arc<dyn Sampler>,
    pub(crate) stages: Vec<StageEntry>,
    pub(crate) plan: Vec<Split>,
    pub(crate) schemas: Vec<SourceSchema>,
    pub(crate) knobs: KnobState,

    /// The stage table lock of preamble 4.2 position 1. Held only while the close cascade of
    /// f.7 runs; never across a call into another component, and never by the checkpoint
    /// thread, which is the deadlock preamble 4.2 exists to prevent.
    pub(crate) stage_table: Mutex<()>,
    /// Queue `k` has been closed; index 0..=n.
    pub(crate) closed: Vec<AtomicBool>,

    pub(crate) state: AtomicU8Cell,
    pub(crate) exit: Mutex<Option<Exit>>,
    pub(crate) exit_signal: Condvar,
    pub(crate) cancel: AtomicBool,
    pub(crate) token: Mutex<Option<CancelToken>>,
    pub(crate) stopping: AtomicBool,
    /// While true no worker picks a task: `init_instances` and the probe run alone (f.4, f.9).
    pub(crate) gate: AtomicBool,
    pub(crate) probing: AtomicBool,

    pub(crate) cursor: Mutex<Cursor>,
    pub(crate) to_recompute: Mutex<Vec<(Seq, amoru_kernel::Origin)>>,
    pub(crate) committed: Mutex<Option<Seq>>,
    pub(crate) last_manifest: Mutex<Option<std::path::PathBuf>>,

    pub(crate) jobs: Vec<Sender<JobRequest>>,
    pub(crate) job_inbox: Mutex<Vec<Option<Receiver<JobRequest>>>>,
    pub(crate) unparkers: Vec<Unparker>,
    pub(crate) parkers: Mutex<Vec<Option<Parker>>>,
    pub(crate) heartbeat: HeartbeatTable,
    pub(crate) worker_handles: Mutex<Vec<Option<JoinHandle<()>>>>,

    pub(crate) helper_tx: Sender<HelperRequest>,
    pub(crate) helper_rx: Mutex<Option<Receiver<HelperRequest>>>,
    pub(crate) source_handle: Mutex<Option<JoinHandle<()>>>,
    pub(crate) sink_handle: Mutex<Option<JoinHandle<()>>>,
    pub(crate) source_mode: AtomicU8Cell,
    pub(crate) sink_mode: AtomicU8Cell,
    pub(crate) source_unparker: Mutex<Option<Unparker>>,
    pub(crate) sink_unparker: Mutex<Option<Unparker>>,

    pub(crate) checkpoint_on: AtomicBool,
    pub(crate) checkpoint_stop: AtomicBool,
    pub(crate) checkpoint_handle: Mutex<Option<JoinHandle<()>>>,

    pub(crate) record_hook: RwLock<Option<RecordHook>>,
    pub(crate) stats_cache: Mutex<StatsCache>,

    pub(crate) workers_busy: AtomicU16,
    pub(crate) reads_in_flight: AtomicU16,
    pub(crate) writes_in_flight: AtomicU16,
    pub(crate) source_exhausted: AtomicBool,
    pub(crate) errors_total: AtomicU32,
    pub(crate) checkpoints: AtomicU64,
    pub(crate) last_checkpoint_us: AtomicU64,
    pub(crate) resumed: AtomicBool,
    pub(crate) recomputed: AtomicU64,
    pub(crate) shutdown_done: AtomicBool,
}

/// A `RunState` or a drive mode as one atomic byte.
pub(crate) struct AtomicU8Cell(std::sync::atomic::AtomicU8);

impl AtomicU8Cell {
    pub(crate) fn new(value: u8) -> AtomicU8Cell {
        AtomicU8Cell(std::sync::atomic::AtomicU8::new(value))
    }

    pub(crate) fn get(&self) -> u8 {
        self.0.load(Ordering::SeqCst)
    }

    pub(crate) fn set(&self, value: u8) {
        self.0.store(value, Ordering::SeqCst);
    }
}

/// `placement.stats()` cached for a millisecond, so a pick costs no lock of its own (f.3).
pub(crate) struct StatsCache {
    pub(crate) at: Instant,
    pub(crate) stats: PlacementStats,
    pub(crate) valid: bool,
}

impl Shared {
    pub(crate) fn build(
        cfg: SchedulerConfig,
        pipeline: Pipeline,
        placement: Arc<dyn Placement>,
        alloc: Arc<dyn Allocator>,
        trace: Arc<dyn TraceSink>,
        sampler: Arc<dyn Sampler>,
    ) -> Result<Arc<Shared>> {
        cfg.validate()?;
        let Pipeline {
            source,
            kernels,
            mut sink,
        } = pipeline;

        // f.1: validate the chain before anything is opened or read.
        let schemas = validate_chain(source.as_ref(), &kernels, &sink)?;
        let plan = source.plan()?;

        // f.1: a sink that cannot say what it committed cannot be resumed, and neither can a
        // source that cannot be re-read; either forces checkpointing off, named in the log.
        let mut checkpoint_enabled = cfg.checkpoint_enabled;
        if checkpoint_enabled && sink.checkpoint()?.is_none() {
            tracing::warn!(
                target: "sched.not_resumable",
                reason = "the sink does not track commits",
                "checkpointing is off for this run"
            );
            checkpoint_enabled = false;
        }
        if checkpoint_enabled && !source.repeatable() {
            tracing::warn!(
                target: "sched.not_resumable",
                reason = "the source is not repeatable",
                "checkpointing is off for this run"
            );
            checkpoint_enabled = false;
        }
        if !cfg.resuming {
            let schema = schemas[0].clone();
            sink.open(&schema)?;
        }

        let sink_spec = sink.accepts();
        let stages: Vec<StageEntry> = kernels
            .iter()
            .enumerate()
            .map(|(index, kernel)| {
                let pool = match kernel.kind() {
                    KernelKind::Stateless => None,
                    KernelKind::Stateful { max_instances } => {
                        let max = max_instances.get();
                        let slots = (0..max)
                            .map(|_| Slot {
                                state: None,
                                owner: None,
                                in_use: false,
                                retired: true,
                            })
                            .collect();
                        Some(InstancePool {
                            slots: Mutex::new(slots),
                            max,
                        })
                    }
                };
                StageEntry {
                    stage: index as StageId + 1,
                    spec: kernel.accepts(),
                    kernel: kernel.clone(),
                    pool,
                    out_bytes: AtomicU64::new(0),
                    tasks: AtomicU64::new(0),
                    busy_ns: AtomicU64::new(0),
                    errors: AtomicU32::new(0),
                    skipped: AtomicU32::new(0),
                    running: AtomicU32::new(0),
                }
            })
            .collect();

        // f.1: every queue's consumer, so the engine promotes toward the right tier.
        for entry in &stages {
            placement.set_consumer(entry.stage - 1, entry.spec);
        }
        placement.set_consumer(stages.len() as StageId, sink_spec);

        let workers = cfg.workers_max as usize;
        let mut jobs = Vec::with_capacity(workers);
        let mut inbox = Vec::with_capacity(workers);
        let mut unparkers = Vec::with_capacity(workers);
        let mut parkers = Vec::with_capacity(workers);
        for _ in 0..workers {
            let (tx, rx) = unbounded();
            jobs.push(tx);
            inbox.push(Some(rx));
            let parker = Parker::new();
            unparkers.push(parker.unparker().clone());
            parkers.push(Some(parker));
        }
        let (helper_tx, helper_rx) = unbounded();
        let queues = stages.len() + 1;

        let shared = Arc::new(Shared {
            knobs: KnobState::new(&cfg, queues),
            cfg,
            source,
            sink: RwLock::new(sink),
            sink_spec,
            placement,
            alloc,
            trace,
            sampler,
            stages,
            plan,
            schemas,
            stage_table: Mutex::new(()),
            closed: (0..queues).map(|_| AtomicBool::new(false)).collect(),
            state: AtomicU8Cell::new(RunState::Init.code()),
            exit: Mutex::new(None),
            exit_signal: Condvar::new(),
            cancel: AtomicBool::new(false),
            token: Mutex::new(None),
            stopping: AtomicBool::new(false),
            gate: AtomicBool::new(false),
            probing: AtomicBool::new(false),
            cursor: Mutex::new(Cursor {
                split_index: 0,
                row_offset: 0,
                next_seq: 0,
            }),
            to_recompute: Mutex::new(Vec::new()),
            committed: Mutex::new(None),
            last_manifest: Mutex::new(None),
            jobs,
            job_inbox: Mutex::new(inbox),
            unparkers,
            parkers: Mutex::new(parkers),
            heartbeat: HeartbeatTable::new(workers),
            worker_handles: Mutex::new(Vec::new()),
            helper_tx,
            helper_rx: Mutex::new(Some(helper_rx)),
            source_handle: Mutex::new(None),
            sink_handle: Mutex::new(None),
            source_mode: AtomicU8Cell::new(DRIVE_IDLE),
            sink_mode: AtomicU8Cell::new(DRIVE_IDLE),
            source_unparker: Mutex::new(None),
            sink_unparker: Mutex::new(None),
            checkpoint_on: AtomicBool::new(checkpoint_enabled),
            checkpoint_stop: AtomicBool::new(false),
            checkpoint_handle: Mutex::new(None),
            record_hook: RwLock::new(None),
            stats_cache: Mutex::new(StatsCache {
                at: Instant::now(),
                stats: PlacementStats::default(),
                valid: false,
            }),
            workers_busy: AtomicU16::new(0),
            reads_in_flight: AtomicU16::new(0),
            writes_in_flight: AtomicU16::new(0),
            source_exhausted: AtomicBool::new(false),
            errors_total: AtomicU32::new(0),
            checkpoints: AtomicU64::new(0),
            last_checkpoint_us: AtomicU64::new(0),
            resumed: AtomicBool::new(false),
            recomputed: AtomicU64::new(0),
            shutdown_done: AtomicBool::new(false),
        });
        tracing::info!(
            target: "sched.start",
            workers = shared.cfg.workers_max,
            stages = shared.stages.len(),
            "scheduler built"
        );
        Ok(shared)
    }

    /// The queue the sink drains: Qn, or Q0 when there is no kernel (h).
    pub(crate) fn last_queue(&self) -> StageId {
        self.stages.len() as StageId
    }

    pub(crate) fn run_state(&self) -> RunState {
        RunState::from_code(self.state.get())
    }

    pub(crate) fn set_run_state(&self, state: RunState) {
        self.state.set(state.code());
    }

    pub(crate) fn checkpoint_enabled(&self) -> bool {
        self.checkpoint_on.load(Ordering::SeqCst)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        if self.cancel.load(Ordering::SeqCst) {
            return true;
        }
        let token = self.token.lock().unwrap_or_else(|e| e.into_inner());
        token.as_ref().is_some_and(|t| t.is_cancelled())
    }

    pub(crate) fn set_record_hook(&self, hook: RecordHook) {
        let mut slot = self.record_hook.write().unwrap_or_else(|e| e.into_inner());
        *slot = Some(hook);
    }

    pub(crate) fn record_hook(&self) -> Option<RecordHook> {
        self.record_hook
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The first thread to report an exit wins; the rest are ignored (e.2).
    pub(crate) fn publish_exit(&self, exit: Exit) -> bool {
        let mut slot = self.exit.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return false;
        }
        *slot = Some(exit);
        self.exit_signal.notify_all();
        true
    }

    pub(crate) fn has_exit(&self) -> bool {
        self.exit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub(crate) fn take_exit(&self) -> Option<Exit> {
        self.exit.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// `placement.stats()`, refreshed by whichever thread finds the cache stale (f.3).
    pub(crate) fn placement_stats(&self) -> PlacementStats {
        let cache = self.stats_cache.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.valid || cache.at.elapsed() >= Duration::from_millis(1) {
            // The cache lock is not the stage table lock and is never held across a call into
            // another component but this one, which is the engine's own stats read.
            drop(cache);
            let fresh = self.placement.stats();
            let mut cache = self.stats_cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.stats = fresh;
            cache.at = Instant::now();
            cache.valid = true;
            return cache.stats.clone();
        }
        cache.stats.clone()
    }

    pub(crate) fn queue_bytes(stats: &PlacementStats, stage: StageId) -> u64 {
        stats
            .queues
            .iter()
            .find(|q| q.stage == stage)
            .map_or(0, |q| q.bytes_by_tier.iter().sum())
    }

    pub(crate) fn queue_count(&self, stage: StageId) -> u64 {
        self.placement
            .stats()
            .queues
            .iter()
            .find(|q| q.stage == stage)
            .map_or(0, |q| q.count)
    }

    pub(crate) fn send_job(&self, worker: u16, job: WorkerJob) -> Receiver<Result<JobOutput>> {
        let (reply, rx) = unbounded();
        let index = worker as usize % self.jobs.len();
        let _ = self.jobs[index].send(JobRequest { job, reply });
        self.unparkers[index].unpark();
        rx
    }

    /// Send a job to one worker and wait for its answer. The caller is the facade's or the
    /// controller's thread; the work happens on the worker, as f.4 and f.9 require.
    pub(crate) fn run_on_worker(&self, worker: u16, job: WorkerJob) -> Result<JobOutput> {
        let rx = self.send_job(worker, job);
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                    self.unparkers[worker as usize % self.unparkers.len()].unpark();
                    if self.stopping.load(Ordering::SeqCst) {
                        return Err(AmoruError::Cancelled);
                    }
                }
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                    return Err(AmoruError::Cancelled);
                }
            }
        }
    }

    pub(crate) fn unpark_all(&self) {
        for unparker in &self.unparkers {
            unparker.unpark();
        }
        if let Some(unparker) = self
            .source_unparker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            unparker.unpark();
        }
        if let Some(unparker) = self
            .sink_unparker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            unparker.unpark();
        }
    }

    /// True when the queue for `stage` has been closed by f.7 or by the source drive.
    pub(crate) fn is_closed(&self, stage: StageId) -> bool {
        self.closed
            .get(stage as usize)
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    /// Mark a queue closed once, without calling into the engine; returns true for the call that
    /// did it (f.7). Kept separate from `close_queue` so the flag can be flipped under the stage
    /// table lock and the engine told afterwards, with no lock held.
    fn mark_closed(&self, stage: StageId) -> bool {
        self.closed.get(stage as usize).is_some_and(|flag| {
            flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        })
    }

    /// Close a queue once, in order, and say whether this call did it (f.7).
    pub(crate) fn close_queue(&self, stage: StageId) -> bool {
        if !self.mark_closed(stage) {
            return false;
        }
        self.placement.close(stage);
        tracing::debug!(target: "sched.close_stage", stage, "queue closed");
        true
    }

    /// The close cascade of f.7, evaluated by whichever worker or drive observes the condition.
    /// Monotonic, so it is safe to run concurrently.
    ///
    /// The one `placement.stats()` read happens before the stage table lock is taken and the
    /// `placement.close` calls happen after it is released: no lock of this component is ever
    /// held across a call into another one, which is what preamble 4.2 requires and the deadlock
    /// it exists to prevent.
    pub(crate) fn advance_closes(&self) {
        if !self.source_exhausted.load(Ordering::SeqCst) {
            return;
        }
        let stats = self.placement.stats();
        let mut closing: Vec<StageId> = Vec::new();
        {
            let _guard = self.stage_table.lock().unwrap_or_else(|e| e.into_inner());
            for index in 0..self.stages.len() {
                let stage = index as StageId + 1;
                if self.is_closed(stage) {
                    continue;
                }
                if !self.is_closed(stage - 1) {
                    break;
                }
                let count = stats
                    .queues
                    .iter()
                    .find(|queue| queue.stage == stage - 1)
                    .map_or(0, |queue| queue.count);
                if count != 0 {
                    break;
                }
                if self.stages[index].running.load(Ordering::SeqCst) != 0 {
                    break;
                }
                if self.mark_closed(stage) {
                    closing.push(stage);
                }
            }
        }
        for stage in closing {
            self.placement.close(stage);
            tracing::debug!(target: "sched.close_stage", stage, "queue closed");
        }
    }
}

/// Spawn the worker pool parked and the two drive threads idle (f.1).
pub(crate) fn spawn_all(shared: &Arc<Shared>) -> Result<()> {
    let mut handles = Vec::new();
    {
        let mut inbox = shared.job_inbox.lock().unwrap_or_else(|e| e.into_inner());
        let mut parkers = shared.parkers.lock().unwrap_or_else(|e| e.into_inner());
        for worker in 0..inbox.len() {
            let (Some(rx), Some(parker)) = (inbox[worker].take(), parkers[worker].take()) else {
                return Err(AmoruError::Plan(
                    "the worker pool was already spawned".into(),
                ));
            };
            let shared = Arc::clone(shared);
            let handle = std::thread::Builder::new()
                .name(format!("amoru-worker-{worker}"))
                .spawn(move || crate::worker::worker_loop(shared, worker as u16, rx, parker))
                .map_err(|e| AmoruError::Plan(format!("a worker thread could not start: {e}")))?;
            handles.push(Some(handle));
        }
    }
    *shared
        .worker_handles
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = handles;

    let Some(helper_rx) = shared
        .helper_rx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    else {
        return Err(AmoruError::Plan("the drives were already spawned".into()));
    };
    let source_parker = Parker::new();
    *shared
        .source_unparker
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(source_parker.unparker().clone());
    let source_shared = Arc::clone(shared);
    let source = std::thread::Builder::new()
        .name("amoru-source-drive".into())
        .spawn(move || crate::source_drive::drive(source_shared, helper_rx, source_parker))
        .map_err(|e| AmoruError::Plan(format!("the source drive could not start: {e}")))?;
    *shared
        .source_handle
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(source);

    let sink_parker = Parker::new();
    *shared
        .sink_unparker
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(sink_parker.unparker().clone());
    let sink_shared = Arc::clone(shared);
    let sink = std::thread::Builder::new()
        .name("amoru-sink-drive".into())
        .spawn(move || crate::sink_drive::drive(sink_shared, sink_parker))
        .map_err(|e| AmoruError::Plan(format!("the sink drive could not start: {e}")))?;
    *shared.sink_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    Ok(())
}
