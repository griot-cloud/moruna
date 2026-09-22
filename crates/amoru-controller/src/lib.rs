//! Amoru component 11, the resource controller: the one writer of every knob.
//!
//! Design: `architecture/sdd/11-controller.md`. The controller derives the budget from the
//! discovered limits (f.1), measures each kernel's amplification with a probe (f.2), sizes
//! morsels and workers from the working-set equation (f.3), adjusts them from measured
//! outcomes (f.4, f.5), classifies the bottleneck each tick and moves the one knob that
//! relieves it (f.6), keeps device memory as a second budget (f.11), and falls back to the
//! rule sizer when a learned one misbehaves (f.8).
//!
//! It names no crate but the contracts: the sampler (3), the trace tail (4), the placement
//! engine (9) and the scheduler (10) all arrive as trait objects, so the whole component is
//! testable against the fakes of contracts d.15.
//!
//! Two properties hold by construction rather than by care. Every knob write is preceded by
//! the working-set check (RC-I1, [`apply`]), so an inconsistent set of knobs cannot be
//! written. And every sizer proposal is clamped to an envelope the controller computes from
//! the budget and the probe (RC-I2, [`model::envelope`]), so a confident wrong model cannot
//! walk the process into a breach.

#![deny(missing_docs)]
#![allow(clippy::result_large_err)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use amoru_kernel::{
    AmoruError, Fingerprint, KernelHints, KernelKind, KnobSnapshot, Knobs, Limits, Placement,
    Prober, Sampler, Seq, SizerKind, StageId, StatsSource, TierBudgets, TraceRecord, TraceTail,
};

mod apply;
mod breach;
mod budget;
mod classify;
mod device;
mod model;
mod probe;
mod profile;
pub mod sizer;
mod summary;
mod tick;

pub use sizer::{Envelope, LearnedSizer, Observation, Proposal, RuleSizer, Sizer, SizerOutcome};
pub use summary::{Bottleneck, ControllerSummary};

/// The contracts' result type; every error here is an `AmoruError` value (contracts d.14).
pub type Result<T> = core::result::Result<T, AmoruError>;

/// How many records of a stage the controller looks back over (f.3 state, f.4 variance).
pub(crate) const WINDOW: usize = 32;
/// The record queue's capacity (g); the oldest is dropped when it overflows.
pub(crate) const RECORD_QUEUE_CAPACITY: usize = 65_536;
/// The tick's lock bound in milliseconds (RC-I10, preamble 4.2). Public because it is the
/// number RC-T17 checks against and the one a report would quote.
pub const TICK_BOUND_MS: u64 = 5;
/// The same bound as a duration.
pub(crate) const TICK_LOCK_BOUND: Duration = Duration::from_millis(TICK_BOUND_MS);
/// Proposals a sizer makes before the fallback rule of f.8 can fire.
pub(crate) const FALLBACK_MIN_PROPOSALS: u32 = 20;
/// Records per stage before the periodic profile write of f.9 starts.
pub(crate) const PROFILE_WRITE_AFTER: u64 = 50;
/// Adjustments the flip window of RC-I5 holds.
pub(crate) const FLIP_WINDOW: usize = 100;
/// Targets the freeze of RC-I5 takes its geometric mean over.
pub(crate) const FREEZE_MEAN_OVER: usize = 10;
/// Consecutive ticks with an unchanging `Sample::at_ns` that mean a stalled sampler (h).
pub(crate) const SAMPLER_STALL_TICKS: u32 = 100;
/// The highest safety multiplier a breach may raise a stage to (RC-I4).
pub(crate) const SAFETY_CAP: f32 = 3.0;
/// The lowest amplification the controller will believe (h, edge cases).
pub(crate) const MIN_AMPLIFICATION: f64 = 0.5;

