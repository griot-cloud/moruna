//! SDD 26 k: the plan document, the planner port, the levels planner and the plan's check
//! (PN-T1 to PN-T20).

use std::sync::Arc;

use moruna_cost::{
    CostKey, CostModel, Digest, FitReport, Inputs, ModelId, ObservationSource, Outcome,
};
use moruna_job::{BudgetDoc, Catalogue, Edge, ElasticDoc, Job, KindDoc, Task};
use moruna_plan::{
    Basis, Estimate, Levels, PLAN_IDENTIFIER, Plan, PlanBytesError, PlanError, PlanInputs,
    PlanRefusalCode, Planner, PlannerId, Planners, Resources, Step, TableStats, Target, Totals,
    Unit, UnitCost, budget_resources, check_plan, cost_key, explain, is_plan_flatbuffer,
    is_step_id, job_ceiling, job_digest, plan_with, step_id,
};
use serde_json::{Value, json};

fn entry(value: Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

fn source() -> KindDoc {
    entry(json!({"kind": "parquet", "url": "/in.parquet"}))
}

fn sink(dir: &str) -> KindDoc {
    entry(json!({"kind": "parquet", "url": dir}))
}

fn cast() -> KindDoc {
    entry(json!({"kind": "std", "name": "cast", "args": {"columns": {"a": "double"}}}))
}

fn union() -> KindDoc {
    entry(json!({"kind": "std", "name": "union"}))
}

fn run() -> Resources {
    Resources {
        workers: 4,
        memory_start_bytes: 1 << 30,
        memory_ceiling_bytes: 2 << 30,
    }
}

fn levels(job: &Job) -> Plan {
    plan_with(&Levels, job, &Catalogue::empty(), &PlanInputs::new(run())).expect("planned")
}

/// `src -> a -> b -> out`.
fn chain() -> Job {
    Job::new(
        vec![
            Task::source("src", source()),
            Task::kernel("a", cast()),
            Task::kernel("b", cast()),
            Task::sink("out", sink("/out")),
        ],
        vec![
            Edge::between("src", "a"),
            Edge::between("a", "b"),
            Edge::between("b", "out"),
        ],
    )
}

/// `src` feeds `a` and `b`, each to its own sink.
fn fan_out() -> Job {
    Job::new(
        vec![
            Task::source("src", source()),
            Task::kernel("a", cast()),
            Task::kernel("b", cast()),
            Task::sink("out-a", sink("/a")),
            Task::sink("out-b", sink("/b")),
        ],
        vec![
            Edge::between("src", "a"),
            Edge::between("src", "b"),
            Edge::between("a", "out-a"),
            Edge::between("b", "out-b"),
        ],
    )
}

/// Two sources joined by a union into one sink.
fn fan_in() -> Job {
    Job::new(
        vec![
            Task::source("left", source()),
            Task::source("right", source()),
            Task::kernel_with_inputs("join", union(), &["l", "r"]),
            Task::sink("out", sink("/out")),
        ],
        vec![
            Edge::new("left", "out", "join", "l"),
            Edge::new("right", "out", "join", "r"),
            Edge::between("join", "out"),
        ],
    )
}

/// `src -> a`, `src -> b`, both into `join`, then `out`.
fn diamond() -> Job {
    Job::new(
        vec![
            Task::source("src", source()),
            Task::kernel("a", cast()),
            Task::kernel("b", cast()),
            Task::kernel_with_inputs("join", union(), &["l", "r"]),
            Task::sink("out", sink("/out")),
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

/// The plan's groups, each its steps' tasks.
fn groups(plan: &Plan) -> Vec<Vec<String>> {
    plan.groups()
        .iter()
        .map(|g| {
            g.iter()
                .flat_map(|id| plan.step(id).expect("a step").tasks.clone())
                .collect()
        })
        .collect()
}

fn tasks(plan: &Plan, step: &str) -> Vec<String> {
    plan.step(step).expect("the step").tasks.clone()
}

fn deps(plan: &Plan, task: &str) -> Vec<String> {
    let step = plan.step_of(task).expect("a step runs it");
    step.depends_on
        .iter()
        .flat_map(|d| tasks(plan, d))
        .collect()
}

fn codes(refusals: &[moruna_plan::PlanRefusal]) -> Vec<PlanRefusalCode> {
    refusals.iter().map(|r| r.code).collect()
}

/// PN-T1 (PN-I1): the same job planned twice by the same planner has the same step ids and the
/// same plan id, whatever the clock said; a host that renames its tasks keeps the step ids; a
/// planner of another version, or another job, gives other ids.
#[test]
fn pn_t1_the_same_job_gives_the_same_ids() {
    let first = levels(&diamond());
    let second = levels(&diamond());
    let ids = |p: &Plan| p.steps.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&first), ids(&second));
    assert_eq!(first.id, second.id);
    assert_eq!(first.id.len(), 32);
    assert!(
        first.steps.iter().all(|s| is_step_id(&s.id)),
        "{:?}",
        ids(&first)
    );
    let mut late = first.clone();
    late.created_unix_ms += 60_000;
    assert_eq!(late.content_id(), first.id, "the clock is not the plan");

    let mut renamed = diamond();
    for task in &mut renamed.tasks {
        task.id = format!("host-{}", task.id);
    }
    for edge in &mut renamed.edges {
        edge.from.task = format!("host-{}", edge.from.task);
        edge.to.task = format!("host-{}", edge.to.task);
    }
    let renamed_plan = levels(&renamed);
    assert_eq!(
        ids(&renamed_plan),
        ids(&first),
        "the ids name tasks by place"
    );
    assert_eq!(renamed_plan.job_digest, first.job_digest);

    let digest = job_digest(&diamond());
    assert_eq!(first.job_digest, digest);
    assert!(digest.starts_with("sha256:") && digest.len() == 71);
    assert_ne!(
        step_id(&digest, &PlannerId::new("levels", 1), &[0]),
        step_id(&digest, &PlannerId::new("levels", 2), &[0])
    );
    assert_ne!(
        step_id(&digest, &PlannerId::new("levels", 1), &[0]),
        step_id(&job_digest(&chain()), &PlannerId::new("levels", 1), &[0])
    );
    assert!(!is_step_id("s123"));
    assert!(!is_step_id("x0123456789abcdef"));
}

/// PN-T2 (PN-I2): a chain is one step per task, one group per task, each waiting for the one
/// before it; the steps are in the chain's order.
#[test]
fn pn_t2_levels_on_a_chain() {
    let plan = levels(&chain());
    assert_eq!(groups(&plan), [["src"], ["a"], ["b"], ["out"]]);
    assert_eq!(deps(&plan, "src"), Vec::<String>::new());
    assert_eq!(deps(&plan, "a"), ["src"]);
    assert_eq!(deps(&plan, "b"), ["a"]);
    assert_eq!(deps(&plan, "out"), ["b"]);
    assert_eq!(plan.planner, PlannerId::new("levels", 1));
    assert!(plan.steps.iter().all(|s| s.tasks.len() == 1));
    assert_eq!(
        plan.totals.critical_path.len(),
        4,
        "with no durations, the longest path by steps"
    );
}

/// PN-T3 (PN-I2): a fan-out runs its branches side by side.
#[test]
fn pn_t3_levels_on_a_fan_out() {
    let plan = levels(&fan_out());
    assert_eq!(
        groups(&plan),
        vec![vec!["src"], vec!["a", "b"], vec!["out-a", "out-b"]]
    );
    assert_eq!(deps(&plan, "out-b"), ["b"]);
}

/// PN-T4 (PN-I2): a fan-in's sources start together and the join waits for both.
#[test]
fn pn_t4_levels_on_a_fan_in() {
    let plan = levels(&fan_in());
    assert_eq!(
        groups(&plan),
        vec![vec!["left", "right"], vec!["join"], vec!["out"]]
    );
    assert_eq!(deps(&plan, "join"), ["left", "right"]);
}

/// PN-T5 (PN-I2): a diamond's two arms are one group, and the join waits for both.
#[test]
fn pn_t5_levels_on_a_diamond() {
    let plan = levels(&diamond());
    assert_eq!(
        groups(&plan),
        vec![vec!["src"], vec!["a", "b"], vec!["join"], vec!["out"]]
    );
    assert_eq!(deps(&plan, "join"), ["a", "b"]);
    // The levels need not follow document order: a later task that is ready earlier starts in an
    // earlier group.
    let mut job = diamond();
    job.tasks.swap(1, 3);
    let plan = levels(&job);
    assert_eq!(
        groups(&plan),
        vec![vec!["src"], vec!["b", "a"], vec!["join"], vec!["out"]]
    );
}

/// PN-T6 (PN-I3): a group's steps split the run's memory and workers; one step alone has it all;
/// a group wider than the workers gives each one worker; the plan holds the run's resources.
#[test]
fn pn_t6_resources_split_across_a_group() {
    let plan = levels(&fan_out());
    let src = plan.step_of("src").expect("src");
    assert_eq!(src.resources, run());
    let a = plan.step_of("a").expect("a");
    assert_eq!(
        a.resources,
        Resources {
            workers: 2,
            memory_start_bytes: 1 << 29,
            memory_ceiling_bytes: 1 << 30,
        }
    );
    assert_eq!(plan.resources, run());

    let narrow = Resources {
        workers: 1,
        ..run()
    };
    let plan = plan_with(
        &Levels,
        &fan_out(),
        &Catalogue::empty(),
        &PlanInputs::new(narrow),
    )
    .expect("planned");
    assert!(plan.steps.iter().all(|s| s.resources.workers == 1));

    // The run's resources from a budget: what it sets wins, the machine fills the rest.
    let machine = Resources {
        workers: 8,
        memory_start_bytes: 16 << 30,
        memory_ceiling_bytes: 16 << 30,
    };
    assert_eq!(budget_resources(&BudgetDoc::default(), machine), machine);
    let budget = BudgetDoc {
        memory_bytes: Some(1 << 30),
        cpu: Some(1.5),
        elastic: Some(ElasticDoc {
            memory_max_bytes: Some(4 << 30),
            cpu_max: Some(4),
        }),
    };
    assert_eq!(
        budget_resources(&budget, machine),
        Resources {
            workers: 2,
            memory_start_bytes: 1 << 30,
            memory_ceiling_bytes: 4 << 30,
        }
    );
    let fixed = BudgetDoc {
        memory_bytes: Some(1 << 30),
        cpu: None,
        elastic: None,
    };
    let mut job = chain();
    job.budget = fixed;
    let inputs = PlanInputs::for_job(&job, machine);
    assert_eq!(
        inputs.resources,
        Resources {
            workers: 8,
            memory_start_bytes: 1 << 30,
            memory_ceiling_bytes: 1 << 30,
        }
    );
    assert!(format!("{inputs:?}").contains("PlanInputs"));
    assert_eq!(
        job_ceiling(&job, machine),
        Resources {
            workers: 8,
            memory_start_bytes: 1 << 30,
            memory_ceiling_bytes: 1 << 30,
        }
    );
    job.budget = budget;
    assert_eq!(job_ceiling(&job, machine).workers, 4);
    assert_eq!(job_ceiling(&job, machine).memory_ceiling_bytes, 4 << 30);

    // Inputs a run cannot have are refused by field.
    let none = Resources {
        workers: 0,
        ..run()
    };
    match plan_with(
        &Levels,
        &chain(),
        &Catalogue::empty(),
        &PlanInputs::new(none),
    ) {
        Err(PlanError::Inputs { field, .. }) => assert_eq!(field, "resources.workers"),
        other => panic!("{other:?}"),
    }
    let upside_down = Resources {
        memory_ceiling_bytes: 1,
        ..run()
    };
    match plan_with(
        &Levels,
        &chain(),
        &Catalogue::empty(),
        &PlanInputs::new(upside_down),
    ) {
        Err(PlanError::Inputs { field, .. }) => assert_eq!(field, "resources.memory_ceiling_bytes"),
        other => panic!("{other:?}"),
    }
}

/// PN-T7 (PN-I3, PN-I7): with no model, every estimate is a declared default resting on no
/// observation; a source gives the rows and bytes its table's counts say, and the steps after it
/// give what they take; a fan-in adds its inputs; a step may hold the memory it is given.
#[test]
fn pn_t7_declared_defaults_follow_the_counts() {
    let stats = |rows: u64, bytes: u64| TableStats {
        rows: Some(rows),
        file_bytes: Some(bytes),
        columns: [(
            "a".to_string(),
            moruna_plan::ColumnStats {
                nulls: Some(1),
                distinct: Some(10),
            },
        )]
        .into(),
    };
    let inputs = PlanInputs::new(run())
        .with_table("left", stats(100, 800))
        .with_table("right", stats(50, 400));
    let plan = plan_with(&Levels, &fan_in(), &Catalogue::empty(), &inputs).expect("planned");
    let cost = |task: &str| plan.step_of(task).expect("a step").cost;
    assert_eq!(cost("left").rows_out.p50, 100.0);
    assert_eq!(cost("left").read_bytes.p50, 800.0);
    assert_eq!(cost("join").rows_out.p50, 150.0);
    assert_eq!(cost("join").bytes_out.p50, 1200.0);
    assert_eq!(cost("out").rows_out.p50, 150.0);
    assert_eq!(cost("join").read_bytes.p50, 0.0);
    assert_eq!(
        cost("join").peak_memory.p95,
        run().memory_ceiling_bytes as f64
    );
    for step in &plan.steps {
        for target in Target::ALL {
            let estimate = step.cost.get(target);
            assert_eq!(estimate.basis, Basis::Default, "{target}");
            assert_eq!(estimate.observations, 0, "{target}");
            assert_eq!(estimate.unit, target.unit(), "{target}");
        }
    }
    assert_eq!(plan.totals.basis, Basis::Default);
    assert_eq!(plan.model, None);

    // One source without counts makes its consumers' counts unknown, a declared zero.
    let inputs = PlanInputs::new(run()).with_table("left", stats(100, 800));
    let plan = plan_with(&Levels, &fan_in(), &Catalogue::empty(), &inputs).expect("planned");
    assert_eq!(plan.step_of("join").expect("join").cost.rows_out.p50, 0.0);
}

/// A model whose units of function 1 take a second, of function 5 five seconds, and of any
/// other half a second; it reads 7 bytes when the kind's declaration was handed over and 3 when
/// not, so the critical path is decided by durations.
struct Clockwork;

/// A key of the `std` kernel running function `n`.
fn function(n: u8) -> CostKey {
    CostKey {
        fingerprint: Digest([n; 32]),
        ..cost_key(&Task::kernel("k", cast()))
    }
}

/// The parts of a model a test does not exercise.
macro_rules! inert {
    () => {
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
    };
}

impl CostModel for Clockwork {
    fn id(&self) -> ModelId {
        ModelId::new("clockwork", 3)
    }

    fn predict(&self, key: &CostKey, inputs: &Inputs) -> UnitCost {
        let seconds = match key.fingerprint.0[0] {
            1 => 1.0,
            5 => 5.0,
            _ => 0.5,
        };
        let measured = |p50: f64, p95: f64, unit: Unit| Estimate {
            p50,
            p95,
            unit,
            basis: Basis::Measured,
            observations: 12,
            extrapolated: false,
            drifting: false,
        };
        let declared = inputs.kernel.deterministic.is_some();
        let rows = inputs.data.rows.unwrap_or(10) as f64;
        UnitCost {
            duration: measured(seconds, seconds * 2.0, Unit::Seconds),
            worker_time: measured(seconds, seconds * 2.0, Unit::Seconds),
            peak_memory: measured(1000.0, 2000.0, Unit::Bytes),
            rows_out: measured(rows, rows, Unit::Rows),
            read_bytes: measured(if declared { 7.0 } else { 3.0 }, 9.0, Unit::Bytes),
            ..UnitCost::zero()
        }
    }

    inert!();
}

/// PN-T8 (PN-I3): with a model, the planner asks it about every task's units and carries its
/// answers; the totals follow its durations; an estimate no plan carries is refused naming the
/// step.
#[test]
fn pn_t8_the_levels_planner_asks_the_model() {
    let catalogue = Catalogue::standard().expect("standard");
    let model = Clockwork;
    let inputs = PlanInputs::new(run())
        .with_model(&model)
        .with_key("a", function(1))
        .with_key("b", function(5));
    let plan = plan_with(&Levels, &diamond(), &catalogue, &inputs).expect("planned");
    assert_eq!(plan.model.as_deref(), Some("clockwork@3"));
    let b = plan.step_of("b").expect("b");
    assert_eq!(b.cost.duration.p50, 5.0);
    assert_eq!(b.cost.duration.basis, Basis::Measured);
    assert_eq!(
        b.cost.read_bytes.p50, 7.0,
        "the declaration was handed over"
    );
    assert_eq!(
        plan.step_of("join").expect("join").cost.rows_out.p50,
        10.0,
        "with no counts, rows are the model's own answer"
    );
    // The longest path goes through b: 0.5 + 5 + 0.5 + 0.5.
    let path: Vec<String> = plan
        .totals
        .critical_path
        .iter()
        .flat_map(|s| tasks(&plan, s))
        .collect();
    assert_eq!(path, ["src", "b", "join", "out"]);
    assert_eq!(plan.totals.finish.p50, 6.5);
    assert_eq!(plan.totals.finish.p95, 13.0);
    assert_eq!(
        plan.totals.basis,
        Basis::Default,
        "the model left most fields declared"
    );
    assert_eq!(
        plan.totals.peak_memory_bytes.p50, 2000.0,
        "the widest group"
    );
    assert_eq!(plan.totals.worker_seconds.p50, 7.5);
    assert_eq!(plan.totals.disk_bytes.p50, 35.0);

    struct Broken;
    impl CostModel for Broken {
        fn id(&self) -> ModelId {
            ModelId::new("broken", 1)
        }
        fn predict(&self, _: &CostKey, _: &Inputs) -> UnitCost {
            UnitCost {
                duration: Estimate {
                    p95: 1.0,
                    ..Estimate::exact(2.0, Unit::Seconds)
                },
                ..UnitCost::zero()
            }
        }
        inert!();
    }
    let model = Broken;
    let inputs = PlanInputs::new(run()).with_model(&model);
    match plan_with(&Levels, &chain(), &catalogue, &inputs) {
        Err(e @ PlanError::Model { .. }) => {
            let PlanError::Model { step, reason, .. } = &e else {
                unreachable!()
            };
            assert!(is_step_id(step));
            assert!(reason.contains("duration: p95"), "{reason}");
            assert!(e.to_string().contains("cost model broken@1"), "{e}");
        }
        other => panic!("{other:?}"),
    }
}

/// A plan with one step per task, in `order`, each its own group, waiting for its producers:
/// the hand-made plans the refusals below start from.
fn by_hand(job: &Job, order: &[&str]) -> Plan {
    let planner = PlannerId::new("hand", 1);
    let digest = job_digest(job);
    let id = |task: &str| step_id(&digest, &planner, &[job.task_index(task).expect("a task")]);
    let steps: Vec<Step> = order
        .iter()
        .enumerate()
        .map(|(g, &task)| Step {
            id: id(task),
            tasks: vec![task.to_string()],
            depends_on: job
                .edges
                .iter()
                .filter(|e| e.to.task == task)
                .map(|e| id(&e.from.task))
                .collect(),
            group: g as u32,
            resources: run(),
            cost: UnitCost::zero(),
        })
        .collect();
    let totals = Totals::of(&steps);
    Plan::new(job, planner, None, 0, run(), steps, totals)
}

/// PN-T9 (PN-I4): a plan that runs no step for a task is refused, naming the task.
#[test]
fn pn_t9_check_refuses_a_missing_task() {
    let job = chain();
    assert_eq!(
        check_plan(&by_hand(&job, &["src", "a", "b", "out"]), &job),
        Ok(())
    );
    let mut plan = by_hand(&job, &["src", "a", "b", "out"]);
    plan.steps.remove(3);
    let refused = check_plan(&plan, &job).expect_err("refused");
    assert_eq!(codes(&refused), [PlanRefusalCode::MissingTask]);
    assert_eq!(refused[0].step, None);
    assert_eq!(refused[0].to_string(), "plan: no step runs task `out`");
}

/// PN-T10 (PN-I4): a task two steps run is refused at the second, naming the first.
#[test]
fn pn_t10_check_refuses_a_duplicated_task() {
    let job = chain();
    let mut plan = by_hand(&job, &["src", "a", "b", "out"]);
    plan.steps[2].tasks.push("a".into());
    let refused = check_plan(&plan, &job).expect_err("refused");
    assert!(codes(&refused).contains(&PlanRefusalCode::DuplicateTask));
    let dup = refused
        .iter()
        .find(|r| r.code == PlanRefusalCode::DuplicateTask)
        .expect("the duplicate");
    assert_eq!(dup.step.as_deref(), Some(plan.steps[2].id.as_str()));
    assert!(dup.message.contains(&plan.steps[1].id));
}

/// PN-T11 (PN-I4): a reversed dependency is refused at the step that runs before its input:
/// listed first and not waiting, or waiting for a step that starts after it.
#[test]
fn pn_t11_check_refuses_a_reversed_dependency() {
    let job = chain();
    // `b` before `a`: `b` waits for `a`, which is in a later group.
    let plan = by_hand(&job, &["src", "b", "a", "out"]);
    let refused = check_plan(&plan, &job).expect_err("refused");
    let b = &plan.steps[1].id;
    assert!(
        refused
            .iter()
            .any(|r| r.code == PlanRefusalCode::DependencyViolated && r.step.as_ref() == Some(b)),
        "{refused:?}"
    );
    // A step that takes an input and does not wait for its step.
    let mut plan = by_hand(&job, &["src", "a", "b", "out"]);
    plan.steps[2].depends_on.clear();
    let refused = check_plan(&plan, &job).expect_err("refused");
    assert_eq!(codes(&refused), [PlanRefusalCode::DependencyViolated]);
    assert!(
        refused[0].message.contains("does not wait"),
        "{}",
        refused[0]
    );
    // Two tasks fused in the wrong order.
    let mut plan = by_hand(&job, &["src", "a", "b", "out"]);
    let b = plan.steps.remove(2);
    plan.steps[1].tasks = vec!["b".into(), "a".into()];
    plan.steps[2].depends_on = vec![plan.steps[1].id.clone()];
    let _ = b;
    let refused = check_plan(&plan, &job).expect_err("refused");
    assert_eq!(codes(&refused), [PlanRefusalCode::DependencyViolated]);
    assert!(refused[0].message.contains("lists task `b` before `a`"));
}

/// PN-T12 (PN-I5): nothing moves across a contract barrier: a barrier task fused with another,
/// or both ends of a barrier edge in one step, is refused at the step; fused without a barrier,
/// the same steps pass.
#[test]
fn pn_t12_check_refuses_a_crossed_barrier() {
    let fuse = |job: &Job| {
        let mut plan = by_hand(job, &["src", "a", "b", "out"]);
        plan.steps.remove(2);
        plan.steps[1].tasks = vec!["a".into(), "b".into()];
        plan.steps[2].depends_on = vec![plan.steps[1].id.clone()];
        plan
    };
    let job = chain();
    assert_eq!(check_plan(&fuse(&job), &job), Ok(()));

    let mut barrier_task = chain();
    barrier_task.tasks[1].barrier = true;
    let plan = fuse(&barrier_task);
    let refused = check_plan(&plan, &barrier_task).expect_err("refused");
    assert_eq!(codes(&refused), [PlanRefusalCode::BarrierCrossed]);
    assert_eq!(refused[0].step.as_deref(), Some(plan.steps[1].id.as_str()));
    assert!(refused[0].message.contains("barrier task `a`"));

    let mut barrier_edge = chain();
    barrier_edge.edges[1].barrier = true;
    let refused = check_plan(&fuse(&barrier_edge), &barrier_edge).expect_err("refused");
    assert_eq!(codes(&refused), [PlanRefusalCode::BarrierCrossed]);
    assert!(
        refused[0]
            .message
            .contains("barrier edge from a.out to b.in")
    );

    // The levels planner never fuses, so it never crosses one.
    assert_eq!(check_plan(&levels(&barrier_task), &barrier_task), Ok(()));
}

/// PN-T13 (PN-I6): a step given more than the run, a group whose starts add up past the run,
/// and a plan given more than the job's ceiling are refused.
#[test]
fn pn_t13_check_refuses_an_over_ceiling_step() {
    let job = chain();
    let mut plan = by_hand(&job, &["src", "a", "b", "out"]);
    plan.steps[1].resources.memory_ceiling_bytes = run().memory_ceiling_bytes + 1;
    let refused = check_plan(&plan, &job).expect_err("refused");
    assert_eq!(codes(&refused), [PlanRefusalCode::OverCeiling]);
    assert_eq!(refused[0].step.as_deref(), Some(plan.steps[1].id.as_str()));

    let mut plan = by_hand(&job, &["src", "a", "b", "out"]);
    plan.steps[2].resources.workers = 5;
    assert_eq!(
        codes(&check_plan(&plan, &job).expect_err("refused")),
        [PlanRefusalCode::OverCeiling]
    );

    let mut plan = levels(&fan_out());
    for step in &mut plan.steps {
        step.resources.memory_start_bytes = run().memory_start_bytes + (1 << 29);
    }
    let refused = check_plan(&plan, &fan_out()).expect_err("refused");
    let b = &plan.step_of("b").expect("b").id;
    assert!(
        refused
            .iter()
            .any(|r| r.code == PlanRefusalCode::OverCeiling && r.step.as_ref() == Some(b)),
        "the second of the group takes it past: {refused:?}"
    );

    let mut tight = chain();
    tight.budget = BudgetDoc {
        memory_bytes: Some(1 << 20),
        cpu: Some(1.0),
        elastic: None,
    };
    let plan = by_hand(&tight, &["src", "a", "b", "out"]);
    let refused = check_plan(&plan, &tight).expect_err("refused");
    assert_eq!(refused[0].step, None);
    assert_eq!(refused[0].code, PlanRefusalCode::OverCeiling);
}

/// PN-T14 (PN-I4): the rest of the check's refusals, each at its step: another job's plan, two
/// steps with one id, a task the job does not have, a step with no task, a dependency the plan
/// does not have, groups listed out of order.
#[test]
fn pn_t14_check_refuses_the_rest() {
    let job = chain();
    let plan = by_hand(&job, &["src", "a", "b", "out"]);
    let refused = check_plan(&plan, &diamond()).expect_err("another job");
    assert_eq!(refused[0].code, PlanRefusalCode::JobMismatch);

    let mut twice = plan.clone();
    twice.steps[2].id = twice.steps[1].id.clone();
    assert!(
        codes(&check_plan(&twice, &job).expect_err("refused"))
            .contains(&PlanRefusalCode::DuplicateStep)
    );

    let mut stranger = plan.clone();
    stranger.steps[3].tasks.push("nobody".into());
    assert!(
        codes(&check_plan(&stranger, &job).expect_err("refused"))
            .contains(&PlanRefusalCode::UnknownTask)
    );

    let mut empty = plan.clone();
    empty.steps.push(Step {
        id: "s0000000000000000".into(),
        tasks: Vec::new(),
        depends_on: Vec::new(),
        group: 9,
        resources: run(),
        cost: UnitCost::zero(),
    });
    assert_eq!(
        codes(&check_plan(&empty, &job).expect_err("refused")),
        [PlanRefusalCode::EmptyStep]
    );

    let mut lost = plan.clone();
    lost.steps[3].depends_on.push("s1111111111111111".into());
    assert_eq!(
        codes(&check_plan(&lost, &job).expect_err("refused")),
        [PlanRefusalCode::UnknownDependency]
    );

    let mut shuffled = plan.clone();
    shuffled.steps[0].group = 5;
    let refused = check_plan(&shuffled, &job).expect_err("refused");
    assert!(codes(&refused).contains(&PlanRefusalCode::GroupOrder));
}

/// PN-T15 (PN-I8): a plan survives `to_flatbuffer` and `from_flatbuffer` whole; the bytes carry
/// the identifier and are the same every time; the JSON rendering names the version, the
/// groups, every step and the totals.
#[test]
fn pn_t15_round_trip_and_rendering() {
    let model = Clockwork;
    let plan = plan_with(
        &Levels,
        &diamond(),
        &Catalogue::empty(),
        &PlanInputs::new(run()).with_model(&model),
    )
    .expect("planned");
    let bytes = plan.to_flatbuffer();
    assert!(is_plan_flatbuffer(&bytes));
    assert_eq!(&bytes[4..8], PLAN_IDENTIFIER.as_bytes());
    assert_eq!(bytes, plan.to_flatbuffer());
    let back = Plan::from_flatbuffer(&bytes).expect("reads back");
    assert_eq!(back, plan);
    assert_eq!(check_plan(&back, &diamond()), Ok(()));

    let json: Value = serde_json::from_str(&plan.to_json()).expect("JSON");
    assert_eq!(json["moruna_plan"], 1);
    assert_eq!(json["planner"]["name"], "levels");
    assert_eq!(json["model"], "clockwork@3");
    assert_eq!(json["groups"].as_array().expect("groups").len(), 4);
    assert_eq!(json["steps"].as_array().expect("steps").len(), 5);
    assert_eq!(json["steps"][0]["cost"]["duration"]["basis"], "measured");
    assert_eq!(
        json["totals"]["critical_path"]
            .as_array()
            .expect("path")
            .len(),
        4
    );
    assert!(levels(&chain()).to_value().get("model").is_none());
}

/// A little generator of its own, so the corruptions are the same on every run.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// PN-T16 (PN-I8): 4,000 seeded corruptions of a plan's bytes (one to four bytes set, and
/// truncations) each read as a plan or a `PlanBytesError`, never a panic; most are refused.
#[test]
fn pn_t16_the_verifier_refuses_corrupted_bytes() {
    let bytes = levels(&diamond()).to_flatbuffer();
    let mut draw = Draw(0x9e37_79b9_7f4a_7c15);
    let mut refused = 0;
    for round in 0..4000 {
        let mut bad = bytes.clone();
        if round % 5 == 4 {
            bad.truncate(draw.below(bytes.len() as u64) as usize);
        } else {
            for _ in 0..=draw.below(4) {
                let at = draw.below(bad.len() as u64) as usize;
                bad[at] = draw.next() as u8;
            }
        }
        if std::panic::catch_unwind(|| Plan::from_flatbuffer(&bad))
            .expect("never a panic")
            .is_err()
        {
            refused += 1;
        }
    }
    assert!(refused > 1000, "only {refused} of 4000 refused");
}

/// PN-T17 (PN-I8): bytes are refused by what they are: no identifier, too short, another version,
/// an enum value this build does not know, an estimate that is not finite or of another unit, an
/// empty id.
#[test]
fn pn_t17_bytes_are_refused_by_what_they_are() {
    assert_eq!(
        Plan::from_flatbuffer(b"short"),
        Err(PlanBytesError::Identifier)
    );
    assert_eq!(
        Plan::from_flatbuffer(&[0u8; 64]),
        Err(PlanBytesError::Identifier)
    );
    let plan = levels(&chain());

    let field_of = |plan: &Plan| match Plan::from_flatbuffer(&plan.to_flatbuffer()) {
        Err(PlanBytesError::Field { field, .. }) => field,
        other => panic!("{other:?}"),
    };
    let mut nan = plan.clone();
    nan.steps[1].cost.duration.p50 = f64::NAN;
    assert_eq!(field_of(&nan), "steps[1].cost.duration");
    let mut unit = plan.clone();
    unit.steps[0].cost.rows_out.unit = Unit::Bytes;
    assert_eq!(field_of(&unit), "steps[0].cost.rows_out");
    let mut empty = plan.clone();
    empty.steps[2].id = String::new();
    assert_eq!(field_of(&empty), "steps[2].id");
    let mut task = plan.clone();
    task.steps[2].tasks[0] = String::new();
    assert_eq!(field_of(&task), "steps[2].tasks[0]");
    let mut dep = plan.clone();
    dep.steps[2].depends_on[0] = String::new();
    assert_eq!(field_of(&dep), "steps[2].depends_on[0]");
    let mut nameless = plan.clone();
    nameless.planner.name = String::new();
    assert_eq!(field_of(&nameless), "planner.name");
    let mut totals = plan.clone();
    totals.totals.finish.p95 = -1.0;
    assert_eq!(field_of(&totals), "totals.finish");
    let mut digest = plan.clone();
    digest.job_digest = String::new();
    assert_eq!(field_of(&digest), "job_digest");

    // An enum value no build writes, and a version this build does not read, by editing the
    // verified buffer's scalars through the generated tables' offsets.
    let bytes = plan.to_flatbuffer();
    let find = |needle: &[u8]| bytes.windows(needle.len()).position(|w| w == needle);
    let version = 1u32.to_le_bytes();
    let mut later = bytes.clone();
    let at = find(&version).expect("the version is in the buffer");
    later[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
    match Plan::from_flatbuffer(&later) {
        Err(PlanBytesError::Version(2)) | Err(PlanBytesError::Field { .. }) => {}
        other => panic!("{other:?}"),
    }
    assert!(PlanBytesError::Version(2).to_string().contains("version 1"));
    assert!(
        PlanBytesError::Malformed("x".into())
            .to_string()
            .contains("not a plan")
    );
}

/// A planner of a test's own: everything in one group, each task its own step, made with nothing
/// but the port and the plan's public types (SDD 26 PN-I9).
struct Flat;

impl Planner for Flat {
    fn id(&self) -> PlannerId {
        PlannerId::new("test.flat", 7)
    }

    fn plan(
        &self,
        job: &Job,
        _catalogue: &Catalogue,
        inputs: &PlanInputs<'_>,
    ) -> Result<Plan, PlanError> {
        let digest = job_digest(job);
        let steps: Vec<Step> = (0..job.tasks.len())
            .map(|i| Step {
                id: step_id(&digest, &self.id(), &[i]),
                tasks: vec![job.tasks[i].id.clone()],
                depends_on: Vec::new(),
                group: 0,
                resources: Resources {
                    workers: 1,
                    memory_start_bytes: 0,
                    memory_ceiling_bytes: 0,
                },
                cost: UnitCost::zero(),
            })
            .collect();
        let totals = Totals::of(&steps);
        Ok(Plan::new(
            job,
            self.id(),
            None,
            1,
            inputs.resources,
            steps,
            totals,
        ))
    }
}

/// PN-T18 (PN-I9): a planner of a test's own registers beside Moruna's and is chosen by its
/// name; what it returns is checked like any planner's, so a plan that does not wait for its
/// inputs is refused before anyone sees it; a planner whose plan names another maker, or whose
/// id is not its content, is refused; names are a kind's, and one name is one planner.
#[test]
fn pn_t18_a_test_planner_is_chosen_by_setting() {
    let catalogue = Catalogue::standard().expect("standard");
    let mut planners = Planners::standard();
    planners.add(Arc::new(Flat)).expect("registers");
    assert_eq!(planners.names(), ["levels", "simulated", "test.flat"]);
    let inputs = PlanInputs::new(run());
    let single = Job::new(vec![Task::source("only", source())], Vec::new());
    let mut single_out = single.clone();
    single_out.tasks.push(Task::sink("out", sink("/out")));
    single_out.edges.push(Edge::between("only", "out"));
    let plan = explain(&single, &catalogue, &inputs, &planners, "levels");
    assert!(
        plan.is_err(),
        "a source alone is refused by the job's check"
    );

    let chosen = explain(&chain(), &catalogue, &inputs, &planners, "test.flat");
    match chosen {
        Err(PlanError::Refused(refusals)) => {
            assert!(codes(&refusals).contains(&PlanRefusalCode::DependencyViolated));
            assert!(
                PlanError::Refused(refusals)
                    .to_string()
                    .starts_with("plan refused:")
            );
        }
        other => panic!("{other:?}"),
    }
    let by_levels = explain(&chain(), &catalogue, &inputs, &planners, "levels").expect("levels");
    assert_eq!(by_levels.planner.name, "levels");

    // A job of one task has no dependency to violate: the test planner's plan stands.
    let one = Job::new(
        vec![
            Task::source("only", source()),
            Task::sink("out", sink("/o")),
        ],
        vec![],
    );
    let flat = Flat.plan(&one, &catalogue, &inputs).expect("planned");
    assert_eq!(flat.planner, PlannerId::new("test.flat", 7));

    match explain(&chain(), &catalogue, &inputs, &planners, "lookahead") {
        Err(e @ PlanError::UnknownPlanner { .. }) => {
            assert!(
                e.to_string().contains("levels, simulated, test.flat"),
                "{e}"
            );
        }
        other => panic!("{other:?}"),
    }
    match planners.add(Arc::new(Flat)) {
        Err(e @ PlanError::Registry { .. }) => assert!(e.to_string().contains("already")),
        other => panic!("{other:?}"),
    }
    struct Nameless;
    impl Planner for Nameless {
        fn id(&self) -> PlannerId {
            PlannerId::new("Not A Name", 1)
        }
        fn plan(&self, job: &Job, c: &Catalogue, i: &PlanInputs<'_>) -> Result<Plan, PlanError> {
            Levels.plan(job, c, i)
        }
    }
    assert!(matches!(
        Planners::empty().add(Arc::new(Nameless)),
        Err(PlanError::Registry { .. })
    ));
    match plan_with(&Nameless, &chain(), &catalogue, &inputs) {
        Err(PlanError::Planner { reason, .. }) => {
            assert!(reason.contains("names planner levels@1"))
        }
        other => panic!("{other:?}"),
    }
    struct Forger;
    impl Planner for Forger {
        fn id(&self) -> PlannerId {
            Levels.id()
        }
        fn plan(&self, job: &Job, c: &Catalogue, i: &PlanInputs<'_>) -> Result<Plan, PlanError> {
            let mut plan = Levels.plan(job, c, i)?;
            plan.steps[0].group = 0;
            plan.steps[0].resources.workers = 1;
            plan.id = "0".repeat(32);
            Ok(plan)
        }
    }
    match plan_with(&Forger, &chain(), &catalogue, &inputs) {
        Err(PlanError::Planner { reason, .. }) => assert!(reason.contains("content address")),
        other => panic!("{other:?}"),
    }
}

/// PN-T19 (PN-I4): `explain` checks the job against the catalogue first and refuses it with
/// every refusal; a cycle is refused by the planner's graph check.
#[test]
fn pn_t19_explain_checks_the_job_first() {
    let catalogue = Catalogue::standard().expect("standard");
    let mut job = chain();
    job.tasks[1].kind = "no.such".into();
    match explain(
        &job,
        &catalogue,
        &PlanInputs::new(run()),
        &Planners::standard(),
        "levels",
    ) {
        Err(e @ PlanError::Job(_)) => assert!(e.to_string().contains("no.such"), "{e}"),
        other => panic!("{other:?}"),
    }
    let mut cycle = chain();
    cycle.edges.push(Edge::new("b", "out", "a", "in"));
    cycle.tasks[1].inputs = vec!["in".into(), "back".into()];
    cycle.edges.last_mut().expect("edge").to.port = "back".into();
    assert!(matches!(
        plan_with(
            &Levels,
            &cycle,
            &Catalogue::empty(),
            &PlanInputs::new(run())
        ),
        Err(PlanError::Job(_))
    ));
}

/// PN-T20: the totals of no steps, and of steps whose dependencies point forward, are what the
/// arithmetic says: nothing, and each step on its own.
#[test]
fn pn_t20_totals_at_the_edges() {
    let none = Totals::of(&[]);
    assert_eq!(none.basis, Basis::Default);
    assert_eq!(none.observations, 0);
    assert!(none.critical_path.is_empty());
    assert_eq!(none.finish.p50, 0.0);

    let mut plan = levels(&chain());
    plan.steps.reverse();
    let totals = Totals::of(&plan.steps);
    assert_eq!(
        totals.critical_path.len(),
        1,
        "a dependency forward is not followed"
    );
}

/// PN-T15's other half (PN-I14): the verified document renders every table for a diagnosis;
/// every enum value has its schema name and an unknown one renders as unknown; a plan is not a
/// size-prefixed buffer; the struct's fields set and read back.
#[test]
fn pn_t15_the_generated_tables_render() {
    use moruna_plan::fb;
    let bytes = levels(&diamond()).to_flatbuffer();
    let root = fb::root_as_plan(&bytes).expect("verifies");
    let shown = format!("{root:?}");
    for table in [
        "Plan",
        "Step",
        "UnitCost",
        "Estimate",
        "Resources",
        "Planner",
        "Totals",
        "Range",
    ] {
        assert!(shown.contains(table), "{table} in {shown}");
    }
    for unit in fb::Unit::ENUM_VALUES {
        assert!(unit.variant_name().is_some());
        assert!(!format!("{unit:?}").contains("UNKNOWN"));
    }
    for basis in fb::Basis::ENUM_VALUES {
        assert!(basis.variant_name().is_some());
        assert!(!format!("{basis:?}").contains("UNKNOWN"));
    }
    assert!(fb::Unit(9).variant_name().is_none());
    assert!(format!("{:?}", fb::Unit(9)).contains("UNKNOWN"));
    assert!(format!("{:?}", fb::Basis(9)).contains("UNKNOWN"));
    // A buffer without its size prefix is not a size-prefixed plan.
    assert!(!fb::plan_size_prefixed_buffer_has_identifier(&bytes));
    assert!(fb::size_prefixed_root_as_plan(&bytes).is_err());
    let mut range = fb::Range::new(1.0, 2.0);
    range.set_p50(3.0);
    range.set_p95(4.0);
    assert_eq!((range.p50(), range.p95()), (3.0, 4.0));
    assert!(format!("{range:?}").contains("p95"));
}
