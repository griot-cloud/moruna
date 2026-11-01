//! Warm-up: what a function costs, measured when it is checked (AR section 6; SDD 24 p).
//!
//! `moruna check` is the local half of registering a function. Once a kernel agrees with its
//! declaration, warm-up runs it on generated data inside the harness a check runs it in, the
//! same kernel object a run builds (a package through its own runtime, 16 f.9), and learns its
//! cost:
//!
//! 1. **data** ([`data`]): batches from the input schema, honouring the embedder's table
//!    statistics, key columns uniform and skewed, byte-exact from the seed;
//! 2. **sizes**: [`SIZES`] rows, [`REPETITIONS`] each, stopping when the model's residual is
//!    small or the budget is spent;
//! 3. **start-up**: the first invocation of a freshly made kernel, cold, and a second, warm;
//! 4. **scaling**: the largest size measured, at each of [`WORKERS`];
//! 5. **state**: a stateful kind at increasing distinct counts, and once with a checkpoint;
//! 6. **result**: every unit to the [`ObservationStore`], the configured [`CostModel`] fitted
//!    from the store, and its cost per [`COST_ROWS`] rows in the report, with what warm-up
//!    itself consumed so the embedder can charge for it.
//!
//! Warm-up talks to the model and the store through their traits alone: which model it fits and
//! where the observations go is the runtime's cost configuration ([`crate::cost`]), chosen by a
//! setting from a [`moruna_cost::CostModels`] registry.

mod bench;
mod cpu;
pub mod data;
mod peak;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use moruna_cost::{
    CostKey, Data, Digest, FitReport, Inputs, Kernel as Facts, ModelId, Observation, Outcome,
    Predicted, Resources, Target, UnitCost,
};
use moruna_kernel::arrow::datatypes::SchemaRef;
use moruna_kernel::{Kernel, KernelKind, MorunaError, OutsideMemory, ResumePolicy};
use serde::Serialize;

pub use peak::CountingAllocator;

use crate::cost::{Cost, CostSettings, default_store_dir};
pub(in crate::check) use bench::{Bench, Harnessed};
use data::{Generator, Keys, Spec};

/// The sizes measured, in rows (AR section 6.2).
pub const SIZES: [u64; 4] = [1_000, 10_000, 100_000, 1_000_000];
/// Repetitions of each size.
pub const REPETITIONS: u32 = 3;
/// The workers the scaling pass runs at (AR section 6.4).
pub const WORKERS: [u32; 3] = [1, 2, 4];
/// The time warm-up may take when the setting says nothing (AR section 6.2).
pub const BUDGET: Duration = Duration::from_secs(60);
/// The residual below which the sizes stop early: the median `|ln(predicted / measured)|` of
/// a size's units, each predicted before it was observed, for wall time and for peak memory
/// alike (about 10%).
pub const RESIDUAL: f64 = 0.10;
/// The residual is judged from this size on (the third): two sizes before it have fitted a
/// slope, so a small residual says the model extrapolates, not only that it interpolates.
pub const JUDGED_FROM: usize = 2;
/// The distinct counts of the state pass, those at or below its rows (AR section 6.5).
pub const STATE_DISTINCT: [u64; 6] = [10, 100, 1_000, 10_000, 100_000, 1_000_000];
/// The share of the budget by whose end each phase is done, cumulative: the sizes may use half of
/// it, the workers until three quarters, the state the rest. A kernel too slow to measure at
/// every size within the budget still has its scaling and its state measured, at the largest
/// size the sizes reached; start-up, a pair of one-row calls, may use the whole.
pub const PHASE_DEADLINES: [(Phase, f64); 4] = [
    (Phase::StartUp, 1.0),
    (Phase::Sizes, 0.5),
    (Phase::Scaling, 0.75),
    (Phase::State, 1.0),
];

/// When `phase` must be done, in seconds into warm-up, for a budget of `budget` seconds.
pub fn deadline(phase: Phase, budget: f64) -> f64 {
    PHASE_DEADLINES
        .iter()
        .find(|(p, _)| *p == phase)
        .map_or(budget, |(_, share)| budget * share)
}

/// The rows the report's cost is for (AR section 6.6).
pub const COST_ROWS: u64 = 10_000;
/// A size fits when this many times its batch fits in half the memory the machine has left:
/// the generated batch, its landed copy, the kernel's output and room besides.
const FIT_FACTOR: f64 = 6.0;
/// The arena warm-up adds to the check's: the largest batch that fits this many times over, its
/// landed copy and what a kernel writing into the arena produces from it.
const ARENA_FACTOR: f64 = 4.0;
/// The rows of the batch warm-up first generates to learn a row's bytes.
const PROBE_ROWS: u64 = 1_000;

/// The stream of each pass's batches ([`Spec::stream`]), so no two passes share data.
const STREAM_PROBE: u64 = 1;
const STREAM_START: u64 = 2;
const STREAM_SIZES: u64 = 16;
const STREAM_SCALING: u64 = 1 << 12;
const STREAM_STATE: u64 = 1 << 13;

/// Makes the kernel again as a run makes it, for start-up (AR section 6.3): a package opened
/// from its file under its own runtime. A kernel checked in process has no maker; it is already
/// made, and its start-up is its first invocation.
pub trait Maker: Send + Sync {
    /// The kernel, freshly made.
    fn make(&self) -> moruna_kernel::Result<Arc<dyn Kernel>>;
}

/// What warm-up needs from whoever checks: the cost configuration (where observations go,
/// which model is fitted), how long it may take, and what is known of the data (AR section 6).
pub struct WarmUp {
    /// The store every unit's inputs and outcome go to, and the model fitted from it; after
    /// warm-up the model holds what warm-up taught it.
    pub cost: Cost,
    /// The time warm-up may take (AR section 6.2, a setting).
    pub budget: Duration,
    /// The embedder's table statistics: each column's null fraction, distinct count, lengths
    /// and whether it is a key.
    pub statistics: Option<Data>,
    /// The digest of the settings the function runs with, which keys its cost.
    pub settings: Digest,
    /// How to make the kernel again for start-up; `None` for a kernel checked in process.
    pub maker: Option<Arc<dyn Maker>>,
    /// Memory the kernel holds outside this process (a packaged stage's child), whose
    /// high-water mark is part of a unit's peak.
    pub outside: Option<Arc<dyn OutsideMemory>>,
}

impl WarmUp {
    /// Warm-up into `cost`, with the default budget, no statistics, and the settings digest of
    /// no settings.
    pub fn new(cost: Cost) -> WarmUp {
        WarmUp {
            cost,
            budget: BUDGET,
            statistics: None,
            settings: settings_digest(&serde_json::json!({})),
            maker: None,
            outside: None,
        }
    }

    /// Warm-up from a surface's settings (the command line's and the Python module's): the
    /// local store at `store` (else [`default_store_dir`]), a budget in seconds, the statistics
    /// as JSON (`moruna_cost::Data`), and the model's name. Each refusal names what it refuses.
    pub fn configured(
        store: Option<&Path>,
        budget_seconds: f64,
        statistics: Option<&str>,
        model: Option<&str>,
    ) -> moruna_kernel::Result<WarmUp> {
        let refuse = |msg: String| MorunaError::Config {
            name: "warm_up",
            msg,
        };
        let dir = match store {
            Some(dir) => dir.to_path_buf(),
            None => default_store_dir().ok_or_else(|| {
                refuse(
                    "warm-up keeps its observations in a store, and none was given and this \
                     host has no home directory for the default"
                        .into(),
                )
            })?,
        };
        let mut settings = CostSettings::directory(dir);
        if let Some(model) = model {
            settings.model = model.to_string();
        }
        Self::with_settings(settings, budget_seconds, statistics)
    }

    /// Use the actual configured native store/model (including governed service adapters)
    /// with the SAME budget/statistics parser as directory-backed warm-up.
    pub fn with_settings(
        settings: CostSettings,
        budget_seconds: f64,
        statistics: Option<&str>,
    ) -> moruna_kernel::Result<WarmUp> {
        let refuse = |msg: String| MorunaError::Config {
            name: "warm_up",
            msg,
        };
        let mut warm = WarmUp::new(settings.open()?);
        warm.budget = Duration::try_from_secs_f64(budget_seconds).map_err(|_| {
            refuse(format!(
                "the budget is {budget_seconds} seconds; it is a duration of zero or more"
            ))
        })?;
        if let Some(text) = statistics {
            warm.statistics =
                Some(serde_json::from_str(text).map_err(|e| {
                    refuse(format!("the statistics are not table statistics: {e}"))
                })?);
        }
        Ok(warm)
    }
}