/// What the facade learned from `Source::plan` (12 f.1); the controller never sees the splits.
#[derive(Clone, Debug, Default)]
pub struct PlanSummary {
    /// Sum of the splits' uncompressed bytes.
    pub total_bytes: u64,
    /// Sum of the splits' rows; with `total_bytes` it gives the bytes per row the probe needs.
    pub total_rows: u64,
    /// How many splits the plan has.
    pub splits: u32,
    /// The largest split.
    pub max_split_bytes: u64,
    /// True when every split can be read in row ranges.
    pub sub_splittable_all: bool,
}

/// Everything the controller is configured with; every row is a preamble section 5 tunable.
#[derive(Clone, Debug)]
pub struct ControllerConfig {
    /// The discovered limits (contracts d.12).
    pub limits: Limits,
    /// What the source plan adds up to.
    pub plan: PlanSummary,
    /// `workers.max`, the `W` of f.3.
    pub workers_max: u16,
    /// `Allocator::is_pinned()`; decides which host pool `TierBudgets` fills (f.1).
    pub pinned: bool,
    /// `budget.reserve_fraction`.
    pub reserve_fraction: f32,
    /// `controller.target_fraction`.
    pub target_fraction: f32,
    /// `controller.safety_initial`.
    pub safety_initial: f32,
    /// `controller.safety_floor`.
    pub safety_floor: f32,
    /// `controller.increase_step`.
    pub increase_step: f32,
    /// `controller.tick_ms`.
    pub tick_ms: u64,
    /// `controller.oscillation_flips`.
    pub oscillation_flips: u32,
    /// `controller.freeze_morsels`.
    pub freeze_morsels: u32,
    /// `morsel.min_bytes`.
    pub morsel_min: u64,
    /// `morsel.max_bytes`.
    pub morsel_max: u64,
    /// `morsel.probe_bytes`.
    pub probe_bytes: u64,
    /// `sizer`: which decision function sizes morsels (contracts d.11).
    pub sizer: SizerKind,
    /// `sizer.fallback_error_ratio`.
    pub fallback_error_ratio: f32,
    /// `profiles.dir`; `None` disables the profile store.
    pub profiles_dir: Option<std::path::PathBuf>,
    /// `budget.disk`, passed through to the placement engine's staging cap.
    pub disk_budget: u64,
    /// `checkpoint.enabled`; turns on the periodic profile writes of f.9.
    pub checkpoint_enabled: bool,
    /// `checkpoint.interval_ms`; the cadence of those writes.
    pub checkpoint_interval_ms: u64,
}

impl Default for ControllerConfig {
    /// The preamble section 5 defaults, with an empty plan and no limits worth running on;
    /// a caller sets `limits`, `plan` and `workers_max`.
    fn default() -> ControllerConfig {
        ControllerConfig {
            limits: Limits {
                memory_ceiling: 0,
                memory_kill: None,
                cpu_quota: 1.0,
                page_bytes: 4096,
                devices: Vec::new(),
                source: amoru_kernel::LimitSource::Explicit,
            },
            plan: PlanSummary::default(),
            workers_max: 1,
            pinned: false,
            reserve_fraction: 0.10,
            target_fraction: 0.85,
            safety_initial: 1.5,
            safety_floor: 1.2,
            increase_step: 0.10,
            tick_ms: 250,
            oscillation_flips: 5,
            freeze_morsels: 100,
            morsel_min: 4 << 20,
            morsel_max: 512 << 20,
            probe_bytes: 16 << 20,
            sizer: SizerKind::Rule,
            fallback_error_ratio: 2.0,
            profiles_dir: None,
            disk_budget: 0,
            checkpoint_enabled: false,
            checkpoint_interval_ms: 5000,
        }
    }
}

/// What the facade knows about one kernel stage before it is probed.
///
/// `schema_hash` is `SourceSchema::hash()` (contracts d.4) of the stage's input schema, which
/// the facade has from the chain validation; with the fingerprint it keys the profile store.
pub struct KernelInfo {
    /// The stage this kernel occupies; stage 1 is the first kernel.
    pub stage: StageId,
    /// The kernel's stable identity (contracts d.7, e.6).
    pub fingerprint: Fingerprint,
    /// The input schema's hash.
    pub schema_hash: [u8; 32],
    /// What the kernel says about itself before the probe.
    pub hints: KernelHints,
    /// Stateless or stateful, and with how many instances.
    pub kind: KernelKind,
}

