//! Warm-up in `moruna check` against real kernels (AR section 6; SDD 24 p.5): the data and its
//! statistics, the seed, start-up, workers, state, skew, the true peak, the swap, the store and
//! the refit, and a failure. What depends on the clock is asserted only coarsely; warm-up's
//! decisions are tested exactly over a fake bench in `check::warmup`'s unit tests. The test model
//! (`ratio`) and the test kernels live in this file and nowhere else.

#![allow(clippy::result_large_err)]

mod support;

/// Every heap byte this test program allocates through Rust is counted while a warm-up runs, as
/// the Python extension counts its own (SDD 24 p.4): what makes CM-T60's heap spike visible on a
/// platform whose allocator keeps freed memory resident.
#[global_allocator]
static ALLOCATOR: moruna_runtime::check::warmup::CountingAllocator =
    moruna_runtime::check::warmup::CountingAllocator;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arrow::array::{Array, Int64Array};
use moruna_cost::{
    Basis, CostError, CostKey, CostModel, CostModels, Data, Estimate, FitReport, Inputs,
    ModelAdapter, ModelId, ObservationSource, Origin, Outcome, Target, UnitCost,
};
use moruna_kernel::declare::{Declared, SchemaDecl};
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError, NoState,
    Payload, PayloadSpec, ResumePolicy, SourceSchema, TierPref,
};
use moruna_runtime::check::warmup::{Maker, WarmUp};
use moruna_runtime::check::{CheckOptions, CheckReport, Verdict, WarmUpReport, check};
use moruna_runtime::cost::CostSettings;
use serde::{Deserialize, Serialize};
use support::{Scratch, one_run_at_a_time};

fn kernel_error(msg: String) -> MorunaError {
    MorunaError::Kernel {
        stage: 1,
        seq: 0,
        msg,
        features: None,
    }
}

fn keys_of(input: &Payload) -> moruna_kernel::Result<Vec<i64>> {
    let Payload::Table(batch, _) = input else {
        return Err(kernel_error("a table".into()));
    };
    let keys = batch
        .column_by_name("id")
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
        .ok_or_else(|| kernel_error("no Int64 `id`".into()))?;
    Ok(keys.iter().flatten().collect())
}

/// A stateless kernel whose cost is what a test says: a sleep per row, a sleep per pair of rows
/// sharing a key (so skewed keys cost more), a one-time first-invocation cost, and a failure at
/// a size.
struct Paced {
    name: &'static str,
    per_row: Duration,
    per_pair: Duration,
    first: Duration,
    fail_from: Option<usize>,
    started: OnceLock<()>,
}

impl Paced {
    fn new(name: &'static str, per_row_ns: u64) -> Paced {
        Paced {
            name,
            per_row: Duration::from_nanos(per_row_ns),
            per_pair: Duration::ZERO,
            first: Duration::ZERO,
            fail_from: None,
            started: OnceLock::new(),
        }
    }
}

impl Kernel for Paced {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-runtime::tests::warmup::Paced", self.name.as_bytes())
    }
    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }
    fn declared(&self) -> Declared {
        declared()
    }
    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }
    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }
    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }
    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        if self.started.set(()).is_ok() {
            std::thread::sleep(self.first);
        }
        let keys = keys_of(&input)?;
        if let Some(at) = self.fail_from
            && keys.len() >= at
        {
            return Err(kernel_error(format!("{} rows are too many", keys.len())));
        }
        let mut pause = self.per_row * keys.len() as u32;
        if !self.per_pair.is_zero() {
            // Counted by index, cheaply even in a debug build, so the sleep is the cost: warm-up
            // draws keys below the batch's rows; `check`'s own batches hold any value, and a key
            // outside is counted as one of its own.
            let mut counts = vec![0u64; keys.len() + 1];
            let mut alone = 0u64;
            for k in &keys {
                match usize::try_from(*k).ok().filter(|k| *k < counts.len()) {
                    Some(slot) => counts[slot] += 1,
                    None => alone += 1,
                }
            }
            let pairs: u64 = counts.iter().map(|c| c * c).sum::<u64>() + alone;
            pause += self.per_pair * u32::try_from(pairs).unwrap_or(u32::MAX);
        }
        std::thread::sleep(pause);
        Ok(input)
    }
}

