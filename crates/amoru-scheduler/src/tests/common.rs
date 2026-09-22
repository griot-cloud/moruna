//! Helpers shared by the SC tests (k).
//!
//! Every test drives the scheduler through the fakes of contracts d.15 and names only their
//! knobs. Four things here are not fakes and are not extensions of them, each a test-local
//! implementation of a contract trait as k allows: `RecordingPlacement`, which records the
//! order of the `close` and `set_committed` calls `FakePlacement` does not keep;
//! `NamingSource`, `NamingSink` and `NamingKernel`, which record the name of the thread each
//! call ran on (SC-T1); `SlowSource`, whose read future resolves only after a number of polls
//! (SC-T3); and the stateful kernels of SC-T6, SC-T15 and SC-T18, which need behaviour no fake
//! knob provides.

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use amoru_kernel::{
    Allocator, AmoruError, BoxFuture, CheckpointExtras, ErrorPolicy, Fingerprint, InitCtx, Kernel,
    KernelHints, KernelKind, KernelState, Locality, Morsel, Origin, Payload, PayloadKind,
    PayloadSpec, Placement, PlacementStats, Result, ResumePoint, ResumePolicy, RowRange, Seq, Sink,
    SinkSummary, Source, SourceSchema, Split, StageId, TierBudgets, TierKind, TierPref,
};
use amoru_sinks::SinkHandle;
use amoru_testkit::{
    FakeAllocator, FakeKernel, FakePlacement, FakeSampler, FakeSink, FakeSource, FakeTrace,
};

use crate::{Pipeline, Scheduler, SchedulerConfig};

/// `FakePlacement::with_manifest_store` keys its manifests on one fixed path shared by every
/// engine in the process (d.15), so the tests that use it run one at a time.
pub fn manifest_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A configuration whose morsel range fits the fake source's eight-byte rows.
pub fn cfg() -> SchedulerConfig {
    SchedulerConfig {
        workers_max: 2,
        workers_active: 2,
        read_ahead: 2,
        sink_concurrency: 2,
        error_policy: ErrorPolicy::Terminate,
        initial_morsel_target: 32,
        morsel_min: 8,
        morsel_max: 1 << 30,
        checkpoint_enabled: false,
        checkpoint_interval_ms: 50,
        heartbeat_interval_ms: 1000,
        resuming: false,
        node: amoru_kernel::LOCAL_NODE,
    }
}

/// What a test holds on to: the scheduler and every fake behind it.
pub struct Rig {
    pub scheduler: Scheduler,
    pub placement: Arc<dyn Placement>,
    pub fake_placement: FakePlacement,
    pub recorder: Option<Arc<Recorder>>,
    pub source: FakeSource,
    pub sink: FakeSink,
    pub trace: FakeTrace,
    pub sampler: FakeSampler,
    pub alloc: FakeAllocator,
}

/// Build a rig from the parts a test names; everything not named takes the default fake.
pub struct RigBuilder {
    pub cfg: SchedulerConfig,
    pub source: FakeSource,
    pub source_over: Option<Arc<dyn Source>>,
    pub kernels: Vec<Arc<dyn Kernel>>,
    pub sink: FakeSink,
    pub sink_over: Option<Box<dyn Sink>>,
    pub placement: FakePlacement,
    pub record_placement: bool,
    pub trace: FakeTrace,
    pub sampler: FakeSampler,
    pub alloc: FakeAllocator,
    pub ordered: bool,
}

impl Default for RigBuilder {
    fn default() -> RigBuilder {
        RigBuilder {
            cfg: cfg(),
            source: FakeSource::new(),
            source_over: None,
            kernels: Vec::new(),
            sink: FakeSink::new(),
            sink_over: None,
            placement: FakePlacement::new(),
            record_placement: false,
            trace: FakeTrace::new().capacity(1 << 17),
            sampler: FakeSampler::new(),
            alloc: FakeAllocator::new(),
            ordered: false,
        }
    }
}

impl RigBuilder {
    pub fn new() -> RigBuilder {
        RigBuilder::default()
    }

