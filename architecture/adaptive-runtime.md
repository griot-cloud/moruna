# The adaptive runtime: cost model and scheduler

What decides what Moruna runs next, and how a job's cost is known before it runs. Built as wave D items KD6 (the cost model, warm-up, `explain`) and KD7 (the adaptive scheduler). The research proposal it answers is `research/residency-aware-adaptive-scheduling.md` (cited RA §n). Decided by Brackly 2026-10-28.

## 1. Decisions

1. **One cost model serves the plan and the run.** `explain` (before a run, in the embedder's process) and the scheduler (during the run) ask the same model the same question. They differ only in what they know: the plan has counts and history, the run also has live measurements.
2. **Estimates are distributions, never single numbers.** Every prediction carries a median, a 95th percentile, how many observations it rests on, and where they came from. The scheduler's robustness to wrong predictions (RA §11, "prediction error") depends on knowing how wrong they may be.
3. **The model is a port.** A model is an adapter behind one trait, named by configuration, registered like a kind. Moruna stores raw observations, not just a model's fitted numbers, so a new model is trained from the history the old one saw. Swapping the model is a configuration change and a retrain, never a code change in the planner, the scheduler or the warm-up.
4. **The scheduling policy is a port too.** RA §9's experimental arms (FIFO, pipeline-first, residency-greedy, bounded lookahead) are adapters of one trait, so they are benchmarked against each other on the same runs, and the default is whichever wins on measurements.
5. **Warm-up runs automatically when a function is registered**, charged to the workspace that registers it.
6. **The cost of an action is its effect on the predicted finish time of the whole job, found by simulation**, never by adding task durations. This answers RA §11's first open question.

## 2. What the research proposal leaves open, and the answer here

| # | Gap | Answer |
|---|---|---|
| G1 | "Incremental makespan under overlap" is named but not defined (RA §5, §11). | A discrete-event simulator over predicted durations and resource capacities (section 8). An action's cost is the change it makes to the simulated finish time. The plan and the lookahead use the same simulator. |
| G2 | "Estimates are assumed available; learning them is later" (RA §2). | Learning is part of the design from the start: warm-up, the observation store, online updates (sections 6, 7, 9.6). A runtime whose estimates are absent schedules no better than FIFO. |
| G3 | RA §5 lists what costs, not what predicts it. | The feature set (section 3): the inputs that determine each cost. |
| G4 | Batch size is treated as given (RA §11 "granularity"). | Batch size is a decision. The controller already sizes morsels (SDD 11); the cost model supplies its per-batch overhead and per-row cost, so the size minimises overhead within the memory reservation. |
| G5 | Stateful operators are a risk, not a design (RA §11). | State is a predicted quantity (its growth per distinct key) and a resource the scheduler reserves. A stateful task's state is a buffer in the catalog: spillable when the kind declares it checkpointable, pinned otherwise. |
| G6 | Contention is a cost term with no model (RA §5 item 5). | Each node has measured capacities (CPU, memory bandwidth, disk read and write, network). Each task has predicted demands. Concurrency slows a task by how far summed demand exceeds capacity, and the scaling curve measured at warm-up gives each kind's own response to more workers (section 3.3). |
| G7 | "Residency value" is an open question (RA §11). | The DAG is known, so every buffer's remaining consumers and their earliest possible start are known. Eviction uses that future (Belady's rule with known consumers): evict the buffer whose next use is furthest away, weighted by its reload cost. This is computed, not guessed. |
| G8 | Fairness is open (RA §11). | Critical-path priority with ageing: a ready unit's priority grows with its wait, bounded so no ready branch waits more than a set multiple of its predicted duration. |
| G9 | Scheduler overhead can outweigh gains for small batches (RA §8 H4). | The policy has a decision budget: a fraction of the median unit duration (default 1%). Lookahead depth falls to greedy, then to the plain admission rule, when the budget would be exceeded. Measured and reported per run. |
| G10 | Only wall-clock time is an objective (RA §13). | The objective is a setting (section 10): finish time, credits, or finish time within a credit ceiling. Griot charges for memory over time, so the cheapest schedule and the fastest are not the same. |
| G11 | Synthetic warm-up data has no real value distribution. | Value-dependent quantities (selectivity, skew, sortedness) are marked as such. Warm-up measures the kernel's sensitivity to them (it runs uniform and skewed keys); real runs supply their actual values. Each estimate records which source it rests on. |
| G12 | The node is assumed to be the job's alone. | Capacities are read at run start and whenever the node reports a change (another run, a resize). On the shared tier the measured capacity is what the run actually gets. |
| G13 | No cold start: the first run of anything has no estimates. | A hierarchy of fallbacks (section 5.4): exact history, the same function on other hardware scaled by the hardware score, the kind's prior, a default. Plus the controller's calibration phase, which measures the first batches before the run commits. |
| G14 | External calls are absent. | Port calls are a resource: count, latency, bytes and tokens per batch, with a concurrency limit per port. A task waiting on a port holds memory but no CPU, and the scheduler knows it. |
| G15 | Decisions are not reported. | Every decision that departs from the plan is an event with the plan step, the unit chosen, the runner-up and both scores (sampled when frequent). A run can be replayed from its decision log. |
| G16 | RA §12 starts with a simulator and ends with a prototype. | The simulator is not a throwaway. It is the planner and the lookahead in production, and it is checked against measured runs (section 12). |