/// A stateful kernel that keeps the distinct keys it has seen: 16 bytes of state per key, a
/// checkpoint of 8 bytes per key.
struct Distinct;

struct Seen(std::collections::BTreeSet<i64>);

impl KernelState for Seen {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn checkpoint(&mut self) -> moruna_kernel::Result<Option<Vec<u8>>> {
        Ok(Some(self.0.iter().flat_map(|k| k.to_le_bytes()).collect()))
    }
    fn footprint(&self) -> moruna_kernel::Result<Option<u64>> {
        Ok(Some(self.0.len() as u64 * 16))
    }
}

impl Kernel for Distinct {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-runtime::tests::warmup::Distinct", b"v1")
    }
    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: std::num::NonZeroUsize::new(2).expect("two"),
        }
    }
    fn hints(&self) -> KernelHints {
        KernelHints {
            resume: ResumePolicy::Checkpoint,
            ..KernelHints::default()
        }
    }
    fn declared(&self) -> Declared {
        declared()
    }
    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }
    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }
    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(Seen(Default::default())))
    }
    fn restore(
        &self,
        _ctx: &InitCtx,
        _state: &[u8],
    ) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(Seen(Default::default())))
    }
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> moruna_kernel::Result<Payload> {
        let seen = state
            .as_any_mut()
            .downcast_mut::<Seen>()
            .ok_or_else(|| kernel_error("not my state".into()))?;
        seen.0.extend(keys_of(&input)?);
        Ok(input)
    }
}

fn declared() -> Declared {
    Declared {
        input: Some(SchemaDecl::from_schema(&support::schema())),
        output: Some(SchemaDecl::Relative {
            adds: Vec::new(),
            drops: Vec::new(),
            changes: Vec::new(),
        }),
    }
}

/// The test model: each target is the last one-worker unit's value per row, times the rows asked
/// about. It ignores everything else, which a linear kernel does not need.
#[derive(Default)]
struct Ratio {
    per_row: std::collections::BTreeMap<Target, f64>,
}

#[derive(Serialize, Deserialize)]
struct RatioState {
    model: ModelId,
    per_row: Vec<(Target, f64)>,
}

impl CostModel for Ratio {
    fn id(&self) -> ModelId {
        ModelId::new("ratio", 1)
    }
    fn predict(&self, _key: &CostKey, inputs: &Inputs) -> UnitCost {
        let rows = inputs.data.rows.unwrap_or(1) as f64;
        let mut cost = UnitCost::zero();
        for (target, per_row) in &self.per_row {
            *cost.get_mut(*target) = Estimate {
                p50: per_row * rows,
                p95: per_row * rows * 1.2,
                unit: target.unit(),
                basis: Basis::WarmUp,
                observations: 1,
                extrapolated: false,
                drifting: false,
            };
        }
        cost
    }
    fn observe(&mut self, _key: &CostKey, inputs: &Inputs, outcome: &Outcome) {
        let Some(rows) = inputs.data.rows.filter(|r| *r > 0) else {
            return;
        };
        if inputs.resources.workers.is_some_and(|w| w > 1) {
            return;
        }
        for target in Target::ALL {
            if let Some(value) = outcome.get(target) {
                self.per_row.insert(target, value / rows as f64);
            }
        }
    }
    fn fit(&mut self, history: &dyn ObservationSource) -> moruna_cost::Result<FitReport> {
        self.per_row.clear();
        let mut report = FitReport::default();
        history.scan_all(&mut |h| {
            report.keys += 1;
            report.observations += h.observations.len() as u64;
            for o in &h.observations {
                self.observe(&o.key, &o.inputs, &o.outcome);
            }
            Ok(())
        })?;
        Ok(report)
    }
    fn save(&self) -> moruna_cost::Result<Vec<u8>> {
        Ok(serde_json::to_vec(&RatioState {
            model: self.id(),
            per_row: self.per_row.iter().map(|(t, v)| (*t, *v)).collect(),
        })
        .expect("serialises"))
    }
    fn load(&mut self, bytes: &[u8]) -> moruna_cost::Result<()> {
        let state: RatioState = serde_json::from_slice(bytes).map_err(|e| CostError::State {
            model: self.id(),
            reason: e.to_string(),
        })?;
        self.per_row = state.per_row.into_iter().collect();
        Ok(())
    }
}

