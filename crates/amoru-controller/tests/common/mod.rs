//! Test-only helpers shared by the RC tests.
//!
//! Every test drives the controller through the fakes of contracts d.15 and names only their
//! knobs (11 k): `FakeKnobs` as `Knobs`, `StatsSource` and `Prober`, `FakeSampler`,
//! `FakeTrace` as `TraceTail`, and `FakePlacement` for `set_budgets`. Two things here are not
//! fakes and are not extensions of them: `RecordingProber`, which counts the probe calls
//! `FakeKnobs` answers but does not observe, and `Watcher`, which wraps each fake to assert
//! the controller's mutex is free while it is called (RC-T17). Both delegate to the fake.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use amoru_controller::{Controller, ControllerConfig, KernelInfo, PlanSummary};
use amoru_kernel::{
    Device, DeviceId, Fingerprint, KernelHints, KernelKind, Knob, Knobs, LimitSource, Limits,
    Outcome, Placement, ProbeResult, Prober, Result, Sample, Sampler, SchedulerStats, Seq,
    SizerKind, StageId, StageStats, StatsSource, TierBudgets, TraceRecord, TraceTail,
};
use amoru_testkit::{FakeKnobs, FakePlacement, FakeSampler, FakeTrace};

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

/// A limits value with the given host ceiling and no device.
pub fn limits(ceiling: u64) -> Limits {
    Limits {
        memory_ceiling: ceiling,
        memory_kill: None,
        cpu_quota: 8.0,
        page_bytes: 4096,
        devices: Vec::new(),
        source: LimitSource::Cgroup,
    }
}

/// The same with one device of the given free bytes.
pub fn limits_with_device(ceiling: u64, free: u64) -> Limits {
    Limits {
        devices: vec![Device {
            id: DeviceId(0),
            total_bytes: free * 2,
            free_bytes: free,
            name: "test device".into(),
        }],
        ..limits(ceiling)
    }
}

/// A plan far larger than any budget a test uses, so the tiny-dataset path of f.10 is off.
pub fn plan() -> PlanSummary {
    PlanSummary {
        total_bytes: 100 * GIB,
        total_rows: 1_000_000_000,
        splits: 100,
        max_split_bytes: GIB,
        sub_splittable_all: true,
    }
}

/// The preamble section 5 defaults, with the limits, the plan and the worker count a test sets.
/// The arena is sized as the facade sizes it (12 f.1): the ceiling less the reserve, with a
/// baseline of nothing. `config_with_baseline` is the variant for a test that scripts one.
pub fn config(ceiling: u64, workers: u16) -> ControllerConfig {
    config_with_baseline(ceiling, workers, 0)
}

/// `config` for a test whose sampler reports `baseline` anonymous bytes before the arena: the
/// arena is `ceiling - baseline - reserve` and that is the controller's whole allowance
/// (11 f.1).
pub fn config_with_baseline(ceiling: u64, workers: u16, baseline: u64) -> ControllerConfig {
    let reserve = (ceiling as f64 * f64::from(ControllerConfig::default().reserve_fraction)) as u64;
    ControllerConfig {
        limits: limits(ceiling),
        plan: plan(),
        workers_max: workers,
        arena_bytes: ceiling.saturating_sub(baseline).saturating_sub(reserve),
        baseline_bytes: baseline,
        // A tick a test never waits for: every test drives `tick_once` itself, so the thread
        // `start` spawns must not race it.
        tick_ms: 3_600_000,
        ..ControllerConfig::default()
    }
}

/// One kernel stage with the given hints.
pub fn kernel(stage: StageId, hints: KernelHints) -> KernelInfo {
    KernelInfo {
        stage,
        fingerprint: Fingerprint::compute(&format!("test kernel {stage}"), b""),
        schema_hash: [stage as u8; 32],
        hints,
        kind: KernelKind::Stateless,
    }
}