/// The budget arithmetic of f.1, as `prepare` computed it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Budgets {
    /// `memory_ceiling - baseline - reserve`: the bytes the runtime may hold on the host.
    pub host: u64,
    /// Per device, `0.9 x free_bytes` measured after every `Kernel::init`.
    pub device: [u64; 8],
    /// Anonymous host bytes of the process after every `init` and before the first morsel.
    pub baseline: u64,
    /// `reserve_fraction x memory_ceiling`: the headroom never allocated.
    pub reserve: u64,
}

/// The phase of e.2. `on_record` before `Running` is buffered and processed at `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Created,
    Prepared,
    Probed,
    Running,
    Stopped,
}

/// One stage's control state (e.1).
pub(crate) struct StageCtl {
    pub stage: StageId,
    pub a_k: f64,
    pub a_k_dev: f64,
    pub safety: f32,
    pub target: u64,
    pub envelope: Envelope,
    pub completions_since_adjust: u32,
    pub last_adjust_sign: i8,
    pub flips_window: VecDeque<i8>,
    pub frozen_until: Option<u64>,
    pub peak_ewma: f64,
    pub wall_ewma: f64,
    pub state_bytes: u64,
    pub device_breaches: u8,
    pub sizer: Box<dyn Sizer>,
    pub sizer_clamps: u32,
    pub sizer_proposals: u32,
    /// Targets the last ten adjustments left, newest last; the freeze takes their mean.
    pub recent_targets: VecDeque<u64>,
    /// Completions of this stage, the unit damping and the freeze are counted in.
    pub completions: u64,
    /// Breaches seen while the target was already at `morsel_min` (f.7).
    pub floor_breaches: u32,
    /// Records seen for this stage, which gates the periodic profile write (f.9).
    pub records: u64,
    /// The profile the store had for this stage, if any.
    pub profile: Option<profile::Profile>,
    /// Whether this stage was seeded from a profile instead of probed (f.14).
    pub seeded: bool,
    /// `peak_delta / bytes_in` per record, newest last, bounded by `WINDOW`.
    pub peak_ratios: VecDeque<f64>,
    /// The largest `state_bytes` seen per instance (f.3).
    pub state_by_instance: HashMap<u16, u64>,
    /// Instances the scheduler reports alive.
    pub instances_live: u16,
    /// Prediction errors of the active sizer, newest last (f.8).
    pub sizer_errors: VecDeque<f64>,
    /// Prediction errors of the shadow rule sizer, newest last (f.8).
    pub shadow_errors: VecDeque<f64>,
    /// The peak this stage's sizer predicted for the morsels now in flight (f.8).
    pub predicted_peak: Option<u64>,
    /// The most recent record of this stage, which is what the sizer's `Observation` carries.
    pub last: Option<RecordSummary>,
    /// Welford state over `peak_delta / bytes_in` for the profile (e.3).
    pub var_count: u64,
    pub var_mean: f64,
    pub var_m2: f64,
}

impl StageCtl {
    fn new(stage: StageId, cfg: &ControllerConfig, sizer: Box<dyn Sizer>) -> StageCtl {
        StageCtl {
            stage,
            a_k: 4.0,
            a_k_dev: 0.0,
            safety: cfg.safety_initial,
            target: cfg.probe_bytes,
            envelope: Envelope {
                min: cfg.morsel_min,
                max: cfg.morsel_max,
            },
            completions_since_adjust: 0,
            last_adjust_sign: 0,
            flips_window: VecDeque::new(),
            frozen_until: None,
            peak_ewma: 0.0,
            wall_ewma: 0.0,
            state_bytes: 0,
            device_breaches: 0,
            sizer,
            sizer_clamps: 0,
            sizer_proposals: 0,
            recent_targets: VecDeque::new(),
            completions: 0,
            floor_breaches: 0,
            records: 0,
            profile: None,
            seeded: false,
            peak_ratios: VecDeque::new(),
            state_by_instance: HashMap::new(),
            instances_live: 0,
            sizer_errors: VecDeque::new(),
            shadow_errors: VecDeque::new(),
            predicted_peak: None,
            last: None,
            var_count: 0,
            var_mean: 0.0,
            var_m2: 0.0,
        }
    }

