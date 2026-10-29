# Moruna SDD 24: The cost model's foundation (`moruna-cost`)

**Document type:** software design document, component 24
**Status:** DRAFT · 2026-10-29 (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** board item KD6a (`BOARD.md`, EK wave D); `architecture/adaptive-runtime.md` (cited **AR §n**) sections 0, 1, 3, 4, 5 and 7, decided by Brackly on 2026-10-28
**Preamble:** `00-preamble.md`; **Cited:** 18 (the registry of kinds, KD-I1, KD-I2), 22 (`moruna-job`, JB-I1), 11 e.3 and f.9 (today's profile store), 21 (`moruna-record`, on `v1`)
**Component location:** `crates/moruna-cost/` (`src/key.rs`, `src/inputs.rs`, `src/estimate.rs`, `src/model.rs`, `src/registry.rs`, `src/loglinear/`, `src/store/`, `src/exact.rs`, `src/error.rs`), `crates/moruna-cost/tests/` (`crate.rs`, `model.rs`, `store.rs`, `swap.rs`)
**Consumes:** `moruna-job` (the `Role` a kind plays, and the rule a kind's name follows, which a model's name follows), `serde`, `serde_json`, `thiserror`. **Consumed by:** KD6b (warm-up in `moruna check`), KD6c (the simulator and `explain`), KD7 (the scheduler), and every embedder that plans in its own process.

**Decisions worth your eye:** (1) a prediction is a distribution with its basis and the observations it rests on, never a number (AR decision 2); (2) the model is a port and a setting: the planner and the scheduler call `CostModel`, never the default model, and a second model trains from the raw records the first one saw (AR decision 3); (3) the default model is recursive least squares on logarithms with forgetting, plus a fixed overhead chosen at the smallest sizes, in about two hundred lines and no linear-algebra crate; (4) fitted state is a cache keyed by model id, refused by any other id; the raw records are the truth; (5) the local store's frame is a stand-in for `moruna-record`'s layout, which is on `v1` only (m).

---

## a. Purpose and boundary

The scheduler decides, continuously, what runs next, where it runs, where memory goes and how big a morsel is (AR §0). Every one of those decisions needs a unit's predicted time, memory and output, and how far the prediction may be wrong. This component predicts them, learns them from what units measured, and keeps the measurements so that a better model learns the same history.

It owns: what is known of a unit (`Inputs`, AR §3), what is costed (`CostKey`), the answer (`Estimate`, `UnitCost`, AR §4) and the measurement (`Outcome`); the model port (`CostModel`) and its registry (`CostModels`), the setting that chooses (`CostSetting`) and the models in use (`InUse`, shadow mode, `compare`); the default model (`LogLinear`, AR §5.3, §5.4); the observation store port (`ObservationSource`, `ObservationStore`) and its local adapter (`LocalStore`, AR §7).

It refuses to know: hardware type, calls outside the run, tokens or egress (AR §0, removed on purpose); the DAG's schedule and the simulator (KD6c); warm-up's data generation (KD6b); the buffer catalog and the policies (KD7); any embedder.

## b. Vocabulary

**Unit.** One task applied to one batch (AR §9.1).

**Key.** A `CostKey`: role, kind, function fingerprint, settings digest, input schema hash. Two units with one key are the same work on different data.

**Target.** One field of `UnitCost`: duration, worker time, peak memory, retained memory, rows out, bytes out, state growth, read bytes, spill bytes.

**Basis.** Which fallback answered: `Measured` (the function's history, some of it from a run), `WarmUp` (the function's history, all of it from warm-up), `Prior` (the kind's, pooled over its functions), `Default` (declared).

**Overhead.** The fixed part of a unit's cost: start-up and per-batch time, a baseline allocation.

**Retention.** The raw records kept per key (10,000); older ones are folded into the key's summary.

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
pub struct Estimate { pub p50: f64, pub p95: f64, pub unit: Unit, pub basis: Basis, pub observations: u32, pub extrapolated: bool }
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

Exactly AR §5.1. `fit` replaces what the model held.

### d.3 The registry and the models in use

```rust
pub trait ModelAdapter: Send + Sync { fn name(&self) -> &str; fn make(&self) -> Box<dyn CostModel>; }
pub struct CostModels;                       // empty(), standard() (loglinear), register(adapter), names(), make(name)
impl CostModels {
    pub fn open(&self, setting: &CostSetting, default: &str, store: &dyn ObservationStore) -> Result<InUse>;
}
pub struct CostSetting { pub model: Option<String>, pub shadow: Option<String> }   // the job's `cost` section
pub enum Readiness { Loaded(ModelId), Fitted(ModelId, FitReport), Refitted(ModelId, FitReport, String) }
pub struct InUse;                            // new(deciding, shadow), deciding(), shadow(), readiness(),
                                             // predict -> Prediction, settle(store, key, inputs, prediction, outcome, at_ms), save(store)
pub struct Prediction { pub deciding: Predicted, pub shadow: Option<Predicted> }
pub fn compare(source: &dyn ObservationSource) -> Result<Comparison>;   // per model, per target: n, median |log error|, p95 coverage
```

`open` makes the setting's model (or `default`), and the shadow when named; each loads the state the store holds for its id, and when there is none fits over the store's observations and saves the state (AR §5.2). State that does not load is refitted, and `Readiness::Refitted` says why. The scheduler's loop calls `in_use.deciding().predict`; `settle` appends the observation with both predictions and lets both models observe it.

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

## g. Concurrency within the component

A `LocalStore` is `Send + Sync`: appends from threads are serialised by one mutex; appends from processes land in one segment through `O_APPEND`, one write per record; compaction is one process at a time under the directory's lock. A scan that finds a segment it listed already retired starts again (three times). A `CostModel` is `Send + Sync` and is mutated only through `&mut`; the scheduler owns it.

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

## l. Implementation notes for the agent

`predict` is in the scheduler's decision loop: it looks the key and the kind up without allocating (the kind map is keyed by `String` and read by `&str`), computes the features once, and does one 6 × 6 quadratic form per target. Keep it that way; CM-T18 measures it. Do not add a linear-algebra crate (CM-I1).

## m. Open items

**CM-M1. The record layout.** AR §7 asks for the record layout of KB4 (`moruna-record`, SDD 21). It is on `v1` only; this crate frames its own records (e.2). KB4's carry-over to `main` replaces the frame with `moruna-record`'s and keeps the body.

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

## o. Deferred

**CM-O1. Start-up and checkpoints in a unit's cost.** The inputs carry cold and warm start-up and checkpoint size and time; a unit's estimate does not add them, because they are paid once per process or per checkpoint, which the simulator (KD6c) schedules.

**CM-O2. Drift.** AR §9.6's drifting key (error outside the spread for several units in a row widens the spread) is KD7c's; it adds a drifting mark to `Estimate` beside `extrapolated`.
