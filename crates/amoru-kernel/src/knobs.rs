//! The scheduler side of the controller's interface: knobs, stats, the probe protocol, the
//! cancel flag and the trace hook (contracts d.11).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::AmoruError;
use crate::ids::{Seq, StageId};
use crate::tier::TierKind;
use crate::trace::TraceRecord;

/// One knob write.
#[derive(Clone, Debug)]
pub enum Knob {
    /// The intended payload size for a stage, in bytes.
    MorselTarget {
        /// The stage.
        stage: StageId,
        /// The target.
        bytes: u64,
    },
    /// How many workers may take tasks.
    ActiveWorkers(u16),
    /// Source splits in flight.
    ReadAhead(u16),
    /// Demotion to disk for one queue.
    StagingTrigger {
        /// The stage.
        stage: StageId,
        /// On or off.
        on: bool,
    },
    /// The admission threshold for one queue and tier.
    HighWater {
        /// The stage.
        stage: StageId,
        /// The tier.
        tier: TierKind,
        /// The high water mark; the scheduler sets low to half of it.
        bytes: u64,
    },
    /// How many morsels ahead of the head are promoted.
    PromotionWindow {
        /// The stage.
        stage: StageId,
        /// The window.
        morsels: u16,
    },
}

/// Implemented by the scheduler; the controller is the only caller (G-I5). The
/// scheduler forwards `StagingTrigger`, `HighWater` and `PromotionWindow` to the
/// placement engine (`set_staging`, `set_water(low = bytes / 2, high = bytes)`,
/// `set_promotion_window`) and keeps the others; the controller never calls a
/// placement setter except `set_budgets`. Values outside the preamble's ranges are
/// clamped by the scheduler and counted in `SchedulerStats::knob_clamps`.
pub trait Knobs: Send + Sync {
    /// Write one knob.
    fn set(&self, knob: Knob);
    /// Every knob's current value.
    fn snapshot(&self) -> KnobSnapshot;
    /// End the run with a diagnostic (the controller's third breach, state growth,
    /// sampler failure). The scheduler enters `Terminating` as for a kernel error.
    fn terminate(&self, diagnostic: AmoruError);
}

/// Every knob's current value.
#[derive(Clone, Debug, Default)]
pub struct KnobSnapshot {
    /// Morsel target per stage.
    pub morsel_target: Vec<(StageId, u64)>,
    /// Workers allowed to take tasks.
    pub active_workers: u16,
    /// Source splits in flight.
    pub read_ahead: u16,
    /// Staging trigger per stage.
    pub staging: Vec<(StageId, bool)>,
    /// High water mark per stage and tier.
    pub high_water: Vec<(StageId, TierKind, u64)>,
    /// Promotion window per stage.
    pub promotion_window: Vec<(StageId, u16)>,
}

/// Live scheduler counters the controller classifies bottlenecks from (RC f.6).
/// Implemented by the scheduler; read by the controller each tick.
pub trait StatsSource: Send + Sync {
    /// The counters as of now.
    fn scheduler_stats(&self) -> SchedulerStats;
}

/// One stage's counters.
#[derive(Clone, Debug, Default)]
pub struct StageStats {
    /// The stage.
    pub stage: StageId,
    /// Tasks completed.
    pub tasks: u64,
    /// Nanoseconds spent inside `apply`.
    pub busy_ns: u64,
    /// Kernel errors seen.
    pub errors: u32,
    /// Morsels skipped by the error policy.
    pub skipped: u32,
    /// Instances alive.
    pub instances_live: u16,
}

/// The scheduler's counters as the controller reads them.
#[derive(Clone, Debug, Default)]
pub struct SchedulerStats {
    /// Per stage.
    pub per_stage: Vec<StageStats>,
    /// Workers allowed to take tasks.
    pub workers_active: u16,
    /// Workers inside `apply`.
    pub workers_busy: u16,
    /// Source reads in flight.
    pub reads_in_flight: u16,
    /// Sink writes in flight.
    pub writes_in_flight: u16,
    /// The sink's write concurrency.
    pub sink_concurrency: u16,
    /// True once the source has no more splits.
    pub source_exhausted: bool,
    /// The highest sequence number issued.
    pub seq_issued: Seq,
    /// The sink's commit watermark.
    pub committed_seq: Option<Seq>,
    /// Manifests written.
    pub checkpoints: u64,
    /// Microseconds the last manifest write took.
    pub last_checkpoint_us: u64,
    /// True when this run was resumed from a manifest.
    pub resumed: bool,
    /// Morsels recomputed on resume.
    pub recomputed: u64,
    /// Knob values clamped to their range.
    pub knob_clamps: u64,
}

/// The probe protocol (RC f.2) as the controller sees it. Implemented by the scheduler.
/// For stage 1 the scheduler issues one read of about `bytes` from the source cursor
/// (advancing it, so the morsel has the next `seq`); for stage k greater than 1 it pops the
/// head of Q(k-1), which is the previous stage's probe output. Runs on one worker with the
/// rest parked; the output is pushed downstream as normal, nothing is wasted.
pub trait Prober: Send + Sync {
    /// Probe one stage with about `bytes` of input.
    fn probe(&self, stage: StageId, bytes: u64) -> crate::Result<ProbeResult>;
}

/// What one probe measured.
#[derive(Clone, Debug)]
pub struct ProbeResult {
    /// Input payload bytes.
    pub bytes_in: u64,
    /// Input rows.
    pub rows_in: u64,
    /// Peak anonymous host bytes above the sample taken before `apply`.
    pub peak_delta: u64,
    /// Peak device bytes above the sample taken before `apply`.
    pub dev_peak_delta: u64,
    /// Wall nanoseconds inside `apply`.
    pub wall_ns: u64,
    /// CPU nanoseconds inside `apply`.
    pub cpu_ns: u64,
}

/// What happens after a kernel error (preamble `errors.policy`).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ErrorPolicy {
    /// End the run with a diagnostic.
    Terminate,
    /// Skip the morsel and carry on.
    Skip,
    /// Skip up to this many morsels, then terminate.
    Budget(u32),
}

/// Which decision function sizes morsels (preamble `sizer`).
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum SizerKind {
    /// The rule-based sizer.
    #[default]
    Rule,
    /// The learned sizer.
    Learned,
}

/// A clonable cancel flag set by the surface; polled by the scheduler's drives and
/// workers between tasks.
#[derive(Clone, Default, Debug)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        CancelToken {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Cancel the run. Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// True once `cancel` has been called on this token or any clone of it.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// A hook the facade installs so the controller sees every trace record as it is
/// emitted (RC `on_record`); the scheduler calls it after `TraceSink::record`.
pub type RecordHook = Arc<dyn Fn(&TraceRecord) + Send + Sync>;