    pub fn cfg(mut self, f: impl FnOnce(&mut SchedulerConfig)) -> RigBuilder {
        f(&mut self.cfg);
        self
    }

    pub fn source(mut self, source: FakeSource) -> RigBuilder {
        self.source = source;
        self
    }

    pub fn source_over(mut self, source: Arc<dyn Source>) -> RigBuilder {
        self.source_over = Some(source);
        self
    }

    pub fn sink(mut self, sink: FakeSink) -> RigBuilder {
        self.sink = sink;
        self
    }

    pub fn sink_over(mut self, sink: Box<dyn Sink>) -> RigBuilder {
        self.sink_over = Some(sink);
        self
    }

    pub fn placement(mut self, placement: FakePlacement) -> RigBuilder {
        self.placement = placement;
        self
    }

    pub fn recording_placement(mut self) -> RigBuilder {
        self.record_placement = true;
        self
    }

    pub fn trace_capacity(mut self, n: usize) -> RigBuilder {
        self.trace = FakeTrace::new().capacity(n);
        self
    }

    pub fn ordered(mut self, ordered: bool) -> RigBuilder {
        self.ordered = ordered;
        self
    }

    pub fn sampler(mut self, sampler: FakeSampler) -> RigBuilder {
        self.sampler = sampler;
        self
    }

    pub fn alloc(mut self, alloc: FakeAllocator) -> RigBuilder {
        self.alloc = alloc;
        self
    }

    pub fn kernel(mut self, kernel: Arc<dyn Kernel>) -> RigBuilder {
        self.kernels.push(kernel);
        self
    }

    pub fn stages(mut self, n: usize) -> RigBuilder {
        for _ in 0..n {
            self.kernels.push(Arc::new(FakeKernel::new()));
        }
        self
    }

    pub fn build(self) -> Result<Rig> {
        let fake_placement = self.placement.clone();
        let (placement, recorder): (Arc<dyn Placement>, Option<Arc<Recorder>>) =
            if self.record_placement {
                let recorded = RecordingPlacement::new(self.placement.clone());
                let recorder = recorded.recorder();
                (Arc::new(recorded), Some(recorder))
            } else {
                (Arc::new(self.placement.clone()), None)
            };
        let source: Arc<dyn Source> = match self.source_over {
            Some(source) => source,
            None => Arc::new(self.source.clone()),
        };
        let inner: Box<dyn Sink> = match self.sink_over {
            Some(sink) => sink,
            None => Box::new(self.sink.clone()),
        };
        let pipeline = Pipeline {
            source,
            kernels: self.kernels,
            sink: SinkHandle::wrap(inner, self.ordered, 256 * 1024 * 1024),
        };
        let scheduler = Scheduler::new(
            self.cfg,
            pipeline,
            placement.clone(),
            Arc::new(self.alloc.clone()),
            Arc::new(self.trace.clone()),
            Arc::new(self.sampler.clone()),
        )?;
        Ok(Rig {
            scheduler,
            placement,
            fake_placement,
            recorder,
            source: self.source,
            sink: self.sink,
            trace: self.trace,
            sampler: self.sampler,
            alloc: self.alloc,
        })
    }

    /// Build and unwrap; a failure here is a broken fixture, not a result.
    pub fn go(self) -> Rig {
        match self.build() {
            Ok(rig) => rig,
            Err(e) => panic!("the rig could not be built: {e}"),
        }
    }
}

/// What `RecordingPlacement` keeps that `FakePlacement` does not.
#[derive(Default)]
pub struct Recorder {
    pub closes: Mutex<Vec<StageId>>,
    pub committed: Mutex<Vec<Seq>>,
    pub replaced: Mutex<Vec<Seq>>,
    pub running_at_close: Mutex<Vec<(StageId, u64)>>,
}

/// A `Placement` that records the order of the calls SC-T9 and SC-T14 assert on, and delegates
/// everything to `FakePlacement`.
pub struct RecordingPlacement {
    inner: FakePlacement,
    recorder: Arc<Recorder>,
}