/// One stateful kernel stage.
pub fn stateful_kernel(stage: StageId, instances: usize, hints: KernelHints) -> KernelInfo {
    KernelInfo {
        kind: KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::new(instances)
                .unwrap_or(core::num::NonZeroUsize::new(1).expect("one is not zero")),
        },
        ..kernel(stage, hints)
    }
}

/// A probe result with the given amplification over the given input bytes.
pub fn probe(bytes_in: u64, amplification: f64) -> ProbeResult {
    ProbeResult {
        bytes_in,
        rows_in: bytes_in / 100,
        peak_delta: (bytes_in as f64 * amplification) as u64,
        dev_peak_delta: 0,
        wall_ns: 1_000_000,
        cpu_ns: 1_000_000,
    }
}

/// A sample with the given anonymous bytes and a moving clock, so the stall detector of h does
/// not fire during a test that takes many ticks.
pub fn sample(anon: u64, at: u64) -> Sample {
    Sample {
        anon_bytes: anon,
        file_bytes: 0,
        peak_anon_bytes: anon,
        throttled_us: 0,
        device_used: [0; 8],
        at_ns: at,
    }
}

/// A run of samples at a steady anonymous figure, one per tick a test will take.
pub fn steady(anon: u64, count: usize) -> Vec<Sample> {
    (0..count)
        .map(|at| sample(anon, 1_000_000 + at as u64 * 1_000_000))
        .collect()
}

/// A trace record for one completed morsel.
pub fn record(seq: Seq, stage: StageId, bytes_in: u64, peak_delta: u64) -> TraceRecord {
    TraceRecord {
        seq,
        stage,
        worker: 0,
        instance: u16::MAX,
        t_start_ns: 1_000 * seq,
        t_end_ns: 1_000 * seq + 500,
        rows_in: bytes_in / 100,
        bytes_in,
        rows_out: bytes_in / 100,
        bytes_out: bytes_in,
        tier_in: 2,
        tier_out: 2,
        feat_mean_string_len: 12.0,
        feat_null_ratio: 0.0,
        feat_column_bytes: vec![bytes_in],
        knob_morsel_target: bytes_in,
        knob_active_workers: 1,
        knob_read_ahead: 2,
        mem_anon_before: 400 * MIB,
        mem_anon_peak: 400 * MIB + peak_delta,
        dev_mem_peak: 0,
        cpu_time_us: 500,
        throttled_delta_us: 0,
        q_bytes_before: vec![0, 0, 0, 0, 0],
        q_bytes_after: vec![0, 0, 0, 0, 0],
        staging_bytes_delta: 0,
        placement_miss_wait_us: 0,
        state_bytes: 0,
        sizer: 0,
        outcome: Outcome::Ok,
        error: None,
    }
}

/// Scheduler statistics with the given worker counts and one stage.
pub fn stats(active: u16, busy: u16) -> SchedulerStats {
    SchedulerStats {
        per_stage: vec![StageStats {
            stage: 1,
            tasks: 0,
            busy_ns: 0,
            errors: 0,
            skipped: 0,
            instances_live: 1,
        }],
        workers_active: active,
        workers_busy: busy,
        ..SchedulerStats::default()
    }
}

/// Every `MorselTarget` written, in call order.
pub fn morsel_targets(writes: &[Knob]) -> Vec<(StageId, u64)> {
    writes
        .iter()
        .filter_map(|knob| match knob {
            Knob::MorselTarget { stage, bytes } => Some((*stage, *bytes)),
            _ => None,
        })
        .collect()
}

/// Every `ActiveWorkers` written, in call order.
pub fn active_workers(writes: &[Knob]) -> Vec<u16> {
    writes
        .iter()
        .filter_map(|knob| match knob {
            Knob::ActiveWorkers(workers) => Some(*workers),
            _ => None,
        })
        .collect()
}

