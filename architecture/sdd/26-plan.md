# Moruna SDD 26: The plan of a job (`moruna-plan`)

**Document type:** software design document, component 26
**Status:** DRAFT · 2026-10-29 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** board items KA7 (`BOARD.md`, wave B) and KD6c (wave D, the simulated planner); `architecture/adaptive-runtime.md` sections 0, 4, 8 and 11; SDD 22 (the job, its DAG and its barriers); MH 4.3 (events version 6)
**Preamble:** `00-preamble.md`; **Cited:** 22 (d.4, e.4, e.5, JB-I11, JB-I13, JB-I14), 12 (PY-I11, PY-I12), MH 4.1, MH 4.3
**Component location:** `crates/moruna-plan/` (`src/plan.rs`, `src/flatbuffer.rs`, `src/planner.rs`, `src/costing.rs`, `src/levels.rs`, `src/simulated.rs`, `src/check.rs`, `src/explain.rs`, `schema/plan.fbs`, `src/generated/plan_generated.rs` (generated, committed), `tests/plan.rs`, `tests/simulated.rs`), `tools/flatc/generate.sh`; in `moruna-runtime`, `src/job/plan.rs`, `src/job/build.rs` (the stage order), `src/host/lifecycle.rs` (steps, deviations), `src/host/session.rs` (`run_plan`), `src/host/protocol.rs` (events version 6), `src/host/cli.rs` (`moruna explain`, `moruna run --plan`), `tests/plan.rs`; in `moruna-py`, `src/explain.rs` and `python/moruna/__init__.py` (`moruna.explain`)
**Consumes:** `moruna-job`, `moruna-cost` (the estimate types, the `CostModel` port, morsel sizing and the simulator, SDD 24), `flatbuffers` (25.12.19, the job document's), `serde`, `serde_json`, `sha2`, `thiserror`. **Consumed by:** `moruna-runtime`, `moruna-py`, and every embedder that shows a job's plan before it submits it.

**Decisions worth your eye:** (1) the plan lives in a crate of its own above `moruna-job`, because `moruna-cost` depends on `moruna-job` and the plan's estimates are the cost model's, so the plan cannot live in `moruna-job` (`architecture/adaptive-runtime.md` section 11 is corrected to say so); (2) a step's id is a hash of the job's digest, the planner's name and version, and the places of its tasks, so the same job planned by the same planner has the same ids on any machine, and a host that renames its tasks keeps them; (3) `check_plan` is an assertion on planners, run on every plan before it is shown or run, not a gate for people; (4) a run always starts from a plan: `run(job)` is `run(levels(job))`, and the build numbers the kernel stages in the plan's order, which is the scheduler's starting order; (5) the plan's estimate types are `moruna-cost`'s, and its estimate table gained only the appended `extrapolated` mark, so plans written before still read; (6) `explain` with no planner named takes `simulated` when the inputs carry a cost model and `levels` otherwise (`default_planner`), and runs keep `levels` (`DEFAULT_PLANNER`) until the runtime configures a model; (7) a simulated plan's groups are what overlapped in the simulated schedule, so steps that ran one after another on few workers are in sequence even when the DAG would let them run side by side.

---

## a. Purpose and boundary

An embedder submits a job and wants to know, before it runs, what Moruna will run when and how: which tasks run side by side, which wait for which, what each is given, what each is expected to cost. Its screen shows that plan, and the run's events light up its steps as they start and end. This component is that plan: the document, the port planners implement, the first planner, the check every plan passes, and `explain`, which an embedder calls in its own process; and, in the runtime, the run that starts from a plan and reports against it.

It owns: the plan document and its two forms (FlatBuffers on the wire, JSON for people), the step ids, the `Planner` port and the `Planners` registry, the `levels` and `simulated` planners, `PlanInputs` (counts-only table statistics, the run's resources, an optional `moruna-cost` model, kernels' declared figures, cost keys, the disk's bandwidth), how a job's tasks become units the model costs (`cost_key`, f.6), the workload the simulated planner plays (`Simulated::simulation`), `check_plan`, `explain` and `default_planner`, and the job's digest (`job_digest`, MH 4.1), which the runtime now takes from here.

It refuses to know: how a kind is built or run (the runtime's), the cost model's fitting and the simulator's mechanics (`moruna-cost`'s, SDD 24 f.8), tenant values (PN-I7), and any embedder.

## b. Vocabulary

**Plan.** Moruna's answer for one job: its steps in the order they start, and the job's totals.

**Step.** One task of the job, or a fused group of them, with its dependencies, its group, its resources and its estimated cost.

**Group.** The steps that may run beside each other. Groups start in their order.

**Planner.** An adapter of the `Planner` port: a job in, a plan out.

**Deviation.** A step that started while a step of an earlier group had not.

**Unit.** One task applied to one batch; a task's units are its rows over its morsel (SDD 24 f.10).

## c. Invariants

**PN-I1. Step ids are stable.** A step's id is `s` and the first 16 hex characters of the SHA-256 of `moruna-plan step`, the job's digest, the planner's name and version and the places of its tasks (f.1). The same job planned twice by the same planner has the same step ids; a host that renames its tasks has the same job (SDD 22 JB-I14) and the same ids; another planner version or another job has others. The plan's id is its content address: 32 hex characters of the SHA-256 of the plan with its id empty and its creation time zero.

**PN-I2. `levels` steps through the DAG by level.** Every task is a step. A task's level is one more than the deepest level of the tasks it takes input from, a source's 0; a group is a level; steps are listed by level, document order within one. Each step waits for the steps of the tasks it takes input from.

**PN-I3. Resources are split, estimates are declared or asked.** A `levels` step's resources are the run's divided by its group's size (workers at least one). Without a cost model every estimate's basis is `Default` resting on no observation, its numbers the declared defaults of f.2; with one, the planner asks it about each task's unit and carries its answer over the task's units (f.6), refused naming the step when the model answers with an estimate no plan carries.

**PN-I4. Every plan is checked before it is shown or run.** `plan_with` (and `explain`, and the runtime's build) run `check_plan`: the plan names the job's digest; its step ids are unique; every task is run by exactly one step and no step runs a task the job does not have or no task; the steps are listed in the order their groups start; every step waits for the steps whose output it takes and every step it waits for is in an earlier group; within a step, tasks are listed so every edge goes forward. A refusal names its step (or the plan) with a code and a sentence. A plan that fails is never run; the run fails `plan_refused` naming the step. `plan_with` also refuses a plan that names another planner as its maker or whose id is not its content address.

**PN-I5. Nothing moves across a contract barrier.** `check_plan` refuses a step that fuses a barrier task with any other task or runs both ends of a barrier edge. The runtime's build never fuses two kernels into one stage across a barrier task or a barrier edge (SDD 22 e.5).

**PN-I6. Nothing is given more than the job's ceiling.** The plan's resources are within the job's ceiling (its budget's elastic maxima, its start, or the plan's own where the budget leaves them to discovery); each step's within the plan's, its start within its ceiling; and a group's starts add up to no more than the plan's memory.

**PN-I7. Statistics are counts.** `TableStats` holds rows, file bytes and per column the null and distinct counts; it has no field for a bound, a value or a sample, because those are tenant values (`architecture/adaptive-runtime.md` section 3).

**PN-I8. Bytes from anyone are verified before they are read.** `Plan::from_flatbuffer` checks the identifier (`MPL1`), runs the FlatBuffers verifier with bounded depth, tables and size, checks the version, then every value: known enums, estimates that are finite, not negative, ordered and of their field's unit, ranges likewise, ids and task names present. Whatever the bytes are, the result is a plan or a typed `PlanBytesError`, never a panic.

**PN-I9. A planner is a setting.** Planners register in `Planners` by name (a kind's name rule; one name, one planner) and `explain` takes the name. A planner of a test's own registers and is chosen with no change outside its module, and its plan is checked like any other.

**PN-I10. A run starts from a plan and says so.** `run_job` plans with the default planner and runs the plan (`run_plan`); `moruna run --plan` and `moruna serve`'s `spec` with `plan` run a given one. Every task event carries `step`, `attempt_started` carries `plan`, and `step_started`/`step_ended` tell each step's start and end once (events version 6, MH 4.3). The report (the file's `plan` and the `report` message's `plan`) names the plan, the order the steps started in, each step's start, end, outcome, planned resources and use, and every deviation.

**PN-I11. Every deviation is reported with its reason.** When a step starts while a step of an earlier group has not started or ended, the run records a deviation naming both, with the earlier step's reason: `ready` (a task of it had its input and was not yet taken), `engine_preparing` (a source the engine had not started), or `awaiting_input` with the steps whose output it was waiting for. The scheduler may start work ahead of the plan wherever the dependencies allow (KD7 makes that adaptive); it never hides that it did.

**PN-I12. The plan's order is the scheduler's starting order.** The build numbers the kernel stages in the order the plan lists their tasks, so the admission rule's tie toward the later stage (SDD 10 f.3) follows the plan; a plan that lists them otherwise numbers them otherwise.

**PN-I13. The plan is on the command line and in Python.** `moruna explain <job> [--planner <name>] [--out <plan>]` prints the plan's JSON and writes its document; `moruna.explain(job, planner=None)` returns the plan as a dict.

**PN-I15. `explain` chooses by the model.** With no planner named, `explain`'s caller takes `default_planner(inputs)`: `simulated` when the inputs carry a cost model, `levels` otherwise; either may be named; `simulated` without a model is refused at `model`.

**PN-I16. A simulated plan is the simulation's.** The `simulated` planner's totals (finish, peak memory, memory-seconds, worker-seconds, disk bytes: p50 and p95) are `moruna-cost`'s forecast of the workload it builds (`Simulated::simulation`), its critical path the medians' run's; each step's time, worker time, peak and spill are its task's in the forecast and its rows, bytes, state and reads its units'; a step's group is one more than the latest of the steps it waits for and of the steps that ended before it started, so what overlapped shares a group; the same job planned twice is the same plan. A step whose unit can never fit is `Infeasible`, naming the step and its task.

**PN-I17. A task's units are its rows over its morsel.** With its rows known (a source's from its table's counts, others' from what their producers give), a task's units are its rows over the morsel `moruna_cost::morsel` chooses for its step's memory per worker and its workers; without, a source has one unit and another task as many as its largest producer.

**PN-I14. The generated code is what the schema generates.** `src/generated/plan_generated.rs` is flatc 25.12.19's output for `schema/plan.fbs`; `tools/flatc/generate.sh --check` regenerates both the job's and the plan's and fails on a difference, in CI.

**PN-I18. The plan and the run see one checkpoint cadence.** The simulated planner reads `checkpoint.interval_ms` through `moruna_job::checkpoint_interval_ms` (SDD 22 JB-I15), the clamp the runtime's build uses, so for any interval the plan's checkpoints come at the run's cadence.

## d. Interfaces

### d.1 The document

```rust
pub struct Plan { id, job_digest, planner: PlannerId, model: Option<String>, created_unix_ms,
                  resources: Resources, steps: Vec<Step>, totals: Totals }
pub struct Step { id, tasks: Vec<String>, depends_on: Vec<String>, group: u32,
                  resources: Resources, cost: UnitCost }
pub struct Resources { workers: u32, memory_start_bytes: u64, memory_ceiling_bytes: u64 }
pub use moruna_cost::{Basis, Estimate, Target, Unit, UnitCost};     // SDD 24 d.1
pub fn weakest(steps: &[Step]) -> (Basis, u32);
pub struct Totals { basis, observations, finish: Range, critical_path: Vec<String>,
                    peak_memory_bytes: Range, memory_byte_seconds: Range,
                    worker_seconds: Range, disk_bytes: Range }
impl Plan { fn from_flatbuffer(&[u8]) -> Result<Plan, PlanBytesError>; fn to_flatbuffer(&self) -> Vec<u8>;
            fn to_json(&self) -> String; fn step_of(&self, task) -> Option<&Step>; fn groups(&self) }
pub fn job_digest(job: &Job) -> String;     // sha256: of Job::identity_json (MH 4.1)
pub fn step_id(job_digest: &str, planner: &PlannerId, places: &[usize]) -> String;
```

`Estimate`, `Unit`, `Basis` and `UnitCost` are `moruna-cost`'s (`architecture/adaptive-runtime.md` section 4, with `Estimate`'s `extrapolated` mark).

### d.2 The planner port

```rust
pub trait Planner: Send + Sync {
    fn id(&self) -> PlannerId;
    fn plan(&self, job: &Job, catalogue: &Catalogue, inputs: &PlanInputs<'_>) -> Result<Plan, PlanError>;
}
pub struct Planners;  // empty(), standard() (levels), add(planner), get(name), names()
pub struct Planners;  // empty(), standard() (levels, simulated), add(planner), get(name), names()
pub struct PlanInputs<'a> { tables: BTreeMap<String, TableStats>, resources: Resources,
                            model: Option<&'a dyn moruna_cost::CostModel>,
                            kernels: BTreeMap<String, moruna_cost::Kernel>,
                            keys: BTreeMap<String, moruna_cost::CostKey>,
                            disk_bytes_per_second: Option<u64> }
pub struct TableStats { rows: Option<u64>, file_bytes: Option<u64>,
                        columns: BTreeMap<String, ColumnStats { nulls, distinct }> }
pub fn cost_key(task: &Task) -> CostKey;
pub fn default_planner(inputs: &PlanInputs<'_>) -> &'static str;
pub const DEFAULT_PLANNER: &str = "levels"; pub const DISK_BYTES_PER_SECOND: u64 = 500_000_000;
pub struct Simulated { pub draws: u32, pub seed: u64 }   // default: 200 draws, SEED
impl Simulated { fn simulation(&self, job, catalogue, inputs) -> Result<Simulation, PlanError> }
pub struct Simulation { pub workload: moruna_cost::sim::Workload, pub machine: moruna_cost::sim::Machine,
                        pub places: Vec<usize> }
```

`PlanInputs::for_job(job, discovered)` takes the job's budget where it sets one and `discovered` elsewhere; `with_model`, `with_table`, `with_kernel`, `with_key` and `with_disk` add the rest; `model_id()` is the model's `name@version`, which the plan names.

### d.3 The check

`check_plan(&Plan, &Job) -> Result<(), Vec<PlanRefusal>>`; `PlanRefusal { step: Option<String>, code: PlanRefusalCode, message }`, codes `job_mismatch`, `duplicate_step`, `unknown_task`, `duplicate_task`, `missing_task`, `empty_step`, `unknown_dependency`, `dependency_violated`, `group_order`, `barrier_crossed`, `over_ceiling`. `job_ceiling(&Job, Resources) -> Resources`.

### d.4 `explain`

`explain(job, catalogue, inputs, planners, planner_name) -> Result<Plan, PlanError>` checks the job against the catalogue (`moruna_job::check`, every refusal: `PlanError::Job`), then `plan_with(planner, job, catalogue, inputs)`, which plans and asserts (PN-I4). A caller that names no planner passes `default_planner(&inputs)` (PN-I15). `PlanError` is `Job`, `UnknownPlanner`, `Registry`, `Inputs`, `Planner`, `Infeasible`, `Model` or `Refused`.

### d.5 The runtime

`moruna_runtime::job::plan`: `discovered(job)`, `levels(job, discovered)`, `default_plan(job)`, `explain(job, planner, kinds)`, `task_order(job, plan)`. `job::build(job, plan, kinds, opts)` refuses a plan that fails `check_plan` (`Plan` error, `plan_refused`) and numbers stages in the plan's order. `host::session::run_job` (default plan) and `run_plan(plan, job, ...)`; `run_file(path, plan_path, ...)`. `host::lifecycle::{Layout, Steps, PlanRun, StepRun, StepUse, Deviation, Behind, DeviationReason}`. `moruna explain` and `moruna run --plan` in `host::cli`; `_core.explain` and `moruna.explain` in Python.

## e. Data model, formats and state machines

### e.1 The FlatBuffers schema

`crates/moruna-plan/schema/plan.fbs`, namespace `moruna.plan.fb`, file identifier `MPL1`, extension `mplan`, root `Plan`:

| Table | Fields |
|---|---|
| `Plan` | `version: uint` (1), `id` (required), `job_digest` (required), `planner: Planner` (required), `model`, `created_unix_ms: ulong`, `resources: Resources` (required), `steps: [Step]` (required), `totals: Totals` (required) |
| `Step` | `id` (required), `tasks: [string]` (required), `depends_on: [string]`, `group: uint`, `resources: Resources` (required), `cost: UnitCost` (required) |
| `UnitCost` | the nine `Estimate`s of section 4, each required |
| `Estimate` | `p50`, `p95: double`, `unit: Unit`, `basis: Basis`, `observations: uint`, `extrapolated: bool` (appended: a plan written before reads as not extrapolated) |
| `Resources` | `workers: uint`, `memory_start_bytes`, `memory_ceiling_bytes: ulong` |
| `Planner` | `name` (required), `version: uint` |
| `Totals` | `basis`, `observations`, `finish: Range` (required), `critical_path: [string]`, `peak_memory_bytes`, `memory_byte_seconds`, `worker_seconds`, `disk_bytes: Range` (required) |
| `Range` (struct) | `p50`, `p95: double` |

The JSON rendering (`Plan::to_json`) adds `"moruna_plan": 1` and `groups`, the steps' ids by group in order; nothing reads it back.

### e.2 Events (MH 4.3 versions 6 and 7)

`attempt_started` gains `plan`; every `task_*` event gains `step` after `task`; `step_started { step, plan, group }`; `step_ended { step, plan, outcome, duration_us }`, `outcome` one of `failed`, `cancelled`, `stopped`, `skipped`, `succeeded`. Version 7 (KA8) moved events onto the record format with these fields unchanged (`crates/moruna-event/schema/event.fbs`, PY-I13): the record's headers say the plan and the step, so a reader that routes by header never opens the body: every event of a planned run carries the header `plan`; every event of a task carries `step` and `task`; `step_started` and `step_ended` carry `step`; `attempt_started` carries `plan`. A document refused before it was planned has no plan to name, and its events carry no `plan` or `step`.

### e.3 The report

The report file's envelope and the `report` message gain `plan`: `{ plan, planner, job_digest, order: [step], steps: [{ step, tasks, group, start_us, end_us, outcome, resources, used: { morsels, rows_in, rows_out, bytes_in, bytes_out, worker_seconds, state_bytes_max } }], deviations: [{ step, t_us, ahead_of: [{ step, reason, blocking: [step] }] }] }`. `worker_seconds` and `state_bytes_max` come from the run report's stages for a step's kernel stages; a fused stage's figures count for every step it carries.

## f. Algorithms and policies

**f.1 Step ids.** `s` + hex(SHA-256(`moruna-plan step\0` + job digest + `\0` + planner name + `\0` + planner version + `\0` + task places joined by `,`))[..16]. Places, not ids, because the job's digest names tasks by place (JB-I14).

**f.2 `levels`' declared defaults.** A source gives the rows and file bytes its `TableStats` count and reads those bytes; a kernel and a sink give what they take (a fan-in adds its inputs, a fan-out gives each consumer the producer's output); a count unknown anywhere upstream is a declared zero downstream. A step may hold the memory it is given: peak memory p50 its start, p95 its ceiling. Times, retained memory, state and spill are declared zero.

**f.3 Totals without the simulator.** Finish: the longest path of the steps' durations through their dependencies, p50 and p95 each on its own. Critical path: the p50 path, more steps first on a tie, then the earlier in plan order. Peak: the largest group's sum of peaks. Held: memory as peak times duration summed, worker time summed, disk as read plus spill summed. Basis: the weakest of any estimate; observations: the fewest. KD6c's simulator replaces these for its own planner (`architecture/adaptive-runtime.md` section 8); a job's cost is then the simulation's, not a sum.

**f.4 The check's order.** The digest, step ids and tasks, missing tasks, group order and dependencies, edges between steps, order within a step, barriers, ceilings; every refusal is collected.

**f.5 A run from a plan.** `run_job` plans with `levels` for the job's budget, discovery filling what it leaves (`job::plan::default_plan`); a planning refusal of the job is the build's refusal at the same field. `build` checks the plan, then numbers the kernel stages in `task_order(job, plan)`, fusing only where no barrier stands. The lifecycle maps each of the run's tasks (a fused stage is one) to the steps it carries; a step starts with its first task and ends with its last.

**f.6 Tasks as units.** Over the job in an order every edge goes forward: a task's key is `PlanInputs::keys`' for it or `cost_key(task)` (its role and kind; a fingerprint of `moruna-plan kind`, the role and the kind; the SHA-256 of the canonical form of the task alone; a schema of zeros); its kernel figures are `PlanInputs::kernels`' with the declaration's facts filling what they leave; its rows and bytes in are its table's counts (a source) or the sum of its producers' rows and bytes out (unknown when any is); a unit's inputs carry the rows and bytes per unit, the table's columns (null fraction, distinct), its kernel, one worker, the step's resources and the disk's bandwidth, its dependencies, consumers and barrier. Its units follow PN-I17; one unit is predicted at its rows in over its units, rounded up, and its rows and bytes out over the task are a unit's times its rows in over a unit's rows. `levels` with a model gives a step `min(workers, units)` units at once: time and worker time summed over the units (`Estimate::sum`), time divided by the kind's speed-up at that many and its start-up added, peak that many units' peaks, rows, bytes, state, reads and spills over its rows, retained memory one unit's. Start-up is the kernel's cold figure, or its warm one when only that is known.

**f.7 The simulated planner.** The workload is the tasks in the order of f.6, each `units` units of its unit's cost, its scaling curve from its kernel's speed-ups, its start-up, its checkpoint figures when the job checkpoints (every `checkpoint.interval_ms` as `moruna_job::checkpoint_interval_ms` clamps it, PN-I18), stateful when its kernel says so; the machine is the run's workers, its memory at the start and the disk's bandwidth (`DISK_BYTES_PER_SECOND` when the inputs give none). The planner forecasts it under the admission rule with `draws` draws (SDD 24 f.9) and builds PN-I16's plan. A step's resources: its task's most units at once (at least one) as workers, its simulated peak as its memory at the start and the run's ceiling as its ceiling; where a group's starts add past the run's ceiling (their peaks came at different moments) they are scaled to it. Steps are listed by group, then simulated start, then the order of f.6.

## g. Concurrency within the component

`moruna-plan` has none: a plan is a value. In the runtime the lifecycle's step state lives under the lifecycle's one lock, so step events are stamped and told in the order they became true (PY-I12).

## h. Behaviour

A job with one source and one sink is two steps in two groups. A plan of a job refused by the graph check is never made: `levels` runs `check_graph` first. A step that never started ends `skipped` with `duration_us: null`. A document refused before it was planned tells its task events with `step: null` and `attempt_started` with `plan: null`.

## i. Configuration

The planner is a setting: `explain`'s `planner` argument, `moruna explain --planner`, `moruna.explain(planner=...)`; the default is `levels` (`DEFAULT_PLANNER`). A run from `moruna run` uses the default unless `--plan` gives a plan.

## j. Observability

The plan's JSON, the step events, and the report's `plan` section (e.3).

## k. Tests

Test names are the ids' lowercase with a slug. PN-T1 to PN-T20 are `crates/moruna-plan/tests/plan.rs` and PN-T28 to PN-T34 `crates/moruna-plan/tests/simulated.rs`; PN-T21 and PN-T22 have a lifecycle half in `crates/moruna-runtime/src/host/lifecycle/tests.rs` and a run half in `crates/moruna-runtime/tests/plan.rs`, as do PN-T23 to PN-T25 and PN-T27; PN-T26 is `python/tests/test_explain.py`.

**PN-T1 the_same_job_gives_the_same_ids.** Twice-planned ids and plan id equal; the clock is not in the id; renamed tasks keep their ids; another planner version or job gives others. PN-I1.

**PN-T2 to PN-T5 levels on a chain, a fan-out, a fan-in, a diamond.** The groups are as expected; each step waits for its producers' steps; a later task ready earlier starts in an earlier group. PN-I2.

**PN-T6 resources_split_across_a_group.** A group's steps split the run; a group wider than the workers gives each one; the budget gives the run's resources, discovery filling the rest; inputs a run cannot have are refused by field. PN-I3, PN-I6.

**PN-T7 declared_defaults_follow_the_counts.** Rows and bytes from the table statistics through a fan-in; every estimate `Default` with no observation. PN-I3, PN-I7.

**PN-T8 the_levels_planner_asks_the_model.** A `moruna-cost` model's answers are carried, keyed by the task's key, with the declaration's facts handed over; the totals follow its durations (critical path, finish, peak, held); an estimate with p95 below p50 is refused naming the step and the model's `name@version`. PN-I3.

**PN-T9 to PN-T14 the check's refusals.** A missing task; a duplicated task; a reversed dependency (out of order, not waiting, fused in the wrong order); a crossed barrier (task and edge); an over-ceiling step, group and plan; another job, a duplicate step id, an unknown task, an empty step, an unknown dependency, groups out of order. Each plan made by hand. PN-I4, PN-I5, PN-I6.

**PN-T15 round_trip_and_rendering.** FlatBuffers round trip whole and byte-stable; the JSON names version, groups, steps and totals. PN-I8.

**PN-T16 the_verifier_refuses_corrupted_bytes.** 4,000 seeded corruptions (bytes set, truncations) each a plan or a `PlanBytesError`, never a panic; most refused. PN-I8.

**PN-T17 bytes_are_refused_by_what_they_are.** No identifier, too short, another version, a non-finite estimate, a unit that is not its field's, empty ids, an unordered range. PN-I8.

**PN-T18 a_test_planner_is_chosen_by_setting.** A test planner registers beside `levels` and `simulated` and is chosen by name; its plan is checked and refused when it violates a dependency; an unknown planner, a taken name, a name that is not a kind's, a plan naming another maker and a forged id are refused. PN-I4, PN-I9.

**PN-T19 explain_checks_the_job_first.** An unknown kind is refused with the job's refusals; a cycle is refused by the planner's graph check. PN-I4.

**PN-T20 totals_at_the_edges.** No steps, and dependencies pointing forward. f.3.

**PN-T21 a_run_from_its_plan.** Every task event names its step, the attempt the plan; every step starts and ends once; the report's order is the order steps started; each step's start, end, outcome, use and planned resources; the report file and message carry it. PN-I10.

**PN-T22 a_forced_deviation_is_reported_with_its_reason.** A step made to wait on a slow input: a later group's step starts first and the deviation names the waiting step, `awaiting_input` and the slow step; `ready` and `engine_preparing` each when they hold; a step that never starts ends skipped, a failed task's step failed. The run half runs a slow Parquet source beside a fast path. PN-I11.

**PN-T23 an_invalid_plan_is_refused_naming_the_step.** A run given a plan whose step does not wait for its input fails `plan_refused` naming the step, with nothing started; another job's plan is refused as such. PN-I4.

**PN-T24 the_stages_follow_the_plan.** The kernel stages are numbered in the plan's order, and in another valid order when the plan lists one. PN-I12.

**PN-T25 explain_and_run_from_the_plan_it_wrote.** `moruna explain` prints the plan and writes it; an unknown planner, a refused document and a missing file exit 2; `moruna run --plan` runs from the plan written; bytes that are not a plan are refused at `plan`; the command lines parse. PN-I13.

**PN-T26 explain_returns_the_plan** (Python). `moruna.explain` returns the plan as a dict, from a path or bytes, the same ids twice; an unknown planner is a `PlanError`, an unknown kind a `ConfigError`. PN-I13.

**PN-T27 nothing_is_fused_across_a_barrier.** Two standard kernels fuse into one stage, and do not across a barrier task or a barrier edge. PN-I5.

**PN-T28 the_simulated_planner_end_to_end.** `loglinear` fitted from 200 synthetic units of each task of a diamond (task `b` five times slower), a source of 10⁷ rows: `explain` with `default_planner` plans with `simulated` and names `loglinear@1`; its totals equal the forecast of `Simulated::simulation`'s workload (finish, peak, memory-seconds, worker-seconds, disk bytes), its critical path the medians' run's, through `b` to the sink; each step's time, worker time and peak its task's, `Measured`; the two arms share a group; planned again, the same plan id; the plan round-trips. PN-I15, PN-I16, PN-I17.

**PN-T29 explain_chooses_the_planner_by_the_model.** No model: `default_planner` is `levels`, and `simulated` named is refused at `model`; a model: `default_planner` is `simulated`, `levels` named asks the model; rows flow from the table's counts, and a fan-in adds its inputs. PN-I15, PN-I17.

**PN-T30 morsels_in_the_plan.** A model with a hundred times the per-batch overhead gives the source fewer units; a 64 MiB run more units than a 16 GiB one. PN-I17.

**PN-T31 infeasible_steps_and_once_per_process_costs.** A unit with a 4 GiB peak in a 1 GiB run is `Infeasible`, naming the source's step and task; a declared cold start-up of 2 s lengthens the finish by it; checkpoints every 500 ms of half a second lengthen a many-batch source's further, and none when the job does not checkpoint. PN-I16.

**PN-T32 one_worker_runs_in_sequence.** On one worker every step is its own group and the finish is the sum of the steps' times. PN-I16.

**PN-T33 the_extrapolated_mark_round_trips.** A plan's steps are not extrapolated by a model that never says so; one marked round-trips through the bytes and renders in the JSON. PN-I8.

**PN-T34 the_planner_clamps_the_checkpoint_interval_as_the_run_does.** For 1, 100, 500, 5,000, 60,000, 120,000 ms and `u64::MAX`, the simulated planner's interval is `moruna_job::checkpoint_interval_ms`'s; a job that does not checkpoint plans none. Its runtime half (`crates/moruna-runtime/src/job/tests.rs`, `pn_t34_the_run_and_the_planner_see_one_checkpoint_interval`) builds each as a run and asserts the run's `checkpoint_interval_ms` is the planner's. PN-I18.

PN-I14 is CI's `tools/flatc/generate.sh --check`.

## l. Implementation notes for the agent

A new planner is an adapter of `Planner` in its own module, registered in `Planners`; it computes step ids with `step_id` and builds its plan with `Plan::new`, and `plan_with` checks it. The simulator-based planner of KD6c is one. A change to `schema/plan.fbs` is a run of `tools/flatc/generate.sh` and a commit of its output; a new field is appended to its table so older plans still read.

## m. Open items

None.

## n. Traceability

| Parent | Invariant | Test |
|---|---|---|
| KA7 (the plan document, stable ids) | PN-I1, PN-I8, PN-I14 | PN-T1, PN-T15 to PN-T17 |
| KA7 (the planner port, `levels`) | PN-I2, PN-I3, PN-I7, PN-I9 | PN-T2 to PN-T8, PN-T18, PN-T20 |
| KA7 (the internal check) | PN-I4, PN-I5, PN-I6 | PN-T9 to PN-T14, PN-T19, PN-T23, PN-T27 |
| KA7 (runs from a plan), MH 4.3 version 6 | PN-I10, PN-I11, PN-I12 | PN-T21, PN-T22, PN-T24 |
| KA7 (CLI and Python) | PN-I13 | PN-T25, PN-T26 |
| KD6c (the simulated planner, `explain`'s choice, morsels, one checkpoint cadence) | PN-I15 to PN-I18 | PN-T28 to PN-T34 |
| `architecture/adaptive-runtime.md` section 11 | the crate layout | `crates/moruna-plan/Cargo.toml` |

## o. Deferred (post-v1)

**PN-O1. Adaptive deviation (KD7).** The scheduler deviating on purpose, with the runner-up and both scores in a decision event (section 9.7).

**PN-O3. Events as records (KA8), done.** The step events and fields above are on the record format (e.2, PY-I13, PY-T30 to PY-T37).