    /// The allowance of b: the bytes one in-flight morsel of this stage may hold at peak.
    pub(crate) fn allowance(&self, target: u64) -> u64 {
        model::scale(target, self.a_k * f64::from(self.safety))
    }
}

/// The numeric fields of one trace record, as the record queue carries them (g).
#[derive(Clone, Debug)]
pub(crate) struct RecordSummary {
    pub seq: Seq,
    pub stage: StageId,
    pub instance: u16,
    pub bytes_in: u64,
    pub rows_in: u64,
    pub peak_delta: u64,
    pub wall_ns: u64,
    pub state_bytes: u64,
    pub column_bytes: Vec<u64>,
    pub mean_string_len: f32,
    pub null_ratio: f32,
}

/// The features of one record, as the controller and the diagnostics need them (contracts d.5).
pub(crate) fn features_of(r: &TraceRecord) -> amoru_kernel::MorselFeatures {
    amoru_kernel::MorselFeatures {
        rows: r.rows_in,
        bytes: r.bytes_in,
        column_bytes: r.feat_column_bytes.clone(),
        mean_string_len: Some(r.feat_mean_string_len),
        null_ratio: Some(r.feat_null_ratio),
        shape: None,
        dtype: None,
    }
}

impl RecordSummary {
    pub(crate) fn of(r: &TraceRecord) -> RecordSummary {
        RecordSummary {
            seq: r.seq,
            stage: r.stage,
            instance: r.instance,
            bytes_in: r.bytes_in,
            rows_in: r.rows_in,
            peak_delta: r.mem_anon_peak.saturating_sub(r.mem_anon_before),
            wall_ns: r.t_end_ns.saturating_sub(r.t_start_ns),
            state_bytes: r.state_bytes,
            column_bytes: r.feat_column_bytes.clone(),
            mean_string_len: r.feat_mean_string_len,
            null_ratio: r.feat_null_ratio,
        }
    }
}

/// Everything the controller mutates, behind one mutex (g).
pub(crate) struct ControllerState {
    pub cfg: ControllerConfig,
    pub kernels: Vec<KernelInfo>,
    pub phase: Phase,
    pub budgets: Budgets,
    pub tier_budgets: TierBudgets,
    pub stages: Vec<StageCtl>,
    pub active_workers: u16,
    pub read_ahead: u16,
    pub promotion_window: u16,
    /// The high water one queue gets on the host tier; the placement half divided by queues.
    pub high_water: u64,
    pub split_bytes: u64,
    pub state_total: u64,
    pub last_state_total: u64,
    pub notes: Vec<String>,
    pub timeline: Vec<(f64, Bottleneck)>,
    pub last_class: Option<Bottleneck>,
    pub freezes: u32,
    pub breaches: u32,
    pub fallback_at: Option<Seq>,
    pub tiny: bool,
    pub terminated: bool,
    pub last_throttled_us: u64,
    pub last_sample_at_ns: u64,
    pub stall_ticks: u32,
    pub qn_history: VecDeque<u64>,
    pub ticks_skipped: u64,
    pub records_dropped: u64,
    pub row_note_done: HashSet<StageId>,
    /// Queues whose staging trigger the controller has already turned on (f.6).
    pub staging_on: HashSet<StageId>,
    pub last_tick: Instant,
    pub last_profile_write: Instant,
    pub source_exhausted: bool,
    pub high_water_override: Option<(StageId, u64)>,
}

impl ControllerState {
    /// The kernel stages, which is `S` in f.3.
    pub(crate) fn stage_count(&self) -> usize {
        self.stages.len()
    }

    /// The queues, Q0 to Qn: one more than the kernel stages.
    pub(crate) fn queue_count(&self) -> usize {
        self.stage_count() + 1
    }