/// The digest of a function's settings: SHA-256 of their JSON with every object's keys sorted,
/// so equal settings have one digest.
pub fn settings_digest(settings: &serde_json::Value) -> Digest {
    use sha2::Digest as _;
    let mut text = Vec::new();
    canonical(settings, &mut text);
    Digest(sha2::Sha256::digest(&text).into())
}

/// `value` as JSON with every object's keys in order, whatever order the map keeps them in (a
/// build may keep insertion order).
fn canonical(value: &serde_json::Value, out: &mut Vec<u8>) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (at, key) in keys.into_iter().enumerate() {
                if at > 0 {
                    out.push(b',');
                }
                out.extend(
                    serde_json::Value::String(key.clone())
                        .to_string()
                        .into_bytes(),
                );
                out.push(b':');
                canonical(&map[key], out);
            }
            out.push(b'}');
        }
        serde_json::Value::Array(items) => {
            out.push(b'[');
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.push(b',');
                }
                canonical(item, out);
            }
            out.push(b']');
        }
        scalar => out.extend(scalar.to_string().into_bytes()),
    }
}

/// The part of warm-up a stop happened in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The cold and warm first invocations.
    StartUp,
    /// The sizes.
    Sizes,
    /// The workers.
    Scaling,
    /// The distinct counts and the checkpoint.
    State,
}

/// How far one size got.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SizeProgress {
    /// Rows.
    pub rows: u64,
    /// Whether its batch fits in the memory this machine has.
    pub fits: bool,
    /// Units run with uniform keys.
    pub uniform: u32,
    /// Units run with skewed keys.
    pub skewed: u32,
}

/// The units the scaling pass ran at one worker count.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WorkersProgress {
    /// Workers (instances, for a stateful kind).
    pub workers: u32,
    /// Units run.
    pub units: u32,
}

/// The residual judged after one size (see [`RESIDUAL`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Residual {
    /// The size.
    pub rows: u64,
    /// Median `|ln(predicted / measured)|` of wall time.
    pub duration: f64,
    /// The same of peak memory.
    pub peak_memory: f64,
}

/// A phase's share of the budget was spent: nothing after this point in the phase ran.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BudgetStop {
    /// Where.
    pub phase: Phase,
    /// The unit that would have run next: its rows and workers.
    pub rows: u64,
    /// Its workers.
    pub workers: u32,
    /// Seconds spent when it stopped.
    pub elapsed_seconds: f64,
    /// What the model predicted the next unit would take.
    pub predicted_seconds: f64,
    /// The phase's deadline, seconds into warm-up.
    pub budget_seconds: f64,
}

/// How far warm-up got (AR section 6.2: a stopped warm-up records it).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Progress {
    /// Whether the cold and the warm start were both measured.
    pub start_up: bool,
    /// Every size, in order.
    pub sizes: Vec<SizeProgress>,
    /// The residuals judged, one per size from [`JUDGED_FROM`] on.
    pub residuals: Vec<Residual>,
    /// The sizes stopped early because the residual was small: after this one.
    pub stopped_on_residual: Option<Residual>,
    /// The rows the scaling pass ran at.
    pub scaling_rows: Option<u64>,
    /// The units it ran at each worker count.
    pub scaling: Vec<WorkersProgress>,
    /// The distinct counts the state pass ran.
    pub state: Vec<u64>,
    /// Whether a checkpoint was timed.
    pub checkpoint: bool,
    /// Each phase the budget stopped, and where (see [`deadline`]).
    pub stopped_on_budget: Vec<BudgetStop>,
}

/// What a kernel's cost depends on beyond the sizes: measured on generated data, which has no
/// real distribution of values, so the figure says how sensitive the kernel is and a run
/// supplies the real one (AR section 2, G11).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SkewSensitivity {
    /// The size it was measured at.
    pub rows: u64,
    /// The uniform batches' skew (the share of rows on the top 1% of keys), median over keys.
    pub uniform_skew: f64,
    /// The skewed batches'.
    pub skewed_skew: f64,
    /// Median wall time skewed over median wall time uniform.
    pub duration_ratio: f64,
    /// The same of peak memory.
    pub peak_memory_ratio: f64,
    /// Always true: a value-dependent quantity, marked as one.
    pub value_dependent: bool,
}

/// Rows out per row in on generated data (AR section 3.1: a run measures the real one).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Selectivity {
    /// The median over the uniform units.
    pub uniform: f64,
    /// The median over the skewed units, when there were any.
    pub skewed: Option<f64>,
    /// Always true.
    pub value_dependent: bool,
}

/// What warm-up itself took from the machine, so the embedder can charge the workspace that
/// registered the function (AR section 6.6). The cost is the resources only: worker time,
/// memory and disk.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Consumed {
    /// Wall seconds from the first unit's data to the fitted model.
    pub wall_seconds: f64,
    /// The process's CPU seconds over warm-up, generation included, as the operating system
    /// counts them; `None` where it cannot be read.
    pub worker_seconds: Option<f64>,
    /// The CPU seconds of the threads that ran the kernel, summed over its units, as a run
    /// measures worker time; `None` where a thread clock was refused.
    pub kernel_worker_seconds: Option<f64>,
    /// The most anonymous memory the process held during warm-up; `None` where it cannot be
    /// read.
    pub memory_peak_bytes: Option<u64>,
    /// Memory integrated over warm-up's samples, byte-seconds; `None` where it cannot be read.
    pub memory_byte_seconds: Option<u64>,
    /// The arena warm-up added to the check's, reserved for its whole length.
    pub arena_bytes: u64,
    /// Bytes handed to the observation store: the records and the fitted state.
    pub store_bytes: u64,
}

/// What warm-up found (AR section 6.6).
#[derive(Clone, Debug, Serialize)]
pub struct WarmUpReport {
    /// The model fitted.
    pub model: ModelId,
    /// What the cost is kept against.
    pub key: CostKey,
    /// The fitted model's cost of [`COST_ROWS`] rows of the uniform data at one worker; each
    /// estimate names its basis.
    pub cost_per_10000_rows: UnitCost,
    /// The kernel's facts warm-up measured (start-up, scaling, state, checkpoint, time per row,
    /// memory per byte), as the cost model's inputs carry them.
    pub kernel: Facts,
    /// How the kernel's cost moves with skew, when it has key columns.
    pub skew: Option<SkewSensitivity>,
    /// Its selectivity on generated data.
    pub selectivity: Option<Selectivity>,
    /// How far it got.
    pub progress: Progress,
    /// What it consumed.
    pub consumed: Consumed,
    /// Observations written to the store.
    pub observations: u64,
    /// What the fit read.
    pub fit: FitReport,
    /// What warm-up decided, in words.
    pub notes: Vec<String>,
}

/// Warm-up could not run, or a unit failed: why, and how far it got.
#[derive(Clone, Debug, Serialize)]
pub struct WarmUpFailure {
    /// The cause.
    pub reason: String,
    /// How far it got.
    pub progress: Progress,
}

/// What warm-up settles before the check's arena is made: its generator and which sizes fit.
pub(super) struct Prepared {
    pub(super) generator: Generator,
    pub(super) plan: Plan,
    /// The arena warm-up needs beside the check's.
    pub(super) arena_bytes: u64,
}

/// What warm-up's decisions are made over, beside what they measure.
#[derive(Clone, Debug)]
pub(super) struct Plan {
    /// Every size, and whether its batch fits.
    pub(super) sizes: Vec<(u64, bool)>,
    /// A row's bytes, for the predictions that admit a unit before it is generated.
    pub(super) bytes_per_row: f64,
    /// Whether any column is generated as a key, so that skewed batches differ.
    pub(super) has_keys: bool,
    /// What the generator decided, in words.
    pub(super) notes: Vec<String>,
}