## 3. The inputs: what determines a kernel's cost

Each input is collected without reading tenant values outside a run. Inside a run the engine sees values, and only ratios and counts leave it.

### 3.1 The data

| input | collected from |
|---|---|
| rows | table statistics; the batch at run time |
| bytes per column (fixed width, and mean and p95 length for strings, binary and lists) | schema; table statistics; the batch's buffers |
| column count read (after projection) | the job |
| null fraction per column | table statistics |
| distinct count per key column | table statistics |
| storage layout: codec, row-group or page size, file count | table metadata |
| rows the source skips through its own statistics | measured per run |
| skew of key columns (share of rows in the top 1% of keys) | measured per run; never in the plan's input |
| sortedness of key columns | measured per run |
| selectivity (rows out per row in) | measured per run; warm-up gives only the kernel's sensitivity |

### 3.2 The kernel

| input | collected from |
|---|---|
| identity: kind, function fingerprint, settings digest, input schema hash | the job |
| declared facts: deterministic, preserves columns, stateful, needs (network, gpu, model port) | the declaration |
| start-up cost, cold (image not cached, fresh sandbox) and warm | warm-up; runs |
| per-batch fixed cost and per-row and per-byte cost | warm-up; runs |
| peak transient memory and retained memory per input byte | warm-up; runs (the controller's amplification) |
| state growth per distinct key; checkpoint size and time | warm-up; runs |
| CPU seconds per wall second (how many cores it actually uses) | warm-up; runs |
| scaling: speed-up at 1, 2 and 4 workers | warm-up |
| port calls per batch: count, latency, request and reply bytes, tokens | runs only |

### 3.3 The machine

| input | collected from |
|---|---|
| hardware class and a measured score: single-core speed, memory bandwidth, disk read and write bandwidth and latency, network bandwidth | a short benchmark when the node starts, kept for the node's life |
| live capacity left: workers, memory, disk and network bandwidth | the node, at run start and on change |
| image layers already cached | the node |
| whether a node must be added first, and how long that takes | the embedder (Griot's pool controller) |

### 3.4 The job

| input | collected from |
|---|---|
| the DAG: dependencies, contract barriers, consumers of each output | the job |
| memory budget and ceiling; worker budget | the job and the embedder |
| the objective (section 10) | the job |

## 4. The output: an estimate

```rust
pub struct Estimate {
    pub p50: f64,
    pub p95: f64,
    pub unit: Unit,            // seconds, bytes, rows, cpu_seconds, tokens
    pub basis: Basis,          // Measured | WarmUp | Scaled | Prior | Default
    pub observations: u32,
}

pub struct UnitCost {          // one task applied to one batch
    pub duration: Estimate,
    pub cpu: Estimate,
    pub peak_memory: Estimate,
    pub retained_memory: Estimate,
    pub rows_out: Estimate,
    pub bytes_out: Estimate,
    pub state_growth: Estimate,
    pub read_bytes: Estimate,
    pub port: PortCost,        // calls, latency, bytes, tokens; zero for a kind with no port
}
```

A task's cost is its units' costs summed over the batches the plan expects. A job's cost is not a sum (G1): it is the simulator's result (section 8):
- finish time as p50 and p95;
- the critical path;
- peak memory;
- credits, itemised into memory × time, CPU × time, model tokens and outbound bytes.

## 5. The model port

### 5.1 The trait

```rust
pub trait CostModel: Send + Sync {
    /// Name and version, stored beside everything it fits; a change of either is a retrain.
    fn id(&self) -> ModelId;
    /// The prediction for one unit from what is known now; missing features widen the estimate.
    fn predict(&self, key: &CostKey, features: &Features) -> UnitCost;
    /// One measured unit. A model may learn online or ignore it until `fit`.
    fn observe(&mut self, key: &CostKey, features: &Features, outcome: &Outcome);
    /// Fit from the store's raw observations: how a new model learns the history.
    fn fit(&mut self, history: &dyn ObservationSource) -> Result<FitReport>;
    /// Its fitted state, versioned by `id`, and back.
    fn save(&self) -> Result<Vec<u8>>;
    fn load(&mut self, bytes: &[u8]) -> Result<()>;
}
```

`CostKey` is the identity in 3.2 (kind, fingerprint, settings digest, schema hash) plus the hardware class. `Features` is a fixed, versioned record of section 3's inputs. Every field is optional, so the planner and the run fill what they know. `Outcome` is the measured values of the `UnitCost` fields.

### 5.2 Swapping a model

- Models register in a `CostModels` registry by name, exactly as kinds do (SDD 18).
- The model in use is a setting: the job's `cost.model`, or the embedder's default.
- A new model ships as a new adapter crate or module; nothing else changes.
- On first use it calls `fit` over the stored observations.
- Both models can run side by side in shadow mode: the configured one decides, the other predicts, and both predictions are recorded against the outcome. A swap is then judged on the same runs.

### 5.3 The default model

Small, dependency-free, fitted incrementally. Per `CostKey` and per target (duration, CPU, memory, rows out, and so on):

- **Form:** a regression on logarithms of the size features, so a straight line in log space covers linear, n log n and power-law behaviour:

  `log(y) = b0 + b1·log(rows) + b2·log(bytes_per_row) + b3·log(columns) + b4·log(distinct) + b5·log(workers) + b6·log(host_score)`

  plus a separate fixed term (start-up and per-batch overhead) fitted on the smallest sizes.
- **Fitting:** recursive least squares. Each observation updates the coefficients in constant time and memory, with a forgetting factor, so a function that changes behaviour is followed.
- **Spread:** the 95th percentile comes from the residual variance at that point, widened when the features are outside the measured range (extrapolation is marked).
- **Size:** a few hundred bytes per key and target.

Selectivity and other value-dependent ratios are kept as ratios (median, p95), not regressed on size.

### 5.4 When a key has no history

The model is asked in this order and answers with the first that has enough observations:
1. this function on this hardware class;
2. this function on other hardware, scaled by the ratio of host scores;
3. this kind's prior: its median behaviour across every function of the kind;
4. a declared default.

`basis` says which.

## 6. Warm-up

Runs when a function is registered, charged to the registering workspace (decision 5), inside the same sandbox real runs use.

1. **Data.** Arrow batches generated from the function's input schema:
   - honouring each column's null fraction and distinct count, and string and list lengths, when the embedder supplies table statistics;
   - type defaults otherwise;
   - key columns generated twice, uniform and Zipf-skewed, so the kernel's sensitivity to skew is measured.
2. **Sizes.** 1,000, 10,000, 100,000 and 1,000,000 rows, three repetitions each. Warm-up stops early when the fit's residual is small, or when its time budget (default 60 seconds, a setting) is spent; a stopped warm-up records how far it got.
3. **Start-up.** The first invocation in a fresh sandbox with a cold image is recorded as cold start, and a second as warm.
4. **Scaling.** The largest size that fits in the budget runs at 1, 2 and 4 workers.
5. **State.** A stateful kind runs with increasing distinct counts to fit state growth, and once with a checkpoint to time it.
6. **Port calls.** Not made: warm-up has no network. A kind that declares a model port or network is recorded as "port cost from runs only".
7. **Result.** Every unit's features and outcome go to the observation store, and the model is fitted. The registration's response carries the fitted per-batch cost at 10,000 rows, so the person registering sees it.

## 7. The observation store

- **What it is.** Append-only records of (`CostKey`, `Features`, `Outcome`, `basis`, timestamp, model id at the time), in the copied record layout (KB4), so they travel like every other Moruna record.
- **Who keeps it.** The embedder. A port, `ObservationStore`, with a local-directory adapter (today's `profiles_dir`) and the embedder's own: Griot keeps them per workspace.
- **Retention.** The last N per key (default 10,000), and older ones summarised.
- **Fitted state.** Models save their fitted state beside the raw records, keyed by model id. The raw records are the source of truth; fitted state is a cache.

## 8. The simulator

One discrete-event simulator, in the cost crate, used three ways: by `explain`, by the scheduler's lookahead, and by tests against measured runs.

- **State:**
  - workers;
  - memory reserved and resident;
  - disk and network bandwidth in use;
  - per buffer: size, location (memory or disk) and remaining consumers;
  - ready and running units.
- **An event** is a unit starting or finishing, a read or spill completing, or a port reply arriving.
- **Contention:** when the summed demand on a resource exceeds its capacity, every unit using it slows in proportion, and a kind's own scaling curve limits what an extra worker gives it.
- **Uncertainty:** `explain` runs it with medians for the plan, then 200 times with durations drawn from each estimate's spread, for the job's p50 and p95 finish time and credits. The scheduler's lookahead uses medians only, for speed.

`explain` returns the plan document (KA7):
- steps with stable ids;
- what runs in parallel and what in sequence (the simulated schedule, grouped);
- each step's `UnitCost` totals and resource settings;
- the job's critical path, finish time and itemised credits.

## 9. The adaptive scheduler

### 9.1 The unit and the state
- **The unit:** one task applied to one batch.
- **The state:** RA §4's, held in two structures. The plan's steps are the DAG at batch granularity. The **buffer catalog** holds every batch's size, location, owner, remaining consumers and reload cost. It extends the placement engine that already tracks residency (SDD 09).

### 9.2 Actions
- admit a ready unit (its memory reserved);
- prefetch a disk-resident input;
- spill a resident buffer;
- release a buffer with no consumers left;
- resize the run (ask the launcher for memory or CPUs within the ceiling);
- wait.

### 9.3 The policy port
```rust
pub trait SchedulingPolicy: Send {
    fn id(&self) -> PolicyId;
    fn decide(&mut self, state: &SchedulerState, model: &dyn CostModel, sim: &Simulator) -> Vec<Action>;
}
```
Adapters:
- `admission` (today's `pick.rs` rule, the baseline);
- `fifo`;
- `residency_greedy` (h = 1);
- `lookahead` (beam search, h = 2 or 3).

The policy is a setting.

### 9.4 Deciding (residency-greedy and lookahead)
Each time a worker frees, an I/O completes, or a unit finishes:
1. **Feasible set.** Every ready unit whose reserved memory plus predicted peak (its 95th percentile) fits the budget. This is a hard rule (RA §5): a unit that doesn't fit is never admitted, however good its score.
2. **Score** each feasible action with the simulator: the change in predicted finish time (or credits, section 10), plus:
   - the reload cost of any input that must come back from disk;
   - the eviction cost of what admitting it would push out: the evicted buffer's reload cost, if its next consumer is near (G7);

   minus:
   - the memory it frees by being a buffer's last consumer;
   - its critical-path weight;
   - its ageing credit (G8).
3. **Choose.** Lookahead expands the best few actions two or three steps ahead in the simulator and picks the first step of the best sequence. Greedy picks the best score.
4. **Within budget.** If deciding took more than the decision budget (G9), the next decision uses a shallower policy, and the run reports it.
5. **Prefetch and spill.** Run alongside: prefetch the inputs of the next likely units while disk bandwidth is free; spill by known-future eviction when admission needs room.

### 9.5 Concurrency
- No worker belongs to a task: the best-scoring unit gets the next free worker, so workers follow the bottleneck (RA §6).
- A kind's measured scaling curve caps how many of its units run at once.
- The contention model stops adding readers when disk bandwidth is saturated.

### 9.6 Learning during the run
- Every finished unit's outcome is passed to `observe`, so the rest of the run is predicted from what this run has measured.
- When the observed error leaves the model's predicted spread for several units in a row, the key is marked drifting: its spread widens and the policy turns more conservative (lower memory headroom, shallower lookahead) until the error settles.
- At the end, the run's observations are written to the store.

### 9.7 What it reports
- Step-tagged events for each unit's lifecycle (KA8).
- An event for every decision that departs from the plan, with the runner-up and both scores.
- The run's report gains:
  - the order actually run;
  - the prediction error per target;
  - decision time;
  - spill and reload bytes;
  - peak and reserved memory.

## 10. The objective

`cost.objective` in the job:
- `time`, the default: the earliest finish;
- `credits`: the least memory × time + CPU × time + tokens + egress;
- `time_within { credits }`: the earliest finish whose predicted p95 credits stay under the ceiling.

The simulator scores with the objective's measure; nothing else changes.

## 11. Where it lives

| crate | holds | depends on |
|---|---|---|
| `moruna-cost` | `Features`, `CostKey`, `Estimate`, `UnitCost`, `Outcome`, the `CostModel` trait and registry, the default model, `ObservationStore` and its local adapter, the simulator | `moruna-job` only |
| `moruna-job` | `explain(job, catalogue, stats, model, store) -> Plan`, the plan document (KA7) | `moruna-cost` |
| `moruna-runtime` | warm-up (`moruna check` with profiling), the buffer catalog over placement, the `SchedulingPolicy` trait and its four adapters, online observation | both |

An embedder (Griot) depends on `moruna-job` and `moruna-cost` to plan in its own process, and supplies an `ObservationStore` adapter.

## 12. How it is proved

1. **The simulator against reality.** For every benchmark run, the simulated finish time, peak memory and spill bytes are compared with the measured ones. Each workload class has an error budget, and the test fails when it is exceeded.
2. **The default model's accuracy.** A held-out split of warm-up and run observations: median absolute percentage error per target, reported per release.
3. **The swap.** A second model adapter (a test model that predicts a constant) is registered, configured, fitted from the store and used by `explain` and the scheduler, with no change outside its own module. This is the test of decision 3.
4. **The policies.** RA §9's matrix runs every policy adapter on RA §7's workloads, varying memory-to-data ratio, batch size, branching, reuse, CPU-to-I/O ratio, skew, selectivity and workers. The default policy is the one that wins on finish time without losing on correctness, and the matrix runs in the benchmark suite on every release.
5. **The hard rule.** A property test: under any sequence of predictions, wrong by any factor, the scheduler never reserves past the budget. When an outcome exceeds the prediction, the run resizes or spills; it never runs out of memory.

## 13. Build order

| item | what | depends on |
|---|---|---|
| KD6a | `moruna-cost`: the types, the model port and registry, the default model, the observation store port and local adapter | `moruna-job` (KA4) |
| KD6b | the node benchmark and hardware class; the live capacity reading | KD6a |
| KD6c | warm-up profiling in `moruna check` | KD6a, KD6b |
| KD6d | the simulator, and `explain` returning the plan | KD6a, KA5 (DAG), KA7 (plan document) |
| KD7a | the buffer catalog with known-future eviction, over placement | KA5 |
| KD7b | the policy port and the `admission` and `fifo` adapters, with the benchmark matrix | KD7a |
| KD7c | `residency_greedy` and `lookahead`, online learning and drift | KD6d, KD7b |
| KD7d | the default policy chosen from the matrix; decisions reported | KD7c |