    pub(crate) fn stage_mut(&mut self, stage: StageId) -> Option<&mut StageCtl> {
        self.stages.iter_mut().find(|s| s.stage == stage)
    }

    pub(crate) fn stage(&self, stage: StageId) -> Option<&StageCtl> {
        self.stages.iter().find(|s| s.stage == stage)
    }

    pub(crate) fn note(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    /// The host ceiling less the reserve: the line a breach is measured against (RC-I4).
    pub(crate) fn breach_line(&self) -> u64 {
        self.cfg
            .limits
            .memory_ceiling
            .saturating_sub(self.budgets.reserve)
    }

    /// The hints of one stage, which seed amplification and state before anything is measured.
    pub(crate) fn hints(&self, stage: StageId) -> KernelHints {
        self.kernels
            .iter()
            .find(|k| k.stage == stage)
            .map(|k| k.hints.clone())
            .unwrap_or_default()
    }

    /// The instance pool size a stateful stage declared; 1 for a stateless one.
    pub(crate) fn max_instances(&self, stage: StageId) -> u16 {
        match self.kernels.iter().find(|k| k.stage == stage) {
            Some(KernelInfo {
                kind: KernelKind::Stateful { max_instances },
                ..
            }) => u16::try_from(max_instances.get()).unwrap_or(u16::MAX),
            _ => 1,
        }
    }
}

/// What one tick or one record decided: knob writes, a budget write and a diagnostic, all
/// performed after the mutex is released (RC-I10).
#[derive(Default)]
pub(crate) struct Actions {
    pub knobs: Vec<amoru_kernel::Knob>,
    pub budgets: Option<TierBudgets>,
    pub terminate: Option<AmoruError>,
}

impl Actions {
    pub(crate) fn is_empty(&self) -> bool {
        self.knobs.is_empty() && self.budgets.is_none() && self.terminate.is_none()
    }
}

/// The components the controller talks to, all as contracts trait objects.
pub(crate) struct Peers {
    pub knobs: Arc<dyn Knobs>,
    pub stats: Arc<dyn StatsSource>,
    pub prober: Arc<dyn Prober>,
    pub sampler: Arc<dyn Sampler>,
    pub trace: Arc<dyn TraceTail>,
    pub placement: Arc<dyn Placement>,
}

/// Instrumentation of the single mutex (RC-I10): the distribution of how long it was held, the
/// longest it was ever held, and whether it is held right now. `held` is what lets a test
/// assert that no call into another component happens while the lock is taken.
///
/// The distribution is kept as well as the maximum because the maximum is a wall-clock figure,
/// and a wall clock on a host with more runnable threads than cores measures the scheduler as
/// much as the controller: a thread holding the lock for twenty microseconds of arithmetic can
/// be descheduled in the middle of it and wake milliseconds later. The histogram is a bucket
/// per power of two nanoseconds, which costs one atomic add per release and is enough to read
/// a percentile off.
pub(crate) struct LockMeter {
    max_held_ns: AtomicU64,
    held: AtomicBool,
    /// Bucket `k` counts holds of under `2^k` nanoseconds.
    buckets: [AtomicU64; 64],
}

impl Default for LockMeter {
    fn default() -> LockMeter {
        LockMeter {
            max_held_ns: AtomicU64::new(0),
            held: AtomicBool::new(false),
            buckets: [const { AtomicU64::new(0) }; 64],
        }
    }
}

impl LockMeter {
    fn record(&self, held_ns: u64) {
        self.max_held_ns.fetch_max(held_ns, Ordering::SeqCst);
        let bucket = (u64::BITS - held_ns.leading_zeros()) as usize;
        if let Some(slot) = self.buckets.get(bucket.min(63)) {
            slot.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The upper edge of the bucket the given quantile falls in, in nanoseconds; zero when
    /// nothing has been measured.
    fn quantile_ns(&self, quantile: f64) -> u64 {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|slot| slot.load(Ordering::SeqCst))
            .collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0;
        }
        let want = (total as f64 * quantile).ceil() as u64;
        let mut seen = 0u64;
        for (bucket, count) in counts.iter().enumerate() {
            seen += count;
            if seen >= want {
                return 1u64 << bucket.min(63);
            }
        }
        u64::MAX
    }
}

/// A guard over the controller's state that meters how long the lock was held (RC-I10).
pub(crate) struct Held<'a> {
    guard: MutexGuard<'a, ControllerState>,
    meter: &'a LockMeter,
    at: Instant,
}