/// Settle the data and the sizes: the generator from the schema and the statistics, a row's
/// bytes from a probe batch, and which sizes fit in half the memory this machine has left.
pub(super) fn prepare(warm: &WarmUp, schema: &SchemaRef, seed: u64) -> Result<Prepared, String> {
    let generator = Generator::new(schema, warm.statistics.as_ref(), seed)?;
    let probe = generator.generate(&Spec {
        rows: PROBE_ROWS,
        keys: Keys::Uniform,
        distinct: None,
        stream: STREAM_PROBE,
    })?;
    let bytes_per_row = probe.batch.get_array_memory_size() as f64 / PROBE_ROWS as f64;
    drop(probe);
    let discovered = moruna_discovery::discover(&moruna_discovery::DiscoveryInput::default())
        .map_err(|e| format!("warm-up cannot see this machine's memory: {e}"))?;
    let resident = moruna_kernel::Sampler::sample(
        &moruna_discovery::Sampler::new(&discovered)
            .map_err(|e| format!("warm-up cannot sample this process's memory: {e}"))?,
    )
    .map_err(|e| format!("warm-up cannot sample this process's memory: {e}"))?
    .anon_bytes;
    let left = discovered.limits.memory_ceiling.saturating_sub(resident) as f64 / 2.0;
    let sizes: Vec<(u64, bool)> = SIZES
        .iter()
        .map(|&rows| (rows, FIT_FACTOR * rows as f64 * bytes_per_row <= left))
        .collect();
    let Some(largest) = sizes
        .iter()
        .filter(|(_, fits)| *fits)
        .map(|(r, _)| *r)
        .max()
    else {
        return Err(format!(
            "not even {} rows fit: a row is {bytes_per_row:.0} bytes and {:.0} bytes are left \
             to warm up in",
            SIZES[0], left
        ));
    };
    let plan = Plan {
        sizes,
        bytes_per_row,
        has_keys: generator.has_keys(),
        notes: generator.notes().to_vec(),
    };
    Ok(Prepared {
        generator,
        plan,
        arena_bytes: (ARENA_FACTOR * largest as f64 * bytes_per_row).ceil() as u64,
    })
}

/// The kind a check's kernel has in the job document, which keys its cost: a `.wasm` or a
/// `.pex` is a `package`.
pub(super) fn cost_kind(check_kind: &str) -> String {
    match check_kind {
        "wasm" | "pex" => "package".to_string(),
        other => other.to_string(),
    }
}

/// What a kernel's kind says warm-up must do.
#[derive(Clone, Copy, Debug)]
pub(super) struct Kind {
    /// Stateful: instances are made with `init`, and the state pass runs.
    pub(super) stateful: bool,
    /// The most instances it keeps.
    pub(super) instances: usize,
    /// Its resume policy: a checkpoint is timed when it is `Checkpoint`.
    pub(super) resume: ResumePolicy,
    /// Whether it was made in this process before the check (no `Maker`).
    pub(super) made_here: bool,
}

impl Kind {
    /// The kind of `kernel`.
    pub(super) fn of(kernel: &dyn Kernel, made_here: bool) -> Kind {
        let (stateful, instances) = match kernel.kind() {
            KernelKind::Stateless => (false, usize::MAX),
            KernelKind::Stateful { max_instances } => (true, max_instances.get()),
        };
        Kind {
            stateful,
            instances,
            resume: kernel.hints().resume,
            made_here,
        }
    }
}

/// One measured unit, held until warm-up ends so that every record carries the kernel's facts.
struct Unit {
    phase: Phase,
    keys: Keys,
    workers: u32,
    inputs: Inputs,
    outcome: Outcome,
    predicted: UnitCost,
    at_ms: u64,
}

/// One warm-up in progress: [`Run::new`] and [`Run::start`] before the check's batches, so the
/// cold start is the kernel's first invocation in this process, and [`Run::complete`] once the
/// kernel has agreed. Every decision is here; every measurement and every second is the
/// [`Bench`]'s.
pub(super) struct Run<'a> {
    warm: &'a mut WarmUp,
    bench: Box<dyn Bench + 'a>,
    plan: Plan,
    kind: Kind,
    key: CostKey,
    units: Vec<Unit>,
    facts: Facts,
    progress: Progress,
    notes: Vec<String>,
    arena_bytes: u64,
    /// Why warm-up stopped, when a phase failed.
    failure: Option<String>,
}

impl<'a> Run<'a> {
    /// Warm-up of a kernel of `kind`, measured by `bench`, its cost kept under `key`.
    ///
    /// The model in use was made ready from the store when the cost configuration was opened, so
    /// its predictions (the budget's, the residual's) start from the history already known; it
    /// observes every unit as it is measured, and is fitted again from the store at the end.
    pub(super) fn new(
        plan: Plan,
        kind: Kind,
        warm: &'a mut WarmUp,
        bench: Box<dyn Bench + 'a>,
        key: CostKey,
        arena_bytes: u64,
    ) -> Run<'a> {
        Run {
            warm,
            bench,
            kind,
            key,
            units: Vec::new(),
            facts: Facts {
                stateful: Some(kind.stateful),
                ..Facts::default()
            },
            progress: Progress {
                sizes: plan
                    .sizes
                    .iter()
                    .map(|&(rows, fits)| SizeProgress {
                        rows,
                        fits,
                        uniform: 0,
                        skewed: 0,
                    })
                    .collect(),
                ..Progress::default()
            },
            notes: plan.notes.clone(),
            plan,
            arena_bytes,
            failure: None,
        }
    }

    /// AR section 6.3, before the check's batches: a failure is kept for [`Run::complete`].
    pub(super) fn start(&mut self) {
        if let Err(reason) = self.start_up() {
            self.failure = Some(reason);
        }
        self.bench.pause();
    }

    /// The rest, once the kernel has agreed: the sizes, the workers, the state, the result.
    pub(super) fn complete(mut self) -> Result<WarmUpReport, WarmUpFailure> {
        self.bench.resume();
        match self.failure.take().map_or_else(|| self.phases(), Err) {
            Ok(()) => self.finish(),
            Err(reason) => Err(WarmUpFailure {
                reason,
                progress: self.progress,
            }),
        }
    }
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `|ln(predicted / measured)|`, with the target's floor added to both.
fn log_error(predicted: f64, measured: f64, target: Target) -> f64 {
    let floor = target.unit().floor();
    ((predicted + floor).ln() - (measured + floor).ln()).abs()
}