struct RatioAdapter;

impl ModelAdapter for RatioAdapter {
    fn name(&self) -> &str {
        "ratio"
    }
    fn make(&self) -> Box<dyn CostModel> {
        Box::new(Ratio::default())
    }
}

/// Settings over a local store in `dir`, with `ratio` registered and chosen when `ratio`.
fn settings(dir: &std::path::Path, ratio: bool) -> CostSettings {
    let mut settings = CostSettings::directory(dir);
    if ratio {
        let mut models = CostModels::standard();
        models.register(Arc::new(RatioAdapter)).expect("registered");
        settings.models = models;
        settings.model = "ratio".into();
    }
    settings
}

fn warm(dir: &std::path::Path, ratio: bool, budget: Duration) -> WarmUp {
    let mut warm = WarmUp::new(settings(dir, ratio).open().expect("opened"));
    warm.budget = budget;
    warm
}

fn checked(kernel: Arc<dyn Kernel>, warm: WarmUp, seed: u64) -> CheckReport {
    let mut opts = CheckOptions::new("warmed");
    opts.seed = seed;
    opts.warm_up = Some(warm);
    check(kernel, opts).expect("the harness runs")
}

fn report(r: &CheckReport) -> &WarmUpReport {
    assert_eq!(r.verdict, Verdict::Agreed, "{}", r.summary());
    match &r.warm_up {
        Some(Ok(report)) => report,
        Some(Err(f)) => panic!("warm-up failed: {} ({:?})", f.reason, f.progress),
        None => panic!("no warm-up"),
    }
}

/// Every observation the store holds, by key, oldest first.
fn observations(dir: &std::path::Path) -> Vec<moruna_cost::Observation> {
    let store = moruna_cost::LocalStore::open(dir).expect("store");
    let mut all = Vec::new();
    store
        .scan_all(&mut |h| {
            all.extend(h.observations);
            Ok(())
        })
        .expect("scans");
    all
}

