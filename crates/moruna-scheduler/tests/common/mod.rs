//! One run of a DAG through the scheduler and the real placement engine (10 k): what SC-T33 checks
//! and what the policy matrix SC-B1 measures. Sources are `FakeSource`s of whole splits, stages are
//! `Resize` kernels (whose output size no prediction knows), sinks are `FakeSink`s, and the
//! placement is `PlacementEngine` over the testkit's allocator and reactor, with a staging
//! directory of its own. A case may give the run a cost model (f.22): `loglinear` learning from
//! nothing, or [`Wrong`], `loglinear` with every prediction scaled by a factor that changes
//! mid-run, so the predictions are wrong by any factor and drift. Nothing here reaches into the
//! scheduler: it is driven as an embedder drives it.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use moruna_cost::{
    CostKey, CostModel, Defaults, Digest, FitReport, InUse, Inputs, LocalStore, LogLinear, ModelId,
    ObservationSource, ObservationStore, Online, Target, UnitCost,
};
use moruna_job::Role;
use moruna_kernel::{
    CancelToken, CatalogStats, Kernel, Placement, PolicyStats, Sink, Source, StatsSource,
    TierBudgets, TierKind, Topology,
};
use moruna_placement::{PlacementConfig, PlacementEngine};
use moruna_scheduler::{Pipeline, RunOutcome, Scheduler, SchedulerConfig};
use moruna_sinks::SinkHandle;
use moruna_testkit::dag::Resize;
use moruna_testkit::{FakeAllocator, FakeReactor, FakeSampler, FakeSink, FakeSource, FakeTrace};

static SCRATCH: AtomicU64 = AtomicU64::new(0);

/// A directory only this run uses, removed when it goes.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(label: &str) -> Scratch {
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "moruna-scheduler-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        if let Err(e) = std::fs::create_dir_all(&path) {
            panic!("scratch {}: {e}", path.display());
        }
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The cost model a case gives its run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Model {
    /// None: the policies read measured ratios.
    None,
    /// `loglinear`, learning from nothing during the run.
    LogLinear,
    /// [`Wrong`]: `loglinear`'s predictions times `first`, then times `then` once `after` units
    /// have finished.
    Wrong { first: f64, then: f64, after: u64 },
}

impl Model {
    /// Its name in a report.
    pub fn name(&self) -> &'static str {
        match self {
            Model::None => "ratio",
            Model::LogLinear => "loglinear",
            Model::Wrong { .. } => "wrong",
        }
    }
}

/// `loglinear`, with every prediction scaled by a factor that changes after a number of units:
/// a model wrong by any factor, and drifting mid-run.
pub struct Wrong {
    inner: LogLinear,
    first: f64,
    then: f64,
    after: u64,
    seen: u64,
}

impl Wrong {
    pub fn new(first: f64, then: f64, after: u64) -> Wrong {
        Wrong {
            inner: LogLinear::new(Defaults::standard()),
            first,
            then,
            after,
            seen: 0,
        }
    }
}

impl CostModel for Wrong {
    fn id(&self) -> ModelId {
        ModelId::new("wrong", 1)
    }

    fn predict(&self, key: &CostKey, inputs: &Inputs) -> UnitCost {
        let factor = if self.seen >= self.after {
            self.then
        } else {
            self.first
        };
        let mut cost = self.inner.predict(key, inputs);
        for target in Target::ALL {
            *cost.get_mut(target) = cost.get(target).scaled(factor);
        }
        cost
    }

    fn observe(&mut self, key: &CostKey, inputs: &Inputs, outcome: &moruna_cost::Outcome) {
        self.seen += 1;
        self.inner.observe(key, inputs, outcome);
    }

    fn fit(&mut self, history: &dyn ObservationSource) -> moruna_cost::Result<FitReport> {
        self.inner.fit(history)
    }

    fn save(&self) -> moruna_cost::Result<Vec<u8>> {
        self.inner.save()
    }

    fn load(&mut self, bytes: &[u8]) -> moruna_cost::Result<()> {
        self.inner.load(bytes)
    }

    fn set_drifting(&mut self, key: &CostKey, drifting: bool) {
        self.inner.set_drifting(key, drifting);
    }
}

/// Stage `stage`'s key: a `resize` kernel salted by its stage.
pub fn stage_key(stage: u16) -> CostKey {
    CostKey {
        role: Role::Kernel,
        kind: "resize".into(),
        fingerprint: Digest([stage as u8; 32]),
        settings: Digest([0; 32]),
        schema: Digest([0; 32]),
    }
}