impl RecordingPlacement {
    pub fn new(inner: FakePlacement) -> RecordingPlacement {
        RecordingPlacement {
            inner,
            recorder: Arc::new(Recorder::default()),
        }
    }

    pub fn recorder(&self) -> Arc<Recorder> {
        Arc::clone(&self.recorder)
    }
}

impl Placement for RecordingPlacement {
    fn push(&self, stage: StageId, morsel: Morsel) -> Result<()> {
        self.inner.push(stage, morsel)
    }

    fn pop(&self, stage: StageId, want: PayloadSpec, locality: Locality) -> Result<Option<Morsel>> {
        self.inner.pop(stage, want, locality)
    }

    fn pop_blocking(
        &self,
        stage: StageId,
        want: PayloadSpec,
        locality: Locality,
    ) -> Result<Option<(Morsel, u64)>> {
        self.inner.pop_blocking(stage, want, locality)
    }

    fn peek_resident(&self, stage: StageId, want: PayloadSpec, locality: Locality) -> bool {
        self.inner.peek_resident(stage, want, locality)
    }

    fn evicted(&self, stage: StageId) -> Vec<(Seq, Origin)> {
        self.inner.evicted(stage)
    }

    fn replace(&self, stage: StageId, morsel: Morsel) -> Result<()> {
        self.recorder
            .replaced
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(morsel.seq);
        self.inner.replace(stage, morsel)
    }

    fn shutdown(&self) {
        self.inner.shutdown();
    }

    fn set_committed(&self, seq: Seq) {
        self.recorder
            .committed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(seq);
        self.inner.set_committed(seq);
    }

    fn checkpoint(&self, extras: &CheckpointExtras) -> Result<PathBuf> {
        self.inner.checkpoint(extras)
    }

    fn restore(
        &self,
        manifest: &Path,
        plan: &[Split],
        fingerprints: &[Fingerprint],
    ) -> Result<ResumePoint> {
        self.inner.restore(manifest, plan, fingerprints)
    }

    fn is_full(&self, stage: StageId) -> bool {
        self.inner.is_full(stage)
    }

    fn set_consumer(&self, stage: StageId, want: PayloadSpec) {
        self.inner.set_consumer(stage, want);
    }

    fn set_budgets(&self, budgets: TierBudgets) {
        self.inner.set_budgets(budgets);
    }

    fn set_water(&self, stage: StageId, tier: TierKind, low: u64, high: u64) {
        self.inner.set_water(stage, tier, low, high);
    }

    fn set_staging(&self, stage: StageId, enabled: bool) {
        self.inner.set_staging(stage, enabled);
    }

    fn set_promotion_window(&self, stage: StageId, morsels: u16) {
        self.inner.set_promotion_window(stage, morsels);
    }

    fn close(&self, stage: StageId) {
        self.recorder
            .closes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(stage);
        self.inner.close(stage);
    }

    fn stats(&self) -> PlacementStats {
        self.inner.stats()
    }
}

/// The name of the thread a call ran on, for SC-T1.
fn thread_name() -> String {
    std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string()
}

/// Every thread name a set of calls ran on.
#[derive(Default)]
pub struct Names(Mutex<HashSet<String>>);

impl Names {
    pub fn add(&self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(thread_name());
    }

    pub fn seen(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        names.sort();
        names
    }
}

/// A `Source` that records the thread every `read` ran on (SC-T1).
pub struct NamingSource {
    inner: FakeSource,
    pub names: Arc<Names>,
}

impl NamingSource {
    pub fn new(inner: FakeSource) -> NamingSource {
        NamingSource {
            inner,
            names: Arc::new(Names::default()),
        }
    }
}

impl Source for NamingSource {
    fn schema(&self) -> SourceSchema {
        Source::schema(&self.inner)
    }

    fn plan(&self) -> Result<Vec<Split>> {
        self.inner.plan()
    }

    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: amoru_kernel::Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        self.names.add();
        self.inner.read(split, rows, alloc, tier)
    }
}

/// A `Sink` that records the thread every `write` ran on (SC-T1).
pub struct NamingSink {
    inner: FakeSink,
    pub names: Arc<Names>,
}