#[test]
fn cm_t45_data_follows_the_statistics_uniform_and_skewed() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("cm_t45");
    let mut w = warm(scratch.path(), true, Duration::from_secs(600));
    w.statistics = Some(
        serde_json::from_value::<Data>(serde_json::json!({"columns": [
            {"name": "id", "key": true, "distinct": 500},
        ]}))
        .expect("statistics"),
    );
    let r = checked(Arc::new(Paced::new("cm_t45", 0)), w, 1);
    let warm_up = report(&r);
    let all = observations(scratch.path());
    assert_eq!(all.len() as u64, warm_up.observations);
    let sizes: Vec<_> = all
        .iter()
        .filter(|o| o.inputs.resources.workers == Some(1))
        .filter(|o| o.inputs.data.rows.is_some_and(|r| r >= 1_000))
        .collect();
    for o in &sizes {
        let id = &o.inputs.data.columns[0];
        assert!(id.key && id.distinct.expect("distinct") <= 500, "{id:?}");
        assert!(!o.inputs.data.columns[1].key, "only the marked key");
        assert_eq!(o.outcome.origin, Origin::WarmUp);
    }
    let skews: Vec<f64> = sizes
        .iter()
        .filter_map(|o| o.inputs.data.columns[0].skew)
        .collect();
    assert!(skews.iter().any(|s| *s > 0.7), "skewed batches: {skews:?}");
    assert!(
        skews.iter().any(|s| *s < 0.05),
        "uniform batches: {skews:?}"
    );
    let skew = warm_up.skew.as_ref().expect("skew measured");
    assert!(skew.value_dependent && skew.skewed_skew > skew.uniform_skew);
    assert!(
        warm_up
            .selectivity
            .as_ref()
            .expect("selectivity")
            .value_dependent
    );
    assert!((warm_up.selectivity.as_ref().expect("s").uniform - 1.0).abs() < 1e-9);
    // Every estimate names its source: warm-up for what warm-up measured, the declared default
    // for what it cannot (disk reads and spills, which a kernel's call does not do).
    let json = r.to_json();
    for target in Target::ALL {
        let basis = &json["warm_up"]["cost_per_10000_rows"][target.as_str()]["basis"];
        let measured = !matches!(target, Target::ReadBytes | Target::SpillBytes);
        assert_eq!(
            basis,
            if measured { "warm_up" } else { "default" },
            "{target}"
        );
    }
    assert_eq!(json["warm_up"]["skew"]["value_dependent"], true);
}

#[test]
fn cm_t46_one_seed_one_data() {
    let _serial = one_run_at_a_time();
    let recorded = |seed: u64| {
        let scratch = Scratch::new("cm_t46");
        let r = checked(
            Arc::new(Paced::new("cm_t46", 0)),
            warm(scratch.path(), true, Duration::from_secs(600)),
            seed,
        );
        report(&r);
        let mut all = observations(scratch.path());
        all.sort_by_key(|o| o.at_ms);
        all.into_iter()
            .filter(|o| o.inputs.data.rows.is_some_and(|r| r <= 10_000))
            .map(|o| o.inputs.data)
            .collect::<Vec<_>>()
    };
    let (a, b) = (recorded(5), recorded(5));
    assert_eq!(a.len(), 12, "every unit up to 10,000 rows runs");
    assert_eq!(a, b, "one seed, one data, measured the same");
}

// Real timing, coarse properties only (SDD 24 p.5): each holds by a wide margin on a loaded
// machine and in a debug build. The decisions themselves are tested exactly over a fake bench
// in `check::warmup`'s unit tests.

#[test]
fn cm_t49_real_start_up_is_cold_then_warm() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("cm_t49");
    let mut kernel = Paced::new("cm_t49", 0);
    kernel.first = Duration::from_millis(80);
    let r = checked(
        Arc::new(kernel),
        warm(scratch.path(), true, Duration::from_secs(600)),
        0,
    );
    let facts = &report(&r).kernel;
    let (cold, warm_start) = (
        facts.start_cold_seconds.expect("cold"),
        facts.start_warm_seconds.expect("warm"),
    );
    // A sleep lasts at least what it asks; the second start sleeps not at all.
    assert!(
        cold >= 0.08,
        "the first invocation pays its one-time cost: {cold}"
    );
    assert!(
        warm_start < cold,
        "the second does not: {warm_start} against {cold}"
    );

    // A kernel made again for each start: two makings.
    struct Count(Arc<AtomicU64>);
    impl Maker for Count {
        fn make(&self) -> moruna_kernel::Result<Arc<dyn Kernel>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(Paced::new("cm_t49_made", 0)))
        }
    }
    let made = Arc::new(AtomicU64::new(0));
    let other = Scratch::new("cm_t49_made");
    let mut w = warm(other.path(), true, Duration::from_secs(600));
    w.maker = Some(Arc::new(Count(Arc::clone(&made))));
    let r = checked(Arc::new(Paced::new("cm_t49_made", 0)), w, 0);
    assert!(report(&r).progress.start_up);
    assert_eq!(
        made.load(Ordering::SeqCst),
        2,
        "cold and warm are each a making"
    );
}

