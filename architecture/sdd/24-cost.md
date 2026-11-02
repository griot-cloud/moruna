# Moruna SDD 24: The cost model's foundation (`moruna-cost`)

**Document type:** software design document, component 24
**Status:** DRAFT · 2026-10-29, amended 2026-10-29 by KD7c (learning during a run, drift, the simulator's start from a live run and its resize: CM-I23 to CM-I26, f.11 to f.13, d.7, CM-T38 to CM-T42; CM-O1 to CM-O3 built; CM-I27 to CM-I29 and CM-T45 to CM-T58 are KD6b's, on `feat/kd6b-warm-up`) (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** board items KD6a, KD6b, KD6c and KD7c (`BOARD.md`, EK wave D); `architecture/adaptive-runtime.md` (cited **AR §n**) sections 0, 1, 3, 4, 5, 7, 8 and 9.6 and gaps G1, G4, G6, G7, G12, decided by Brackly on 2026-10-28
**Preamble:** `00-preamble.md`; **Cited:** 18 (the registry of kinds, KD-I1, KD-I2), 22 (`moruna-job`, JB-I1), 11 e.3 and f.9 (today's profile store), 21 (`moruna-record`, on `v1`)
**Component location:** `crates/moruna-cost/` (`src/key.rs`, `src/inputs.rs`, `src/estimate.rs`, `src/model.rs`, `src/registry.rs`, `src/loglinear/`, `src/store/`, `src/exact.rs`, `src/error.rs`, `src/sample.rs`, `src/morsel.rs`, `src/online.rs`, `src/sim/` (`mod.rs`, `policy.rs`, `scaling.rs`, `forecast.rs`, `start.rs`)), `crates/moruna-cost/tests/` (`crate.rs`, `model.rs`, `store.rs`, `swap.rs`, `sim.rs`, `online.rs`)
**Consumes:** `moruna-job` (the `Role` a kind plays, and the rule a kind's name follows, which a model's name follows), `serde`, `serde_json`, `thiserror`. **Consumed by:** `moruna-plan` (its planners and `explain`, SDD 26), KD6b (warm-up in `moruna check`), KD7 (the scheduler and its lookahead, which play the simulator), and every embedder that plans in its own process.

**Warm-up/runtime location:** `crates/moruna-cost/` (`src/key.rs`, `src/inputs.rs`, `src/estimate.rs`, `src/model.rs`, `src/registry.rs`, `src/loglinear/`, `src/store/`, `src/exact.rs`, `src/error.rs`), `crates/moruna-cost/tests/` (`crate.rs`, `model.rs`, `store.rs`, `swap.rs`); warm-up (section p, KD6b) in `crates/moruna-runtime/src/cost.rs`, `crates/moruna-runtime/src/check/warmup/` (`mod.rs`, `bench.rs`, `data.rs`, `peak.rs`, `cpu.rs`) and `crates/moruna-runtime/tests/warmup.rs`, with the arena's high-water mark (`moruna-arena`), `OutsideMemory::reset_peak` and `peak` (`moruna-kernel`) and a stage cgroup's `memory.peak` (`moruna-sandbox`)

**Decisions worth your eye:** (1) a prediction is a distribution with its basis and the observations it rests on, never a number (AR decision 2); (2) the model is a port and a setting: the planner and the scheduler call `CostModel`, never the default model, and a second model trains from the raw records the first one saw (AR decision 3); (3) the default model is recursive least squares on logarithms with forgetting, plus a fixed overhead chosen at the smallest sizes, in about two hundred lines and no linear-algebra crate; (4) fitted state is a cache keyed by model id, refused by any other id; the raw records are the truth; (5) the local store's frame is a stand-in for `moruna-record`'s layout, which is on `v1` only (m); (6) the simulator lives here, not in `moruna-plan`: it needs only this crate's estimates and a workload of its own (tasks, units, edges), so no cycle forces it elsewhere, and the scheduler's lookahead (KD7) plays it without the plan; (7) the simulator holds the hard rules (memory, contention, scaling) and the policy only chooses among ready units, so a policy cannot break the budget; (8) a checkpoint is paid at a batch boundary by the task's next unit, so checkpoints can never starve units of workers.

---

## a. Purpose and boundary

The scheduler decides, continuously, what runs next, where it runs, where memory goes and how big a morsel is (AR §0). Every one of those decisions needs a unit's predicted time, memory and output, and how far the prediction may be wrong. This component predicts them, learns them from what units measured, and keeps the measurements so that a better model learns the same history.

It owns: what is known of a unit (`Inputs`, AR §3), what is costed (`CostKey`), the answer (`Estimate`, `UnitCost`, AR §4) and the measurement (`Outcome`); the model port (`CostModel`) and its registry (`CostModels`), the setting that chooses (`CostSetting`) and the models in use (`InUse`, shadow mode, `compare`); the default model (`LogLinear`, AR §5.3, §5.4); the observation store port (`ObservationSource`, `ObservationStore`) and its local adapter (`LocalStore`, AR §7); drawing from an estimate (`Rng`, `Estimate::quantile`, `Estimate::sample`, `Estimate::sum`); a target's cost as a line in rows (`CostModel::marginal`) and the morsel size it gives (`morsel`, AR G4); the discrete-event simulator (`sim::Simulator`, AR §8, G1, G6, G7), the policy port it plays (`sim::Policy`) and its first adapter (`sim::Admission`), and its Monte Carlo forecast (`Simulator::forecast`).

It refuses to know: hardware type, calls outside the run, tokens or egress (AR §0, removed on purpose); the job document and the plan (the simulator reads a `Workload` of its own, which `moruna-plan` builds, SDD 26 f.7); warm-up's Arrow data generation (KD6b, owned by moruna-runtime, section p); the runtime's buffer catalog and the runtime's policies (KD7); any embedder.

## b. Vocabulary

**Unit.** One task applied to one batch (AR §9.1).

**Key.** A `CostKey`: role, kind, function fingerprint, settings digest, input schema hash. Two units with one key are the same work on different data.

**Target.** One field of `UnitCost`: duration, worker time, peak memory, retained memory, rows out, bytes out, state growth, read bytes, spill bytes.

**Basis.** Which fallback answered: `Measured` (the function's history, some of it from a run), `WarmUp` (the function's history, all of it from warm-up), `Prior` (the kind's, pooled over its functions), `Default` (declared).

**Overhead.** The fixed part of a unit's cost: start-up and per-batch time, a baseline allocation.

**Retention.** The raw records kept per key (10,000); older ones are folded into the key's summary.

**Workload.** What the simulator plays: tasks, each with its units (batches) and one unit's `UnitCost`, and the edges between them.

**Buffer.** A unit's output while a consumer unit still needs it: in memory, on disk, or moving between them.

**Forecast.** The simulator's run at the medians and the spread of every figure over Monte Carlo runs.

**Morsel.** The rows per batch a task is given.

## c. Invariants

**CM-I1. The crate depends on four crates.** `moruna-job`, `serde`, `serde_json` and `thiserror`, and nothing else; no linear-algebra crate.

**CM-I2. Every estimate is a distribution.** Every `Estimate` a model returns has a finite `p50 >= 0`, a finite `p95 >= p50`, the unit of its target, a basis and an observation count.

**CM-I3. Fitted state belongs to its model id.** A model's saved state carries its `ModelId` (name and version); `load` refuses state another id saved, naming both, and is unchanged by the refusal; a store keeps state by id and never returns one id's state for another. A save and a load give back the same predictions to the bit.

**CM-I4. Fallbacks answer in order, per target.** The function's history when it has measured the target; else the kind's prior when any function of the kind has; else the declared default. `basis` says which.

**CM-I5. What is not known widens.** A feature missing from the inputs takes its history's mean and adds its variance; a feature outside the measured range (by more than a tenth) marks the estimate `extrapolated` and widens it.

**CM-I6. The store keeps what it can read back.** `append` refuses inputs of another version, a measured amount that is not finite and non-negative, and a kind that is not a kind's name. A record cut short by a crash is skipped and counted, and every record before and after it is read. A whole record that is not an observation is an error, not a skip.

**CM-I7. Retention holds.** A scan returns at most `per_key` records of a key, its last ones, oldest first, and the older ones as its summary; the file system holds at most a retention and one segment of a key once compaction runs.

**CM-I8. A model is a setting.** `CostModels` registers adapters by name, refuses a taken name, a name that is not one, and an adapter whose model carries another name; a setting naming an unknown model is refused naming the registered ones. Choosing another model changes nothing outside its adapter.

**CM-I9. Shadow predictions are kept with the outcome.** With a shadow configured, the deciding model's prediction is the one used; both are recorded with the unit's outcome, deciding first, and `compare` scores each on the same units.

**CM-I10. A prediction is cheap.** `LogLinear::predict` for a warm key allocates nothing and takes under 2 µs in an optimised build.

**CM-I11. The default model follows a change.** After a function's cost changes, its predictions follow within 150 units.

**CM-I12. The simulator schedules what a hand schedule does.** With exact costs, a workload's simulated finish, its tasks' spans and the critical path are the hand schedule's under the policy played, to the microsecond; a workload that is not a DAG of tasks with units and usable costs, or a machine without workers, memory or bandwidth, is refused by what is wrong; a policy that starts nothing while nothing runs, or chooses a candidate that is not there, stops the simulation with its name.

**CM-I13. Memory is a hard limit.** Under any policy and any costs, memory held (reservations at the 95th-percentile peak, buffers in memory, state) never passes the budget. A unit that does not fit waits while the simulator spills by known-future eviction (f.8), and its spill and read-back bytes are the hand count.

**CM-I14. What can never fit is named.** A unit whose reservation, inputs and task's state pass the budget is `Infeasible` with its task's name when the simulator is made; a unit that fits alone but cannot beside what is pinned when nothing else can move is `Infeasible` by name when it is reached.

**CM-I15. Contention slows in proportion.** When summed demand on the workers or the disk exceeds its capacity, every unit using it slows by capacity over demand; a kind's scaling curve slows its concurrent units to `s(k)/k` each and caps them where another adds less than `MIN_MARGINAL_GAIN`.

**CM-I16. Uncertainty is drawn from each estimate.** A Monte Carlo run draws each unit's duration from its estimate's log-normal through p50 and p95; with known distributions the forecast's p50 and p95 lie within 5% of the analytic answer, and the same seed gives the same forecast.

**CM-I17. Any model answers the overhead.** `CostModel::marginal` answers a target's per-batch and per-row cost for every model through two predictions; a higher per-batch overhead gives bigger morsels and a tighter memory smaller ones.

**CM-I18. The simulator is reusable and allocation-light.** A `Simulator` is built once per workload and run again and again; a run allocates nothing once built (its state is reset in place), so the lookahead (KD7) can run it inside a decision. CM-T26 measures it.

**CM-I19. No unit runs before its process is up.** A task's first unit runs its start-up before its body, and no other unit of the task is admitted until the start-up ends, so a cold-start-heavy task is forecast at the time its start-up takes, while its workers run other tasks' units.

**CM-I23. A simulation starts where a live run is.** `Simulator::run_from(start, policy)` plays the workload from a snapshot: per task the units finished, the units under way with the work each has left, and which finished outputs a consumer still needs are on disk. Time counts from the snapshot; a started task does not pay its start-up again; a finished output no consumer needs is freed; a running unit holds its reservation and pins its inputs; a stateful task holds the state its finished units grew. A snapshot that does not fit the workload is refused saying what is wrong (task count, more units started than a task has, a unit started before its inputs finished, more units under way than workers, a running unit's input on disk, an unfinished unit on disk). A fresh start is a fresh run, and the same simulator runs either way.

**CM-I24. A simulator follows a resize.** `Simulator::resize(machine)` makes later runs play the new machine: each task's cap follows its workers, a stateful task stays at one; a machine without workers, memory or bandwidth is refused, and memory that no longer holds a unit is `Infeasible` by name with the simulator unchanged. Nothing is allocated.

**CM-I25. A run learns online and writes off the hot path.** `Online` holds the models in use and the store. Every finished unit is one constant-time `observe` on the deciding model and the shadow (never a refit), and its observation, with both predictions, is kept in memory; `flush` appends what is kept, in the order the units finished, on whatever thread calls it, holding no lock across the store; `save` writes the fitted state. A store that refuses is returned and counted (written, unwritten, the first reason), never hidden; an outcome that is not a measurement is refused, counted with its reason, and learned from by no one. The shadow never steers.

**CM-I26. Drift is found, widens, forgets faster and settles, and never stops a run.** Per key and target, a two-sided CUSUM of the outcome's log-error standardised by the prediction's own spread (f.11): one unit, however far out, never marks a key; three units in a row far outside the spread do, more a little outside it. While a key drifts its estimates are widened and marked `drifting`, and the deciding model is told (`set_drifting`), which for `loglinear` forgets the level at once and forgets at `DRIFT_FORGETTING` (0.9) until it settles; it settles after `SETTLE_UNITS` (8) units in a row inside the model's own spread. Every spell is reported: the key, the target, the unit it began at, how many units it lasted, whether it settled.

## d. Interfaces

### d.1 The types

```rust
pub struct Digest(pub [u8; 32]);                               // hex in JSON
pub struct CostKey { pub role: Role, pub kind: String, pub fingerprint: Digest, pub settings: Digest, pub schema: Digest }

pub const INPUTS_VERSION: u32 = 1;
pub struct Inputs { pub version: u32, pub data: Data, pub kernel: Kernel, pub resources: Resources, pub job: Job }
pub struct Data { rows, bytes, columns: Vec<Column>, columns_read, layout: Option<Layout>, selectivity }   // AR §3.1
pub struct Column { name, bytes_mean, bytes_p95, null_fraction, distinct, key: bool, skew }
pub struct Layout { codec, row_group_rows, files }
pub struct Kernel { deterministic, preserves_columns, stateful, start_cold_seconds, start_warm_seconds,
                    per_batch_seconds, per_row_seconds, per_byte_seconds, peak_per_byte, retained_per_byte,
                    state_per_key_bytes, checkpoint_bytes, checkpoint_seconds, worker_seconds_per_second,
                    speedup_2, speedup_4 }                                                                   // AR §3.2
pub struct Resources { workers, given: Capacity, ceiling: Capacity, free: Capacity, image_cached }         // AR §3.3
pub struct Capacity { workers, memory_bytes, disk_bytes, disk_bytes_per_second }
pub struct Job { dependencies, consumers, contract_barrier }                                               // AR §3.4

pub enum Unit { Seconds, Bytes, Rows }
pub enum Basis { Measured, WarmUp, Prior, Default }
pub struct Estimate { pub p50: f64, pub p95: f64, pub unit: Unit, pub basis: Basis, pub observations: u32, pub extrapolated: bool,
                     pub drifting: bool }   // KD7c: set by `widened` only; omitted from JSON when false
pub enum Target { Duration, WorkerTime, PeakMemory, RetainedMemory, RowsOut, BytesOut, StateGrowth, ReadBytes, SpillBytes }
pub struct UnitCost { duration, worker_time, peak_memory, retained_memory, rows_out, bytes_out, state_growth, read_bytes, spill_bytes }
pub enum Origin { Run, WarmUp }
pub struct Outcome { pub origin: Origin, /* each target: Option<f64> */ }
```

Every field of `Inputs` below the version is optional (an `Option`, or an empty list, or a section of options). The record is written without what is unknown, so `{"version":1}` is a unit nothing is known of.

### d.2 The model port

```rust
pub struct ModelId { pub name: String, pub version: u32 }
pub trait CostModel: Send + Sync {
    fn id(&self) -> ModelId;
    fn predict(&self, key: &CostKey, inputs: &Inputs) -> UnitCost;
    fn observe(&mut self, key: &CostKey, inputs: &Inputs, outcome: &Outcome);
    fn fit(&mut self, history: &dyn ObservationSource) -> Result<FitReport>;
    fn save(&self) -> Result<Vec<u8>>;
    fn load(&mut self, bytes: &[u8]) -> Result<()>;
}
pub struct FitReport { pub keys: u64, pub observations: u64, pub torn: u64, pub summarised: u64 }
```

Exactly AR §5.1, and one provided method:

```rust
fn marginal(&self, key: &CostKey, inputs: &Inputs, target: Target, rows: u64) -> Marginal;
pub struct Marginal { pub p50: Line, pub p95: Line }
pub struct Line { pub fixed: f64, pub per_row: f64 }
```

`fit` replaces what the model held. `marginal` (f.7) is answered for every model by two predictions; a model that holds its overhead apart may answer it from that.

A second provided method (KD7c, f.11):

```rust
fn set_drifting(&mut self, key: &CostKey, drifting: bool) {}   // a model that forgets raises its forgetting
```

`loglinear` keeps the drifting keys apart from its fitted state (a run's, not the fit's: not saved, and cleared by `fit` and `load`).

### d.3 The registry and the models in use

```rust
pub trait ModelAdapter: Send + Sync { fn name(&self) -> &str; fn make(&self) -> Box<dyn CostModel>; }
pub struct CostModels;                       // empty(), standard() (loglinear), register(adapter), names(), make(name)
impl CostModels {
    pub fn open(&self, setting: &CostSetting, default: &str, store: &dyn ObservationStore) -> Result<InUse>;
}
pub struct CostSetting { pub model: Option<String>, pub shadow: Option<String> }   // the job's `cost` section
pub enum Readiness { Loaded(ModelId), Fitted(ModelId, FitReport), Refitted(ModelId, FitReport, String) }
pub struct InUse;                            // new(deciding, shadow), deciding(), deciding_mut(), shadow(), readiness(),
                                             // predict -> Prediction, settle(key, inputs, prediction, outcome, at_ms) -> Observation, save(store)
pub struct Prediction { pub deciding: Predicted, pub shadow: Option<Predicted> }
pub fn compare(source: &dyn ObservationSource) -> Result<Comparison>;   // per model, per target: n, median |log error|, p95 coverage
```

`open` makes the setting's model (or `default`), and the shadow when named; each loads the state the store holds for its id, and when there is none fits over the store's observations and saves the state (AR §5.2). State that does not load is refitted, and `Readiness::Refitted` says why. `settle` lets both models observe the unit and returns its observation with both predictions; it writes nothing (KD7c: the caller appends, off the hot path). A run uses them through `Online` (d.7).

### d.4 The store

```rust
pub trait ObservationSource { fn scan(&self, key: &CostKey) -> Result<History>;
                              fn scan_all(&self, each: &mut dyn FnMut(History) -> Result<()>) -> Result<()>; }
pub trait ObservationStore: ObservationSource + Send + Sync {
    fn append(&self, observation: &Observation) -> Result<()>;
    fn save_state(&self, model: &ModelId, state: &[u8]) -> Result<()>;
    fn load_state(&self, model: &ModelId) -> Result<Option<Vec<u8>>>;
}
pub struct Observation { pub key, pub inputs, pub outcome, pub at_ms: u64, pub model: Option<ModelId>, pub predictions: Vec<Predicted> }
pub struct History { pub key, pub summary: Option<Summary>, pub observations: Vec<Observation>, pub torn: u64 }
pub struct Summary { pub key, pub count, pub first_at_ms, pub last_at_ms, pub rows: Moments, pub targets: BTreeMap<Target, Moments> }
pub struct LocalStore;                       // open(root), with_retention(root, Retention { per_key }), root(), retention()
```

The record of AR §7 is `(CostKey, Inputs, Outcome, basis, time, model id)`: the basis of a record is its outcome's `origin` (a run's or warm-up's), which is what makes an estimate `Measured` or `WarmUp`.

### d.5 Drawing from an estimate

```rust
impl Estimate { fn exact(value, unit); fn check(&self, unit) -> Result<(), String>; fn sigma(&self) -> f64;
                fn quantile(&self, q: f64) -> f64; fn sample(&self, rng: &mut Rng) -> f64;
                fn sum(&self, n: f64) -> Estimate; fn scaled(&self, factor: f64) -> Estimate }
impl UnitCost { fn check(&self) -> Result<(), String>; fn weakest(&self) -> (Basis, u32) }
pub struct Rng;                              // new(seed), next_u64(), open01(), normal()
pub fn normal_quantile(p: f64) -> f64; pub const Z95: f64;
pub fn morsel(model, key, inputs, total_rows, memory_per_unit, workers) -> Morsel;   // Morsel { rows, units, bound, duration, peak }
```

### d.6 The simulator

```rust
pub struct Workload { pub tasks: Vec<TaskLoad>, pub edges: Vec<(usize, usize)>, pub checkpoint_interval_seconds: Option<f64> }
pub struct TaskLoad { pub name, pub units: u32, pub unit: UnitCost, pub scaling: Scaling, pub start_seconds,
                      pub checkpoint_seconds, pub checkpoint_bytes, pub stateful: bool }
pub struct Machine { pub workers: u32, pub memory_bytes: f64, pub disk_bytes_per_second: f64 }
pub struct Scaling;                          // linear(), serial(), from_speedups(s2, s4), speedup(k), each(k), cap(workers)
pub struct Simulator;                        // new(&Workload, Machine) -> Result<Simulator>
impl Simulator { fn run(&mut self, &mut dyn Policy) -> Result<&Run>;
                 fn run_sampled(&mut self, &mut dyn Policy, &mut Rng) -> Result<&Run>;
                 fn forecast(&mut self, &mut dyn Policy, draws: u32, seed: u64) -> Result<Forecast> }
pub struct Run { finish, peak_memory, memory_byte_seconds, worker_seconds, disk_bytes, spill_bytes, reload_bytes,
                 idle_worker_seconds, events, tasks: Vec<TaskRun>, critical_path: Vec<usize> }
pub struct TaskRun { start, end, worker_seconds, peak_memory, spill_bytes, max_running }
pub struct Forecast { median: Run, draws, finish, peak_memory, memory_byte_seconds, worker_seconds, disk_bytes,
                      spill_bytes: Spread, tasks: Vec<TaskSpread> }      // Spread { p50, p95 }
pub trait Policy: Send { fn name(&self) -> &str; fn reset(&mut self) {}
                         fn choose(&mut self, view: &View<'_>) -> Option<usize>; }
pub struct View<'a> { now, free_workers, held_bytes, budget_bytes, tasks: &'a [TaskStatus], candidates: &'a [Candidate] }
pub struct Candidate { task, unit, need_bytes, reload_bytes, fits }
pub struct Admission;                        // the first policy: SDD 10 f.3's rule over the DAG
pub const DRAWS: u32 = 200; pub const SEED: u64;
```

`CostError` gains `Infeasible { task, needs, reason }` and `Simulation(reason)`.

KD7c adds the start from a live run (f.13) and the resize:

```rust
pub struct Start { pub tasks: Vec<TaskStart> }                // fresh(tasks)
pub struct TaskStart { pub done: u32, pub running: Vec<f64> /* seconds left */, pub on_disk: Vec<u32> /* finished units */ }
impl Simulator { fn run_from(&mut self, start: &Start, policy: &mut dyn Policy) -> Result<&Run>;
                 fn resize(&mut self, machine: Machine) -> Result<()>; }
```

### d.7 Learning during a run (KD7c; AR §9.6)

```rust
pub struct Online;                           // new(InUse, Arc<dyn ObservationStore>)
impl Online {
    fn predict(&self, key, inputs) -> Prediction;            // drifting targets widened and marked
    fn settle(&self, key, inputs, prediction: Option<&Prediction>, outcome, at_ms) -> Result<()>;   // no IO
    fn flush(&self) -> Result<u64>;                          // append what is kept; outside the lock
    fn save(&self) -> Result<()>;                            // the fitted state, deciding and shadow
    fn pending(&self) -> usize; fn is_drifting(&self, key) -> bool; fn model(&self) -> ModelId;
    fn morsel(&self, key, inputs, total_rows, memory_per_unit, workers) -> Morsel;
    fn report(&self) -> LearningReport;
}
pub struct LearningReport { model, shadow, observed, written, unwritten, store_error, refused, refusal, drifts: Vec<DriftEvent> }
pub struct DriftEvent { key, target, at_unit, units, settled }
pub struct Drift;                            // new(), update(&Estimate, measured) -> Option<Change>, drifting(), statistic(), error(&Estimate, measured)
impl Estimate { fn widened(&self, factor: f64, floor_sigma: f64) -> Estimate }
```

`Online` is shared by a run's threads: `predict` takes a read lock, `settle` a write lock for one update, and nothing holds either across the store.

## e. Data model and formats

### e.1 The local store's layout

```text
<root>/observations/<fnv-1a 64 of CostKey::canonical(), 16 hex>/<seq, 16 hex>.seg
<root>/observations/<...>/summary.json
<root>/observations/<...>/compact.lock
<root>/models/<name>@<version>.state
```

Two keys whose hashes collide share a directory; every record carries its key, and a scan returns its own.

### e.2 The record frame

```text
offset  size  field
0       4     magic   C0 5D E7 0B
4       4     len     u32 LE, the body's bytes
8       4     crc     crc32c (Castagnoli) of bytes [4, 8) and the body
12      len   body    one Observation, JSON
```

0xC0 never occurs in UTF-8, so the magic never occurs in a body, and a reader that meets a record that is cut short or does not check counts one torn record and resumes at the next magic. This is a stand-in for `moruna-record`'s layout (SDD 21), which is on `v1` only (m).

### e.3 The summary file

`{"retired_through": <seq>, "summaries": [Summary, ...]}`, written whole (a temporary file and a rename). A segment at or below `retired_through` is already in the summaries: a scan ignores it and the next compaction deletes it, so a crash between the write and the delete counts nothing twice.

### e.4 Fitted state

`loglinear`'s state is JSON: `{"model": ModelId, "keys": [[CostKey, Fit], ...], "kinds": [[Role, kind, Fit], ...]}`, keys sorted. Every float is written as the integer of its bits, so a load is exact (CM-I3) without asking the JSON parser for exact floats, a feature every crate in a build would share.

## f. Algorithms and policies

### f.1 The default model's form

Per key and per regressed target (duration, worker time, peak and retained memory, state growth, read and spill bytes):

`y = c + exp(θ·φ)`, `φ = [1, log(rows/10⁴), log(bytes_per_row/64), log(columns/8), log(distinct/10³), log(workers)]`

`c` is the overhead (f.2). `θ` is fitted by recursive least squares on `z = log(max(y - c, 0) + floor)` (`floor` is a nanosecond, a byte or a row, so an exact zero has a logarithm), one observation at a time in constant time and memory, with forgetting factor **λ = 0.98** (`loglinear::FORGETTING`): the fit rests on about 1/(1-λ) = 50 recent units; old units weigh a tenth after 115 new ones. Each variance of `P` is held at most at its prior after every step, so a direction no unit excites forgets back to the prior instead of growing without bound. The prior is the declared default's cost of the reference unit for the intercept (variance 100), slope 1 on rows (on distinct keys for state growth) and on row width for byte targets, 0 elsewhere (variance 1). A unit missing a feature is learned at that feature's mean.

The kind's prior is the same fit over every function of the kind, with λ = 1 (`KIND_FORGETTING`): it is their median behaviour, not the last function's.

### f.2 The overhead

Each target keeps its last 32 units. Every 8 units, and at the end of a `fit`, when those units span at least four times in rows, the overhead is chosen from 0, 25, 50, 65, 80, 90 and 95% of the cost at the smallest sizes (within 1.5 times the smallest rows): for each, `a + b·log(rows)` is fitted to `log(y - c)` over the units not dominated by the overhead (`y - c >= y / 4`), and the candidate whose `c + exp(a + b·log(rows))` best explains every kept unit in `log(y + floor)` wins, if it explains them at least 10% better than the current one. When the overhead changes, the line is fitted again from its prior over the kept units. Units dominated by the overhead teach the line nothing: in them, `y - c` is the noise of `c`.

### f.3 The spread

In log space the p95 is `p50 · exp(t · sd)` (with the floor added and taken away), where

`sd² = s_full² + share² · (s_line² · φᵀPφ + Σ θᵢ² var(missing featureᵢ) + (0.25 · outside)²)`

- `s_full²`: the forgetting-weighted mean square of each unit's error against the prediction made before it (a prior of 0.3 worth one unit);
- `s_line²`: the same for the line's standardised residuals;
- `share`: the part of p50 that is the line's, not the overhead's;
- `outside`: how far, in log units and beyond a margin of a tenth, the present features lie outside the key's measured range; any is marked `extrapolated`;
- `t`: Student's one-sided 95% quantile at the weight of the residuals, so a few observations give a wide estimate.

The spread is capped at e^9.2 (about 10,000 times).

### f.4 Ratios and fallbacks

Selectivity (rows out per row in) and output bytes per row are kept as ratios: the median and p95 of the last 128 values, not regressed on size. Rows out is the unit's rows times selectivity; a selectivity in the inputs (measured in the run) answers as `Measured`, widened by the history's p95 over median. Bytes out is rows out times output bytes per row. An unknown row count takes the history's typical rows.

The declared default (`Defaults`) reads the kernel's declared figures in the inputs (per-batch, per-row and per-byte time; peak and retained memory per byte; state per key for a stateful kernel; speed-up at 2 and 4 workers) and falls back to constants: a millisecond per batch, 0.1 µs per row, a peak of twice the input, the output held, selectivity 1, 64-byte rows; a p95 of four times the median, sixteen when the size is unknown. A source reads its bytes; nothing spills.

### f.5 Retention and compaction

A key's records go to its newest segment, which is closed at a quarter of the retention. When a directory holds a retention and a segment, `append` takes `compact.lock` (`create_new`; a lock older than a minute was left by a dead process and is taken), recounts the segments from disk, folds the oldest whole segments into the summary while the rest still hold a retention, writes the summary, and deletes them. A lock held elsewhere skips compaction: the other process compacts, and a scan folds anything past the retention into the returned summary either way.

### f.6 Drawing from an estimate

An estimate is read as the log-normal through its median and its 95th percentile: `sigma = ln((p95 + floor) / (p50 + floor)) / Z95` and the quantile at `q` is `(p50 + floor) · exp(sigma · z(q)) - floor`, `z` the standard normal's quantile (Acklam's approximation, relative error below 1.2e-9), the floor the target unit's (f.1). A draw is the quantile at a uniform draw from SplitMix64 on the open interval. The sum of `n` independent units is `n · p50` at the median and `n · p50 + sqrt(n) · (p95 - p50)` at the 95th percentile.

### f.7 The marginal cost

`marginal(key, inputs, target, rows)` predicts at `rows` and at `2 · rows` with the same bytes per row; the per-row cost is the difference over `rows` (never negative), the fixed part the first prediction less `rows` times it (never negative), at p50 and at p95. The overhead `LogLinear` fits (f.2) is reachable through it like any model's.

### f.8 The simulator

- **Units and buffers.** Task `t` with `n` units: unit `b` covers rows `[b/n, (b+1)/n)`; it needs every unit of each producer of `m` units that overlaps (units `floor(b·m/n)` to `ceil((b+1)·m/n) - 1`), finished. A task's units start in order. A finished unit's output (its `bytes_out` median, when the task has consumers) is a buffer in memory until every consumer unit that overlaps it has finished.
- **Events.** A unit finishes, a spill or a read-back completes, a checkpoint interval passes. Between events every rate is constant, so time jumps to the earliest completion; completions within a billionth of an item's work are the same event.
- **Admission.** At each event, while a worker is free: the candidates are each task's next unit whose inputs have finished and whose running units are below its cap; the policy chooses one; a unit that fits (`held + need + inputs on disk <= budget`, `need = max(peak p95, out + state)`) starts and holds a worker; one that does not is claimed, spills are started for it, and nothing else is admitted until it starts.
- **Spill.** Victims are buffers in memory pinned by no admitted unit and not the claimed unit's inputs; the one whose next use is furthest goes first (its consumers' units still to start before the first that needs it, times their unit median), ties to the larger, then the later. Writing and reading back cost the same per byte on one disk, so the reload cost weighs every byte alike. A buffer spills at the disk's bandwidth share; its memory is freed when the write completes. A unit whose input is on disk reads it back before it computes (its worker held, its rate zero).
- **Contention.** A computing unit demands `worker_time / duration` workers and `(read_bytes + spill_bytes) / duration` bytes a second; a transfer demands the whole bandwidth. With summed demand `D` over capacity `C`, everything using the resource runs at `C / D`; a unit's rate is the least of its resources' factors times its kind's `s(k)/k`.
- **Scaling.** `s(1) = 1`, `s(2)` and `s(4)` measured (Amdahl's law through one gives the other; neither: `s(k) = k`), straight lines between, Amdahl's law through `s(4)` past 4; the cap is the largest `k` at which each extra unit adds at least `MIN_MARGINAL_GAIN` (0.05); a stateful task runs one unit at a time and holds its state, which grows by `state_growth` a unit, until its last unit ends.
- **Once per process and per checkpoint.** A task's first unit runs its start-up (wall time, not slowed by contention, holding its worker and its reservation) before its body; until the start-up ends the task has no candidate, so no other unit of it starts and its workers take other tasks' units. Every `checkpoint_interval_seconds`, each task under way with a checkpoint cost owes one; its next unit to start adds `max(checkpoint_seconds, checkpoint_bytes / bandwidth)` and writes the bytes through the disk. A task with no unit left to start does not pay what it owes.
- **Critical path.** Each unit records what it waited for last: the input that finished at its start, else the unit whose end freed what it needed. From the last unit to finish back through those, as tasks, in order.

### f.9 The forecast

The medians' run is the schedule. Then `draws` runs (`DRAWS`, 200) draw each unit's duration as it starts from one `Rng` seeded with `seed` (`SEED`), so a forecast is reproducible. Each figure's p50 and p95 are its nearest-rank quantiles over the draws (with none, the medians' run).

### f.10 Morsel sizing

The rows the overhead asks for are `fixed · (1 - s) / (s · per_row)` of the duration's p50 line, `s = OVERHEAD_SHARE` (0.05), held to `MIN_MORSEL_ROWS` (1,024) and `MAX_MORSEL_ROWS` (2²²); the memory allows `(memory_per_unit - fixed) / per_row` of the peak's p95 line; the input allows its rows; the workers allow the rows over the workers, rounded up. The smallest wins and names its bound. The lines are read at `PROBE_ROWS` (65,536, or the input's rows when fewer), then again at the size chosen.

### f.11 Drift (KD7c; AR §9.6)

For each measured target of a settled unit, `z = (ln(y + floor) - ln(p50 + floor)) / max(sigma, 0.05)`, where `sigma` is the deciding prediction's log-normal spread (f.6), halved back by `DRIFT_WIDEN` when the prediction was widened, so a key is judged against the model's own spread and not the learner's widening of it. Two one-sided sums move by the excess beyond the 95th percentile, held to one percentile's worth a unit: `up = max(0, up + min(z - Z95, Z95))`, `down` the same of `-z`. Either past `2.5 · Z95` marks the key and target drifting: at least three units at the most a unit adds. While drifting, each unit with `|z| <= Z95` counts toward settling and any other resets the count; `SETTLE_UNITS` (8) in a row settle it and clear both sums. A value that is not finite moves nothing.

A drifting target's estimate is widened: its sigma is the larger of its own and `DRIFT_SIGMA_FLOOR` (0.1), times `DRIFT_WIDEN` (2), the median kept, the p95's logarithm above the median held to 20; it is marked `drifting`. When a key's first target drifts the deciding model is told `set_drifting(key, true)`, and `set_drifting(key, false)` when its last settles. `loglinear` then forgets the level of each of the key's fits at once (the intercept's variance back to its prior, which adding to a diagonal keeps positive semi-definite) and observes the key at `DRIFT_FORGETTING` (0.9, about ten units) until it settles; the kind's prior keeps its own forgetting, since one function drifting is not the kind changing.

### f.12 Learning off the hot path (KD7c)

`Online::settle` refuses an outcome that is not a measurement (CM-I6's check) with its reason, counted; otherwise `InUse::settle` updates both models, the drift is judged (f.11) against the prediction the unit was chosen on, and the observation is kept. At most `PENDING_LIMIT` (65,536) wait: past it an observation is counted unwritten with the reason, since a run that never flushes would otherwise grow without bound. `flush` takes what is kept under the lock and appends it outside it; the first refusal stops the flush, the rest are counted unwritten, and the first reason is kept. `save` makes each model's state under the read lock and writes it outside; a failure is kept as the store's reason when it is the first. Only one flush may be active: a concurrent call is refused immediately with `CostError::Invalid` naming the active flush, without touching pending observations or accounting. An atomic guard is released on every return and on unwinding; no state lock is held across store IO. `abandon` counts the pending and in-flight observations unwritten, preserving the first reason, and a waiting append returning afterward cannot count them again.

### f.13 The start from a live run and the resize (KD7c; AR §9.4 step 3, G12)

`run_from` resets, then, after the checks of CM-I23: marks each task's first `done` units finished at time zero (its prefix, `next_unit = done + running`), holds a stateful task's state of its finished units, makes a buffer of each finished output whose remaining uses (consumer units not yet finished that overlap it) are not zero, in memory or on disk as the start says, pinned by the running consumer units that overlap it, and starts each running unit on a worker of its own with the work it has left, its reservation held and no start-up. Then it plays on as `run` does. `resize` checks the machine as `new` does and the memory against every task's need (f.8), and only then sets each task's cap and the machine.

## g. Concurrency within the component

A `LocalStore` is `Send + Sync`: appends from threads are serialised by one mutex; appends from processes land in one segment through `O_APPEND`, one write per record; compaction is one process at a time under the directory's lock. A scan that finds a segment it listed already retired starts again (three times). A `CostModel` is `Send + Sync` and is mutated only through `&mut`. A run shares its models through `Online` (d.7): an `RwLock` over the models, the drift sums and the kept observations, read for predictions and written for one constant-time update per unit, never held across the store.

## h. Behaviour

A key with no history answers from the kind's prior, then the default, per target. `fit` over an empty store leaves the model with no history and reports zero. A torn tail is never truncated: the next append lands after it, and both are read. Fitted state that does not load is refitted from the records, and the readiness says why.

## i. Configuration

The model in use is the job's `cost.model` (`CostSetting`), else the embedder's default; `cost.shadow` names a model in shadow. The retention is `Retention::per_key` (10,000). The default model's figures are `Defaults`. Every other number is a named constant: λ, the window, the overhead shares, the margins, the spread cap.

## j. Observability

`Readiness` says how each model became ready. `History::torn` and `FitReport::torn` count torn records. `compare` reports, per model and per target, the median absolute log error and the p95 coverage on the units both predicted.

## k. Tests

Test names are the ids' lowercase with a slug.

**CM-T1 dependencies_are_the_four.** The manifest's dependency tables name `moruna-job`, `serde`, `serde_json` and `thiserror` and nothing else. CM-I1.

**CM-T2 inputs_are_a_versioned_record.** Every field of AR §3 is written and read back; a record that leaves them out reads as unknown at this version.

**CM-T3 estimates_are_distributions.** At every basis, over random outcomes (zeros among them) and odd inputs (zero rows, zero workers, 10³⁰ rows), every estimate is finite with `p95 >= p50 >= 0` and its target's unit. CM-I2.

**CM-T4 accuracy_on_synthetic_truth.** Five families with 10% log-normal noise (linear, n log n, power 1.5, power 0.7, constant plus linear), 400 units on 10³ to 10⁶ rows, 1,000 held out: the median of |p50 / measured − 1| is under 12% and the p95 covers between 90% and 99% of held-out units. CM-I2.

**CM-T5 extrapolation_is_marked_and_wider.** Fitted on 10³ to 10⁵ rows, 10⁷ is marked and its spread is more than 1.5 times the spread inside; a column count that moves past the only one seen is marked; a feature never seen is not. CM-I5.

**CM-T6 forgetting_follows_a_change.** A function three times slower than its 300-unit history is followed: the median error over 50 probes is under 15% within 150 units and under 10% after them. CM-I11.

**CM-T7 fallbacks_answer_in_order.** No history: every target `Default`; warm-up history: `WarmUp`; another function of the kind: `Prior`; another kind: `Default`; one unit from a run: `Measured`; a target never measured falls through per target; the declared default reads the kernel's figures. CM-I4.

**CM-T8 missing_inputs_widen.** A unit with its rows unknown is wider than with them known. CM-I5.

**CM-T9 ratios_are_ratios.** Selectivity uniform on 0.1 to 0.3 gives a median of 0.2 and a p95 of 0.29 at every size, 10⁷ included; a measured selectivity answers as measured; bytes out is rows out times the bytes per row.

**CM-T10 save_and_load_round_trip.** Predictions after a load equal those before, to the bit; saving again gives the same bytes. CM-I3.

**CM-T11 a_mismatched_id_is_refused.** State of another version or another model is `WrongModel` naming both; bytes that are not state are `State`; the model is unchanged. CM-I3.

**CM-T12 appends_survive_a_crash.** A tail cut at any byte, or extended by zeros, is one torn record; the records before it and the ones appended after it by another process are read; a fit reports the count. CM-I6.

**CM-T13 retention_holds.** With a retention of 40, after 200 appends a scan returns the last 40 and a summary of 160, opened again it says the same, the disk holds at most a retention and a segment, and another key in the store is untouched. CM-I7.

**CM-T14 fitted_state_beside_the_records.** State is kept per id, replaced by a later save, never returned for another id; a name that is not a name is refused. CM-I3.

**CM-T15 registry_by_name.** Register, a taken name, a bad name, an unknown name (refused naming the registered ones), an adapter whose model carries another name. CM-I8.

**CM-T16 the_swap.** A test model that predicts each target's median is registered in the test, chosen by `{"model": "constant"}`, fitted over a store `loglinear` filled through 300 settled units, and used through the same calls; opened again it loads its state; `loglinear`'s corrupt state is refitted with the reason. CM-I8.

**CM-T17 shadow_mode.** `loglinear` deciding and the test model in shadow: the deciding prediction is `loglinear`'s, every record carries both predictions in order, and `compare` scores both on the same units, `loglinear` better. CM-I9.

**CM-T18 predict_is_cheap.** In a model holding 100 keys, a warm key's prediction takes under 2 µs, asserted in an optimised build or on the reference host and reported otherwise. CM-I10.

**CM-T19 overhead_is_the_fixed_term.** A function with a fixed 10 ms per unit keeps an overhead between half of it and a quarter more; a pure power law keeps none. f.2.

**CM-T20 many_writers.** Four threads over two stores on one directory append 400 records; all are read back, none torn. CM-I6.

**CM-T21 refusals.** Inputs of another version, a negative amount and a bad kind are refused at `append`; a whole frame whose body is not an observation is `Unreadable`, not skipped. CM-I6.

CM-T22 to CM-T31 are `tests/sim.rs`.

**CM-T22 hand_computed_schedules.** A chain (3.5 s), a pipelined chain on one worker (3 s), a fan-out (4 s, critical path through the slower arm), two units each through a fan-out on two workers (3 s), a fan-in (3 s), a diamond on two workers (6 s) and on one (8 s), start-up once per task (4.75 s), and checkpoints at 1.5 s intervals (4.2 s, 2,000 bytes): each finish within a microsecond. CM-I12.

**CM-T23 memory_is_a_hard_limit.** Four 100-byte outputs read in order under a 250-byte budget, a policy taking the producer first: 200 bytes spilled and 200 read back (a1, then a2, the furthest next uses), finish 12 s, 2 s idle with work ready, peak within the budget; the admission rule spills nothing (8 s); a unit with a 300-byte p95 peak and a task whose state passes the budget are `Infeasible` by name. CM-I13, CM-I14.

**CM-T24 contention.** Two units reading 1,000 bytes in a second alone finish together at 2 s on a 1,000 B/s disk, at 4/3 s on 1,500 and at 1 s on 4,000; two units each using two workers' time on two workers take 2 s; a kind with `s(2) = 1.5` runs two units in 4/3 s; a serial kind and a stateful one run one at a time. CM-I15.

**CM-T25 monte_carlo.** One unit with p50 1 s and p95 2 s, 4,000 draws: the finish p50 and p95 within 5% of the log-normal's; two such units side by side: within 5% of the maximum's (`F(x)²`); the same seed, the same forecast; no draws, the medians'. CM-I16.

**CM-T26 simulation_speed** (ignored; a measurement for the reference host, in an optimised build: `cargo test -p moruna-cost --release --test sim -- --ignored --nocapture cm_t26`). A 50-task DAG (five sources, each later task taking the one before it and the one five before) on 16 workers, over 10,000 batches (200 a task) and over 10,000 batches a task, and a forecast of the first with 200 draws; it reports the median time of a run and per unit. CM-I18.

**CM-T27 morsel_sizing.** A linear model's overhead and per-row cost come back from `marginal`; 1e-4 s a batch and 1e-7 s a row give 19,000 rows (bound: overhead), a hundred times the overhead a bigger morsel, a second a batch the largest; 640,100 bytes for a unit of 128 bytes a row and one fixed give 5,000 rows (bound: memory); four workers over 40,000 rows give 10,000 (bound: workers); 500 rows on one worker give one morsel (bound: rows); no overhead gives the smallest. CM-I17.

**CM-T28 sampling_an_estimate.** The quantiles at 0.5 and 0.95 are p50 and p95; the median of 20,001 draws within 5% of p50; an exact estimate answers its value at every quantile; the sum of four; `check` refuses another unit, an unordered, a negative and a non-finite estimate, and `UnitCost::check` names the field. CM-I16.

**CM-T29 refusals.** No worker, a memory budget or bandwidth that is not a positive amount, a checkpoint interval of zero, an edge to a task that is not there, a task taking its own output, a cycle, a task with no unit, an unordered estimate and a negative start-up are refused by what they are; a policy that never chooses and one that chooses past the candidates stop the run by name; a duplicated edge is one edge. CM-I12.

**CM-T30 the_hard_rule_holds_on_random_workloads.** 400 random workloads (2 to 7 tasks, random edges forward, 1 to 6 units, random durations with spread, peaks, outputs, reads, state, scaling, start-up and checkpoints) on random machines and budgets under a random policy: memory held never passes the budget, every task's span lies within the finish, the same seed gives the same run, a drawn run holds the budget too, and each workload finishes or is `Infeasible` by name (more than 200 finish, more than 10 spill); a unit that fits alone but not beside another task's pinned state is `Infeasible` when reached, and runs when it goes first. CM-I13, CM-I14.

**CM-T31 units_wait_for_their_start_up.** Eight 0.1 s units with a 2 s cold start beside four 0.5 s units on four workers: the first unit ends at 2.1, the other task at 1.0 on the workers the cold task cannot use, three more at 2.1 and the last four at 2.2 (2.1 if they had not waited); 4.8 worker-seconds. A cold task of two units feeding a consumer on two workers: the consumer starts at 2.1, the finish 2.2. CM-I19.

**CM-T38 a_simulation_starts_from_a_live_run** (`tests/sim.rs`). A chain of four one-second units a stage on one worker, a's first three finished and its fourth under way with half a second left, b's first finished: 3.5 s to go, the critical path through both, 3.5 worker-seconds; the same simulator then runs fresh (8 s) and from a fresh start alike. An output b still needs on disk is read back (100 bytes at 100 a second) before b runs, 3 s in all; everything finished is 0 s; a stateful task's grown state is held from the snapshot. Each malformed start is refused saying what is wrong. CM-I23.

**CM-T39 a_simulator_follows_a_resize** (`tests/sim.rs`). Eight one-second units finish at 4 s on two workers, 2 s after a resize to four, 4 s back at two; a stateful task stays one at a time on eight; memory shrunk under a unit's need is `Infeasible` naming the task and leaves the machine as it was; a machine with no worker is refused. CM-I24.

**CM-T40 a_run_learns_online_and_writes_off_the_hot_path** (`tests/online.rs`). 300 units through `loglinear` deciding and `loglinear` in shadow: the median error of the deciding prediction over the last 50 is under 10%; nothing is in the store before `flush`, which writes all 300 in order, each with both predictions, deciding first, and a second flush writes none; the saved state predicts what the deciding model does; a steady key never drifts; an outcome that is not a measurement is refused and counted. CM-I25.

**CM-T41 drift_is_found_widens_forgets_and_settles** (`tests/online.rs`). A key steady for 200 units turns three times slower: it is marked within three to eight units (three, at the reference seed), its predictions are marked and at least twice as wide in sigma, and twelve to twenty units after the change its median error is under 15% (6%) where a model told nothing is over 25% (49%); it settles, the mark goes, and the report has one spell, settled, begun after unit 200, of at least eight units. `loglinear` itself marks and clears a key as told, keeps no mark across a load or a fit, and takes a mark for a key it has not seen. CM-I26.

**CM-T42 a_store_that_refuses_is_reported** (`tests/online.rs`). A store that takes four appends: a flush of ten writes four and fails with the store's error; the report counts four written and six unwritten with the reason; the model learned all ten. A store that refuses the fitted state fails `save`; the report keeps the first reason, which on a store refusing nothing else is the save's. CM-I25.

**CM-T43 a_store_that_does_not_keep_up_is_counted** (`tests/online.rs`). With room for four observations, ten completed units keep four and count six unwritten with the limit named. Hold a flush at a controlled store gate, complete three more units and abandon the store: written plus unwritten equals all thirteen units exactly once, and later flushes and saves write nothing. The gate signals arrival without polling. CM-I25.

**CM-T44 concurrent_flushes_preserve_order_and_accounting** (`tests/online.rs`). Hold the first two-observation flush inside a controlled store, then complete two more observations. A concurrent flush refuses without waiting on the store or taking those pending observations. After releasing the first flush, a later flush writes the second batch: four observations stored once in completion order, four written and none unwritten. Release and join both callers even when the regression fails. CM-I25.

## l. Implementation notes for the agent

`predict` is in the scheduler's decision loop: it looks the key and the kind up without allocating (the kind map is keyed by `String` and read by `&str`), computes the features once, and does one 6 × 6 quadratic form per target. Keep it that way; CM-T18 measures it. Do not add a linear-algebra crate (CM-I1).

## m. Open items

**CM-M1. The record layout.** AR §7 asks for the record layout of KB4 (`moruna-record`, SDD 21). It is on `v1` only; this crate frames its own records (e.2). KB4's carry-over to `main` replaces the frame with `moruna-record`'s and keeps the body.

## p. Warm-up (KD6b, `moruna-runtime`)

AR §6, in `moruna check`, the local half of registering a function. It lives in `moruna-runtime` (AR §11): it needs Arrow, the arena and the kernel, which this crate refuses (CM-I1). It reaches the model and the store through `CostModel` and `ObservationStore` only.

### p.1 The cost configuration

```rust
pub enum StoreSetting { Directory(PathBuf), Embedder(Arc<dyn ObservationStore>) }
pub struct CostSettings { pub store: StoreSetting, pub models: CostModels, pub model: String }   // directory(dir), embedder(store)
impl CostSettings { pub fn open(self) -> moruna_kernel::Result<Cost>; }
pub struct Cost;     // model() -> &dyn CostModel, model_mut(), model_id(), store(), models(), readiness(), refit()
pub fn default_store_dir() -> Option<PathBuf>;   // ~/.moruna/observations
```

The runtime owns the configuration: the store adapter (the local directory, or the embedder's), the model chosen by name from the registry, and its fitted state loaded from the store (else fitted from the store's observations and saved; state that does not load is refitted, and `readiness` says why). `open` refuses, with the cause, a store that cannot be opened or read and a model the registry does not have. `Cost::model` is what a planner is given (`moruna_plan::PlanInputs::with_model`, KD6c). It is configured by settings (`CostSettings`, the command line's `--store` and `--cost-model`, the Python keywords), never by the environment.

### p.2 Warm-up's interface

```rust
pub struct WarmUp { pub cost: Cost, pub budget: Duration, pub statistics: Option<Data>, pub settings: Digest,
                   pub maker: Option<Arc<dyn Maker>>, pub outside: Option<Arc<dyn OutsideMemory>> }
pub struct CountingAllocator;      // a program's #[global_allocator]: counts the heap while warm-up runs
pub trait Maker: Send + Sync { fn make(&self) -> moruna_kernel::Result<Arc<dyn Kernel>>; }
impl WarmUp { pub fn new(cost: Cost) -> WarmUp; pub fn configured(store, budget_seconds, statistics_json, model) -> Result<WarmUp>; }
CheckOptions { .., pub warm_up: Option<WarmUp>, .. }      // replaces profiles_dir
CheckReport  { .., pub gil: &'static str, pub warm_up: Option<Result<WarmUpReport, WarmUpFailure>>, .. }   // replaces profile
pub struct WarmUpReport { model, key, cost_per_10000_rows: UnitCost, kernel: moruna_cost::Kernel, skew, selectivity, progress, consumed, observations, fit, notes }
```

The cost key is (`kernel`, the check's kind with `.wasm` and `.pex` as `package`, the kernel's fingerprint, `settings`, the input schema's hash). `settings` is the SHA-256 of the settings' JSON with sorted keys: `{}` for a kernel checked in process, a package's arguments for a package. A package's `Maker` opens the file again under its runtime; a kernel checked in process has none and is already made.

### p.3 Invariants

**CM-I27. Warm-up is behind the ports.** It calls `CostModel` and `ObservationStore` only, through `Cost`; the model it fits is the setting's, and a model registered by an embedder is fitted and answers with no change outside its adapter.

**CM-I28. One seed, one data.** Every generated value is a function of the seed, the batch's stream and the column's position; the arithmetic turning statistics into draws uses no transcendental function, so a seed gives the same bytes on any host. What a batch drew is measured from the values (null fraction, lengths, and for the key columns the exact distinct count and skew) and is what its observation records.

**CM-I29. The statistics are honoured or refused.** A column's null fraction, distinct count and lengths are followed when given and are the type's defaults otherwise. Statistics for a column the input lacks, twice for one, a null fraction outside 0 to 1 or above 0 for a column declared non-null, a length that is not an amount, a distinct count of 0 or a list marked as a key refuse warm-up, naming the column, before anything runs.

**CM-I30. Value-dependent quantities are marked.** Skew sensitivity and selectivity are reported with `value_dependent: true`; every estimate of the cost names its basis; every observation's origin is `warm_up`.

**CM-I31. Warm-up runs where a run runs.** The kernel is the object a run builds (a package under its own runtime); each unit lands its batch in the check's arena, cuts it into one part per worker, and applies each part on its own thread to its own instance (one per worker, up to a stateful kind's pool), calling `init` and `finish` as the scheduler does; worker time is the threads' CPU time, as the scheduler measures it.

**CM-I36. A unit's peak memory is a high-water mark over the call.** Every mark is reset when the call starts and read when it ends, never sampled before and after: the arena's (exact), the process's (macOS `phys_footprint`'s interval peak, Linux `VmHWM` after `clear_refs`, and Moruna's counting allocator's when the program installed it, the larger), and a packaged child's cgroup `memory.peak`. A spike the call allocates and frees is in the peak. A required process or child mark that cannot be reset/read is named in the notes and the unit's PeakMemory is absent, never replaced by sampled or partial memory. Overlapping warm-up intervals invalidate both process-wide marks; they continue measuring other quantities without training PeakMemory. Counting remains armed until the last interval leaves.

**CM-I37. Decisions are separate from measurement.** Warm-up's decisions (which units run, the residual stop, each phase's deadline, the facts and ratios) take every measurement and every second from a `Bench`; the decisions are tested over a bench of exact values, and the real bench is tested only for properties that hold by a wide margin.

**CM-I32. The budget holds.** No unit starts after its phase's deadline or when the model predicts it would finish past it; each stop is recorded with its phase, the unit it did not run, the time spent and the prediction.

**CM-I33. Every unit is stored and the model refitted.** Every measured unit's inputs (with the kernel facts warm-up measured) and outcome are appended, with the prediction made before the unit was observed; then the model is fitted from the whole store and its state saved; the report's cost per 10,000 rows is that fitted model's.

**CM-I34. A warm-up that cannot run says why.** A kernel error, a store that refuses a record, statistics that do not apply or a model that cannot be made is a `WarmUpFailure` with its cause and how far it got, and the check's exit code is 2.

**CM-I35. What warm-up consumed is reported.** Wall seconds (the check's own batches excluded), the process's CPU seconds and peak memory where the operating system gives them (otherwise absent, never zero), the kernel's worker seconds, the arena it added and the bytes it handed the store.

### p.4 Algorithms and policies

1. **Data** (`data.rs`). Key columns are those the statistics mark; when none is marked, every column but a list is a key (the kernel's keys are not declared, so all are varied). A key column's domain is its distinct count, else the batch's rows, capped at the type's cardinality; uniform keys draw from it evenly, skewed keys put 80% of the rows on the hottest one in 500 of the keys a batch can draw (the domain, or the rows when fewer), so that the top 1% of the keys a skewed batch holds carry most of its rows (one in 100 of a domain as large as the rows left the measured skew near 0.15). A column with a domain makes each index one value (a string or binary value starts with the index's fixed-width digits, and its length and filler are the index's). Lengths with a mean and a 95th percentile are a point at zero, a uniform body and 5% at the percentile, chosen to meet both (a mean below 5% of the percentile comes out at 5%); with a mean alone, uniform to twice it. A list's statistics are in bytes and become items at the item's width (8 for a variable one). Nullable columns are one in eight null unless the statistics say; defaults are 0 to 16 bytes for strings and binary, 0 to 3 items for lists.
2. **Sizes.** 1,000, 10,000, 100,000 and 1,000,000 rows that fit (six times a batch within half the memory the machine has left), three repetitions, uniform and, with keys, skewed. From the third size on, the residual is the median `|ln(predicted/measured)|` over the size's units of wall time and of peak memory, each predicted before it was observed; at or below 0.10 for both, the larger sizes are not run.
3. **Start-up**, before the check's batches so it is the kernel's first invocation in this process: the making (a package's opening), `init` and a one-row batch through its output, cold, then again, warm. A kernel checked in process is already made; its start-up is `init` and the first batch. Start-up is a kernel fact, not a unit (CM-O1).
4. **Scaling**: the largest size the sizes measured, at 1, 2 and 4 workers, three repetitions; a stateful kind's pool caps its instances, and a worker count it caps is not run again. Speed-up is the median wall time at one worker over the median at two or four.
5. **State**: a stateful kind at 10, 100, …, 10⁶ distinct keys up to the largest size measured, a fresh instance each; state growth is the footprint's growth, state per key the least-squares slope over the counts. When the resume policy is `checkpoint`, each instance's checkpoint is taken and timed before it finishes, and the largest state's is reported; a kind whose checkpoint returns nothing fails warm-up.
6. **Budget**: 60 seconds unless set. The phases share it by deadline: start-up within the whole, the sizes within half, scaling within three quarters, state within the whole, so a slow kernel still has its scaling and state measured at the largest size reached.
7. **Facts**: time per batch and per row (the least-squares line over the uniform sizes, through the origin when its intercept would be negative), peak and retained memory per input byte at the largest size, worker seconds per wall second at one worker, the speed-ups, state per key, start-up and checkpoint.
8. **Peak memory** (CM-I36). Before the call, after the batch is landed: the arena's mark (`Arena::reset_host_peak`), the counting allocator's, the operating system's mark of the process and the child's `memory.peak` are reset. After it, the peak is `max(arena + process + child, input + pools)`, where `arena` is the arena's mark above what it held before the batch was landed (so the input is in it), `process` the larger of the operating system's and the counting allocator's marks above their reset, `child` the child's mark above what it held, and `pools` what the kernel's NumPy, pyarrow and wasm-linear memory held at once (contracts d.13). The arena is pre-faulted, so its use never moves the process's mark, and a child is another process: the three are disjoint, and their sum bounds the call's peak from above, never below. The pools are a view of bytes the others count, taken when larger. Retained memory is the output's allocation outside the arena and the arena's growth past the input.

   What each mark misses, said here and in a unit's notes: memory a C library's allocator frees and reuses inside the process without returning it to the system moves neither the operating system's mark (macOS keeps freed large blocks resident; glibc with Moruna's settings unmaps every buffer of 256 KiB or more) nor, when it is not Rust's, the counting allocator's. The Python extension and the test programs install the counting allocator; a library cannot install one for its embedder. Linux's `VmHWM` counts file-backed pages too. The cgroup's `memory.peak` is reset through its open file (Linux 6.12 and later); an older kernel refuses the reset, so the unit's total PeakMemory is absent and the notes name the unavailable child mark. The process-wide mark/allocator interval coordinator invalidates overlapping intervals rather than serializing arbitrary kernel calls or labelling mixed measurements exact; no sampled replacement is used. Whole-process consumed peak is reported only when its source is exact.

### p.5 Tests

**CM-T68 unavailable_or_overlapping_marks_do_not_train_peak.** A real warm-up unit with an unavailable process mark, or a declared child without a resettable mark, still records its duration/output but no PeakMemory. Controlled interval entry/exit invalidates both overlapping measurements even after one leaves; the next independent interval is valid. Existing true freed-spike tests retain their full assertions. CM-I36.


**CM-T45 data_follows_the_statistics_uniform_and_skewed.** A key marked with 500 distinct values: every unit's key column records it as the only key, within the domain; skewed units' skew is above 0.7 and uniform ones' below 0.05; skew and selectivity are marked value-dependent; every target of the cost names its basis. CM-I29, CM-I30. (`data.rs` unit tests: lengths meet their mean and percentile, every scalar type generates as a key with distinct indexes distinct values, refusals name the column.)

**CM-T46 one_seed_one_data.** Two warm-ups with one seed record the same inputs for all twelve units up to 10,000 rows; another seed's batches differ (`data.rs`). CM-I28.

**CM-T47 the_sizes_stop_when_the_residual_is_small_and_not_otherwise.** (fake bench) An exact test model: the sizes stop after 100,000 rows with a residual of 0, the million-row size not run, three uniform and three skewed units at each size run; a model twice wrong has a residual of ln 2 at both judged sizes and every size runs. p.4 item 2.

**CM-T48 each_phase_stops_at_its_deadline_and_says_where.** (fake bench) 10 µs a row and a budget of 4 s: the sizes stop at 1.72 s before a second unit of 100,000 rows (their deadline 2 s), scaling runs one unit at 100,000 rows and stops at 2.72 s (deadline 3 s), every stop with its phase, unit, time and prediction; a budget of zero stops start-up and the sizes and runs nothing. CM-I32.

**CM-T49 start_up_records_the_cold_and_the_warm_start.** (fake bench) The bench's cold and warm starts are the facts; a kernel made here says so. Real: a kernel whose first call sleeps 80 ms starts cold in at least that and warm in less; with a `Maker`, each start is a making. AR §6.3.

**CM-T50 the_speed_ups_are_the_ratios_of_the_medians.** (fake bench) A kernel whose time goes as workers^-0.5: three units at each of 1, 2 and 4 workers at 100,000 rows, speed-ups of √2 and 2, a worker second per wall second, time per row and memory per byte the truth's. Real: four workers are not slower than one for a kernel that sleeps per row. CM-I31.

**CM-T51 the_state_pass_fits_growth_per_key_and_times_the_checkpoint.** (fake bench) 16 bytes a key: counts 10 to 100,000, state per key 16, the largest state's checkpoint, a pool of two running four workers as two and saying so; a kind that keeps no checkpoint times none and says so. Real: a kind holding 16 bytes a key, under a budget it cannot reach, fits within a byte of 16 from its footprint. AR §6.5.

**CM-T52 skew_sensitivity_is_the_ratio_of_skewed_to_uniform.** (fake bench) Skewed keys three times as slow: a duration ratio of 3, a peak ratio of 1, the measured skews, value-dependent. Real: a kernel whose time grows with the pairs of rows sharing a key is more than 1.5 times as slow on skewed keys. G11.

**CM-T53 the_swap.** A test model registered in the test and chosen by name is fitted, answers the cost (its own spread), saves its state beside the records and names itself in every record; `loglinear` opened on the same store fits from what it left, as `warm_up`. CM-I27, AR §12.3.

**CM-T54 observations_land_in_the_store_and_refit_the_model.** Every unit is in the store under the key, with the kernel facts and its prediction; the fit read them all; the cost's basis is `warm_up`; a second warm-up opens the saved state and refits over both (real); the same over the fake bench, with the exact cost of the test model. CM-I33.

**CM-T55 what_warm_up_consumed_is_reported.** (real, no timing bound) Wall, worker and kernel worker seconds, peak memory, the arena and the store's bytes, in the report, the JSON and the summary. CM-I35.

**CM-T56 a_warm_up_that_cannot_run_says_why.** A kernel that fails at 100,000 rows: the declaration agreed, the exit is 2, the failure names the rows and the kernel's error and records the sizes done (real, and over the fake bench); statistics for an absent column refuse before anything is stored. CM-I34, CM-I29.

**CM-T57 the_cost_configuration_opens_or_refuses.** (`cost.rs`) An empty store is fitted and saved, saved state is loaded, corrupt state is refitted saying why, `refit` replaces the model; an unknown model and a store that cannot be opened are refused naming the cause. p.1.

**CM-T59 the_marks.** (`moruna-arena`) The arena's host high-water mark holds a freed spike, resets to what is in use, and is one mark across a grown region. (`peak.rs`) The process's mark holds a spike mapped and unmapped since its reset; the counting allocator's mark holds a freed spike and counts nothing disarmed; `/proc/self/status` fields parse in bytes and refuse otherwise. (`moruna-sandbox`, Linux) A stage cgroup's `memory.peak` refuses a read before a reset and reads through the file it reset. CM-I36.

**CM-T60 a_spike_freed_inside_the_call_is_in_the_peak.** Real kernels that allocate, write and free 64 MiB on the heap, or 32 MiB in the arena, inside every call: every unit's recorded peak is at least the spike (with the input, for the arena's). CM-I36.

**CM-T58 python_check_returns_the_fitted_cost.** (`python/tests/test_check.py`) `moruna.check` returns each kernel's cost per 10,000 rows with its basis, what warm-up consumed, value-dependent selectivity; an unknown model and absent-column statistics are refused; `warm_up=False` costs nothing. AR §6.6.

### p.6 Superseded

The check's profile row (MH 4.9 step 6: `CheckOptions::profiles_dir`, `ProfileRow`, the controller-format file written beside a run's profiles, `--profiles-dir`, `--no-profile`, `_core.default_profiles_dir`, CK-T2 and CK-T4) is deleted. The controller's own profile store (11 e.3) is the run's and is unchanged; it no longer receives a seed from `moruna check`, and the first real run of a function sizes from its probe until a run reads the cost model (KD7).

## n. Traceability

| Parent | Invariant | Test |
|---|---|---|
| AR §5.3 (small, dependency-free) | CM-I1, CM-I10 | CM-T1, CM-T18 |
| AR §3 (the inputs) | CM-I5 | CM-T2, CM-T8 |
| AR §4, decision 2 (estimates are distributions) | CM-I2 | CM-T3, CM-T4 |
| AR §5.3 (form, fitting, spread, ratios) | CM-I5, CM-I11 | CM-T4, CM-T5, CM-T6, CM-T9, CM-T19 |
| AR §5.4 (no history) | CM-I4 | CM-T7 |
| AR §5.1, §7 (fitted state, versioned by id) | CM-I3 | CM-T10, CM-T11, CM-T14 |
| AR §7 (records, retention) | CM-I6, CM-I7 | CM-T12, CM-T13, CM-T20, CM-T21 |
| AR §5.2, §12.3 (the swap, shadow) | CM-I8, CM-I9 | CM-T15, CM-T16, CM-T17 |
| AR §6.1, G11 (data, value dependence) | CM-I28, CM-I29, CM-I30 | CM-T45, CM-T46, CM-T52 |
| AR §6.2 (sizes, residual, budget) | CM-I32 | CM-T47, CM-T48 |
| AR §6.3, §6.4, §6.5 (start-up, scaling, state) | CM-I31 | CM-T49, CM-T50, CM-T51 |
| AR §6.6 (result, charging), §5.2, §7 (the configuration) | CM-I27, CM-I33, CM-I34, CM-I35 | CM-T53, CM-T54, CM-T55 to CM-T58 |
| AR §12.5 (never reserve past the budget: the peak a reservation rests on) | CM-I36 | CM-T59, CM-T60 |
| (the decisions apart from the clock) | CM-I37 | CM-T47 to CM-T52, CM-T54, CM-T56 |
| AR §8, G1 (the simulator, its schedule) | CM-I12, CM-I18, CM-I19 | CM-T22, CM-T26, CM-T29, CM-T31 |
| AR §8, G7, §9.4, §12.5 (memory a hard limit, known-future eviction) | CM-I13, CM-I14 | CM-T23, CM-T30 |
| AR G6, §9.5 (contention, scaling curves) | CM-I15 | CM-T24 |
| AR §8 (uncertainty, 200 draws), decision 2 | CM-I16 | CM-T25, CM-T28 |
| AR §0, G4 (morsel size) | CM-I17 | CM-T27 |
| AR §9.4 step 3 (the lookahead plays from the run's state) | CM-I23 | CM-T38 |
| AR G12 (the run's resources change; a resize) | CM-I24 | CM-T39 |
| AR §9.6, §5.2 (learning during the run, the shadow, the store at the end) | CM-I25 | CM-T40, CM-T42, CM-T43, CM-T44 |
| AR §9.6 (a drifting key: wider, conservative, until the error settles) | CM-I26 | CM-T41 |

## o. Deferred

None. CM-O1 (drift), CM-O2 (the lookahead's snapshot) and CM-O3 (resizes) were built by KD7c: CM-I26, CM-I23, CM-I24.

### Registration and live key parity at the language boundary

The native `registration_settings` rule is shared by package registration/check and live `RunLearning`. Packages key by actual invocation `args`, with absent/null args meaning the existing empty settings object. Std arguments and Python source/declaration/locked dependencies are already part of their canonical executable fingerprints; their existing registration settings digest is empty. Package locations, expected fingerprint pins, Python import aliases and sandbox paths do not create extra model inputs. Fused executables retain their actual composite fingerprint and an ordered identity of member kinds and canonical settings. Actual loaded input schemas remain part of every live key.

The public Python `CostSession` opens this same configured native store/model, reloads its actual fitted state after required registration warm-up, and installs the resulting online authority for live observations. SDK registration preparation returns the original measurement/consumption facts without publishing an external function registry entry. Missing exact peak measurements remain unknown, never inferred from endpoint RSS samples or lifetime account peaks. Conformance compares keys produced by real package registration to the exact live key builder and checks two actual argument-bearing invocations remain distinct under the same package fingerprint.

The governed cost-store adapter is another implementation of the existing ObservationStore port, not another record format or model. Public helpers exported from the existing local codec own observation validation/framing, decoding, key partition names, model-state names and raw-history retention/folding. The adapter uses actual immutable object acknowledgements and conditional model-state replacement under the injected verifier. Explicit record/object/scan/queue ceilings refuse rather than silently erase measurements; a read denial never becomes empty history or absent state. Remote compaction/lifecycle is explicit when the immutable record ceiling is reached. An acknowledgement timeout does not assert the remote object is absent. Initial model/history reads and live flush use owned bounded IO outside the hot prediction/settlement loop. Model publication is compare-and-swap against the exact version (including genuine absence) observed when that adapter loaded the fitted state. A save cannot list and adopt another fitter's intervening version; only its own successful publication advances the expected version. Conflict remains a refusal until an explicit reload/refit. The stale-fitter regression exercises two independent governed adapters against the same actual durable objects, verifies refusal, then explicit reload and successful replacement.


The Python run entrypoint uses canonical `Guest::for_current_process` when explicit
embedding ports are omitted. On the Moruna kernel this consumes the same validated
startup bindings, assigned run, history/checkpoint/cache and cost authorities; a
missing/invalid native grant is a refusal, never host `Components::default`. Ordinary
off-kernel Python retains its existing unbound host behavior. Explicit embedding
ports continue through their existing canonical checked-job binding.