impl std::ops::Deref for Held<'_> {
    type Target = ControllerState;
    fn deref(&self) -> &ControllerState {
        &self.guard
    }
}

impl std::ops::DerefMut for Held<'_> {
    fn deref_mut(&mut self) -> &mut ControllerState {
        &mut self.guard
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let held = self.at.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.meter.record(held);
        self.meter.held.store(false, Ordering::SeqCst);
    }
}

/// Everything the controller shares with its tick thread, behind one `Arc`.
///
/// The split exists so the tick thread can hold the state it works on without holding the
/// `Controller` the facade owns: the thread gets an `Arc<Inner>`, which is why nothing in this
/// crate needs `unsafe` (l).
pub(crate) struct Inner {
    state: Mutex<ControllerState>,
    meter: LockMeter,
    peers: Peers,
    /// The bounded record queue of g, drained by the tick thread.
    queue: Mutex<VecDeque<RecordSummary>>,
    /// The sizer factory `prepare` builds each stage's sizer with.
    factory: Mutex<SizerFactory>,
}

/// The resource controller.
///
/// Every method takes `&self`: the mutable state lives in one `Mutex<ControllerState>` (g), so
/// the facade holds the controller in an `Arc`, installs [`Controller::on_record`] as the
/// scheduler's `RecordHook` before `prepare`, and still calls `prepare`, `probe_all`,
/// `probe_missing`, `start` and `stop` through the same shared handle.
pub struct Controller {
    inner: Arc<Inner>,
    tick: Mutex<Option<std::thread::JoinHandle<()>>>,
    stop_flag: Arc<AtomicBool>,
}

/// Builds one sizer per stage. The default follows `ControllerConfig::sizer`.
pub type SizerFactory = Arc<dyn Fn(StageId) -> Box<dyn Sizer> + Send + Sync>;

impl Inner {
    /// Take the controller's state, metering how long it stays taken (RC-I10).
    pub(crate) fn held(&self) -> Held<'_> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.meter.held.store(true, Ordering::SeqCst);
        Held {
            guard,
            meter: &self.meter,
            at: Instant::now(),
        }
    }

    pub(crate) fn peers(&self) -> &Peers {
        &self.peers
    }

    pub(crate) fn factory(&self) -> SizerFactory {
        Arc::clone(&self.factory.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Push one record's numbers onto the bounded queue, dropping the oldest on overflow.
    pub(crate) fn enqueue(&self, summary: RecordSummary) -> bool {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let dropped = queue.len() >= RECORD_QUEUE_CAPACITY;
        if dropped {
            queue.pop_front();
        }
        queue.push_back(summary);
        dropped
    }

    /// Take everything the queue holds.
    pub(crate) fn drain(&self) -> Vec<RecordSummary> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.drain(..).collect()
    }

    /// Perform the writes a tick or a record decided, with the mutex released (RC-I10).
    pub(crate) fn perform(&self, actions: Actions) {
        if actions.is_empty() {
            return;
        }
        if let Some(budgets) = actions.budgets {
            self.peers.placement.set_budgets(budgets);
        }
        for knob in actions.knobs {
            tracing::debug!(target: "ctl.adjust", knob = ?knob, "knob");
            self.peers.knobs.set(knob);
        }
        if let Some(diagnostic) = actions.terminate {
            tracing::error!(target: "ctl.terminate", error = %diagnostic, "terminating the run");
            self.peers.knobs.terminate(diagnostic);
        }
    }

    /// Every knob's current value, as the scheduler holds it.
    pub(crate) fn knob_snapshot(&self) -> KnobSnapshot {
        self.peers.knobs.snapshot()
    }
}

