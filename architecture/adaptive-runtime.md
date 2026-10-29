# The adaptive runtime: cost model and scheduler

Built as wave D items KD6 (the cost model, warm-up, `explain`) and KD7 (the adaptive scheduler). It answers the research proposal `research/residency-aware-adaptive-scheduling.md` (cited RA §n). This document is the north star for both; Brackly, 2026-10-28.

## 0. The objective

Moruna runs a job with the lowest latency and the highest throughput the run's resources allow. So, while work is ready, every worker is busy and the memory budget is in use. Idle workers and unused memory with ready work are waste, measured and reported as waste.

The scheduler holds that by making four decisions continuously, all from one view of the run:

| decision | what it decides | what it needs to know |
|---|---|---|
| **what runs next** | which task, on which batch | each unit's predicted time and memory; which inputs are already in memory; what is on the critical path |
| **where it runs** | which worker (core) takes it | which workers are free; whether that worker already holds the input in cache or in the same memory node; how much a kind gains from another worker |
| **where memory goes** | what is reserved, kept, prefetched, spilled or released | every batch's size, location and remaining consumers; each unit's predicted peak |
| **how big a morsel is** | the batch size for each task | per-batch overhead against per-row cost; the memory left; how many workers are waiting for work |

Everything below exists to make those four decisions well and fast:
- the cost model predicts the numbers they need;
- warm-up gives the model its first numbers;
- the buffer catalog is the view of memory;
- the simulator compares choices;
- the policy port lets a better decision procedure replace a worse one without touching the rest.

A mechanism that does not improve one of the four decisions, or that costs more decision time than it saves in run time, does not belong.

A kernel's cost is the resources it takes from what the run is given, and for how long: worker time, memory, disk traffic. The machine is standard and calls outside are not a kernel's cost, so neither is modelled.

## 1. Decisions

1. **One cost model serves the plan and the run.** `explain` (before a run, in the embedder's process) and the scheduler (during it) ask the same model the same question. They differ only in what they know: the plan has counts and history, the run also has live measurements.
2. **Estimates are distributions.** Every prediction carries a median, a 95th percentile, how many observations it rests on, and where they came from. The scheduler's robustness to wrong predictions (RA §11) depends on knowing how wrong they may be.
3. **The model is a port.** A model is an adapter behind one trait, named by configuration, registered like a kind. Moruna stores raw observations, not only fitted numbers, so a new model trains from the history the old one saw. Swapping the model is a setting and a retrain, never a code change in the planner, the scheduler or warm-up.
4. **The scheduling policy is a port.** RA §9's arms (FIFO, pipeline-first, residency-greedy, bounded lookahead) are adapters of one trait, benchmarked against each other on the same runs. The default is whichever wins on measurements.
5. **Warm-up runs automatically when a function is registered**, charged to the workspace that registers it.
6. **The cost of an action is its effect on the job's predicted finish, found by simulation**, never by adding task durations (RA §11, first open question).
7. **Utilisation is the check on every decision.** A run reports worker-idle time and unused memory while work was ready, per decision kind; a policy that leaves either on the table loses to one that doesn't, on the benchmark matrix.

## 2. What the research proposal leaves open, and the answer here

| # | Gap | Answer |
|---|---|---|
| G1 | "Incremental makespan under overlap" is named but not defined (RA §5, §11). | A discrete-event simulator over predicted durations and the run's resources (section 8). An action's cost is the change it makes to the simulated finish. The plan and the lookahead use the same simulator. |
| G2 | Estimates are "assumed available; learning them is later" (RA §2). | Learning is part of the design from the start: warm-up, the observation store, online updates (sections 6, 7, 9.6). A runtime without estimates schedules no better than FIFO. |
| G3 | RA §5 lists what costs, not what predicts it. | The inputs (section 3). |
| G4 | Batch size is treated as given (RA §11, granularity). | Batch size is a decision. The controller already sizes morsels (SDD 11); the model supplies per-batch overhead and per-row cost, so the size minimises overhead within the memory reservation. |
| G5 | Stateful operators are a risk, not a design (RA §11). | State is a predicted quantity (growth per distinct key) and a resource the scheduler reserves. A stateful task's state is a buffer in the catalog: spillable when the kind is checkpointable, pinned otherwise. |
| G6 | Contention has no model (RA §5, item 5). | The run's workers, memory and disk bandwidth are known capacities, and each unit has predicted demands. When summed demand exceeds a capacity, the units using it slow in proportion. Each kind's measured scaling curve says what an extra worker gives it. |
| G7 | "Residency value" is open (RA §11). | The DAG is known, so every buffer's remaining consumers and their earliest start are known. Eviction uses that future (Belady's rule with known consumers): the buffer whose next use is furthest away, weighted by its reload cost, goes first. |
| G8 | Fairness is open (RA §11). | Critical-path priority with ageing: a ready unit's priority grows with its wait, bounded so no ready branch waits more than a set multiple of its predicted duration. |
| G9 | Scheduler overhead can outweigh gains for small batches (RA §8, H4). | A decision budget: a fraction of the median unit duration (default 1%). Lookahead falls to greedy, then to the plain admission rule, when it would be exceeded. Reported per run. |
| G11 | Synthetic warm-up data has no real value distribution. | Value-dependent quantities (selectivity, skew) are marked as such. Warm-up measures the kernel's sensitivity to skew (uniform and skewed keys); real runs supply actual values. Every estimate records its source. |
| G12 | The run's resources are assumed fixed. | They are read at run start and on every resize; the simulator and the policy use what the run has now. |
| G13 | The first run of anything has no estimates. | Fallbacks (section 5.4): this function's history, the kind's prior, a default. The controller's calibration phase measures the first batches before the run commits. |
| G14 | Decisions are not reported. | Every decision that departs from the plan is an event with the plan step, the unit chosen, the runner-up and both scores (sampled when frequent). |