#[test]
fn cm_t50_real_four_workers_are_not_slower_than_one_for_a_parallel_kernel() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("cm_t50");
    // A sleep per row: each worker sleeps for its part only.
    let r = checked(
        Arc::new(Paced::new("cm_t50", 200)),
        warm(scratch.path(), true, Duration::from_secs(600)),
        0,
    );
    let warm_up = report(&r);
    let workers: Vec<u32> = warm_up.progress.scaling.iter().map(|w| w.workers).collect();
    assert_eq!(workers, vec![1, 2, 4]);
    let facts = &warm_up.kernel;
    assert!(facts.speedup_4.expect("4") >= 1.0, "{facts:?}");
    let recorded: Vec<u32> = observations(scratch.path())
        .iter()
        .filter_map(|o| o.inputs.resources.workers)
        .collect();
    assert!(recorded.contains(&4) && recorded.contains(&2));
}

#[test]
fn cm_t51_real_a_stateful_kind_fits_its_state_and_times_a_checkpoint() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("cm_t51");
    // A budget no debug build on a loaded machine reaches: the state pass runs every count.
    let r = checked(
        Arc::new(Distinct),
        warm(scratch.path(), false, Duration::from_secs(1_800)),
        0,
    );
    let warm_up = report(&r);
    assert!(warm_up.progress.stopped_on_budget.is_empty());
    let largest = *warm_up.progress.state.last().expect("the state pass ran");
    assert!(
        warm_up.progress.state.len() >= 5,
        "{:?}",
        warm_up.progress.state
    );
    let facts = &warm_up.kernel;
    assert_eq!(facts.stateful, Some(true));
    // Measured from the footprint, not the clock: 16 bytes a key.
    let per_key = facts.state_per_key_bytes.expect("state per key");
    assert!((per_key - 16.0).abs() < 1.0, "16 bytes a key: {per_key}");
    assert!(warm_up.progress.checkpoint);
    let bytes = facts.checkpoint_bytes.expect("checkpoint bytes");
    assert!(bytes <= 8.0 * largest as f64 && bytes > 0.0, "{bytes}");
    let workers: Vec<u32> = warm_up.progress.scaling.iter().map(|w| w.workers).collect();
    assert_eq!(workers, vec![1, 2], "a pool of two");
}

#[test]
fn cm_t52_real_skewed_keys_are_slower_for_a_kernel_whose_cost_depends_on_them() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("cm_t52");
    // A sleep per pair of rows sharing a key: skewed keys make many more pairs.
    let mut kernel = Paced::new("cm_t52", 0);
    kernel.per_pair = Duration::from_nanos(20);
    let r = checked(
        Arc::new(kernel),
        warm(scratch.path(), true, Duration::from_secs(600)),
        0,
    );
    let skew = report(&r).skew.clone().expect("skew");
    assert!(skew.duration_ratio > 1.5, "{skew:?}");
}

/// A kernel that, inside one call, allocates `heap` bytes on the heap and `arena` bytes in the
/// arena, writes them, and frees both before it returns.
struct Spiky {
    heap: usize,
    arena: usize,
}

struct Alloc(Arc<dyn moruna_kernel::Allocator>);

impl KernelState for Alloc {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl Kernel for Spiky {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute(
            "moruna-runtime::tests::warmup::Spiky",
            format!("{}-{}", self.heap, self.arena).as_bytes(),
        )
    }
    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: std::num::NonZeroUsize::new(1).expect("one"),
        }
    }
    fn declared(&self) -> Declared {
        declared()
    }
    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }
    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }
    fn init(&self, ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(Alloc(Arc::clone(&ctx.alloc))))
    }
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> moruna_kernel::Result<Payload> {
        let alloc = &state
            .as_any_mut()
            .downcast_mut::<Alloc>()
            .ok_or_else(|| kernel_error("not my state".into()))?
            .0;
        if self.heap > 0 {
            let spike = vec![1u8; self.heap];
            std::hint::black_box(&spike);
        }
        if self.arena > 0 {
            let mut spike = alloc.alloc(self.arena, moruna_kernel::Tier::Host)?;
            spike.fill(1);
            std::hint::black_box(&spike);
        }
        Ok(input)
    }
}