impl Run<'_> {
    fn phases(&mut self) -> Result<(), String> {
        self.sizes()?;
        self.scaling()?;
        self.state()
    }

    /// Whether the next unit may run: the phase's [`deadline`] not passed, and the unit
    /// predicted (by the model in use) to finish before it. When not, the stop is recorded.
    fn admit(&mut self, phase: Phase, rows: u64, workers: u32) -> bool {
        let elapsed = self.bench.elapsed();
        let mut inputs = Inputs::batch(rows, (rows as f64 * self.plan.bytes_per_row) as u64);
        inputs.resources.workers = Some(workers);
        let predicted = self
            .warm
            .cost
            .model()
            .predict(&self.key, &inputs)
            .duration
            .p50;
        let budget = deadline(phase, self.warm.budget.as_secs_f64());
        if elapsed < budget && elapsed + predicted <= budget {
            return true;
        }
        self.progress.stopped_on_budget.push(BudgetStop {
            phase,
            rows,
            workers,
            elapsed_seconds: elapsed,
            predicted_seconds: predicted,
            budget_seconds: budget,
        });
        false
    }

    /// The instances a unit at `workers` workers runs with: one per worker, up to the kind's
    /// pool.
    fn instances_at(&self, workers: u32) -> u32 {
        (workers as usize).min(self.kind.instances) as u32
    }

    /// AR section 6.3: the first invocation of a freshly made kernel, cold, then a second, warm.
    fn start_up(&mut self) -> Result<(), String> {
        if self.kind.made_here {
            self.notes.push(
                "the kernel was made in this process before the check, so its start-up is its \
                 init and first batch, with nothing to unpack"
                    .into(),
            );
        }
        let mut seconds = [0.0f64; 2];
        for (attempt, slot) in seconds.iter_mut().enumerate() {
            if !self.admit(Phase::StartUp, 1, 1) {
                return Ok(());
            }
            *slot = self.bench.start(attempt)?;
        }
        self.facts.start_cold_seconds = Some(seconds[0]);
        self.facts.start_warm_seconds = Some(seconds[1]);
        self.progress.start_up = true;
        Ok(())
    }

    /// AR section 6.2: every size that fits, [`REPETITIONS`] times, uniform and (with key
    /// columns) skewed, until the residual is small or the sizes' share of the budget is spent.
    fn sizes(&mut self) -> Result<(), String> {
        let variants: &[Keys] = if self.plan.has_keys {
            &[Keys::Uniform, Keys::Skewed]
        } else {
            self.notes
                .push("the input has no key column, so skew is not measured".into());
            &[Keys::Uniform]
        };
        let sizes = self.plan.sizes.clone();
        for (index, (rows, fits)) in sizes.iter().copied().enumerate() {
            if !fits {
                self.notes.push(format!(
                    "{rows} rows do not fit in the memory this machine has left, so the sizes \
                     stop below them"
                ));
                break;
            }
            for rep in 0..REPETITIONS {
                for &keys in variants {
                    if !self.admit(Phase::Sizes, rows, 1) {
                        return Ok(());
                    }
                    let spec = Spec {
                        rows,
                        keys,
                        distinct: None,
                        stream: STREAM_SIZES + (index as u64) * 16 + u64::from(rep),
                    };
                    self.unit(Phase::Sizes, &spec, 1, false)?;
                    let size = &mut self.progress.sizes[index];
                    match keys {
                        Keys::Uniform => size.uniform += 1,
                        Keys::Skewed => size.skewed += 1,
                    }
                }
            }
            if index >= JUDGED_FROM {
                let residual = self.residual(rows);
                self.progress.residuals.push(residual.clone());
                let more = sizes.get(index + 1).is_some_and(|(_, fits)| *fits);
                if more && residual.duration <= RESIDUAL && residual.peak_memory <= RESIDUAL {
                    self.progress.stopped_on_residual = Some(residual);
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// The residual of the units of `rows` rows in the sizes pass, each against the prediction
    /// made before it was observed.
    fn residual(&self, rows: u64) -> Residual {
        let of = |target: Target| {
            median(
                self.units
                    .iter()
                    .filter(|u| u.phase == Phase::Sizes && u.inputs.data.rows == Some(rows))
                    .filter_map(|u| {
                        u.outcome
                            .get(target)
                            .map(|m| log_error(u.predicted.get(target).p50, m, target))
                    })
                    .collect(),
            )
            .unwrap_or(f64::INFINITY)
        };
        Residual {
            rows,
            duration: of(Target::Duration),
            peak_memory: of(Target::PeakMemory),
        }
    }

    /// The largest size the sizes pass measured with uniform keys.
    fn largest_measured(&self) -> Option<u64> {
        self.progress
            .sizes
            .iter()
            .filter(|s| s.uniform > 0)
            .map(|s| s.rows)
            .max()
    }

    /// AR section 6.4: the largest size measured, at each of [`WORKERS`], [`REPETITIONS`]
    /// times. A stateful kind runs one instance per worker up to its pool.
    fn scaling(&mut self) -> Result<(), String> {
        let Some(rows) = self.largest_measured() else {
            return Ok(());
        };
        self.progress.scaling_rows = Some(rows);
        let mut ran: Vec<u32> = Vec::new();
        for workers in WORKERS {
            let instances = self.instances_at(workers);
            if ran.contains(&instances) {
                self.notes.push(format!(
                    "the kind keeps at most {} instances, so {workers} workers run as {instances}",
                    self.kind.instances
                ));
                continue;
            }
            ran.push(instances);
            self.progress.scaling.push(WorkersProgress {
                workers: instances,
                units: 0,
            });
            for rep in 0..REPETITIONS {
                if !self.admit(Phase::Scaling, rows, instances) {
                    return Ok(());
                }
                let spec = Spec {
                    rows,
                    keys: Keys::Uniform,
                    distinct: None,
                    stream: STREAM_SCALING + u64::from(instances) * 16 + u64::from(rep),
                };
                self.unit(Phase::Scaling, &spec, instances, false)?;
                if let Some(last) = self.progress.scaling.last_mut() {
                    last.units += 1;
                }
            }
        }
        Ok(())
    }

    /// AR section 6.5: a stateful kind at increasing distinct counts, a fresh instance each. When
    /// its resume policy keeps a checkpoint, each instance's is taken and timed before it
    /// finishes, and the largest state's is the one reported: the budget may stop the counts at
    /// any one, and the checkpoint is then still timed.
    fn state(&mut self) -> Result<(), String> {
        if !self.kind.stateful {
            return Ok(());
        }
        let Some(rows) = self.largest_measured() else {
            return Ok(());
        };
        let counts: Vec<u64> = STATE_DISTINCT
            .iter()
            .copied()
            .filter(|d| *d <= rows)
            .collect();
        let checkpointable = self.kind.resume == ResumePolicy::Checkpoint;
        if !checkpointable {
            self.notes.push(format!(
                "the kind's resume policy is {:?}, which keeps no checkpoint, so none is timed",
                self.kind.resume
            ));
        }
        for (at, distinct) in counts.iter().copied().enumerate() {
            if !self.admit(Phase::State, rows, 1) {
                return Ok(());
            }
            let spec = Spec {
                rows,
                keys: Keys::Uniform,
                distinct: Some(distinct),
                stream: STREAM_STATE + at as u64,
            };
            let checkpoint = self.unit(Phase::State, &spec, 1, checkpointable)?;
            self.progress.state.push(distinct);
            if let Some((seconds, bytes)) = checkpoint {
                self.facts.checkpoint_seconds = Some(seconds);
                self.facts.checkpoint_bytes = Some(bytes as f64);
                self.progress.checkpoint = true;
            }
        }
        Ok(())
    }

    /// Measure, predict and observe one unit; its checkpoint when one was asked for.
    fn unit(
        &mut self,
        phase: Phase,
        spec: &Spec,
        workers: u32,
        checkpoint: bool,
    ) -> Result<Option<(f64, u64)>, String> {
        let measured = self.bench.unit(spec, workers, checkpoint).map_err(|e| {
            format!(
                "{} rows ({:?} keys) at {workers} workers: {e}",
                spec.rows, spec.keys
            )
        })?;
        let inputs = Inputs {
            data: measured.data,
            kernel: Facts {
                stateful: Some(self.kind.stateful),
                ..Facts::default()
            },
            resources: Resources {
                workers: Some(measured.workers),
                ..Resources::default()
            },
            ..Inputs::default()
        };
        let predicted = self.warm.cost.model().predict(&self.key, &inputs);
        self.warm
            .cost
            .model_mut()
            .observe(&self.key, &inputs, &measured.outcome);
        self.units.push(Unit {
            phase,
            keys: spec.keys,
            workers: measured.workers,
            inputs,
            outcome: measured.outcome,
            predicted,
            at_ms: now_ms(),
        });
        Ok(measured.checkpoint)
    }

    /// The facts the units say about the kernel: time per batch and per row, memory per byte,
    /// worker time per wall second, the speed-ups, state per key.
    fn settle_facts(&mut self) {
        let sizes: Vec<&Unit> = self
            .units
            .iter()
            .filter(|u| u.phase == Phase::Sizes && u.keys == Keys::Uniform)
            .collect();
        let points: Vec<(f64, f64)> = sizes
            .iter()
            .filter_map(|u| Some((u.inputs.data.rows? as f64, u.outcome.duration?)))
            .collect();
        if let Some((fixed, per_row)) = line(&points) {
            self.facts.per_batch_seconds = Some(fixed);
            self.facts.per_row_seconds = Some(per_row);
        }
        if let Some(rows) = self.largest_measured() {
            let at: Vec<&&Unit> = sizes
                .iter()
                .filter(|u| u.inputs.data.rows == Some(rows))
                .collect();
            let per_byte = |target: Target| {
                median(
                    at.iter()
                        .filter_map(|u| {
                            let bytes = u.inputs.data.bytes? as f64;
                            (bytes > 0.0).then(|| u.outcome.get(target).map(|v| v / bytes))?
                        })
                        .collect(),
                )
            };
            self.facts.peak_per_byte = per_byte(Target::PeakMemory);
            self.facts.retained_per_byte = per_byte(Target::RetainedMemory);
        }
        let scaling = |workers: u32| {
            median(
                self.units
                    .iter()
                    .filter(|u| u.phase == Phase::Scaling && u.workers == workers)
                    .filter_map(|u| u.outcome.duration)
                    .collect(),
            )
        };
        let one = scaling(1);
        let speedup = |workers: u32| {
            one.zip(scaling(workers))
                .filter(|(_, w)| *w > 0.0)
                .map(|(one, w)| one / w)
        };
        self.facts.speedup_2 = speedup(2);
        self.facts.speedup_4 = speedup(4);
        self.facts.worker_seconds_per_second = median(
            self.units
                .iter()
                .filter(|u| u.phase == Phase::Scaling && u.workers == 1)
                .filter_map(|u| {
                    let wall = u.outcome.duration?;
                    (wall > 0.0).then(|| u.outcome.worker_time.map(|w| w / wall))?
                })
                .collect(),
        );
        let state: Vec<(f64, f64)> = self
            .units
            .iter()
            .filter(|u| u.phase == Phase::State)
            .filter_map(|u| Some((u.inputs.distinct()? as f64, u.outcome.state_growth?)))
            .collect();
        if self.kind.stateful && !self.progress.state.is_empty() && state.is_empty() {
            self.notes.push(
                "the kind's state reports no footprint, so its growth per key is not measured"
                    .into(),
            );
        }
        self.facts.state_per_key_bytes = match state.as_slice() {
            [] => None,
            [(d, g)] => Some(g / d),
            many => line(many).map(|(_, slope)| slope),
        };
    }

    fn skew(&self) -> Option<SkewSensitivity> {
        let rows = self
            .progress
            .sizes
            .iter()
            .filter(|s| s.uniform > 0 && s.skewed > 0)
            .map(|s| s.rows)
            .max()?;
        let at = |keys: Keys| {
            self.units
                .iter()
                .filter(move |u| {
                    u.phase == Phase::Sizes && u.keys == keys && u.inputs.data.rows == Some(rows)
                })
                .collect::<Vec<_>>()
        };
        let (uniform, skewed) = (at(Keys::Uniform), at(Keys::Skewed));
        let of = |units: &[&Unit], target: Target| {
            median(units.iter().filter_map(|u| u.outcome.get(target)).collect())
        };
        let measured_skew = |units: &[&Unit]| {
            median(
                units
                    .iter()
                    .flat_map(|u| u.inputs.data.columns.iter().filter_map(|c| c.skew))
                    .collect(),
            )
        };
        let ratio = |target: Target| {
            of(&skewed, target).zip(of(&uniform, target)).map(|(s, u)| {
                let floor = target.unit().floor();
                (s + floor) / (u + floor)
            })
        };
        Some(SkewSensitivity {
            rows,
            uniform_skew: measured_skew(&uniform)?,
            skewed_skew: measured_skew(&skewed)?,
            duration_ratio: ratio(Target::Duration)?,
            peak_memory_ratio: ratio(Target::PeakMemory)?,
            value_dependent: true,
        })
    }

    fn selectivity(&self) -> Option<Selectivity> {
        let of = |keys: Keys| {
            median(
                self.units
                    .iter()
                    .filter(|u| u.phase == Phase::Sizes && u.keys == keys)
                    .filter_map(|u| {
                        let rows = u.inputs.data.rows?;
                        (rows > 0).then(|| u.outcome.rows_out.map(|o| o / rows as f64))?
                    })
                    .collect(),
            )
        };
        Some(Selectivity {
            uniform: of(Keys::Uniform)?,
            skewed: of(Keys::Skewed),
            value_dependent: true,
        })
    }

    /// The inputs of [`COST_ROWS`] rows of the uniform data at one worker: a measured unit of
    /// that size when there is one, else the nearest uniform unit's, scaled.
    fn cost_inputs(&self) -> Inputs {
        let nearest = self
            .units
            .iter()
            .filter(|u| u.phase == Phase::Sizes && u.keys == Keys::Uniform)
            .min_by_key(|u| u.inputs.data.rows.unwrap_or(0).abs_diff(COST_ROWS));
        let mut inputs = match nearest {
            Some(unit) => {
                let mut inputs = unit.inputs.clone();
                if let (Some(rows), Some(bytes)) = (inputs.data.rows, inputs.data.bytes)
                    && rows > 0
                {
                    inputs.data.bytes =
                        Some((bytes as f64 / rows as f64 * COST_ROWS as f64) as u64);
                }
                inputs.data.rows = Some(COST_ROWS);
                inputs
            }
            None => Inputs::batch(
                COST_ROWS,
                (COST_ROWS as f64 * self.plan.bytes_per_row) as u64,
            ),
        };
        inputs.kernel = self.facts.clone();
        inputs.resources.workers = Some(1);
        inputs
    }

    /// AR section 6.6: every unit to the store, the model fitted from it, the cost per
    /// [`COST_ROWS`] rows.
    fn finish(mut self) -> Result<WarmUpReport, WarmUpFailure> {
        self.settle_facts();
        let failed = |progress: &Progress, reason: String| WarmUpFailure {
            reason,
            progress: progress.clone(),
        };
        let store = Arc::clone(self.warm.cost.store());
        let id = self.warm.cost.model_id();
        let mut store_bytes = 0u64;
        for unit in &self.units {
            let mut inputs = unit.inputs.clone();
            inputs.kernel = self.facts.clone();
            let mut observation =
                Observation::new(self.key.clone(), inputs, unit.outcome.clone(), unit.at_ms);
            observation.model = Some(id.clone());
            observation.predictions.push(Predicted {
                model: id.clone(),
                cost: unit.predicted,
            });
            store
                .append(&observation)
                .map_err(|e| failed(&self.progress, format!("writing an observation: {e}")))?;
            store_bytes += serde_json::to_vec(&observation).map_or(0, |b| b.len() as u64);
        }
        let fit = self
            .warm
            .cost
            .refit()
            .map_err(|e| failed(&self.progress, format!("fitting {id}: {e}")))?;
        let state = store
            .load_state(&id)
            .map_err(|e| failed(&self.progress, format!("reading {id}'s state back: {e}")))?
            .map_or(0, |state| state.len() as u64);
        store_bytes += state;
        let cost = self
            .warm
            .cost
            .model()
            .predict(&self.key, &self.cost_inputs());
        let kernel_worker_seconds = self
            .units
            .iter()
            .map(|u| u.outcome.worker_time)
            .sum::<Option<f64>>();
        let usage = self.bench.usage();
        let consumed = Consumed {
            wall_seconds: self.bench.elapsed(),
            worker_seconds: usage.worker_seconds,
            kernel_worker_seconds,
            memory_peak_bytes: usage.memory_peak_bytes,
            memory_byte_seconds: usage.memory_byte_seconds,
            arena_bytes: self.arena_bytes,
            store_bytes,
        };
        self.notes.extend(self.bench.notes());
        Ok(WarmUpReport {
            model: self.warm.cost.model_id(),
            key: self.key.clone(),
            cost_per_10000_rows: cost,
            kernel: self.facts.clone(),
            skew: self.skew(),
            selectivity: self.selectivity(),
            consumed,
            observations: self.units.len() as u64,
            fit,
            progress: self.progress,
            notes: self.notes,
        })
    }
}

/// The least-squares line `y = a + b x` through `points`; when its intercept would be negative,
/// the line through the origin. `None` below two distinct `x`.
fn line(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = points.len() as f64;
    if points.len() < 2 {
        return None;
    }
    let (mx, my) = (
        points.iter().map(|p| p.0).sum::<f64>() / n,
        points.iter().map(|p| p.1).sum::<f64>() / n,
    );
    let sxx: f64 = points.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
    if sxx <= 0.0 {
        return None;
    }
    let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let b = sxy / sxx;
    let a = my - b * mx;
    if a >= 0.0 {
        return Some((a, b));
    }
    let through: f64 = points.iter().map(|p| p.0 * p.1).sum::<f64>()
        / points.iter().map(|p| p.0 * p.0).sum::<f64>();
    Some((0.0, through))
}

/// The cost per [`COST_ROWS`] rows in one line of the check's summary, and how far warm-up got.
pub(super) fn summary(report: &WarmUpReport) -> String {
    let c = &report.cost_per_10000_rows;
    let mut out = format!(
        "  warm-up ({}): per {COST_ROWS} rows {} (p95 {}), worker {}, peak {}, retained {}{} \
         [{}, {} observations]\n",
        report.model,
        seconds(c.duration.p50),
        seconds(c.duration.p95),
        seconds(c.worker_time.p50),
        bytes(c.peak_memory.p50),
        bytes(c.retained_memory.p50),
        if report.kernel.stateful == Some(true) {
            format!(", state {}", bytes(c.state_growth.p50))
        } else {
            String::new()
        },
        basis(c.duration.basis),
        c.duration.observations,
    );
    let k = &report.kernel;
    let mut facts = Vec::new();
    if let (Some(cold), Some(warm)) = (k.start_cold_seconds, k.start_warm_seconds) {
        facts.push(format!(
            "start-up cold {}, warm {}",
            seconds(cold),
            seconds(warm)
        ));
    }
    if let Some(s) = k.speedup_2 {
        facts.push(format!("speed-up {s:.2}x at 2 workers"));
    }
    if let Some(s) = k.speedup_4 {
        facts.push(format!("{s:.2}x at 4"));
    }
    if let Some(s) = &report.skew {
        facts.push(format!(
            "skewed keys {:.2}x the time (value-dependent)",
            s.duration_ratio
        ));
    }
    if let Some(b) = k.state_per_key_bytes {
        facts.push(format!("state {b:.1} bytes per key"));
    }
    if let (Some(s), Some(b)) = (k.checkpoint_seconds, k.checkpoint_bytes) {
        facts.push(format!("checkpoint {} in {}", bytes(b), seconds(s)));
    }
    if !facts.is_empty() {
        out.push_str(&format!("  warm-up: {}\n", facts.join(", ")));
    }
    let measured: Vec<String> = report
        .progress
        .sizes
        .iter()
        .filter(|s| s.uniform + s.skewed > 0)
        .map(|s| format!("{} x{}", s.rows, s.uniform + s.skewed))
        .collect();
    let mut how_far = format!("  warm-up: measured {}", measured.join(", "));
    if let Some(r) = &report.progress.stopped_on_residual {
        how_far.push_str(&format!(
            "; stopped after {} rows, residual {:.1}%",
            r.rows,
            r.duration.max(r.peak_memory) * 100.0
        ));
    }
    for b in &report.progress.stopped_on_budget {
        how_far.push_str(&format!(
            "; budget spent in {:?} at {:.1} s of its {:.1} s, before {} rows",
            b.phase, b.elapsed_seconds, b.budget_seconds, b.rows
        ));
    }
    out.push_str(&how_far);
    out.push('\n');
    let used = &report.consumed;
    out.push_str(&format!(
        "  warm-up used {:.2} s wall, {} worker, {} peak, {} to the store\n",
        used.wall_seconds,
        used.worker_seconds
            .map_or_else(|| "unmeasured".to_string(), seconds),
        used.memory_peak_bytes
            .map_or_else(|| "unmeasured".to_string(), |b| bytes(b as f64)),
        bytes(used.store_bytes as f64),
    ));
    out
}

fn basis(b: moruna_cost::Basis) -> &'static str {
    match b {
        moruna_cost::Basis::Measured => "measured",
        moruna_cost::Basis::WarmUp => "warm_up",
        moruna_cost::Basis::Prior => "prior",
        moruna_cost::Basis::Default => "default",
    }
}

fn seconds(s: f64) -> String {
    if s >= 1.0 {
        format!("{s:.2} s")
    } else if s >= 1e-3 {
        format!("{:.2} ms", s * 1e3)
    } else {
        format!("{:.1} us", s * 1e6)
    }
}

fn bytes(b: f64) -> String {
    if b >= (1u64 << 30) as f64 {
        format!("{:.1} GiB", b / (1u64 << 30) as f64)
    } else if b >= (1u64 << 20) as f64 {
        format!("{:.1} MiB", b / (1u64 << 20) as f64)
    } else if b >= 1024.0 {
        format!("{:.1} KiB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_and_its_origin_fallback() {
        let (a, b) = line(&[(1.0, 3.0), (2.0, 5.0), (3.0, 7.0)]).expect("a line");
        assert!((a - 1.0).abs() < 1e-12 && (b - 2.0).abs() < 1e-12);
        let (a, b) = line(&[(1.0, 0.0), (2.0, 3.0)]).expect("a line");
        assert_eq!(a, 0.0, "a negative intercept falls back to the origin");
        assert!((b - 1.2).abs() < 1e-12, "{b}");
        assert_eq!(line(&[(1.0, 1.0)]), None);
        assert_eq!(line(&[(1.0, 1.0), (1.0, 2.0)]), None);
    }

    #[test]
    fn medians_and_errors() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert!((log_error(2.0, 1.0, Target::RowsOut) - (3.0f64 / 2.0).ln()).abs() < 1e-12);
    }

    #[test]
    fn kinds_settings_and_units_print() {
        assert_eq!(cost_kind("pex"), "package");
        assert_eq!(cost_kind("wasm"), "package");
        assert_eq!(cost_kind("python"), "python");
        let a = settings_digest(&serde_json::json!({"b": 1, "a": 2}));
        let b = settings_digest(&serde_json::json!({"a": 2, "b": 1}));
        assert_eq!(a, b, "key order does not change the digest");
        assert_ne!(a, settings_digest(&serde_json::json!({})));
        assert_eq!(seconds(2.0), "2.00 s");
        assert_eq!(seconds(0.002), "2.00 ms");
        assert_eq!(seconds(2e-6), "2.0 us");
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(2048.0), "2.0 KiB");
        assert_eq!(bytes(3.0 * (1u64 << 20) as f64), "3.0 MiB");
        assert_eq!(bytes(5.0 * (1u64 << 30) as f64), "5.0 GiB");
        for b in [
            moruna_cost::Basis::Measured,
            moruna_cost::Basis::WarmUp,
            moruna_cost::Basis::Prior,
            moruna_cost::Basis::Default,
        ] {
            assert!(!basis(b).is_empty());
        }
    }

    #[test]
    fn configured_refuses_what_it_cannot_use() {
        let dir =
            std::env::temp_dir().join(format!("moruna-warmup-configured-{}", std::process::id()));
        let refused = |r: moruna_kernel::Result<WarmUp>| r.err().expect("refused").to_string();
        assert!(refused(WarmUp::configured(Some(&dir), -1.0, None, None)).contains("budget"));
        assert!(refused(WarmUp::configured(Some(&dir), f64::NAN, None, None)).contains("budget"));
        assert!(
            refused(WarmUp::configured(
                Some(&dir),
                1.0,
                Some(r#"{"columns": 3}"#),
                None
            ))
            .contains("not table statistics")
        );
        assert!(
            refused(WarmUp::configured(Some(&dir), 1.0, None, Some("nope")))
                .contains("registry has loglinear")
        );
        let warm = WarmUp::configured(
            Some(&dir),
            2.5,
            Some(r#"{"columns": [{"name": "k", "key": true, "distinct": 10}]}"#),
            Some("loglinear"),
        )
        .expect("configured");
        assert_eq!(warm.budget, Duration::from_millis(2500));
        assert_eq!(
            warm.statistics.expect("statistics").columns[0].distinct,
            Some(10)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The decisions, over a bench of known values (SDD 24 p.5, CM-T47 to CM-T52, CM-T56): no
    // clock, no kernel, every figure exact.

    use super::bench::{Measured, Usage};
    use moruna_cost::{
        Basis, Column, CostError, CostModel, CostModels, Estimate, ModelAdapter, ObservationSource,
        Origin,
    };
    use moruna_job::Role;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// What the fake kernel costs, which the fake bench measures and the oracle model predicts.
    #[derive(Clone, Copy, Debug)]
    struct Truth {
        per_row: f64,
        skewed: f64,
        parallel: f64,
        state_per_key: Option<f64>,
        starts: [f64; 2],
        fail_from: Option<u64>,
    }

    impl Truth {
        fn linear(per_row: f64) -> Truth {
            Truth {
                per_row,
                skewed: 1.0,
                parallel: 1.0,
                state_per_key: None,
                starts: [0.05, 0.01],
                fail_from: None,
            }
        }

        fn seconds(&self, rows: u64, skewed: bool, workers: u32) -> f64 {
            let skew = if skewed { self.skewed } else { 1.0 };
            self.per_row * rows as f64 * skew / f64::from(workers).powf(self.parallel)
        }
    }

    const BYTES_PER_ROW: u64 = 16;

    /// A unit the fake bench was asked for: its rows, its keys, its workers.
    type Asked = (u64, Keys, u32);

    /// The fake bench: each unit takes exactly what `truth` says, on a clock that moves by it.
    struct Fake {
        truth: Truth,
        clock: f64,
        /// Every unit asked for, kept where the test reads it after the run.
        units: std::rc::Rc<std::cell::RefCell<Vec<Asked>>>,
    }

    impl Bench for Fake {
        fn elapsed(&self) -> f64 {
            self.clock
        }
        fn pause(&mut self) {}
        fn resume(&mut self) {}
        fn start(&mut self, attempt: usize) -> Result<f64, String> {
            self.clock += self.truth.starts[attempt];
            Ok(self.truth.starts[attempt])
        }
        fn unit(
            &mut self,
            spec: &Spec,
            workers: u32,
            checkpoint: bool,
        ) -> Result<Measured, String> {
            if self.truth.fail_from.is_some_and(|at| spec.rows >= at) {
                return Err(format!("{} rows are too many", spec.rows));
            }
            let skewed = spec.keys == Keys::Skewed;
            let seconds = self.truth.seconds(spec.rows, skewed, workers);
            self.clock += seconds;
            self.units
                .borrow_mut()
                .push((spec.rows, spec.keys, workers));
            let distinct = spec.distinct.unwrap_or(spec.rows);
            let bytes = spec.rows * BYTES_PER_ROW;
            let mut outcome = Outcome::new(Origin::WarmUp);
            outcome
                .set(Target::Duration, seconds)
                .set(Target::WorkerTime, seconds * f64::from(workers))
                .set(Target::PeakMemory, (2 * bytes) as f64)
                .set(Target::RetainedMemory, bytes as f64)
                .set(Target::RowsOut, spec.rows as f64)
                .set(Target::BytesOut, bytes as f64);
            if let Some(per_key) = self.truth.state_per_key {
                outcome.set(Target::StateGrowth, per_key * distinct as f64);
            }
            Ok(Measured {
                data: Data {
                    rows: Some(spec.rows),
                    bytes: Some(bytes),
                    columns: vec![Column {
                        name: "id".into(),
                        bytes_mean: Some(8.0),
                        null_fraction: Some(0.0),
                        distinct: Some(distinct),
                        key: true,
                        skew: Some(if skewed { 0.8 } else { 0.01 }),
                        ..Column::default()
                    }],
                    columns_read: Some(1),
                    ..Data::default()
                },
                workers,
                outcome,
                checkpoint: checkpoint.then_some((0.002, distinct * 8)),
            })
        }
        fn usage(&self) -> Usage {
            Usage {
                worker_seconds: Some(self.clock),
                memory_peak_bytes: Some(1 << 20),
                memory_byte_seconds: Some(1 << 21),
            }
        }
        fn notes(&mut self) -> Vec<String> {
            vec!["measured by the fake bench".into()]
        }
    }

    /// The test model: it predicts what `truth` says (or `wrong` times it), exactly.
    struct Oracle {
        truth: Truth,
        wrong: f64,
    }

    impl CostModel for Oracle {
        fn id(&self) -> ModelId {
            ModelId::new("oracle", 1)
        }
        fn predict(&self, _key: &CostKey, inputs: &Inputs) -> UnitCost {
            let rows = inputs.data.rows.unwrap_or(0);
            let skewed = inputs
                .data
                .columns
                .iter()
                .any(|c| c.skew.is_some_and(|s| s > 0.5));
            let workers = inputs.resources.workers.unwrap_or(1);
            let mut cost = UnitCost::zero();
            let at = |p50: f64, target: Target| Estimate {
                p50,
                p95: p50,
                unit: target.unit(),
                basis: Basis::WarmUp,
                observations: 1,
                extrapolated: false,
                drifting: false,
            };
            cost.duration = at(
                self.wrong * self.truth.seconds(rows, skewed, workers),
                Target::Duration,
            );
            cost.peak_memory = at((2 * rows * BYTES_PER_ROW) as f64, Target::PeakMemory);
            cost
        }
        fn observe(&mut self, _: &CostKey, _: &Inputs, _: &Outcome) {}
        fn fit(&mut self, history: &dyn ObservationSource) -> Result<FitReport, CostError> {
            let mut report = FitReport::default();
            history.scan_all(&mut |h| {
                report.keys += 1;
                report.observations += h.observations.len() as u64;
                Ok(())
            })?;
            Ok(report)
        }
        fn save(&self) -> Result<Vec<u8>, CostError> {
            Ok(br#"{"model":{"name":"oracle","version":1}}"#.to_vec())
        }
        fn load(&mut self, _: &[u8]) -> Result<(), CostError> {
            Ok(())
        }
    }

    struct OracleAdapter(Truth, f64);

    impl ModelAdapter for OracleAdapter {
        fn name(&self) -> &str {
            "oracle"
        }
        fn make(&self) -> Box<dyn CostModel> {
            Box::new(Oracle {
                truth: self.0,
                wrong: self.1,
            })
        }
    }

    static STORES: AtomicU64 = AtomicU64::new(0);

    fn warm(truth: Truth, wrong: f64, budget: f64) -> (WarmUp, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "moruna-warmup-logic-{}-{}",
            std::process::id(),
            STORES.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut settings = CostSettings::directory(&dir);
        let mut models = CostModels::standard();
        models
            .register(Arc::new(OracleAdapter(truth, wrong)))
            .expect("registered");
        settings.models = models;
        settings.model = "oracle".into();
        let mut warm = WarmUp::new(settings.open().expect("opened"));
        warm.budget = Duration::from_secs_f64(budget);
        (warm, dir)
    }

    fn key() -> CostKey {
        CostKey {
            role: Role::Kernel,
            kind: "rust".into(),
            fingerprint: Digest([1; 32]),
            settings: Digest([2; 32]),
            schema: Digest([3; 32]),
        }
    }

    fn kind(stateful: bool, instances: usize) -> Kind {
        Kind {
            stateful,
            instances,
            resume: if stateful {
                ResumePolicy::Checkpoint
            } else {
                ResumePolicy::Reinit
            },
            made_here: true,
        }
    }

    /// One warm-up of `truth` under the oracle (`wrong` times off), with `budget` seconds.
    fn warmed(
        truth: Truth,
        wrong: f64,
        budget: f64,
        kind: Kind,
    ) -> (Result<WarmUpReport, WarmUpFailure>, Vec<Asked>) {
        let (mut warm, dir) = warm(truth, wrong, budget);
        let plan = Plan {
            sizes: SIZES.iter().map(|&r| (r, true)).collect(),
            bytes_per_row: BYTES_PER_ROW as f64,
            has_keys: true,
            notes: Vec::new(),
        };
        let units = std::rc::Rc::default();
        let fake = Fake {
            truth,
            clock: 0.0,
            units: std::rc::Rc::clone(&units),
        };
        let mut run = Run::new(plan, kind, &mut warm, Box::new(fake), key(), 7);
        run.start();
        let report = run.complete();
        let _ = std::fs::remove_dir_all(&dir);
        let units = units.borrow().clone();
        (report, units)
    }

    fn ok(r: Result<WarmUpReport, WarmUpFailure>) -> WarmUpReport {
        match r {
            Ok(report) => report,
            Err(f) => panic!("warm-up failed: {} ({:?})", f.reason, f.progress),
        }
    }

    #[test]
    fn cm_t47_the_sizes_stop_when_the_residual_is_small_and_not_otherwise() {
        let truth = Truth::linear(1e-6);
        let (report, units) = warmed(truth, 1.0, 600.0, kind(false, usize::MAX));
        let report = ok(report);
        let stop = report
            .progress
            .stopped_on_residual
            .clone()
            .expect("stopped");
        assert_eq!(stop.rows, 100_000);
        assert_eq!(
            (stop.duration, stop.peak_memory),
            (0.0, 0.0),
            "the oracle is exact"
        );
        let counts: Vec<(u32, u32)> = report
            .progress
            .sizes
            .iter()
            .map(|s| (s.uniform, s.skewed))
            .collect();
        assert_eq!(counts, vec![(3, 3), (3, 3), (3, 3), (0, 0)]);
        assert!(
            units.iter().all(|u| u.0 < 1_000_000 || u.2 > 1),
            "no 1M size unit"
        );
        assert_eq!(report.progress.residuals.len(), 1);

        // A model twice wrong never has a small residual: every size runs, judged twice.
        let (report, _) = warmed(truth, 2.0, 600.0, kind(false, usize::MAX));
        let report = ok(report);
        assert!(report.progress.stopped_on_residual.is_none());
        let residuals: Vec<f64> = report
            .progress
            .residuals
            .iter()
            .map(|r| r.duration)
            .collect();
        assert_eq!(residuals.len(), 2);
        for r in residuals {
            assert!((r - 2f64.ln()).abs() < 1e-6, "{r}");
        }
        assert_eq!(report.progress.sizes[3].uniform, 3);
    }

    #[test]
    fn cm_t48_each_phase_stops_at_its_deadline_and_says_where() {
        // 10 us a row; a budget of 4 s: the sizes until 2 s, scaling until 3 s, state until 4 s.
        let truth = Truth::linear(1e-5);
        let (report, units) = warmed(truth, 1.0, 4.0, kind(false, usize::MAX));
        let report = ok(report);
        let stops = &report.progress.stopped_on_budget;
        assert_eq!(stops.len(), 2, "{stops:?}");
        // Start-up 0.06 s, 1,000 rows 6 x 0.01, 10,000 rows 6 x 0.1, one of 100,000 (1 s):
        // 1.72 s, and the next 1 s would pass 2 s.
        let sizes = &stops[0];
        assert_eq!(
            (sizes.phase, sizes.rows, sizes.workers),
            (Phase::Sizes, 100_000, 1)
        );
        assert!((sizes.elapsed_seconds - 1.72).abs() < 1e-9, "{sizes:?}");
        assert!((sizes.predicted_seconds - 1.0).abs() < 1e-9);
        assert_eq!(sizes.budget_seconds, 2.0);
        assert_eq!(
            (
                report.progress.sizes[2].uniform,
                report.progress.sizes[2].skewed
            ),
            (1, 0)
        );
        // Scaling at the largest size reached: one unit (to 2.72 s), then 3 s is passed.
        let scaling = &stops[1];
        assert_eq!((scaling.phase, scaling.rows), (Phase::Scaling, 100_000));
        assert!((scaling.elapsed_seconds - 2.72).abs() < 1e-9, "{scaling:?}");
        assert_eq!(scaling.budget_seconds, 3.0);
        assert_eq!(report.progress.scaling_rows, Some(100_000));
        assert_eq!(report.progress.scaling[0].units, 1);
        assert_eq!(units.len(), 6 + 6 + 1 + 1);
        assert!((report.consumed.wall_seconds - 2.72).abs() < 1e-9);
        assert_eq!(report.observations, 14);

        // A budget of nothing: start-up and the sizes stop, nothing runs.
        let (report, units) = warmed(truth, 1.0, 0.0, kind(false, usize::MAX));
        let report = ok(report);
        let phases: Vec<Phase> = report
            .progress
            .stopped_on_budget
            .iter()
            .map(|s| s.phase)
            .collect();
        assert_eq!(phases, vec![Phase::StartUp, Phase::Sizes]);
        assert!(units.is_empty() && !report.progress.start_up);
        assert_eq!(report.observations, 0);
    }

    #[test]
    fn cm_t49_start_up_records_the_cold_and_the_warm_start() {
        let (report, _) = warmed(Truth::linear(1e-7), 1.0, 600.0, kind(false, usize::MAX));
        let report = ok(report);
        assert!(report.progress.start_up);
        assert_eq!(report.kernel.start_cold_seconds, Some(0.05));
        assert_eq!(report.kernel.start_warm_seconds, Some(0.01));
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("made in this process"))
        );
    }

    #[test]
    fn cm_t50_the_speed_ups_are_the_ratios_of_the_medians() {
        let mut truth = Truth::linear(1e-6);
        truth.parallel = 0.5;
        let (report, units) = warmed(truth, 1.0, 600.0, kind(false, usize::MAX));
        let report = ok(report);
        let workers: Vec<(u32, u32)> = report
            .progress
            .scaling
            .iter()
            .map(|w| (w.workers, w.units))
            .collect();
        assert_eq!(workers, vec![(1, 3), (2, 3), (4, 3)]);
        assert!((report.kernel.speedup_2.expect("2") - 2f64.sqrt()).abs() < 1e-9);
        assert!((report.kernel.speedup_4.expect("4") - 2.0).abs() < 1e-9);
        assert_eq!(report.kernel.worker_seconds_per_second, Some(1.0));
        assert!(units.iter().filter(|u| u.2 == 4).all(|u| u.0 == 100_000));
        // Time per row from the line through the uniform sizes: the truth's, no fixed part.
        assert!((report.kernel.per_row_seconds.expect("per row") - 1e-6).abs() < 1e-15);
        assert!(report.kernel.per_batch_seconds.expect("fixed").abs() < 1e-12);
        assert_eq!(report.kernel.peak_per_byte, Some(2.0));
        assert_eq!(report.kernel.retained_per_byte, Some(1.0));
    }

    #[test]
    fn cm_t51_the_state_pass_fits_growth_per_key_and_times_the_checkpoint() {
        let mut truth = Truth::linear(1e-6);
        truth.state_per_key = Some(16.0);
        let (report, units) = warmed(truth, 1.0, 600.0, kind(true, 2));
        let report = ok(report);
        assert_eq!(report.progress.state, vec![10, 100, 1_000, 10_000, 100_000]);
        assert!((report.kernel.state_per_key_bytes.expect("per key") - 16.0).abs() < 1e-9);
        assert!(report.progress.checkpoint);
        assert_eq!(
            report.kernel.checkpoint_bytes,
            Some(800_000.0),
            "the largest state's"
        );
        assert_eq!(report.kernel.checkpoint_seconds, Some(0.002));
        let workers: Vec<u32> = report.progress.scaling.iter().map(|w| w.workers).collect();
        assert_eq!(
            workers,
            vec![1, 2],
            "a pool of two runs four workers as two"
        );
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("at most 2 instances"))
        );
        assert!(units.iter().all(|u| u.2 <= 2));
        // A kind that keeps no checkpoint times none, and says so.
        let mut reinit = kind(true, 2);
        reinit.resume = ResumePolicy::Reinit;
        let report = ok(warmed(truth, 1.0, 600.0, reinit).0);
        assert!(!report.progress.checkpoint && report.kernel.checkpoint_bytes.is_none());
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("keeps no checkpoint"))
        );
    }

    #[test]
    fn cm_t52_skew_sensitivity_is_the_ratio_of_skewed_to_uniform() {
        let mut truth = Truth::linear(1e-6);
        truth.skewed = 3.0;
        let report = ok(warmed(truth, 1.0, 600.0, kind(false, usize::MAX)).0);
        let skew = report.skew.expect("skew");
        assert_eq!(skew.rows, 100_000);
        assert!((skew.duration_ratio - 3.0).abs() < 1e-6, "{skew:?}");
        assert!((skew.peak_memory_ratio - 1.0).abs() < 1e-12);
        assert_eq!((skew.uniform_skew, skew.skewed_skew), (0.01, 0.8));
        assert!(skew.value_dependent);
        let selectivity = report.selectivity.expect("selectivity");
        assert_eq!((selectivity.uniform, selectivity.skewed), (1.0, Some(1.0)));
    }

    #[test]
    fn cm_t54_every_unit_is_stored_with_its_prediction_and_the_model_refitted() {
        let (mut warm, dir) = warm(Truth::linear(1e-6), 1.0, 600.0);
        let plan = Plan {
            sizes: SIZES.iter().map(|&r| (r, true)).collect(),
            bytes_per_row: BYTES_PER_ROW as f64,
            has_keys: true,
            notes: vec!["a note of the plan".into()],
        };
        let fake = Fake {
            truth: Truth::linear(1e-6),
            clock: 0.0,
            units: std::rc::Rc::default(),
        };
        let mut run = Run::new(
            plan,
            kind(false, usize::MAX),
            &mut warm,
            Box::new(fake),
            key(),
            7,
        );
        run.start();
        let report = ok(run.complete());
        let store = LocalStoreProbe::all(&dir);
        assert_eq!(store.len() as u64, report.observations);
        assert_eq!(
            report.fit.observations, report.observations,
            "refitted over the store"
        );
        for o in &store {
            assert_eq!(o.outcome.origin, Origin::WarmUp);
            assert_eq!(o.model, Some(ModelId::new("oracle", 1)));
            assert_eq!(o.predictions.len(), 1);
            assert_eq!(
                o.inputs.kernel.start_cold_seconds,
                Some(0.05),
                "the facts ride along"
            );
        }
        assert_eq!(report.consumed.arena_bytes, 7);
        assert_eq!(report.consumed.memory_peak_bytes, Some(1 << 20));
        assert!(report.consumed.store_bytes > 0);
        assert!(report.notes.iter().any(|n| n == "a note of the plan"));
        assert!(
            report
                .notes
                .iter()
                .any(|n| n == "measured by the fake bench")
        );
        let d = report.cost_per_10000_rows.duration;
        assert!(
            (d.p50 - 0.01).abs() < 1e-12,
            "the oracle's 10,000 rows: {d:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cm_t56_a_unit_that_fails_fails_warm_up_with_how_far_it_got() {
        let mut truth = Truth::linear(1e-6);
        truth.fail_from = Some(100_000);
        let Err(failure) = warmed(truth, 1.0, 600.0, kind(false, usize::MAX)).0 else {
            panic!("failed")
        };
        assert!(
            failure
                .reason
                .starts_with("100000 rows (Uniform keys) at 1 workers")
        );
        assert!(failure.reason.contains("too many"));
        assert_eq!(failure.progress.sizes[1].uniform, 3);
        assert_eq!(failure.progress.sizes[2].uniform, 0);
    }

    /// The store's records, read back.
    struct LocalStoreProbe;

    impl LocalStoreProbe {
        fn all(dir: &std::path::Path) -> Vec<Observation> {
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
    }
}
