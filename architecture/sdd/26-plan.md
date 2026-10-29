# Moruna SDD 26: The plan of a job (`moruna-plan`)

**Document type:** software design document, component 26
**Status:** DRAFT · 2026-10-29 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** board item KA7 (`BOARD.md`, wave B); `architecture/adaptive-runtime.md` sections 0, 4, 8 and 11; SDD 22 (the job, its DAG and its barriers); MH 4.3 (events version 6)
**Preamble:** `00-preamble.md`; **Cited:** 22 (d.4, e.4, e.5, JB-I11, JB-I13, JB-I14), 12 (PY-I11, PY-I12), MH 4.1, MH 4.3
**Component location:** `crates/moruna-plan/` (`src/plan.rs`, `src/flatbuffer.rs`, `src/planner.rs`, `src/levels.rs`, `src/check.rs`, `src/explain.rs`, `schema/plan.fbs`, `src/generated/plan_generated.rs` (generated, committed), `tests/plan.rs`), `tools/flatc/generate.sh`; in `moruna-runtime`, `src/job/plan.rs`, `src/job/build.rs` (the stage order), `src/host/lifecycle.rs` (steps, deviations), `src/host/session.rs` (`run_plan`), `src/host/protocol.rs` (events version 6), `src/host/cli.rs` (`moruna explain`, `moruna run --plan`), `tests/plan.rs`; in `moruna-py`, `src/explain.rs` and `python/moruna/__init__.py` (`moruna.explain`)
**Consumes:** `moruna-job`, `flatbuffers` (25.12.19, the job document's), `serde`, `serde_json`, `sha2`, `thiserror`; `moruna-cost` once it is on `main` (section m). **Consumed by:** `moruna-runtime`, `moruna-py`, and every embedder that shows a job's plan before it submits it.

**Decisions worth your eye:** (1) the plan lives in a crate of its own above `moruna-job`, because `moruna-cost` depends on `moruna-job` and the plan's estimates are the cost model's, so the plan cannot live in `moruna-job` (`architecture/adaptive-runtime.md` section 11 is corrected to say so); (2) a step's id is a hash of the job's digest, the planner's name and version, and the places of its tasks, so the same job planned by the same planner has the same ids on any machine, and a host that renames its tasks keeps them; (3) `check_plan` is an assertion on planners, run on every plan before it is shown or run, not a gate for people; (4) a run always starts from a plan: `run(job)` is `run(levels(job))`, and the build numbers the kernel stages in the plan's order, which is the scheduler's starting order; (5) the plan's estimate fields are section 4's `UnitCost` exactly, so KD6c fills them without a format change.

---

## a. Purpose and boundary

An embedder submits a job and wants to know, before it runs, what Moruna will run when and how: which tasks run side by side, which wait for which, what each is given, what each is expected to cost. Its screen shows that plan, and the run's events light up its steps as they start and end. This component is that plan: the document, the port planners implement, the first planner, the check every plan passes, and `explain`, which an embedder calls in its own process; and, in the runtime, the run that starts from a plan and reports against it.

It owns: the plan document and its two forms (FlatBuffers on the wire, JSON for people), the step ids, the `Planner` port and the `Planners` registry, the `levels` planner, `PlanInputs` (counts-only table statistics, the run's resources, an optional cost model), the `Estimator` port a planner asks a cost model through, `check_plan`, `explain`, and the job's digest (`job_digest`, MH 4.1), which the runtime now takes from here.

It refuses to know: how a kind is built or run (the runtime's), the cost model's fitting (`moruna-cost`'s), the simulator (KD6c's), tenant values (PN-I7), and any embedder.

## b. Vocabulary

**Plan.** Moruna's answer for one job: its steps in the order they start, and the job's totals.

**Step.** One task of the job, or a fused group of them, with its dependencies, its group, its resources and its estimated cost.

**Group.** The steps that may run beside each other. Groups start in their order.

**Planner.** An adapter of the `Planner` port: a job in, a plan out.

**Deviation.** A step that started while a step of an earlier group had not.

## c. Invariants

**PN-I1. Step ids are stable.** A step's id is `s` and the first 16 hex characters of the SHA-256 of `moruna-plan step`, the job's digest, the planner's name and version and the places of its tasks (f.1). The same job planned twice by the same planner has the same step ids; a host that renames its tasks has the same job (SDD 22 JB-I14) and the same ids; another planner version or another job has others. The plan's id is its content address: 32 hex characters of the SHA-256 of the plan with its id empty and its creation time zero.

**PN-I2. `levels` steps through the DAG by level.** Every task is a step. A task's level is one more than the deepest level of the tasks it takes input from, a source's 0; a group is a level; steps are listed by level, document order within one. Each step waits for the steps of the tasks it takes input from.

**PN-I3. Resources are split, estimates are declared or asked.** A `levels` step's resources are the run's divided by its group's size (workers at least one). Without a cost model every estimate's basis is `Default` resting on no observation, its numbers the declared defaults of f.2; with one, the planner asks it about each step and carries its answer, refused naming the step when the model fails or answers with an estimate no plan carries.

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

**PN-I14. The generated code is what the schema generates.** `src/generated/plan_generated.rs` is flatc 25.12.19's output for `schema/plan.fbs`; `tools/flatc/generate.sh --check` regenerates both the job's and the plan's and fails on a difference, in CI.

## d. Interfaces

### d.1 The document

```rust
pub struct Plan { id, job_digest, planner: PlannerId, model: Option<String>, created_unix_ms,
                  resources: Resources, steps: Vec<Step>, totals: Totals }
pub struct Step { id, tasks: Vec<String>, depends_on: Vec<String>, group: u32,
                  resources: Resources, cost: UnitCost }
pub struct Resources { workers: u32, memory_start_bytes: u64, memory_ceiling_bytes: u64 }
pub struct Estimate { p50: f64, p95: f64, unit: Unit, basis: Basis, observations: u32 }
pub struct UnitCost { duration, worker_time, peak_memory, retained_memory, rows_out, bytes_out,
                      state_growth, read_bytes, spill_bytes }            // each an Estimate
pub struct Totals { basis, observations, finish: Range, critical_path: Vec<String>,
                    peak_memory_bytes: Range, memory_byte_seconds: Range,
                    worker_seconds: Range, disk_bytes: Range }
impl Plan { fn from_flatbuffer(&[u8]) -> Result<Plan, PlanBytesError>; fn to_flatbuffer(&self) -> Vec<u8>;
            fn to_json(&self) -> String; fn step_of(&self, task) -> Option<&Step>; fn groups(&self) }
pub fn job_digest(job: &Job) -> String;     // sha256: of Job::identity_json (MH 4.1)
pub fn step_id(job_digest: &str, planner: &PlannerId, places: &[usize]) -> String;
```

`Estimate`, `Unit` (`Seconds`, `Bytes`, `Rows`), `Basis` (`Measured`, `WarmUp`, `Prior`, `Default`) and `UnitCost` are `architecture/adaptive-runtime.md` section 4's, field for field.

### d.2 The planner port

```rust
pub trait Planner: Send + Sync {
    fn id(&self) -> PlannerId;
    fn plan(&self, job: &Job, catalogue: &Catalogue, inputs: &PlanInputs<'_>) -> Result<Plan, PlanError>;
}
pub struct Planners;  // empty(), standard() (levels), add(planner), get(name), names()
pub struct PlanInputs<'a> { tables: BTreeMap<String, TableStats>, resources: Resources,
                            model: Option<&'a dyn Estimator> }
pub struct TableStats { rows: Option<u64>, file_bytes: Option<u64>,
                        columns: BTreeMap<String, ColumnStats { nulls, distinct }> }
pub trait Estimator: Send + Sync {
    fn id(&self) -> String;
    fn estimate(&self, query: &StepQuery<'_>) -> Result<UnitCost, String>;
}
```

`PlanInputs::for_job(job, discovered)` takes the job's budget where it sets one and `discovered` elsewhere. `StepQuery` gives the model the job, the step's tasks and their declarations, a source's table statistics, the rows and bytes the planner expects in, and the step's resources.

### d.3 The check

`check_plan(&Plan, &Job) -> Result<(), Vec<PlanRefusal>>`; `PlanRefusal { step: Option<String>, code: PlanRefusalCode, message }`, codes `job_mismatch`, `duplicate_step`, `unknown_task`, `duplicate_task`, `missing_task`, `empty_step`, `unknown_dependency`, `dependency_violated`, `group_order`, `barrier_crossed`, `over_ceiling`. `job_ceiling(&Job, Resources) -> Resources`.

### d.4 `explain`

`explain(job, catalogue, inputs, planners, planner_name) -> Result<Plan, PlanError>` checks the job against the catalogue (`moruna_job::check`, every refusal: `PlanError::Job`), then `plan_with(planner, job, catalogue, inputs)`, which plans and asserts (PN-I4). `PlanError` is `Job`, `UnknownPlanner`, `Registry`, `Inputs`, `Planner`, `Model` or `Refused`.

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
| `Estimate` | `p50`, `p95: double`, `unit: Unit`, `basis: Basis`, `observations: uint` |
| `Resources` | `workers: uint`, `memory_start_bytes`, `memory_ceiling_bytes: ulong` |
| `Planner` | `name` (required), `version: uint` |
| `Totals` | `basis`, `observations`, `finish: Range` (required), `critical_path: [string]`, `peak_memory_bytes`, `memory_byte_seconds`, `worker_seconds`, `disk_bytes: Range` (required) |
| `Range` (struct) | `p50`, `p95: double` |

The JSON rendering (`Plan::to_json`) adds `"moruna_plan": 1` and `groups`, the steps' ids by group in order; nothing reads it back.

### e.2 Events (MH 4.3 version 6)

`attempt_started` gains `plan`; every `task_*` event gains `step` after `task`; `step_started { step, plan, group }`; `step_ended { step, plan, outcome, duration_us }`, `outcome` one of `failed`, `cancelled`, `stopped`, `skipped`, `succeeded`. Field names are stable for KA8, which moves events onto the record format.

### e.3 The report

The report file's envelope and the `report` message gain `plan`: `{ plan, planner, job_digest, order: [step], steps: [{ step, tasks, group, start_us, end_us, outcome, resources, used: { morsels, rows_in, rows_out, bytes_in, bytes_out, worker_seconds, state_bytes_max } }], deviations: [{ step, t_us, ahead_of: [{ step, reason, blocking: [step] }] }] }`. `worker_seconds` and `state_bytes_max` come from the run report's stages for a step's kernel stages; a fused stage's figures count for every step it carries.

## f. Algorithms and policies

**f.1 Step ids.** `s` + hex(SHA-256(`moruna-plan step\0` + job digest + `\0` + planner name + `\0` + planner version + `\0` + task places joined by `,`))[..16]. Places, not ids, because the job's digest names tasks by place (JB-I14).

**f.2 `levels`' declared defaults.** A source gives the rows and file bytes its `TableStats` count and reads those bytes; a kernel and a sink give what they take (a fan-in adds its inputs, a fan-out gives each consumer the producer's output); a count unknown anywhere upstream is a declared zero downstream. A step may hold the memory it is given: peak memory p50 its start, p95 its ceiling. Times, retained memory, state and spill are declared zero.

**f.3 Totals without the simulator.** Finish: the longest path of the steps' durations through their dependencies, p50 and p95 each on its own. Critical path: the p50 path, more steps first on a tie, then the earlier in plan order. Peak: the largest group's sum of peaks. Held: memory as peak times duration summed, worker time summed, disk as read plus spill summed. Basis: the weakest of any estimate; observations: the fewest. KD6c's simulator replaces these for its own planner (`architecture/adaptive-runtime.md` section 8); a job's cost is then the simulation's, not a sum.

**f.4 The check's order.** The digest, step ids and tasks, missing tasks, group order and dependencies, edges between steps, order within a step, barriers, ceilings; every refusal is collected.

**f.5 A run from a plan.** `run_job` plans with `levels` for the job's budget, discovery filling what it leaves (`job::plan::default_plan`); a planning refusal of the job is the build's refusal at the same field. `build` checks the plan, then numbers the kernel stages in `task_order(job, plan)`, fusing only where no barrier stands. The lifecycle maps each of the run's tasks (a fused stage is one) to the steps it carries; a step starts with its first task and ends with its last.

## g. Concurrency within the component

`moruna-plan` has none: a plan is a value. In the runtime the lifecycle's step state lives under the lifecycle's one lock, so step events are stamped and told in the order they became true (PY-I12).

## h. Behaviour

A job with one source and one sink is two steps in two groups. A plan of a job refused by the graph check is never made: `levels` runs `check_graph` first. A step that never started ends `skipped` with `duration_us: null`. A document refused before it was planned tells its task events with `step: null` and `attempt_started` with `plan: null`.

## i. Configuration

The planner is a setting: `explain`'s `planner` argument, `moruna explain --planner`, `moruna.explain(planner=...)`; the default is `levels` (`DEFAULT_PLANNER`). A run from `moruna run` uses the default unless `--plan` gives a plan.

## j. Observability

The plan's JSON, the step events, and the report's `plan` section (e.3).

## k. Tests

Test names are the ids' lowercase with a slug. PN-T1 to PN-T20 are `crates/moruna-plan/tests/plan.rs`; PN-T21 and PN-T22 have a lifecycle half in `crates/moruna-runtime/src/host/lifecycle/tests.rs` and a run half in `crates/moruna-runtime/tests/plan.rs`, as do PN-T23 to PN-T25 and PN-T27; PN-T26 is `python/tests/test_explain.py`.

**PN-T1 the_same_job_gives_the_same_ids.** Twice-planned ids and plan id equal; the clock is not in the id; renamed tasks keep their ids; another planner version or job gives others. PN-I1.

**PN-T2 to PN-T5 levels on a chain, a fan-out, a fan-in, a diamond.** The groups are as expected; each step waits for its producers' steps; a later task ready earlier starts in an earlier group. PN-I2.

**PN-T6 resources_split_across_a_group.** A group's steps split the run; a group wider than the workers gives each one; the budget gives the run's resources, discovery filling the rest; inputs a run cannot have are refused by field. PN-I3, PN-I6.

**PN-T7 declared_defaults_follow_the_counts.** Rows and bytes from the table statistics through a fan-in; every estimate `Default` with no observation. PN-I3, PN-I7.

**PN-T8 the_levels_planner_asks_the_model.** The model's answers are carried; the totals follow its durations (critical path, finish, peak, held); a model's failure and an estimate with p95 below p50 are refused naming the step. PN-I3.

**PN-T9 to PN-T14 the check's refusals.** A missing task; a duplicated task; a reversed dependency (out of order, not waiting, fused in the wrong order); a crossed barrier (task and edge); an over-ceiling step, group and plan; another job, a duplicate step id, an unknown task, an empty step, an unknown dependency, groups out of order. Each plan made by hand. PN-I4, PN-I5, PN-I6.

**PN-T15 round_trip_and_rendering.** FlatBuffers round trip whole and byte-stable; the JSON names version, groups, steps and totals. PN-I8.

**PN-T16 the_verifier_refuses_corrupted_bytes.** 4,000 seeded corruptions (bytes set, truncations) each a plan or a `PlanBytesError`, never a panic; most refused. PN-I8.

**PN-T17 bytes_are_refused_by_what_they_are.** No identifier, too short, another version, a non-finite estimate, a unit that is not its field's, empty ids, an unordered range. PN-I8.

**PN-T18 a_test_planner_is_chosen_by_setting.** A test planner registers and is chosen by name; its plan is checked and refused when it violates a dependency; an unknown planner, a taken name, a name that is not a kind's, a plan naming another maker and a forged id are refused. PN-I4, PN-I9.

**PN-T19 explain_checks_the_job_first.** An unknown kind is refused with the job's refusals; a cycle is refused by the planner's graph check. PN-I4.

**PN-T20 totals_at_the_edges.** No steps, and dependencies pointing forward. f.3.

**PN-T21 a_run_from_its_plan.** Every task event names its step, the attempt the plan; every step starts and ends once; the report's order is the order steps started; each step's start, end, outcome, use and planned resources; the report file and message carry it. PN-I10.

**PN-T22 a_forced_deviation_is_reported_with_its_reason.** A step made to wait on a slow input: a later group's step starts first and the deviation names the waiting step, `awaiting_input` and the slow step; `ready` and `engine_preparing` each when they hold; a step that never starts ends skipped, a failed task's step failed. The run half runs a slow Parquet source beside a fast path. PN-I11.

**PN-T23 an_invalid_plan_is_refused_naming_the_step.** A run given a plan whose step does not wait for its input fails `plan_refused` naming the step, with nothing started; another job's plan is refused as such. PN-I4.

**PN-T24 the_stages_follow_the_plan.** The kernel stages are numbered in the plan's order, and in another valid order when the plan lists one. PN-I12.

**PN-T25 explain_and_run_from_the_plan_it_wrote.** `moruna explain` prints the plan and writes it; an unknown planner, a refused document and a missing file exit 2; `moruna run --plan` runs from the plan written; bytes that are not a plan are refused at `plan`; the command lines parse. PN-I13.

**PN-T26 explain_returns_the_plan** (Python). `moruna.explain` returns the plan as a dict, from a path or bytes, the same ids twice; an unknown planner is a `PlanError`, an unknown kind a `ConfigError`. PN-I13.

**PN-T27 nothing_is_fused_across_a_barrier.** Two standard kernels fuse into one stage, and do not across a barrier task or a barrier edge. PN-I5.

PN-I14 is CI's `tools/flatc/generate.sh --check`.

## l. Implementation notes for the agent

A new planner is an adapter of `Planner` in its own module, registered in `Planners`; it computes step ids with `step_id` and builds its plan with `Plan::new`, and `plan_with` checks it. The simulator-based planner of KD6c is one. A change to `schema/plan.fbs` is a run of `tools/flatc/generate.sh` and a commit of its output; a new field is appended to its table so older plans still read.

## m. Open items

**PN-M1. `moruna-cost` is not on `main`.** KD6a (PR 76) was open when this was built, so `moruna-plan` depends on nothing from it: the estimate types are defined here to section 4's shape and a cost model is asked through `Estimator`. When KD6a merges, KD6c makes `moruna-plan` depend on `moruna-cost`, uses its `Estimate`, `Unit`, `Basis` and `UnitCost` in place of these (the FlatBuffers fields do not change), adapts `CostModel` to the planner, and drops `Estimator`. KD6a's `Estimate` has an `extrapolated` flag section 4 does not; it is appended to the schema's `Estimate` then.

## n. Traceability

| Parent | Invariant | Test |
|---|---|---|
| KA7 (the plan document, stable ids) | PN-I1, PN-I8, PN-I14 | PN-T1, PN-T15 to PN-T17 |
| KA7 (the planner port, `levels`) | PN-I2, PN-I3, PN-I7, PN-I9 | PN-T2 to PN-T8, PN-T18, PN-T20 |
| KA7 (the internal check) | PN-I4, PN-I5, PN-I6 | PN-T9 to PN-T14, PN-T19, PN-T23, PN-T27 |
| KA7 (runs from a plan), MH 4.3 version 6 | PN-I10, PN-I11, PN-I12 | PN-T21, PN-T22, PN-T24 |
| KA7 (CLI and Python) | PN-I13 | PN-T25, PN-T26 |
| `architecture/adaptive-runtime.md` section 11 | the crate layout | `crates/moruna-plan/Cargo.toml` |

## o. Deferred (post-v1)

**PN-O1. The simulator's plan (KD6c).** Finish and resources held by simulation, with uncertainty from 200 draws, as `architecture/adaptive-runtime.md` section 8 describes.

**PN-O2. Adaptive deviation (KD7).** The scheduler deviating on purpose, with the runner-up and both scores in a decision event (section 9.7).

**PN-O3. Events as records (KA8).** The step events and fields above, on the record format.