#[test]
fn cm_t60_a_spike_freed_inside_the_call_is_in_the_peak() {
    let _serial = one_run_at_a_time();
    for (name, heap, arena) in [("heap", 64usize << 20, 0usize), ("arena", 0, 32 << 20)] {
        let scratch = Scratch::new("cm_t60");
        let r = checked(
            Arc::new(Spiky { heap, arena }),
            warm(scratch.path(), true, Duration::from_secs(600)),
            0,
        );
        let warm_up = report(&r);
        let stored = observations(scratch.path());
        assert!(!stored.is_empty());
        for o in &stored {
            let peak = o.outcome.peak_memory.expect("peak");
            let input = o.inputs.data.bytes.expect("bytes") as f64;
            assert!(
                peak >= (heap + arena) as f64,
                "{name}: a unit of {:?} rows recorded a peak of {peak}, under its spike",
                o.inputs.data.rows
            );
            if arena > 0 {
                assert!(
                    peak >= arena as f64 + input,
                    "{name}: the input with it: {peak}"
                );
            }
        }
        assert!(warm_up.notes.iter().any(|n| n.contains("high-water mark")));
    }
}

#[test]
fn cm_t53_the_swap() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("cm_t53");
    let r = checked(
        Arc::new(Paced::new("cm_t53", 200)),
        warm(scratch.path(), true, Duration::from_secs(600)),
        0,
    );
    let warm_up = report(&r);
    assert_eq!(warm_up.model, ModelId::new("ratio", 1));
    // Its answer is its own: its spread is exactly 1.2.
    let d = warm_up.cost_per_10000_rows.duration;
    assert!(d.p50 > 0.0);
    assert!(
        (d.p95 / d.p50 - 1.2).abs() < 1e-9,
        "the test model's spread"
    );
    // Its state is beside the records, and every record names it.
    let store = moruna_cost::LocalStore::open(scratch.path()).expect("store");
    use moruna_cost::ObservationStore as _;
    assert!(
        store
            .load_state(&ModelId::new("ratio", 1))
            .expect("reads")
            .is_some()
    );
    for o in observations(scratch.path()) {
        assert_eq!(o.model, Some(ModelId::new("ratio", 1)));
        assert_eq!(o.predictions[0].model, ModelId::new("ratio", 1));
    }
    // The same function under the default model, from the same store: fitted from what the
    // test model's warm-up left.
    let cost = CostSettings::directory(scratch.path())
        .open()
        .expect("opened");
    assert!(
        matches!(cost.readiness(), moruna_cost::Readiness::Fitted(_, report) if report.observations > 0),
        "{:?}",
        cost.readiness()
    );
    assert_eq!(
        cost.model()
            .predict(&warm_up.key, &Inputs::batch(10_000, 160_000))
            .duration
            .basis,
        Basis::WarmUp
    );
}

#[test]
fn cm_t54_observations_land_in_the_store_and_refit_the_model() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("cm_t54");
    let first = checked(
        Arc::new(Paced::new("cm_t54", 100)),
        warm(scratch.path(), false, Duration::from_secs(600)),
        0,
    );
    let first = report(&first).clone();
    assert_eq!(first.model, moruna_cost::LogLinear::model_id());
    let stored = observations(scratch.path());
    assert_eq!(stored.len() as u64, first.observations);
    assert!(stored.iter().all(|o| o.key == first.key));
    assert!(
        stored
            .iter()
            .all(|o| o.inputs.kernel.start_cold_seconds.is_some()),
        "facts recorded"
    );
    assert_eq!(first.fit.observations, first.observations);
    let d = first.cost_per_10000_rows.duration;
    assert_eq!(d.basis, Basis::WarmUp);
    assert!(d.observations > 0 && d.p50 > 0.0);

    // Again: the model opens from its saved state and refits over both warm-ups.
    let second = checked(
        Arc::new(Paced::new("cm_t54", 100)),
        warm(scratch.path(), false, Duration::from_secs(600)),
        0,
    );
    let second = report(&second);
    assert_eq!(
        second.fit.observations,
        first.observations + second.observations
    );
    assert_eq!(
        observations(scratch.path()).len() as u64,
        second.fit.observations
    );
}