impl Controller {
    /// Build a controller over the four components it reads and writes, all as trait objects.
    ///
    /// Fails with `Config` when the stages the kernels name are not the contiguous range
    /// `1..=S` the linear chain requires.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: ControllerConfig,
        knobs: Arc<dyn Knobs>,
        stats: Arc<dyn StatsSource>,
        prober: Arc<dyn Prober>,
        sampler: Arc<dyn Sampler>,
        trace: Arc<dyn TraceTail>,
        placement: Arc<dyn Placement>,
        kernels: Vec<KernelInfo>,
    ) -> Result<Controller> {
        let mut kernels = kernels;
        kernels.sort_by_key(|k| k.stage);
        for (at, kernel) in kernels.iter().enumerate() {
            let expected = StageId::try_from(at + 1).unwrap_or(StageId::MAX);
            if kernel.stage != expected {
                return Err(AmoruError::Config {
                    name: "pipeline",
                    msg: format!(
                        "kernel stages must be 1..=S in order; found {} where {} was expected",
                        kernel.stage, expected
                    ),
                });
            }
        }
        let sizer_kind = cfg.sizer;
        let target_fraction = cfg.target_fraction;
        let increase_step = cfg.increase_step;
        let factory: SizerFactory = Arc::new(move |_stage| match sizer_kind {
            SizerKind::Rule => {
                Box::new(RuleSizer::new(target_fraction, increase_step)) as Box<dyn Sizer>
            }
            SizerKind::Learned => {
                Box::new(LearnedSizer::new(target_fraction, increase_step)) as Box<dyn Sizer>
            }
        });
        let now = Instant::now();
        let state = ControllerState {
            cfg,
            kernels,
            phase: Phase::Created,
            budgets: Budgets::default(),
            tier_budgets: TierBudgets::default(),
            stages: Vec::new(),
            active_workers: 1,
            read_ahead: 2,
            promotion_window: 2,
            high_water: 0,
            split_bytes: 0,
            state_total: 0,
            last_state_total: 0,
            notes: Vec::new(),
            timeline: Vec::new(),
            last_class: None,
            freezes: 0,
            breaches: 0,
            fallback_at: None,
            tiny: false,
            terminated: false,
            last_throttled_us: 0,
            last_sample_at_ns: 0,
            stall_ticks: 0,
            qn_history: VecDeque::new(),
            ticks_skipped: 0,
            records_dropped: 0,
            row_note_done: HashSet::new(),
            staging_on: HashSet::new(),
            last_tick: now,
            last_profile_write: now,
            source_exhausted: false,
            high_water_override: None,
        };
        Ok(Controller {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                meter: LockMeter::default(),
                peers: Peers {
                    knobs,
                    stats,
                    prober,
                    sampler,
                    trace,
                    placement,
                },
                queue: Mutex::new(VecDeque::new()),
                factory: Mutex::new(factory),
            }),
            tick: Mutex::new(None),
            stop_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Install the factory that builds each stage's sizer, instead of the one
    /// `ControllerConfig::sizer` names. Must be called before [`Controller::prepare`].
    ///
    /// This is the seam the phase 8 learned sizer and RC-T2's test sizers are installed
    /// through: the trait is public (d.1) but `new` takes no sizer, so without it a sizer
    /// other than the two this crate ships could never reach the controller.
    pub fn set_sizer_factory(&self, factory: SizerFactory) {
        let mut slot = self.inner.factory.lock().unwrap_or_else(|e| e.into_inner());
        *slot = factory;
    }

    /// Phase 1 (f.1): sample the baseline, after `Scheduler::init_instances` has run every
    /// stateful `init`, compute the budgets and hand the placement engine its tier budgets.
    pub fn prepare(&self) -> Result<Budgets> {
        budget::prepare(&self.inner)
    }

    /// Phase 2 (f.2): probe every kernel stage in order and set its amplification.
    pub fn probe_all(&self) -> Result<()> {
        probe::probe_all(&self.inner, false)
    }

    /// The resume variant of [`Controller::probe_all`] (f.14): probe only the stages without a
    /// usable profile, seed the rest from the profile store, and write the profile at once so
    /// a second crash keeps what this run was told.
    pub fn probe_missing(&self) -> Result<()> {
        probe::probe_all(&self.inner, true)
    }

    /// Phase 3 (f.3): the initial knobs from the working-set equation, then the tick thread.
    pub fn start(&self) -> Result<()> {
        model::start(&self.inner)?;
        self.spawn_tick();
        Ok(())
    }

    /// The hook the scheduler calls after every trace record (contracts d.11 `RecordHook`).
    ///
    /// Cheap: it enqueues the record's numbers for the tick thread. The two paths that cannot
    /// wait for a tick run here on the calling worker: the breach of f.7 and the device out of
    /// memory of f.11.
    pub fn on_record(&self, r: &TraceRecord) {
        tick::on_record(&self.inner, r);
    }

    /// Run one tick on the calling thread.
    ///
    /// The tick thread calls this every `controller.tick_ms`; it is public so a test can drive
    /// the loop deterministically (RC-T8, RC-T9, RC-T17) rather than by sleeping.
    pub fn tick_once(&self) {
        tick::tick(&self.inner);
    }

    /// The summary so far: the timeline, the notes and the sizer state for the run report.
    pub fn summary(&self) -> ControllerSummary {
        summary::summary(&self.inner)
    }

    /// Join the tick thread, write the profile when the run completed (f.9), and return the
    /// summary for the run report.
    pub fn stop(&self) -> ControllerSummary {
        self.join_tick();
        summary::stop(&self.inner)
    }

    /// The given quantile of how long the mutex has been held, in nanoseconds, rounded up to
    /// the next power of two (RC-I10, RC-T17).
    ///
    /// This is the figure that describes the controller rather than the machine it ran on: an
    /// implementation that does unbounded work under the lock moves the whole distribution,
    /// while a scheduler that preempts a holder moves only the tail.
    pub fn lock_held_quantile_ns(&self, quantile: f64) -> u64 {
        self.inner.meter.quantile_ns(quantile)
    }

    /// The longest the controller's mutex has been held, in nanoseconds (RC-I10, RC-T17).
    ///
    /// A wall-clock figure, so on a host with more runnable threads than cores it includes
    /// whatever time the holder spent descheduled. Report it; assert on
    /// [`Controller::lock_held_quantile_ns`].
    pub fn max_lock_held_ns(&self) -> u64 {
        self.inner.meter.max_held_ns.load(Ordering::SeqCst)
    }

    /// Whether the controller's mutex is held right now. A test wraps each fake with this to
    /// assert that no call into another component happens under the lock (RC-T17).
    pub fn lock_is_held(&self) -> bool {
        self.inner.meter.held.load(Ordering::SeqCst)
    }

    /// Start the one tick thread of g.
    fn spawn_tick(&self) {
        let mut slot = self.tick.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        let interval = Duration::from_millis({
            let state = self.inner.held();
            state.cfg.tick_ms.max(1)
        });
        let inner = Arc::clone(&self.inner);
        let stop = Arc::clone(&self.stop_flag);
        // The loop sleeps before its first tick, not after it: `start` has just written the
        // initial knobs and drained what the probes recorded, so there is nothing for a tick to
        // do until `controller.tick_ms` has passed, and a test that drives `tick_once` itself
        // is not racing a thread that ticked the instant it was spawned.
        *slot = Some(std::thread::spawn(move || {
            // The wait is sliced so that `stop` joins promptly whatever `tick_ms` is; a run
            // that ends between two ticks should not wait a whole tick to be joined.
            let slice = Duration::from_millis(5).min(interval);
            while !stop.load(Ordering::SeqCst) {
                let mut waited = Duration::ZERO;
                while waited < interval && !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(slice);
                    waited += slice;
                }
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                tick::tick(&inner);
            }
        }));
    }

    fn join_tick(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        let handle = {
            let mut slot = self.tick.lock().unwrap_or_else(|e| e.into_inner());
            slot.take()
        };
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.join_tick();
    }
}