/// Every `ReadAhead` written, in call order.
pub fn read_aheads(writes: &[Knob]) -> Vec<u16> {
    writes
        .iter()
        .filter_map(|knob| match knob {
            Knob::ReadAhead(splits) => Some(*splits),
            _ => None,
        })
        .collect()
}

/// Every `HighWater` written, in call order.
pub fn high_waters(writes: &[Knob]) -> Vec<(StageId, amoru_kernel::TierKind, u64)> {
    writes
        .iter()
        .filter_map(|knob| match knob {
            Knob::HighWater { stage, tier, bytes } => Some((*stage, *tier, *bytes)),
            _ => None,
        })
        .collect()
}

/// Every `StagingTrigger` written, in call order.
pub fn staging_triggers(writes: &[Knob]) -> Vec<(StageId, bool)> {
    writes
        .iter()
        .filter_map(|knob| match knob {
            Knob::StagingTrigger { stage, on } => Some((*stage, *on)),
            _ => None,
        })
        .collect()
}

/// A `Prober` that counts what it was asked, because `FakeKnobs` observes knob writes and the
/// diagnostic but not the probe calls RC-T6 and RC-T11 are about. It answers from the fake.
pub struct RecordingProber {
    inner: FakeKnobs,
    calls: Mutex<Vec<(StageId, u64)>>,
    /// When set, every probe fails with this `Plan` message instead of answering.
    fails_with: Option<String>,
}

impl RecordingProber {
    pub fn new(inner: FakeKnobs) -> Arc<RecordingProber> {
        Arc::new(RecordingProber {
            inner,
            calls: Mutex::new(Vec::new()),
            fails_with: None,
        })
    }

    /// A prober that refuses every probe with a `Plan` error carrying `msg`, which is what the
    /// scheduler's helper answers when the source plan has nothing left to read (SC f.9).
    pub fn failing(inner: FakeKnobs, msg: &str) -> Arc<RecordingProber> {
        Arc::new(RecordingProber {
            inner,
            calls: Mutex::new(Vec::new()),
            fails_with: Some(msg.to_string()),
        })
    }

    /// Every probe the controller asked for, in call order.
    pub fn calls(&self) -> Vec<(StageId, u64)> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Prober for RecordingProber {
    fn probe(&self, stage: StageId, bytes: u64) -> Result<ProbeResult> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((stage, bytes));
        if let Some(msg) = &self.fails_with {
            return Err(amoru_kernel::AmoruError::Plan(msg.clone()));
        }
        self.inner.probe(stage, bytes)
    }
}

/// The controller plus the fakes it was built over.
pub struct Rig {
    pub controller: Controller,
    pub knobs: FakeKnobs,
    pub sampler: FakeSampler,
    pub trace: FakeTrace,
    pub placement: FakePlacement,
    pub prober: Arc<RecordingProber>,
}

impl Rig {
    /// Build the controller over the fakes. The fakes must be fully configured first: every
    /// `FakeKnobs` and `FakeTrace` builder method returns a new fake, so a clone taken before
    /// the last one would observe a different instance.
    pub fn new(
        cfg: ControllerConfig,
        kernels: Vec<KernelInfo>,
        knobs: FakeKnobs,
        sampler: FakeSampler,
    ) -> Rig {
        let prober = RecordingProber::new(knobs.clone());
        Rig::with_prober(cfg, kernels, knobs, sampler, prober)
    }

    /// `new` with a `Prober` of the test's own, for a test that needs a probe to fail
    /// (`FakeKnobs` only ever succeeds).
    pub fn with_prober(
        cfg: ControllerConfig,
        kernels: Vec<KernelInfo>,
        knobs: FakeKnobs,
        sampler: FakeSampler,
        prober: Arc<RecordingProber>,
    ) -> Rig {
        let trace = FakeTrace::new();
        let placement = FakePlacement::new();
        let controller = Controller::new(
            cfg,
            Arc::new(knobs.clone()),
            Arc::new(knobs.clone()),
            prober.clone(),
            Arc::new(sampler.clone()),
            Arc::new(trace.clone()),
            Arc::new(placement.clone()),
            kernels,
        )
        .expect("controller");
        Rig {
            controller,
            knobs,
            sampler,
            trace,
            placement,
            prober,
        }
    }

