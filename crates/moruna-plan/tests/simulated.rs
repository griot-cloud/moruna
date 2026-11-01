//! SDD 26 k: the simulated planner, `explain`'s choice of planner, morsels in a plan, and the
//! estimate's appended field (PN-T28 to PN-T33).

use moruna_cost::sim::{Admission, DRAWS, SEED, Simulator};
use moruna_cost::{
    CostKey, CostModel, Defaults, Estimate, FitReport, Inputs, Kernel, LogLinear, ModelId,
    ObservationSource, Origin, Outcome, Target, Unit, UnitCost,
};
use moruna_job::{Catalogue, Edge, Job, KindDoc, Task};
use moruna_plan::{
    Basis, DEFAULT_PLANNER, Levels, Plan, PlanError, PlanInputs, Planners, Range, Resources,
    Simulated, TableStats, cost_key, default_planner, explain, plan_with,
};
use serde_json::json;

fn entry(value: serde_json::Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

/// `src -> a`, `src -> b`, both into `join`, then `out`.
fn diamond() -> Job {
    let cast = |column: &str| {
        entry(json!({"kind": "std", "name": "cast", "args": {"columns": {column: "double"}}}))
    };
    Job::new(
        vec![
            Task::source(
                "src",
                entry(json!({"kind": "parquet", "url": "/in.parquet"})),
            ),
            Task::kernel("a", cast("a")),
            Task::kernel("b", cast("b")),
            Task::kernel_with_inputs(
                "join",
                entry(json!({"kind": "std", "name": "union"})),
                &["l", "r"],
            ),
            Task::sink("out", entry(json!({"kind": "parquet", "url": "/out"}))),
        ],
        vec![
            Edge::between("src", "a"),
            Edge::between("src", "b"),
            Edge::new("a", "out", "join", "l"),
            Edge::new("b", "out", "join", "r"),
            Edge::between("join", "out"),
        ],
    )
}

fn run(workers: u32) -> Resources {
    Resources {
        workers,
        memory_start_bytes: 1 << 30,
        memory_ceiling_bytes: 2 << 30,
    }
}

fn table(rows: u64) -> TableStats {
    TableStats {
        rows: Some(rows),
        file_bytes: Some(rows * 64),
        columns: Default::default(),
    }
}

/// `loglinear` fitted from synthetic units of every task of `job`: a millisecond a batch and
/// `per_row` seconds a row (five times that for task `b`), a peak of twice the input, a row out
/// per row in, a source reading its bytes; on 1,000 to 64,000 rows.
fn fitted(job: &Job, per_row: f64) -> LogLinear {
    let mut model = LogLinear::new(Defaults::standard());
    for task in &job.tasks {
        let key = cost_key(task);
        let slow = if task.id == "b" { 5.0 } else { 1.0 };
        for i in 0..200u64 {
            let rows = 1_000 * (1 + (i * 37) % 64);
            let inputs = Inputs::batch(rows, rows * 64);
            let mut outcome = Outcome::new(Origin::Run);
            let seconds = 1e-3 + slow * per_row * rows as f64;
            outcome
                .set(Target::Duration, seconds)
                .set(Target::WorkerTime, seconds)
                .set(Target::PeakMemory, 128.0 * rows as f64)
                .set(Target::RetainedMemory, 64.0 * rows as f64)
                .set(Target::RowsOut, rows as f64)
                .set(Target::BytesOut, 64.0 * rows as f64)
                .set(Target::StateGrowth, 0.0)
                .set(Target::SpillBytes, 0.0)
                .set(
                    Target::ReadBytes,
                    if task.id == "src" {
                        64.0 * rows as f64
                    } else {
                        0.0
                    },
                );
            model.observe(&key, &inputs, &outcome);
        }
    }
    model
}

/// PN-T28 (PN-I15, PN-I16): with a model fitted from observations, `explain` chooses `simulated`;
/// its totals are the simulator's forecast of the very workload it builds (finish, peak,
/// memory-seconds, worker-seconds, disk bytes, critical path), each step's time, worker time and
/// peak its task's, measured where the history is; steps that overlapped share a group, and the
/// plan passes the check.
#[test]
fn pn_t28_the_simulated_planner_end_to_end() {
    let job = diamond();
    let catalogue = Catalogue::standard().expect("standard");
    let model = fitted(&job, 1e-7);
    let inputs = PlanInputs::new(run(4))
        .with_model(&model)
        .with_table("src", table(10_000_000));
    assert_eq!(default_planner(&inputs), Simulated::NAME);
    let planners = Planners::standard();
    let plan = explain(
        &job,
        &catalogue,
        &inputs,
        &planners,
        default_planner(&inputs),
    )
    .expect("planned");
    assert_eq!(plan.planner.name, "simulated");
    assert_eq!(plan.model.as_deref(), Some("loglinear@1"));

    let simulation = Simulated::default()
        .simulation(&job, &catalogue, &inputs)
        .expect("a simulation");
    assert_eq!(simulation.places, [0, 1, 2, 3, 4], "a topological order");
    assert!(
        simulation.workload.tasks[0].units > 1,
        "the source is cut into morsels"
    );
    let mut sim = Simulator::new(&simulation.workload, simulation.machine).expect("a workload");
    let forecast = sim.forecast(&mut Admission, DRAWS, SEED).expect("forecast");
    let range = |s: moruna_cost::sim::Spread| Range {
        p50: s.p50,
        p95: s.p95,
    };
    assert_eq!(plan.totals.finish, range(forecast.finish));
    assert_eq!(plan.totals.peak_memory_bytes, range(forecast.peak_memory));
    assert_eq!(
        plan.totals.memory_byte_seconds,
        range(forecast.memory_byte_seconds)
    );
    assert_eq!(plan.totals.worker_seconds, range(forecast.worker_seconds));
    assert_eq!(plan.totals.disk_bytes, range(forecast.disk_bytes));
    assert!(plan.totals.finish.p95 >= plan.totals.finish.p50);
    assert!(plan.totals.finish.p50 > 0.0);
    let path: Vec<&str> = plan
        .totals
        .critical_path
        .iter()
        .map(|id| plan.step(id).expect("a step").tasks[0].as_str())
        .collect();
    let expected: Vec<&str> = forecast
        .median
        .critical_path
        .iter()
        .map(|&w| job.tasks[simulation.places[w]].id.as_str())
        .collect();
    assert_eq!(path, expected);
    assert_eq!(path.last(), Some(&"out"));
    assert!(
        path.contains(&"b"),
        "the slow arm sets the finish: {path:?}"
    );

    for (w, &place) in simulation.places.iter().enumerate() {
        let step = plan.step_of(&job.tasks[place].id).expect("a step");
        let spread = forecast.tasks[w];
        assert_eq!(step.cost.duration.p50, spread.span.p50);
        assert_eq!(step.cost.worker_time.p50, spread.worker_seconds.p50);
        assert_eq!(step.cost.peak_memory.p50, spread.peak_memory.p50);
        assert_eq!(step.cost.duration.basis, Basis::Measured);
        assert!(step.resources.workers >= 1 && step.resources.workers <= 4);
    }
    let group = |task: &str| plan.step_of(task).expect("a step").group;
    assert_eq!(group("a"), group("b"), "the two arms ran side by side");
    assert!(group("src") < group("a") && group("b") < group("join"));
    assert!(group("join") < group("out"));

    // The same job planned again is the same plan.
    let again = explain(&job, &catalogue, &inputs, &planners, Simulated::NAME).expect("planned");
    assert_eq!(again.id, plan.id);
    let bytes = plan.to_flatbuffer();
    assert_eq!(Plan::from_flatbuffer(&bytes).expect("reads back"), plan);
}

/// PN-T29 (PN-I15): `explain` uses `simulated` when a cost model is configured and `levels`
/// otherwise; either may be named; `simulated` without a model is refused at `model`.
#[test]
fn pn_t29_explain_chooses_the_planner_by_the_model() {
    let job = diamond();
    let catalogue = Catalogue::standard().expect("standard");
    let planners = Planners::standard();
    let without = PlanInputs::new(run(4)).with_table("src", table(100_000));
    assert_eq!(default_planner(&without), DEFAULT_PLANNER);
    let plan = explain(
        &job,
        &catalogue,
        &without,
        &planners,
        default_planner(&without),
    )
    .expect("planned");
    assert_eq!(plan.planner.name, Levels::NAME);
    match explain(&job, &catalogue, &without, &planners, Simulated::NAME) {
        Err(PlanError::Inputs { field, reason }) => {
            assert_eq!(field, "model");
            assert!(reason.contains("cost model"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    let model = fitted(&job, 1e-7);
    let with = without.clone().with_model(&model);
    let levels = explain(&job, &catalogue, &with, &planners, Levels::NAME).expect("planned");
    assert_eq!(levels.planner.name, Levels::NAME);
    assert_eq!(levels.model.as_deref(), Some("loglinear@1"));
    assert_eq!(
        levels.step_of("b").expect("b").cost.duration.basis,
        Basis::Measured
    );
    let simulated =
        explain(&job, &catalogue, &with, &planners, default_planner(&with)).expect("planned");
    assert_eq!(simulated.planner.name, Simulated::NAME);
    assert_eq!(
        levels.step_of("b").expect("b").cost.rows_out.p50.round(),
        100_000.0,
        "rows flow from the table's counts"
    );
    assert_eq!(
        simulated
            .step_of("join")
            .expect("join")
            .cost
            .rows_out
            .p50
            .round(),
        200_000.0,
        "a fan-in adds its inputs"
    );
}

/// A model whose cost is a line in rows: `overhead + 1e-7 · rows` seconds, a peak of 128 bytes a
/// row, one row out per row in.
struct Lined {
    overhead: f64,
}

impl CostModel for Lined {
    fn id(&self) -> ModelId {
        ModelId::new("lined", 1)
    }

    fn predict(&self, _: &CostKey, inputs: &Inputs) -> UnitCost {
        let rows = inputs.data.rows.unwrap_or(1) as f64;
        let mut u = UnitCost::zero();
        u.duration = Estimate::exact(self.overhead + 1e-7 * rows, Unit::Seconds);
        u.worker_time = u.duration;
        u.peak_memory = Estimate::exact(128.0 * rows, Unit::Bytes);
        u.rows_out = Estimate::exact(rows, Unit::Rows);
        u.bytes_out = Estimate::exact(64.0 * rows, Unit::Bytes);
        u
    }

    fn observe(&mut self, _: &CostKey, _: &Inputs, _: &Outcome) {}
    fn fit(&mut self, _: &dyn ObservationSource) -> moruna_cost::Result<FitReport> {
        Ok(FitReport::default())
    }
    fn save(&self) -> moruna_cost::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn load(&mut self, _: &[u8]) -> moruna_cost::Result<()> {
        Ok(())
    }
}

fn units(model: &dyn CostModel, memory: u64) -> u32 {
    let job = diamond();
    let inputs = PlanInputs::new(Resources {
        workers: 4,
        memory_start_bytes: memory,
        memory_ceiling_bytes: memory,
    })
    .with_model(model)
    .with_table("src", table(100_000_000));
    let simulation = Simulated::default()
        .simulation(&job, &Catalogue::empty(), &inputs)
        .expect("a simulation");
    simulation.workload.tasks[0].units
}

/// PN-T30 (PN-I17): a task's units are its rows over its morsel: a model with a high per-batch
/// overhead gives bigger morsels (fewer units), a tight memory smaller ones (more).
#[test]
fn pn_t30_morsels_in_the_plan() {
    let low = units(&Lined { overhead: 1e-4 }, 1 << 34);
    let high = units(&Lined { overhead: 1e-2 }, 1 << 34);
    assert!(
        high < low,
        "{high} units with a high overhead, {low} with a low one"
    );
    let tight = units(&Lined { overhead: 1e-2 }, 64 << 20);
    assert!(tight > high, "{tight} units in 64 MiB, {high} in 16 GiB");
}

/// PN-T31 (PN-I16): a step whose unit can never fit the run's memory is refused, naming the step
/// and its task; a declared start-up and checkpoint lengthen the finish.
#[test]
fn pn_t31_infeasible_steps_and_once_per_process_costs() {
    struct Huge;
    impl CostModel for Huge {
        fn id(&self) -> ModelId {
            ModelId::new("huge", 1)
        }
        fn predict(&self, _: &CostKey, _: &Inputs) -> UnitCost {
            let mut u = UnitCost::zero();
            u.peak_memory = Estimate::exact(4.0 * f64::from(1u32 << 30), Unit::Bytes);
            u
        }
        fn observe(&mut self, _: &CostKey, _: &Inputs, _: &Outcome) {}
        fn fit(&mut self, _: &dyn ObservationSource) -> moruna_cost::Result<FitReport> {
            Ok(FitReport::default())
        }
        fn save(&self) -> moruna_cost::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn load(&mut self, _: &[u8]) -> moruna_cost::Result<()> {
            Ok(())
        }
    }
    let job = diamond();
    let inputs = PlanInputs::new(run(4)).with_model(&Huge);
    match plan_with(&Simulated::default(), &job, &Catalogue::empty(), &inputs) {
        Err(e @ PlanError::Infeasible { .. }) => {
            let PlanError::Infeasible { step, task, .. } = &e else {
                unreachable!()
            };
            assert_eq!(task, "src");
            assert!(moruna_plan::is_step_id(step));
            assert!(e.to_string().contains("is infeasible"), "{e}");
        }
        other => panic!("{other:?}"),
    }

    let model = Lined { overhead: 1e-3 };
    let base = PlanInputs::new(run(4))
        .with_model(&model)
        .with_table("src", table(1_000_000));
    let plain =
        plan_with(&Simulated::default(), &job, &Catalogue::empty(), &base).expect("planned");
    let kernel = Kernel {
        start_cold_seconds: Some(2.0),
        checkpoint_seconds: Some(0.5),
        ..Kernel::default()
    };
    let started = base.clone().with_kernel("src", kernel.clone());
    let slower =
        plan_with(&Simulated::default(), &job, &Catalogue::empty(), &started).expect("planned");
    assert!(
        slower.totals.finish.p50 >= plain.totals.finish.p50 + 2.0 - 1e-9,
        "{} then {}",
        plain.totals.finish.p50,
        slower.totals.finish.p50
    );
    // A checkpoint every 500 ms (the shortest cadence) while the source is under way, half a second each, paid at its
    // batch boundaries: a source of many batches pays several.
    let started = PlanInputs::new(run(4))
        .with_model(&model)
        .with_table("src", table(100_000_000))
        .with_kernel("src", kernel);
    let slower =
        plan_with(&Simulated::default(), &job, &Catalogue::empty(), &started).expect("planned");
    let mut often = job.clone();
    often.checkpoint.interval_ms = 500;
    let checked =
        plan_with(&Simulated::default(), &often, &Catalogue::empty(), &started).expect("planned");
    assert!(
        checked.totals.finish.p50 > slower.totals.finish.p50 + 0.5,
        "{} then {}",
        slower.totals.finish.p50,
        checked.totals.finish.p50
    );
    let mut never = often.clone();
    never.checkpoint.enabled = false;
    let unchecked =
        plan_with(&Simulated::default(), &never, &Catalogue::empty(), &started).expect("planned");
    // The job's own cadence (5 s) charged a few; with checkpoints off, none.
    assert!(unchecked.totals.finish.p50 < slower.totals.finish.p50);
}

/// PN-T32 (PN-I16): on one worker nothing overlaps, so every step is its own group, and the
/// finish is the sum of the steps' times.
#[test]
fn pn_t32_one_worker_runs_in_sequence() {
    let job = diamond();
    let model = Lined { overhead: 1e-3 };
    let inputs = PlanInputs::new(run(1))
        .with_model(&model)
        .with_table("src", table(10_000));
    let plan =
        plan_with(&Simulated::default(), &job, &Catalogue::empty(), &inputs).expect("planned");
    let mut groups: Vec<u32> = plan.steps.iter().map(|s| s.group).collect();
    groups.dedup();
    assert_eq!(groups.len(), job.tasks.len(), "{groups:?}");
    let sum: f64 = plan.steps.iter().map(|s| s.cost.duration.p50).sum();
    assert!(
        (plan.totals.finish.p50 - sum).abs() < 1e-9,
        "{} vs {sum}",
        plan.totals.finish.p50
    );
}

/// PN-T33 (PN-I8, PN-M1): the estimate's `extrapolated` mark, appended to the schema's table,
/// round-trips; a plan that never sets it reads it as not extrapolated.
#[test]
fn pn_t33_the_extrapolated_mark_round_trips() {
    let job = diamond();
    let model = Lined { overhead: 1e-3 };
    let inputs = PlanInputs::new(run(4))
        .with_model(&model)
        .with_table("src", table(10_000));
    let mut plan = plan_with(&Levels, &job, &Catalogue::empty(), &inputs).expect("planned");
    assert!(plan.steps.iter().all(|s| !s.cost.duration.extrapolated));
    plan.steps[1].cost.duration.extrapolated = true;
    let back = Plan::from_flatbuffer(&plan.to_flatbuffer()).expect("reads back");
    assert!(back.steps[1].cost.duration.extrapolated);
    assert!(!back.steps[0].cost.duration.extrapolated);
    assert_eq!(back, plan);
    let json = plan.to_value();
    assert_eq!(json["steps"][1]["cost"]["duration"]["extrapolated"], true);
}

/// PN-T34 (PN-I18): the simulated planner reads the job's checkpoint cadence through
/// `moruna_job`'s one clamp, as the run does: an interval below the range plans at 500 ms, one
/// above it at 60 s; a job that does not checkpoint plans none.
#[test]
fn pn_t34_the_planner_clamps_the_checkpoint_interval_as_the_run_does() {
    let model = Lined { overhead: 1e-3 };
    let inputs = PlanInputs::new(run(2)).with_model(&model);
    for given in [1, 100, 500, 5_000, 60_000, 120_000, u64::MAX] {
        let mut job = diamond();
        job.checkpoint.interval_ms = given;
        let simulation = Simulated::default()
            .simulation(&job, &Catalogue::empty(), &inputs)
            .expect("a simulation");
        assert_eq!(
            simulation.workload.checkpoint_interval_seconds,
            Some(moruna_job::checkpoint_interval_ms(given).0 as f64 / 1_000.0),
            "interval_ms {given}"
        );
    }
    let mut off = diamond();
    off.checkpoint.enabled = false;
    let simulation = Simulated::default()
        .simulation(&off, &Catalogue::empty(), &inputs)
        .expect("a simulation");
    assert_eq!(simulation.workload.checkpoint_interval_seconds, None);
}