/// One run: the DAG, the data, the kernels' behaviour, the machine and the policy.
#[derive(Clone, Debug)]
pub struct Case {
    /// The DAG.
    pub topology: Topology,
    /// Per source, its splits and the rows in each.
    pub splits: Vec<(u32, u64)>,
    /// Every stage's output factor range, thousandths of its input's rows.
    pub factors: (u64, u64),
    /// Per stage, stage 1 first, its own factor range in place of `factors`; empty: every
    /// stage has `factors`.
    pub stage_factors: Vec<(u64, u64)>,
    /// Factors at the ends of the range only: one batch in sixteen at the top.
    pub skewed: bool,
    /// Worker time per batch.
    pub kernel_latency: Duration,
    /// Every IO the reactor does takes this long.
    pub io_latency: Duration,
    /// Workers.
    pub workers: u16,
    /// The host tier's budget.
    pub budget: u64,
    /// Every queue's promotion window.
    pub window: u16,
    /// The scheduling policy, by name.
    pub policy: String,
    /// The cost model.
    pub model: Model,
}

impl Case {
    /// Stage `stage`'s factor range.
    pub fn factor_of(&self, stage: u16) -> (u64, u64) {
        usize::from(stage)
            .checked_sub(1)
            .and_then(|i| self.stage_factors.get(i))
            .copied()
            .unwrap_or(self.factors)
    }
}

/// What a run measured.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// Wall time of `run`.
    pub finished: Duration,
    /// The buffer catalog's figures at the end.
    pub catalog: CatalogStats,
    /// The policy's.
    pub policy: PolicyStats,
    /// `Ok` when the run completed, every sink got every batch once, and the catalog never
    /// passed its budget; the first thing that did not hold otherwise.
    pub correct: Result<(), String>,
    /// The refusal, when the scheduler refused the run before it started (its floor).
    pub refused: Option<String>,
    /// Observations the run's model kept in the store, when it had one.
    pub stored: u64,
    /// Actual completed row count per sink, for comparison across policies/models.
    pub output_rows: Vec<u64>,
}

/// The scheduler's floor for a case (10 f.20, placement f.18 rule 13): every queue's window and
/// one unit's input and output, at the smallest batch the run makes, a whole split.
pub fn scheduler_floor(case: &Case) -> u64 {
    let queues = u64::from(case.topology.queue_count());
    (queues * u64::from(case.window.max(1)) + 2) * smallest_batch(case)
}

/// The smallest batch a case's scheduler can make: a source's whole split, as it reads them.
pub fn smallest_batch(case: &Case) -> u64 {
    case.splits
        .iter()
        .map(|(_, rows)| rows * 8)
        .max()
        .unwrap_or(8)
}

/// The largest batch a case can make: its largest split, grown by the top factor at every stage.
pub fn max_batch_bytes(case: &Case) -> u64 {
    let rows = case.splits.iter().map(|(_, rows)| *rows).max().unwrap_or(1);
    let mut bytes = rows * 8;
    for _ in 0..case.topology.stages() {
        bytes = (bytes * case.factors.1).div_ceil(1000).max(8);
    }
    if !case.stage_factors.is_empty() {
        // Stages of their own ranges: the largest batch any of them makes, or the split's.
        let mut at = rows * 8;
        bytes = at;
        for (_, top) in &case.stage_factors {
            at = (at * top).div_ceil(1000).max(8);
            bytes = bytes.max(at);
        }
    }
    bytes
}

/// The smallest budget a case always makes progress in: room for every queue's window, every
/// worker's input and output, a read and a replacement, at the largest batch.
pub fn floor_budget(case: &Case) -> u64 {
    let queues = u64::from(case.topology.queue_count());
    (queues * u64::from(case.window.max(1)) + 2 * u64::from(case.workers) + 4)
        * max_batch_bytes(case)
}

/// How long a run may take before it is reported stuck: `MORUNA_SC_STALL_S`, 300 by default.
fn stall_seconds() -> u64 {
    match std::env::var("MORUNA_SC_STALL_S") {
        Ok(value) => match value.parse() {
            Ok(seconds) => seconds,
            Err(e) => panic!("MORUNA_SC_STALL_S={value:?} is not a number: {e}"),
        },
        Err(_) => 300,
    }
}