#[test]
fn cm_t55_what_warm_up_consumed_is_reported() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("cm_t55");
    let r = checked(
        Arc::new(Paced::new("cm_t55", 100)),
        warm(scratch.path(), true, Duration::from_secs(600)),
        0,
    );
    let used = &report(&r).consumed;
    assert!(used.wall_seconds > 0.0);
    assert!(used.kernel_worker_seconds.expect("kernel worker time") >= 0.0);
    assert!(used.worker_seconds.expect("worker time") > 0.0, "{used:?}");
    if cfg!(target_vendor = "apple") {
        // Exact only when the warm-up raised the process's lifetime mark: in a process an
        // earlier, larger run has been through, it may not, and a run gives its memory back
        // (0.6.4), so the mark is not raised by memory a run before it kept. The sampler's
        // tests raise the mark deliberately; the warm-up's own pass it through.
        if let Some(peak) = used.memory_peak_bytes {
            assert!(peak > 0, "{used:?}");
        }
    } else {
        // The Linux sampler observes peaks; it cannot assert a total lifetime
        // high-water mark. Billing deliberately omits that non-exact reading.
        assert_eq!(used.memory_peak_bytes, None);
    }
    assert!(used.arena_bytes > 0 && used.store_bytes > 0);
    let summary = r.summary();
    assert!(
        summary.contains("warm-up (`ratio@1`): per 10000 rows"),
        "{summary}"
    );
    assert!(summary.contains("warm-up used"), "{summary}");
    assert_eq!(
        r.to_json()["warm_up"]["consumed"]["store_bytes"],
        used.store_bytes
    );
}

#[test]
fn cm_t56_a_warm_up_that_cannot_run_says_why() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("cm_t56");
    let mut kernel = Paced::new("cm_t56", 0);
    kernel.fail_from = Some(50_000);
    let r = checked(
        Arc::new(kernel),
        warm(scratch.path(), true, Duration::from_secs(600)),
        0,
    );
    assert_eq!(r.verdict, Verdict::Agreed, "the declaration holds");
    assert_eq!(r.exit_code(), 2, "and the function cannot be costed");
    let Some(Err(failure)) = &r.warm_up else {
        panic!("a failure: {:?}", r.warm_up.as_ref().map(|w| w.is_ok()))
    };
    assert!(failure.reason.contains("100000 rows"), "{}", failure.reason);
    assert!(failure.reason.contains("too many"), "{}", failure.reason);
    assert_eq!(failure.progress.sizes[1].uniform, 3, "how far it got");
    assert!(r.summary().contains("warm-up failed"));
    assert!(r.to_json()["warm_up"]["error"].is_string());

    // Statistics for a column the input does not have refuse warm-up before anything runs.
    let other = Scratch::new("cm_t56_stats");
    let mut w = warm(other.path(), true, Duration::from_secs(600));
    w.statistics = Some(Data {
        columns: vec![moruna_cost::Column {
            name: "ghost".into(),
            ..moruna_cost::Column::default()
        }],
        ..Data::default()
    });
    let r = checked(Arc::new(Paced::new("cm_t33b", 0)), w, 0);
    let Some(Err(failure)) = &r.warm_up else {
        panic!("refused")
    };
    assert!(failure.reason.contains("`ghost`"), "{}", failure.reason);
    assert!(observations(other.path()).is_empty());
}