## 3. The inputs

Collected without reading tenant values outside a run. Inside a run the engine sees values, and only counts and ratios leave it.

### 3.1 The data

| input | from |
|---|---|
| rows | table statistics; the batch at run time |
| bytes per column (mean and p95 length for strings, binary and lists) | schema; table statistics; the batch |
| columns read, after projection | the job |
| null fraction per column | table statistics |
| distinct count per key column | table statistics |
| storage layout: codec, row-group size, file count | table metadata |
| skew of key columns (share of rows in the top 1% of keys) | measured in the run |
| selectivity (rows out per row in) | measured in the run; warm-up gives only the sensitivity |

### 3.2 The kernel

| input | from |
|---|---|
| identity: kind, function fingerprint, settings digest, input schema hash | the job |
| declared facts: deterministic, preserves columns, stateful | the declaration |
| start-up time, cold (image not cached) and warm | warm-up; runs |
| per-batch fixed time; time per row and per byte | warm-up; runs |
| peak and retained memory per input byte | warm-up; runs (the controller's amplification) |
| state growth per distinct key; checkpoint size and time | warm-up; runs |
| worker time used per wall second | warm-up; runs |
| speed-up at 1, 2 and 4 workers | warm-up |

### 3.3 The run's resources

| input | from |
|---|---|
| workers, memory and disk the run is given: its start and its ceiling | the job and the embedder |
| what is free now | the run, at start and on every resize |
| image layers already cached | the node |

### 3.4 The job

| input | from |
|---|---|
| the DAG: dependencies, contract barriers, consumers of each output | the job |

## 4. The output

```rust
pub struct Estimate {
    pub p50: f64,
    pub p95: f64,
    pub unit: Unit,            // seconds, bytes, rows
    pub basis: Basis,          // Measured | WarmUp | Prior | Default
    pub observations: u32,
}

pub struct UnitCost {          // one task applied to one batch
    pub duration: Estimate,
    pub worker_time: Estimate,
    pub peak_memory: Estimate,
    pub retained_memory: Estimate,
    pub rows_out: Estimate,
    pub bytes_out: Estimate,
    pub state_growth: Estimate,
    pub read_bytes: Estimate,
    pub spill_bytes: Estimate,
}
```

- A task's cost is its units' costs over the batches the plan expects.
- A job's cost is the simulator's result (section 8), not a sum: its finish time (p50 and p95), the critical path, peak memory, and the resources held over time (memory × time, worker time, disk bytes moved).

## 5. The model port

### 5.1 The trait

```rust
pub trait CostModel: Send + Sync {
    /// Name and version, stored beside everything it fits; a change of either is a retrain.
    fn id(&self) -> ModelId;
    /// The prediction for one unit from what is known now; missing inputs widen the estimate.
    fn predict(&self, key: &CostKey, inputs: &Inputs) -> UnitCost;
    /// One measured unit. A model may learn online, or ignore it until `fit`.
    fn observe(&mut self, key: &CostKey, inputs: &Inputs, outcome: &Outcome);
    /// Fit from the store's raw observations: how a new model learns the history.
    fn fit(&mut self, history: &dyn ObservationSource) -> Result<FitReport>;
    /// Its fitted state, versioned by `id`, and back.
    fn save(&self) -> Result<Vec<u8>>;
    fn load(&mut self, bytes: &[u8]) -> Result<()>;
}
```

- `CostKey` is section 3.2's identity: kind, fingerprint, settings digest, schema hash.
- `Inputs` is a fixed, versioned record of section 3, every field optional, so the planner and the run fill what they know.
- `Outcome` is the measured values of the `UnitCost` fields.

### 5.2 Swapping a model

- Models register in a `CostModels` registry by name, as kinds do (SDD 18).
- The model in use is a setting: the job's `cost.model`, or the embedder's default.
- A new model is a new adapter. On first use it calls `fit` over the stored observations.
- Two models can run in shadow: the configured one decides, the other predicts, and both predictions are recorded against the outcome. A swap is judged on the same runs.

### 5.3 The default model

Small, dependency-free, fitted incrementally. Per `CostKey` and per target:

- **Form:** a regression on logarithms, so one straight line covers linear, n log n and power-law growth:

  `log(y) = b0 + b1·log(rows) + b2·log(bytes_per_row) + b3·log(columns) + b4·log(distinct) + b5·log(workers)`

  plus a fixed term (start-up and per-batch overhead) fitted on the smallest sizes.
- **Fitting:** recursive least squares. Each observation updates the coefficients in constant time and memory, with a forgetting factor, so a function whose behaviour changes is followed.
- **Spread:** the 95th percentile comes from the residual variance, widened outside the measured range (extrapolation is marked).
- **Ratios:** selectivity and other value-dependent ratios are kept as ratios (median, p95), not regressed on size.

### 5.4 When a key has no history

1. this function's history;
2. the kind's prior: its median behaviour across every function of the kind;
3. a declared default.

`basis` says which.

## 6. Warm-up

Runs when a function is registered, charged to the registering workspace, inside the same sandbox real runs use.

1. **Data:** Arrow batches generated from the function's input schema:
   - honouring each column's null fraction, distinct count and string and list lengths when the embedder supplies table statistics;
   - type defaults otherwise;
   - key columns generated twice, uniform and skewed, so sensitivity to skew is measured.
2. **Sizes:** 1,000, 10,000, 100,000 and 1,000,000 rows, three repetitions each. It stops early when the fit's residual is small or its time budget (default 60 seconds, a setting) is spent, and a stopped warm-up records how far it got.
3. **Start-up:** the first invocation with a cold image is recorded as cold start, a second as warm.
4. **Scaling:** the largest size that fits runs at 1, 2 and 4 workers.
5. **State:** a stateful kind runs with increasing distinct counts to fit state growth, and once with a checkpoint to time it.
6. **Result:** every unit's inputs and outcome go to the observation store and the model is fitted. Registration's response carries the fitted cost per 10,000 rows, so whoever registers sees it.

## 7. The observation store

- **Records:** append-only (`CostKey`, `Inputs`, `Outcome`, `basis`, time, model id), in the record layout of KB4.
- **The port:** `ObservationStore` is a port with a local-directory adapter (today's `profiles_dir`). The embedder supplies its own; Griot keeps one per workspace.
- **Retention:** the last N per key (default 10,000), with older records summarised.
- **Fitted state:** models save it beside the raw records, keyed by model id. The raw records are the truth; fitted state is a cache.

## 8. The simulator

One discrete-event simulator in the cost crate, used three ways: by `explain`, by the scheduler's lookahead, and by tests against measured runs.

- **State:**
  - workers, memory reserved and resident, disk bandwidth in use;
  - each buffer's size, location (memory or disk) and remaining consumers;
  - ready and running units.
- **Events:** a unit starts or finishes; a read or a spill completes.
- **Contention:** when summed demand on a resource exceeds its capacity, every unit using it slows in proportion, and a kind's scaling curve caps what an extra worker gives it.
- **Uncertainty:** `explain` runs it once with medians for the plan, then 200 times with durations drawn from each estimate's spread, for the job's p50 and p95 finish and resources held. The scheduler's lookahead uses medians only, for speed.

`explain` returns the plan document (KA7):
- steps with stable ids;
- what runs in parallel and what in sequence;
- each step's `UnitCost` totals and resource settings;
- the job's critical path, finish time and resources held.

## 9. The adaptive scheduler

### 9.1 The unit and the state

- **The unit** is one task applied to one batch.
- **The state** is RA §4's, held in two structures:
  - the plan's steps, as the DAG at batch granularity;
  - the **buffer catalog**: every batch's size, location, owner, remaining consumers and reload cost. It extends the placement engine, which already tracks residency (SDD 09).

### 9.2 Actions

Admit a ready unit (reserving its memory), prefetch a disk-resident input, spill a resident buffer, release a buffer with no consumers left, ask for a resize within the ceiling, or wait.

### 9.3 The policy port

```rust
pub trait SchedulingPolicy: Send {
    fn id(&self) -> PolicyId;
    fn decide(&mut self, state: &SchedulerState, model: &dyn CostModel, sim: &Simulator) -> Vec<Action>;
}
```

Adapters:
- `admission`: today's `pick.rs` rule, the baseline;
- `fifo`;
- `residency_greedy`: one step;
- `lookahead`: beam search, two or three steps.

The policy is a setting.

### 9.4 Deciding

Each time a worker frees, a read completes or a unit finishes:

1. **Feasible set:** every ready unit whose reserved memory plus its predicted peak (the 95th percentile) fits the budget. This is a hard rule (RA §5): a unit that does not fit is never admitted, whatever its score.
2. **Score** each feasible action with the simulator, by its change to the predicted finish. Then:
   - add the reload cost of any input that must come back from disk;
   - add the reload cost of whatever admitting it would push out, when that buffer's next consumer is near;
   - subtract the memory freed by being a buffer's last consumer;
   - subtract the action's critical-path weight;
   - subtract its ageing credit.
3. **Choose:** lookahead expands the best few actions two or three steps in the simulator and takes the first step of the best sequence. Greedy takes the best score.
4. **Stay within the decision budget** (G9): if deciding took too long, the next decision uses a shallower policy, and the run reports it.
5. **Prefetch and spill alongside:** prefetch the inputs of the next likely units while disk bandwidth is free, and spill by known-future eviction when admission needs room.

### 9.5 Concurrency

- No worker belongs to a task. The best-scoring unit gets the next free worker, so workers follow the bottleneck (RA §6).
- A kind's scaling curve caps how many of its units run at once.
- The contention model stops adding readers when disk bandwidth is saturated.

### 9.6 Learning during the run

- Every finished unit's outcome goes to `observe`, so the rest of the run is predicted from what this run measured.
- When the error leaves the predicted spread for several units in a row, the key is marked drifting: its spread widens and the policy turns conservative (less memory headroom, shallower lookahead) until the error settles.
- At the end, the run's observations are written to the store.

### 9.7 What it reports

- Step-tagged events for each unit's lifecycle (KA8).
- An event for every decision that departs from the plan, with the runner-up and both scores.
- The report gains:
  - the order actually run;
  - the prediction error per target;
  - decision time;
  - spill and reload bytes;
  - peak and reserved memory.

## 10. The measure

One measure, the job's finish (section 0), and its consequence, utilisation. The simulator scores every action by its change to the predicted finish. The run reports finish time, throughput (rows per second per stage), and idle worker time and unused memory while work was ready. A policy is judged on those four numbers.

## 11. Where it lives

| crate | holds | depends on |
|---|---|---|
| `moruna-cost` | `Inputs`, `CostKey`, `Estimate`, `UnitCost`, `Outcome`; the `CostModel` trait and registry; the default model; `ObservationStore` and its local adapter; the simulator | `moruna-job` only |
| `moruna-job` | `explain(job, catalogue, stats, model, store) -> Plan`; the plan document (KA7) | `moruna-cost` |
| `moruna-runtime` | warm-up (`moruna check` with profiling); the buffer catalog over placement; the `SchedulingPolicy` trait and its four adapters; online observation | both |

An embedder depends on `moruna-job` and `moruna-cost` to plan in its own process, and supplies an `ObservationStore` adapter.

## 12. How it is proved

1. **The simulator against reality:** for every benchmark run, the simulated finish, peak memory and spill bytes are compared with the measured ones, within an error budget per workload class.
2. **The default model's accuracy:** median absolute percentage error per target on a held-out split of warm-up and run observations, reported per release.
3. **The swap:** a second model adapter (a test model) is registered, configured, fitted from the store and used by `explain` and the scheduler, with no change outside its own module.
4. **The policies:** RA §9's matrix runs every policy adapter on RA §7's workloads, varying memory-to-data ratio, batch size, branching, reuse, CPU-to-I/O ratio, skew, selectivity and workers. The default is the one with the earliest finish and the least idle worker time and unused memory while work was ready, with correct output. The matrix runs in the benchmark suite on every release.
5. **The hard rule:** a property test that under any sequence of predictions, wrong by any factor, the scheduler never reserves past the budget. When an outcome exceeds its prediction, the run resizes or spills; it never runs out of memory.

## 13. Build order

| item | what | depends on |
|---|---|---|
| KD6a | `moruna-cost`: the types, the model port and registry, the default model, the observation store port and local adapter | `moruna-job` (KA4) |
| KD6b | warm-up profiling in `moruna check` | KD6a |
| KD6c | the simulator, and `explain` returning the plan | KD6a, KA5 (DAG), KA7 (plan document) |
| KD7a | the buffer catalog with known-future eviction, over placement | KA5 |
| KD7b | the policy port, the `admission` and `fifo` adapters, the benchmark matrix | KD7a |
| KD7c | `residency_greedy` and `lookahead`; online learning and drift | KD6c, KD7b |
| KD7d | the default policy chosen from the matrix; decisions reported | KD7c |