/// Run `case` to the end and judge it.
pub fn run(case: &Case, label: &str) -> Outcome {
    let scratch = Scratch::new(label);
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(case.io_latency);
    let topology = case.topology.clone();
    let sources: Vec<Arc<dyn Source>> = case
        .splits
        .iter()
        .map(|&(n, rows)| {
            Arc::new(
                FakeSource::new()
                    .splits(n, rows, rows * 8)
                    .sub_splittable(false),
            ) as Arc<dyn Source>
        })
        .collect();
    let kernels: Vec<Arc<dyn Kernel>> = (1..=topology.stages())
        .map(|stage| {
            Arc::new(
                Resize::new(alloc.clone())
                    .factors(case.factor_of(stage).0, case.factor_of(stage).1)
                    .skewed(case.skewed)
                    .latency(case.kernel_latency)
                    .salt(u64::from(stage)),
            ) as Arc<dyn Kernel>
        })
        .collect();
    let sinks: Vec<FakeSink> = (0..topology.sinks()).map(|_| FakeSink::new()).collect();
    let budgets = TierBudgets {
        host: case.budget,
        pinned_host: case.budget,
        disk: 1 << 40,
        ..TierBudgets::default()
    };
    let engine = match PlacementEngine::new(
        PlacementConfig {
            topology: topology.clone(),
            budgets,
            staging_dir: Some(scratch.0.clone()),
            disk_budget: 1 << 40,
            segment_bytes: 1 << 20,
            plan_digests: vec![[0; 32]; usize::from(topology.sources())],
            fingerprints: kernels.iter().map(|k| k.fingerprint()).collect(),
            resume_policy: kernels.iter().map(|k| k.hints().resume).collect(),
            checkpoint_enabled: false,
            ..PlacementConfig::default()
        },
        Arc::new(alloc.clone()),
        Arc::new(reactor.clone()),
    ) {
        Ok(engine) => engine,
        Err(e) => panic!("{label}: the placement engine: {e}"),
    };
    for q in 0..topology.queue_count() {
        engine.set_promotion_window(q, case.window);
    }
    let placement: Arc<dyn Placement> = engine.clone();
    let largest = case
        .splits
        .iter()
        .map(|(_, rows)| rows * 8)
        .max()
        .unwrap_or(8);
    let cfg = SchedulerConfig {
        workers_max: case.workers,
        workers_active: case.workers,
        initial_morsel_target: largest.max(8),
        // Whole splits: the smallest batch the run makes is a split, and its floor is counted
        // in it.
        morsel_min: largest.max(8),
        morsel_max: 1 << 30,
        policy: case.policy.clone(),
        ..SchedulerConfig::default()
    };
    let pipeline = Pipeline {
        sources,
        kernels,
        sinks: sinks
            .iter()
            .map(|sink| SinkHandle::wrap(Box::new(sink.clone()) as Box<dyn Sink>, false, 1 << 28))
            .collect(),
        topology: topology.clone(),
    };
    let online = match case.model {
        Model::None => None,
        Model::LogLinear => {
            Some(Box::new(LogLinear::new(Defaults::standard())) as Box<dyn CostModel>)
        }
        Model::Wrong { first, then, after } => {
            Some(Box::new(Wrong::new(first, then, after)) as Box<dyn CostModel>)
        }
    }
    .map(|model| {
        let store: Arc<dyn ObservationStore> =
            match LocalStore::open(scratch.0.join("observations")) {
                Ok(store) => Arc::new(store),
                Err(e) => panic!("{label}: the observation store: {e}"),
            };
        (
            Arc::new(Online::new(InUse::new(model, None), Arc::clone(&store))),
            store,
        )
    });
    let scheduler = match Scheduler::new(
        cfg,
        pipeline,
        placement,
        Arc::new(alloc.clone()),
        Arc::new(FakeTrace::new().capacity(1 << 20)),
        Arc::new(FakeSampler::new()),
    ) {
        Ok(scheduler) => Arc::new(scheduler),
        Err(e) => {
            return Outcome {
                finished: Duration::ZERO,
                catalog: engine.stats().catalog,
                policy: PolicyStats::default(),
                correct: Err(format!("{label}: refused at start: {e}")),
                refused: Some(e.to_string()),
                stored: 0,
                output_rows: Vec::new(),
            };
        }
    };
    if let Some((online, _)) = &online {
        let keys = (1..=topology.stages()).map(stage_key).collect();
        if let Err(e) = scheduler.set_cost(Arc::clone(online), keys) {
            panic!("{label}: the cost model: {e}");
        }
    }
    let token = CancelToken::new();
    let started = Instant::now();
    let (send, receive) = std::sync::mpsc::channel();
    let running = Arc::clone(&scheduler);
    let run_token = token.clone();
    let worker = std::thread::spawn(move || {
        let _ = send.send(running.run(run_token));
    });
    let outcome = match receive.recv_timeout(Duration::from_secs(stall_seconds())) {
        Ok(outcome) => outcome,
        Err(_) => {
            // What every queue holds and says, before the run is cancelled.
            let queues: Vec<String> = (0..topology.queue_count())
                .map(|q| {
                    format!(
                        "q{q}: {} entries, full {}, evicted {:?}, head {:?}",
                        engine.stats().queues[usize::from(q)].count,
                        engine.is_full(q),
                        engine.evicted(q),
                        moruna_kernel::Residency::head(&*engine, q),
                    )
                })
                .collect();
            let snapshot = moruna_kernel::Residency::snapshot(&*engine);
            token.cancel();
            let _ = worker.join();
            return Outcome {
                finished: started.elapsed(),
                catalog: engine.stats().catalog,
                policy: scheduler.scheduler_stats().policy,
                refused: None,
                stored: 0,
                output_rows: Vec::new(),
                correct: Err(format!(
                    "{label}: the run did not end within {} s\n  {}\n  batches {:?}\n  \
                     scheduler {:?}",
                    stall_seconds(),
                    queues.join("\n  "),
                    snapshot.batches,
                    scheduler.scheduler_stats()
                )),
            };
        }
    };
    let finished = started.elapsed();
    let _ = worker.join();
    let catalog = engine.stats().catalog;
    let policy = scheduler.scheduler_stats().policy;
    let mut correct = judge(case, &outcome, &sinks, &catalog);
    let mut stored = 0;
    if let Some((_, store)) = &online {
        let mut kept = 0u64;
        let scanned = store.scan_all(&mut |h| {
            kept += h.observations.len() as u64;
            Ok(())
        });
        stored = kept;
        if correct.is_ok() {
            correct = learned(&policy, scanned.map(|()| kept));
        }
    }
    let output_rows = match outcome {
        Ok(RunOutcome::Completed { sinks }) => sinks.iter().map(|sink| sink.rows).collect(),
        _ => Vec::new(),
    };
    Outcome {
        finished,
        catalog,
        policy,
        correct: correct.map_err(|e| format!("{label}: {e}")),
        refused: None,
        stored,
        output_rows,
    }
}