    /// `prepare`, `probe_all` and `start`, which is the facade's order (preamble 4.4).
    pub fn run_up(&self) {
        self.controller.prepare().expect("prepare");
        self.controller.probe_all().expect("probe_all");
        self.controller.start().expect("start");
    }

    /// Feed one record to the trace and then to the controller's hook, in that order, which is
    /// the order the scheduler uses (contracts d.11).
    pub fn feed(&self, r: &TraceRecord) {
        use amoru_kernel::TraceSink;
        self.trace.record(r.clone());
        self.controller.on_record(r);
    }

    /// Every knob written so far.
    pub fn writes(&self) -> Vec<Knob> {
        self.knobs.writes()
    }
}

/// A scratch directory unique to this process and this test, because several gates run on one
/// machine at a time and a fixed path makes two runs delete each other's files (preamble 6.7).
pub struct Scratch(PathBuf);

static SCRATCHES: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    pub fn new(tag: &str) -> Scratch {
        let at = SCRATCHES.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "amoru-controller-{tag}-{}-{at}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch dir");
        Scratch(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The tier budgets the placement engine was last given.
pub fn last_budgets(placement: &FakePlacement) -> Option<TierBudgets> {
    placement.budgets_set().last().cloned()
}

/// The host pool of a tier budget, whichever of the two host tiers it is on.
pub fn host_pool(budgets: &TierBudgets) -> u64 {
    budgets.host + budgets.pinned_host
}

/// Assert that the controller's mutex is free. Used by `Watcher` (RC-T17).
pub fn assert_free(controller: &Controller, what: &str) {
    assert!(
        !controller.lock_is_held(),
        "{what} was called while the controller's mutex was held (RC-I10)"
    );
}

/// Wraps the fakes so every call through them asserts the controller's mutex is free (RC-T17).
pub struct Watcher<T> {
    inner: T,
    controller: Arc<std::sync::OnceLock<std::sync::Weak<Controller>>>,
}

impl<T> Watcher<T> {
    pub fn new(
        inner: T,
        controller: Arc<std::sync::OnceLock<std::sync::Weak<Controller>>>,
    ) -> Watcher<T> {
        Watcher { inner, controller }
    }

    fn check(&self, what: &str) {
        if let Some(weak) = self.controller.get()
            && let Some(controller) = weak.upgrade()
        {
            assert_free(&controller, what);
        }
    }
}

impl Knobs for Watcher<FakeKnobs> {
    fn set(&self, knob: Knob) {
        self.check("Knobs::set");
        self.inner.set(knob);
    }

    fn snapshot(&self) -> amoru_kernel::KnobSnapshot {
        self.check("Knobs::snapshot");
        self.inner.snapshot()
    }

    fn terminate(&self, diagnostic: amoru_kernel::AmoruError) {
        self.check("Knobs::terminate");
        self.inner.terminate(diagnostic);
    }
}

impl StatsSource for Watcher<FakeKnobs> {
    fn scheduler_stats(&self) -> SchedulerStats {
        self.check("StatsSource::scheduler_stats");
        self.inner.scheduler_stats()
    }
}

impl Prober for Watcher<FakeKnobs> {
    fn probe(&self, stage: StageId, bytes: u64) -> Result<ProbeResult> {
        self.check("Prober::probe");
        self.inner.probe(stage, bytes)
    }
}

impl Sampler for Watcher<FakeSampler> {
    fn sample(&self) -> Sample {
        self.check("Sampler::sample");
        self.inner.sample()
    }

    fn reset_peak(&self) {
        self.check("Sampler::reset_peak");
        self.inner.reset_peak();
    }
}

impl TraceTail for Watcher<FakeTrace> {
    fn tail(&self, stage: StageId, n: usize) -> Vec<TraceRecord> {
        self.check("TraceTail::tail");
        self.inner.tail(stage, n)
    }
}

/// `FakePlacement` implements the whole of `Placement`; only `set_budgets` is ever called by
/// the controller (11 l, anti-patterns), and every other method here says so.
pub struct WatchedPlacement {
    pub inner: FakePlacement,
    pub controller: Arc<std::sync::OnceLock<std::sync::Weak<Controller>>>,
    pub other_calls: AtomicU64,
}

impl Placement for WatchedPlacement {
    fn push(&self, _stage: StageId, _morsel: amoru_kernel::Morsel) -> Result<()> {
        self.other();
        Ok(())
    }

    fn pop(
        &self,
        _stage: StageId,
        _want: amoru_kernel::PayloadSpec,
        _locality: amoru_kernel::Locality,
    ) -> Result<Option<amoru_kernel::Morsel>> {
        self.other();
        Ok(None)
    }

    fn pop_blocking(
        &self,
        _stage: StageId,
        _want: amoru_kernel::PayloadSpec,
        _locality: amoru_kernel::Locality,
    ) -> Result<Option<(amoru_kernel::Morsel, u64)>> {
        self.other();
        Ok(None)
    }

    fn peek_resident(
        &self,
        _stage: StageId,
        _want: amoru_kernel::PayloadSpec,
        _locality: amoru_kernel::Locality,
    ) -> bool {
        self.other();
        false
    }

    fn evicted(&self, _stage: StageId) -> Vec<(Seq, amoru_kernel::Origin)> {
        self.other();
        Vec::new()
    }

    fn replace(&self, _stage: StageId, _morsel: amoru_kernel::Morsel) -> Result<()> {
        self.other();
        Ok(())
    }

    fn shutdown(&self) {
        self.other();
    }

    fn set_committed(&self, _seq: Seq) {
        self.other();
    }

    fn checkpoint(&self, _extras: &amoru_kernel::CheckpointExtras) -> Result<PathBuf> {
        self.other();
        Err(amoru_kernel::AmoruError::Resume(
            "no staging directory".into(),
        ))
    }

    fn restore(
        &self,
        _manifest: &std::path::Path,
        _plan: &[amoru_kernel::Split],
        _fingerprints: &[Fingerprint],
    ) -> Result<amoru_kernel::ResumePoint> {
        self.other();
        Err(amoru_kernel::AmoruError::Resume("not a resumed run".into()))
    }

    fn is_full(&self, _stage: StageId) -> bool {
        self.other();
        false
    }

    fn set_consumer(&self, _stage: StageId, _want: amoru_kernel::PayloadSpec) {
        self.other();
    }

    fn set_budgets(&self, budgets: TierBudgets) {
        if let Some(weak) = self.controller.get()
            && let Some(controller) = weak.upgrade()
        {
            assert_free(&controller, "Placement::set_budgets");
        }
        self.inner.set_budgets(budgets);
    }

    fn set_water(&self, _stage: StageId, _tier: amoru_kernel::TierKind, _low: u64, _high: u64) {
        self.other();
    }

    fn set_staging(&self, _stage: StageId, _enabled: bool) {
        self.other();
    }

    fn set_promotion_window(&self, _stage: StageId, _morsels: u16) {
        self.other();
    }

    fn close(&self, _stage: StageId) {
        self.other();
    }

    fn stats(&self) -> amoru_kernel::PlacementStats {
        self.other();
        amoru_kernel::PlacementStats::default()
    }
}

impl WatchedPlacement {
    fn other(&self) {
        self.other_calls.fetch_add(1, Ordering::SeqCst);
    }
}

/// The default sizer kind, named so a test that changes it says so.
pub const RULE: SizerKind = SizerKind::Rule;