impl NamingSink {
    pub fn new(inner: FakeSink) -> NamingSink {
        NamingSink {
            inner,
            names: Arc::new(Names::default()),
        }
    }
}

impl Sink for NamingSink {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        self.inner.open(schema)
    }

    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        self.names.add();
        self.inner.write(seq, payload)
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        self.inner.finish()
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.inner.committed_seq()
    }

    fn skip(&self, seq: Seq) {
        self.inner.skip(seq);
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        self.inner.checkpoint()
    }
}

/// A `Kernel` that records the thread every `apply` ran on (SC-T1).
pub struct NamingKernel {
    inner: FakeKernel,
    pub names: Arc<Names>,
}

impl NamingKernel {
    pub fn new() -> NamingKernel {
        NamingKernel {
            inner: FakeKernel::new(),
            names: Arc::new(Names::default()),
        }
    }
}

impl Kernel for NamingKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.inner.fingerprint()
    }

    fn kind(&self) -> KernelKind {
        self.inner.kind()
    }

    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.inner.output_schema(input)
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        self.inner.init(ctx)
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        self.names.add();
        self.inner.apply(state, input)
    }
}

/// A `Source` whose read future resolves only after `polls` polls, so SC-T3 can hold reads open
/// and count how many the drive keeps in flight.
pub struct SlowSource {
    inner: FakeSource,
    polls: usize,
    pub peak_in_flight: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
}