/// Did a run with a model learn every unit it ran, write every observation to the store, and
/// predict every unit it admitted from the model?
fn learned(policy: &PolicyStats, stored: moruna_cost::Result<u64>) -> Result<(), String> {
    let Some(learning) = &policy.learning else {
        return Err("a run with a model reported no learning".into());
    };
    let stored = stored.map_err(|e| format!("the store could not be read: {e}"))?;
    if learning.observed == 0 || learning.unwritten != 0 || learning.store_error.is_some() {
        return Err(format!("learning did not keep what it saw: {learning:?}"));
    }
    if learning.written != learning.observed || stored != learning.observed {
        return Err(format!(
            "{} units were observed, {} written and {stored} are in the store",
            learning.observed, learning.written
        ));
    }
    if policy.predicted_by_ratio != 0 || policy.predicted_by_model == 0 {
        return Err(format!(
            "with a model, {} units were predicted by ratios and {} by the model",
            policy.predicted_by_ratio, policy.predicted_by_model
        ));
    }
    Ok(())
}

/// Did the run complete, every sink get each batch once, and the catalog keep its budget?
fn judge(
    case: &Case,
    outcome: &moruna_kernel::Result<RunOutcome>,
    sinks: &[FakeSink],
    catalog: &CatalogStats,
) -> Result<(), String> {
    match outcome {
        Ok(RunOutcome::Completed { .. }) => {}
        other => return Err(format!("the run did not complete: {other:?}")),
    }
    // Every source split is one batch, and every path from a source to a sink maps each of its
    // batches to one batch there.
    for (index, sink) in sinks.iter().enumerate() {
        let mut written = sink.written();
        let expected: u64 = case
            .topology
            .paths(index as u16)
            .iter()
            .map(|(source, _)| u64::from(case.splits[usize::from(*source)].0))
            .sum();
        let total = written.len() as u64;
        written.sort_unstable();
        written.dedup();
        if written.len() as u64 != total {
            return Err(format!("sink {index} was written a batch twice"));
        }
        if total != expected {
            return Err(format!(
                "sink {index} got {total} batches of the {expected} its paths give"
            ));
        }
    }
    let host = TierKind::Host.index();
    if catalog.peak_by_tier[host] > case.budget {
        return Err(format!(
            "the catalog held {} bytes, past its budget of {}",
            catalog.peak_by_tier[host], case.budget
        ));
    }
    if catalog.overrun_bytes > 0 {
        return Err(format!(
            "{} bytes were pushed past their claims",
            catalog.overrun_bytes
        ));
    }
    if catalog.buffers != 0 || catalog.resident_by_tier.iter().any(|b| *b != 0) {
        return Err(format!("the catalog still holds {:?}", catalog));
    }
    if catalog.reserved_by_tier.iter().any(|b| *b != 0) || catalog.outside_bytes != 0 {
        return Err(format!("claims or writes were left over: {catalog:?}"));
    }
    Ok(())
}