impl SlowSource {
    pub fn new(inner: FakeSource, polls: usize) -> SlowSource {
        SlowSource {
            inner,
            polls,
            peak_in_flight: Arc::new(AtomicUsize::new(0)),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Source for SlowSource {
    fn schema(&self) -> SourceSchema {
        Source::schema(&self.inner)
    }

    fn plan(&self) -> Result<Vec<Split>> {
        self.inner.plan()
    }

    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: amoru_kernel::Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        let live = Arc::clone(&self.live);
        let peak = Arc::clone(&self.peak_in_flight);
        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        let inner = self.inner.read(split, rows, alloc, tier);
        let mut left = self.polls;
        let mut inner = Some(inner);
        Box::pin(std::future::poll_fn(move |cx| {
            if left > 0 {
                left -= 1;
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            match inner.as_mut() {
                Some(future) => {
                    let poll = future.as_mut().poll(cx);
                    if poll.is_ready() {
                        live.fetch_sub(1, Ordering::SeqCst);
                    }
                    poll
                }
                None => std::task::Poll::Pending,
            }
        }))
    }
}

/// A stateful kernel that fails `init` for one instance (SC-T18) or refuses to checkpoint
/// (SC-T15), and that shouts when two applies overlap on one instance (SC-T6).
pub struct StatefulKernel {
    pub instances: usize,
    pub fail_init_for: Option<usize>,
    pub checkpoint_returns_none: bool,
    pub resume: ResumePolicy,
    pub latency: Duration,
    pub init_calls: Arc<AtomicU64>,
    pub restore_calls: Arc<AtomicU64>,
    pub checkpoint_calls: Arc<AtomicU64>,
    pub checkpoint_threads: Arc<Names>,
    pub overlap: Arc<AtomicBool>,
    pub applies: Arc<Mutex<Vec<(usize, std::thread::ThreadId)>>>,
    busy: Arc<Mutex<Vec<bool>>>,
}

impl StatefulKernel {
    pub fn new(instances: usize) -> StatefulKernel {
        StatefulKernel {
            instances,
            fail_init_for: None,
            checkpoint_returns_none: false,
            resume: ResumePolicy::Reinit,
            latency: Duration::ZERO,
            init_calls: Arc::new(AtomicU64::new(0)),
            restore_calls: Arc::new(AtomicU64::new(0)),
            checkpoint_calls: Arc::new(AtomicU64::new(0)),
            checkpoint_threads: Arc::new(Names::default()),
            overlap: Arc::new(AtomicBool::new(false)),
            applies: Arc::new(Mutex::new(Vec::new())),
            busy: Arc::new(Mutex::new(vec![false; instances])),
        }
    }

    pub fn fail_init_for(mut self, instance: usize) -> StatefulKernel {
        self.fail_init_for = Some(instance);
        self
    }

    pub fn checkpointing(mut self) -> StatefulKernel {
        self.resume = ResumePolicy::Checkpoint;
        self
    }

    pub fn checkpoint_returns_none(mut self) -> StatefulKernel {
        self.checkpoint_returns_none = true;
        self
    }

    pub fn latency(mut self, latency: Duration) -> StatefulKernel {
        self.latency = latency;
        self
    }

    /// The share of applies that reused the instance this thread last held (SC-T6).
    pub fn affinity(&self) -> f64 {
        let applies = self.applies.lock().unwrap_or_else(|e| e.into_inner());
        let mut last: std::collections::HashMap<std::thread::ThreadId, usize> =
            std::collections::HashMap::new();
        let (mut same, mut total) = (0u64, 0u64);
        for (instance, thread) in applies.iter() {
            if let Some(previous) = last.get(thread) {
                total += 1;
                if previous == instance {
                    same += 1;
                }
            }
            last.insert(*thread, *instance);
        }
        if total == 0 {
            1.0
        } else {
            same as f64 / total as f64
        }
    }
}

/// The state a `StatefulKernel` instance keeps.
pub struct WatchedState {
    instance: usize,
    busy: Arc<Mutex<Vec<bool>>>,
    overlap: Arc<AtomicBool>,
    checkpoint_calls: Arc<AtomicU64>,
    checkpoint_threads: Arc<Names>,
    returns_none: bool,
}

impl KernelState for WatchedState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn checkpoint(&mut self) -> Result<Option<Vec<u8>>> {
        self.checkpoint_calls.fetch_add(1, Ordering::SeqCst);
        self.checkpoint_threads.add();
        if self.returns_none {
            return Ok(None);
        }
        Ok(Some((self.instance as u64).to_le_bytes().to_vec()))
    }

    fn footprint(&self) -> Option<u64> {
        Some(self.instance as u64)
    }
}

impl Kernel for StatefulKernel {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("amoru-scheduler::tests::StatefulKernel", &[])
    }

    fn kind(&self) -> KernelKind {
        match core::num::NonZeroUsize::new(self.instances) {
            Some(max_instances) => KernelKind::Stateful { max_instances },
            None => KernelKind::Stateless,
        }
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            resume: self.resume,
            ..KernelHints::default()
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Either,
            tier: TierPref::Any,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        if self.fail_init_for == Some(ctx.instance) {
            return Err(AmoruError::Kernel {
                stage: 0,
                seq: 0,
                msg: "init failed".into(),
                features: None,
            });
        }
        self.init_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(WatchedState {
            instance: ctx.instance,
            busy: Arc::clone(&self.busy),
            overlap: Arc::clone(&self.overlap),
            checkpoint_calls: Arc::clone(&self.checkpoint_calls),
            checkpoint_threads: Arc::clone(&self.checkpoint_threads),
            returns_none: self.checkpoint_returns_none,
        }))
    }

    fn restore(&self, ctx: &InitCtx, _state: &[u8]) -> Result<Box<dyn KernelState>> {
        self.restore_calls.fetch_add(1, Ordering::SeqCst);
        self.init(ctx)
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let instance = state
            .as_any_mut()
            .downcast_mut::<WatchedState>()
            .map_or(usize::MAX, |watched| watched.instance);
        {
            let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = busy.get_mut(instance) {
                if *slot {
                    self.overlap.store(true, Ordering::SeqCst);
                }
                *slot = true;
            }
        }
        self.applies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((instance, std::thread::current().id()));
        if !self.latency.is_zero() {
            std::thread::sleep(self.latency);
        }
        {
            let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = busy.get_mut(instance) {
                *slot = false;
            }
        }
        Ok(input)
    }
}

/// Wait until `predicate` holds or the deadline passes; returns whether it held.
pub fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A sampler whose scripted samples give the probe a peak to report (SC-T10).
pub fn scripted_sampler(samples: Vec<amoru_kernel::Sample>) -> FakeSampler {
    FakeSampler::new().scripted(samples)
}
