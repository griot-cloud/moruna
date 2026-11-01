//! SDD 24 k: the simulator, its policy port, its Monte Carlo runs and morsel sizing (CM-T22 to
//! CM-T31), and its start from a live run and its resize (CM-T38, CM-T39).

use moruna_cost::morsel::{Bound, MAX_MORSEL_ROWS};
use moruna_cost::sim::{
    Admission, Machine, Policy, Scaling, Simulator, Start, TaskLoad, TaskStart, View, Workload,
};
use moruna_cost::{
    Basis, CostError, CostKey, CostModel, Digest, Estimate, FitReport, Inputs, ModelId,
    ObservationSource, Outcome, Rng, Target, Unit, UnitCost, Z95, morsel, normal_quantile,
};
use moruna_job::Role;

/// A unit known exactly: `seconds` long, `peak` bytes at its peak, `out` bytes out, `read`
/// bytes read.
fn unit(seconds: f64, peak: f64, out: f64, read: f64) -> UnitCost {
    let mut u = UnitCost::zero();
    u.duration = Estimate::exact(seconds, Unit::Seconds);
    u.worker_time = Estimate::exact(seconds, Unit::Seconds);
    u.peak_memory = Estimate::exact(peak, Unit::Bytes);
    u.bytes_out = Estimate::exact(out, Unit::Bytes);
    u.read_bytes = Estimate::exact(read, Unit::Bytes);
    u
}

fn task(name: &str, units: u32, seconds: f64) -> TaskLoad {
    TaskLoad::new(name, units, unit(seconds, 0.0, 1.0, 0.0))
}

fn machine(workers: u32) -> Machine {
    Machine {
        workers,
        memory_bytes: 1e12,
        disk_bytes_per_second: 1e9,
    }
}

fn simulate(tasks: Vec<TaskLoad>, edges: &[(usize, usize)], m: Machine) -> moruna_cost::sim::Run {
    let workload = Workload {
        tasks,
        edges: edges.to_vec(),
        checkpoint_interval_seconds: None,
    };
    let mut sim = Simulator::new(&workload, m).expect("a workload");
    sim.run(&mut Admission).expect("simulated").clone()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// CM-T22 (CM-I12): a chain, a fan-out, a fan-in and a diamond with exact costs finish at the
/// optimum a hand schedule gives, to the microsecond, with the critical path through what set
/// it; start-up is paid once per task and a checkpoint once per interval.
#[test]
fn cm_t22_hand_computed_schedules() {
    // A chain of one unit each: 1 + 2 + 0.5.
    let run = simulate(
        vec![task("a", 1, 1.0), task("b", 1, 2.0), task("c", 1, 0.5)],
        &[(0, 1), (1, 2)],
        machine(2),
    );
    assert!(close(run.finish, 3.5), "{}", run.finish);
    assert_eq!(run.critical_path, [0, 1, 2]);
    assert!(close(run.tasks[1].start, 1.0) && close(run.tasks[1].end, 3.0));
    assert!(close(run.worker_seconds, 3.5));

    // A pipelined chain on one worker: every unit in turn, 4 × 0.25 + 4 × 0.5.
    let run = simulate(
        vec![task("a", 4, 0.25), task("b", 4, 0.5)],
        &[(0, 1)],
        machine(1),
    );
    assert!(close(run.finish, 3.0), "{}", run.finish);

    // A fan-out: a, then b and c side by side; c sets the finish.
    let run = simulate(
        vec![task("a", 1, 1.0), task("b", 1, 2.0), task("c", 1, 3.0)],
        &[(0, 1), (0, 2)],
        machine(2),
    );
    assert!(close(run.finish, 4.0), "{}", run.finish);
    assert_eq!(run.critical_path, [0, 2]);

    // Two units each through a fan-out on two workers: six seconds of work, three of time.
    let run = simulate(
        vec![task("a", 2, 1.0), task("b", 2, 1.0), task("c", 2, 1.0)],
        &[(0, 1), (0, 2)],
        machine(2),
    );
    assert!(close(run.finish, 3.0), "{}", run.finish);
    assert_eq!(run.tasks[0].max_running, 2);

    // A fan-in: c waits for the slower of a and b.
    let run = simulate(
        vec![task("a", 1, 1.0), task("b", 1, 2.0), task("c", 1, 1.0)],
        &[(0, 2), (1, 2)],
        machine(2),
    );
    assert!(close(run.finish, 3.0), "{}", run.finish);
    assert_eq!(run.critical_path, [1, 2]);

    // A diamond: 1 + max(2, 4) + 1 on two workers, the sum on one.
    let diamond = || {
        vec![
            task("a", 1, 1.0),
            task("b", 1, 2.0),
            task("c", 1, 4.0),
            task("d", 1, 1.0),
        ]
    };
    let edges = [(0, 1), (0, 2), (1, 3), (2, 3)];
    let run = simulate(diamond(), &edges, machine(2));
    assert!(close(run.finish, 6.0), "{}", run.finish);
    assert_eq!(run.critical_path, [0, 2, 3]);
    let run = simulate(diamond(), &edges, machine(1));
    assert!(close(run.finish, 8.0), "{}", run.finish);

    // Start-up once per task: 4 units of work and 0.5 + 0.25 of start-up on one worker.
    let mut a = task("a", 2, 1.0);
    a.start_seconds = 0.5;
    let mut b = task("b", 2, 1.0);
    b.start_seconds = 0.25;
    let run = simulate(vec![a, b], &[(0, 1)], machine(1));
    assert!(close(run.finish, 4.75), "{}", run.finish);

    // A checkpoint every 1.5 s of 0.1 s, due at 1.5 and 3.0, paid by the next unit to start.
    let mut a = task("a", 4, 1.0);
    a.checkpoint_seconds = 0.1;
    a.checkpoint_bytes = 1_000.0;
    let workload = Workload {
        tasks: vec![a],
        edges: Vec::new(),
        checkpoint_interval_seconds: Some(1.5),
    };
    let mut sim = Simulator::new(&workload, machine(1)).expect("a workload");
    let run = sim.run(&mut Admission).expect("simulated");
    assert!(close(run.finish, 4.2), "{}", run.finish);
    assert!(close(run.disk_bytes, 2_000.0));
    assert!(close(run.worker_seconds, 4.2));
}

/// Takes the earliest task's unit whether or not it fits, so a test can force the working set
/// past the budget.
struct EarliestFirst;

impl Policy for EarliestFirst {
    fn name(&self) -> &str {
        "earliest_first"
    }

    fn choose(&mut self, view: &View<'_>) -> Option<usize> {
        (0..view.candidates.len()).min_by_key(|&i| view.candidates[i].task)
    }
}

/// CM-T23 (CM-I13, CM-I14): a working set past the budget spills by known-future eviction, and
/// the spill and read-back bytes are the hand count; memory held never passes the budget; a unit
/// that can never fit is infeasible, by name.
#[test]
fn cm_t23_memory_is_a_hard_limit() {
    // a: four units of 100 bytes out; c reads them in order. 250 bytes: a2 and a3 each need a
    // buffer spilled; the one needed furthest away goes (a1, then a2), at 100 bytes a second.
    let a = TaskLoad::new("a", 4, unit(1.0, 100.0, 100.0, 0.0));
    let c = TaskLoad::new("c", 4, unit(1.0, 0.0, 0.0, 0.0));
    let workload = Workload {
        tasks: vec![a, c],
        edges: vec![(0, 1)],
        checkpoint_interval_seconds: None,
    };
    let m = Machine {
        workers: 1,
        memory_bytes: 250.0,
        disk_bytes_per_second: 100.0,
    };
    let mut sim = Simulator::new(&workload, m).expect("a workload");
    let run = sim.run(&mut EarliestFirst).expect("simulated");
    assert!(close(run.spill_bytes, 200.0), "spilled {}", run.spill_bytes);
    assert!(
        close(run.reload_bytes, 200.0),
        "read back {}",
        run.reload_bytes
    );
    assert!(close(run.disk_bytes, 400.0));
    assert!(close(run.finish, 12.0), "{}", run.finish);
    assert!(run.peak_memory <= 250.0, "peak {}", run.peak_memory);
    assert!(
        close(run.idle_worker_seconds, 2.0),
        "{}",
        run.idle_worker_seconds
    );
    assert!(close(run.tasks[0].spill_bytes, 200.0));
    assert_eq!(sim.machine(), m);
    assert_eq!(sim.names(), ["a", "c"]);

    // The admission rule drains a's outputs as they come, so nothing spills.
    let run = sim.run(&mut Admission).expect("simulated");
    assert_eq!(run.spill_bytes, 0.0);
    assert!(close(run.finish, 8.0), "{}", run.finish);

    // A unit whose 95th-percentile peak passes the budget can never start.
    let mut big = unit(1.0, 100.0, 0.0, 0.0);
    big.peak_memory.p95 = 300.0;
    let workload = Workload {
        tasks: vec![TaskLoad::new("huge", 1, big)],
        edges: Vec::new(),
        checkpoint_interval_seconds: None,
    };
    match Simulator::new(&workload, m) {
        Err(CostError::Infeasible { task, needs, .. }) => {
            assert_eq!(task, "huge");
            assert_eq!(needs, 300.0);
        }
        other => panic!("{:?}", other.err()),
    }
    // Nor one whose task's state grows past it.
    let mut stateful = TaskLoad::new("keeps", 4, unit(1.0, 10.0, 0.0, 0.0));
    stateful.unit.state_growth = Estimate::exact(100.0, Unit::Bytes);
    stateful.stateful = true;
    let workload = Workload {
        tasks: vec![stateful],
        edges: Vec::new(),
        checkpoint_interval_seconds: None,
    };
    let refused = Simulator::new(&workload, m).err().expect("refused");
    assert!(
        refused.to_string().contains("`keeps` is infeasible"),
        "{refused}"
    );
}

/// CM-T24 (CM-I15): two disk-heavy tasks side by side take the time the bandwidth says; the
/// workers likewise; a kind's scaling curve slows its concurrent units and caps them.
#[test]
fn cm_t24_contention() {
    let reader = |name: &str| TaskLoad::new(name, 1, unit(1.0, 0.0, 0.0, 1_000.0));
    let disk = |bandwidth: f64| Machine {
        workers: 2,
        memory_bytes: 1e9,
        disk_bytes_per_second: bandwidth,
    };
    // Each reads 1,000 bytes in a second alone; together they want 2,000 a second.
    let run = simulate(vec![reader("a"), reader("b")], &[], disk(1_000.0));
    assert!(close(run.finish, 2.0), "{}", run.finish);
    let run = simulate(vec![reader("a"), reader("b")], &[], disk(1_500.0));
    assert!(close(run.finish, 4.0 / 3.0), "{}", run.finish);
    let run = simulate(vec![reader("a"), reader("b")], &[], disk(4_000.0));
    assert!(close(run.finish, 1.0), "{}", run.finish);

    // Units that use two workers' time each: four workers' demand on two.
    let mut busy = unit(1.0, 0.0, 0.0, 0.0);
    busy.worker_time = Estimate::exact(2.0, Unit::Seconds);
    let run = simulate(
        vec![TaskLoad::new("a", 1, busy), TaskLoad::new("b", 1, busy)],
        &[],
        machine(2),
    );
    assert!(close(run.finish, 2.0), "{}", run.finish);

    // Two units of a kind that speeds up 1.5 times at two: each at three quarters.
    let mut scaled = task("a", 2, 1.0);
    scaled.scaling = Scaling::from_speedups(Some(1.5), None);
    let run = simulate(vec![scaled], &[], machine(2));
    assert!(close(run.finish, 4.0 / 3.0), "{}", run.finish);
    // A kind that gains nothing runs one at a time; so does a stateful one.
    let mut serial = task("a", 2, 1.0);
    serial.scaling = Scaling::serial();
    let run = simulate(vec![serial], &[], machine(2));
    assert!(close(run.finish, 2.0), "{}", run.finish);
    assert_eq!(run.tasks[0].max_running, 1);
    let mut stateful = task("a", 2, 1.0);
    stateful.stateful = true;
    let run = simulate(vec![stateful], &[], machine(2));
    assert!(close(run.finish, 2.0), "{}", run.finish);
}

/// A unit with a log-normal duration of median 1 s and 95th percentile 2 s.
fn spread_unit() -> UnitCost {
    let mut u = unit(1.0, 0.0, 0.0, 0.0);
    u.duration.p95 = 2.0;
    u
}

/// CM-T25 (CM-I16): with known distributions, the Monte Carlo p50 and p95 land within 5% of the
/// analytic answer; the same seed gives the same forecast; no draws give the medians' run.
#[test]
fn cm_t25_monte_carlo() {
    let sigma = 2f64.ln() / Z95;
    let analytic = |q: f64| (1.0 + 1e-9) * (sigma * normal_quantile(q)).exp() - 1e-9;

    let one = Workload {
        tasks: vec![TaskLoad::new("a", 1, spread_unit())],
        edges: Vec::new(),
        checkpoint_interval_seconds: None,
    };
    let mut sim = Simulator::new(&one, machine(1)).expect("a workload");
    let forecast = sim.forecast(&mut Admission, 4_000, 11).expect("forecast");
    assert!(
        (forecast.finish.p50 / analytic(0.5) - 1.0).abs() < 0.05,
        "{:?}",
        forecast.finish
    );
    assert!(
        (forecast.finish.p95 / analytic(0.95) - 1.0).abs() < 0.05,
        "{:?}",
        forecast.finish
    );
    assert!(close(forecast.median.finish, 1.0));
    assert_eq!(forecast.draws, 4_000);
    assert!(forecast.tasks[0].span.p95 >= forecast.tasks[0].span.p50);

    // Two independent units side by side: the finish is the larger, P(max <= x) = F(x)².
    let two = Workload {
        tasks: vec![
            TaskLoad::new("a", 1, spread_unit()),
            TaskLoad::new("b", 1, spread_unit()),
        ],
        edges: Vec::new(),
        checkpoint_interval_seconds: None,
    };
    let mut sim = Simulator::new(&two, machine(2)).expect("a workload");
    let forecast = sim.forecast(&mut Admission, 4_000, 12).expect("forecast");
    let p50 = analytic(0.5f64.sqrt());
    let p95 = analytic(0.95f64.sqrt());
    assert!(
        (forecast.finish.p50 / p50 - 1.0).abs() < 0.05,
        "{:?} {p50}",
        forecast.finish
    );
    assert!(
        (forecast.finish.p95 / p95 - 1.0).abs() < 0.05,
        "{:?} {p95}",
        forecast.finish
    );

    let again = sim.forecast(&mut Admission, 4_000, 12).expect("forecast");
    assert_eq!(again, forecast, "the same seed, the same forecast");
    let none = sim.forecast(&mut Admission, 0, 12).expect("forecast");
    assert_eq!(none.finish.p50, none.median.finish);
    assert_eq!(none.finish.p95, none.median.finish);
    assert_eq!(none.draws, 0);
}

/// Always chooses a candidate that is not there.
struct OutOfRange;

impl Policy for OutOfRange {
    fn name(&self) -> &str {
        "out_of_range"
    }

    fn choose(&mut self, view: &View<'_>) -> Option<usize> {
        Some(view.candidates.len())
    }
}

/// Never chooses.
struct Never;

impl Policy for Never {
    fn name(&self) -> &str {
        "never"
    }

    fn choose(&mut self, _: &View<'_>) -> Option<usize> {
        None
    }
}

/// CM-T29 (CM-I12): a workload the simulator cannot run is refused by what is wrong; a policy
/// that chooses nothing, or a candidate that is not there, stops the simulation with its name.
#[test]
fn cm_t29_refusals() {
    let ok = || Workload {
        tasks: vec![task("a", 1, 1.0), task("b", 1, 1.0)],
        edges: vec![(0, 1)],
        checkpoint_interval_seconds: None,
    };
    let refused = |w: &Workload, m: Machine| match Simulator::new(w, m) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("accepted"),
    };
    let mut m = machine(1);
    m.workers = 0;
    assert!(refused(&ok(), m).contains("no worker"));
    let mut m = machine(1);
    m.memory_bytes = f64::NAN;
    assert!(refused(&ok(), m).contains("memory budget"));
    let mut m = machine(1);
    m.disk_bytes_per_second = 0.0;
    assert!(refused(&ok(), m).contains("bandwidth"));
    let mut w = ok();
    w.checkpoint_interval_seconds = Some(0.0);
    assert!(refused(&w, machine(1)).contains("checkpoint interval"));
    let mut w = ok();
    w.edges.push((0, 5));
    assert!(refused(&w, machine(1)).contains("does not have"));
    let mut w = ok();
    w.edges.push((1, 1));
    assert!(refused(&w, machine(1)).contains("its own output"));
    let mut w = ok();
    w.edges.push((1, 0));
    assert!(refused(&w, machine(1)).contains("cycle"));
    let mut w = ok();
    w.tasks[1].units = 0;
    assert!(refused(&w, machine(1)).contains("no unit"));
    let mut w = ok();
    w.tasks[1].unit.duration.p95 = 0.5;
    assert!(refused(&w, machine(1)).contains("duration"));
    let mut w = ok();
    w.tasks[1].start_seconds = -1.0;
    assert!(refused(&w, machine(1)).contains("start-up"));

    let mut sim = Simulator::new(&ok(), machine(1)).expect("a workload");
    let stopped = sim.run(&mut Never).expect_err("stopped").to_string();
    assert!(stopped.contains("policy `never`"), "{stopped}");
    let stopped = sim.run(&mut OutOfRange).expect_err("stopped").to_string();
    assert!(stopped.contains("chose candidate 1 of 1"), "{stopped}");
    // Duplicate edges are one edge.
    let mut w = ok();
    w.edges.push((0, 1));
    let mut sim = Simulator::new(&w, machine(1)).expect("a workload");
    assert!(close(
        sim.run(&mut Admission).expect("simulated").finish,
        2.0
    ));
}

/// CM-T28 (CM-I16): an estimate's quantiles are the log-normal through its median and its 95th
/// percentile; draws from it have that median; the sum of `n` units adds medians and spreads
/// in quadrature.
#[test]
fn cm_t28_sampling_an_estimate() {
    let e = Estimate {
        p50: 10.0,
        p95: 40.0,
        unit: Unit::Seconds,
        basis: Basis::Measured,
        observations: 3,
        extrapolated: false,
        drifting: false,
    };
    assert!((e.quantile(0.5) - 10.0).abs() < 1e-6);
    assert!((e.quantile(0.95) - 40.0).abs() < 1e-5);
    assert!(e.quantile(0.05) < 10.0);
    let mut rng = Rng::new(3);
    let mut draws: Vec<f64> = (0..20_001).map(|_| e.sample(&mut rng)).collect();
    draws.sort_by(f64::total_cmp);
    assert!(
        (draws[10_000] / 10.0 - 1.0).abs() < 0.05,
        "{}",
        draws[10_000]
    );
    let exact = Estimate::exact(5.0, Unit::Rows);
    assert_eq!(exact.sigma(), 0.0);
    assert_eq!(exact.quantile(0.99), 5.0);
    let sum = e.sum(4.0);
    assert_eq!((sum.p50, sum.p95), (40.0, 100.0));
    assert_eq!(sum.basis, Basis::Measured);
    assert_eq!(e.scaled(0.5).p95, 20.0);
    assert!(e.check(Unit::Seconds).is_ok());
    assert!(e.check(Unit::Bytes).unwrap_err().contains("counts bytes"));
    let mut bad = e;
    bad.p95 = 1.0;
    assert!(bad.check(Unit::Seconds).unwrap_err().contains("below"));
    bad.p50 = -1.0;
    assert!(bad.check(Unit::Seconds).unwrap_err().contains("negative"));
    bad.p50 = f64::INFINITY;
    assert!(bad.check(Unit::Seconds).unwrap_err().contains("finite"));
    let mut cost = UnitCost::zero();
    assert!(cost.check().is_ok());
    cost.rows_out.unit = Unit::Bytes;
    assert!(cost.check().unwrap_err().starts_with("rows_out"));
    let mut cost = UnitCost::zero();
    cost.duration = e;
    assert_eq!(cost.weakest(), (Basis::Default, 0));
}

/// A model whose cost is a straight line in rows: `overhead + per_row · rows` seconds, and a
/// peak of `peak_fixed + peak_per_row · rows` bytes.
struct Linear {
    overhead: f64,
    per_row: f64,
    peak_fixed: f64,
    peak_per_row: f64,
}

impl CostModel for Linear {
    fn id(&self) -> ModelId {
        ModelId::new("linear", 1)
    }

    fn predict(&self, _: &CostKey, inputs: &Inputs) -> UnitCost {
        let rows = inputs.data.rows.unwrap_or(0) as f64;
        let mut u = UnitCost::zero();
        u.duration = Estimate::exact(self.overhead + self.per_row * rows, Unit::Seconds);
        u.peak_memory =
            Estimate::declared(self.peak_fixed + self.peak_per_row * rows, 2.0, Unit::Bytes);
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

fn key() -> CostKey {
    CostKey {
        role: Role::Kernel,
        kind: "std".into(),
        fingerprint: Digest([1; 32]),
        settings: Digest([2; 32]),
        schema: Digest([3; 32]),
    }
}

/// CM-T27 (CM-I17): any model answers its per-batch overhead and per-row cost through two
/// predictions; a high overhead gives bigger morsels, a tight memory smaller ones, and each says
/// what bounded it.
#[test]
fn cm_t27_morsel_sizing() {
    let low = Linear {
        overhead: 1e-4,
        per_row: 1e-7,
        peak_fixed: 0.0,
        peak_per_row: 64.0,
    };
    let inputs = Inputs::batch(1, 64);
    let line = low.marginal(&key(), &inputs, Target::Duration, 10_000);
    assert!((line.p50.fixed - 1e-4).abs() < 1e-12 && (line.p50.per_row - 1e-7).abs() < 1e-15);
    let peak = low.marginal(&key(), &inputs, Target::PeakMemory, 10_000);
    assert!((peak.p95.per_row - 128.0).abs() < 1e-6, "{peak:?}");

    let total = 100_000_000;
    let small = morsel(&low, &key(), &inputs, total, 1e12, 4);
    // 1e-4 · 0.95 / (0.05 · 1e-7) = 19,000 rows.
    assert_eq!(small.bound, Bound::Overhead);
    assert!((18_990..=19_010).contains(&small.rows), "{}", small.rows);
    assert_eq!(small.units, total.div_ceil(small.rows) as u32);
    let high = Linear {
        overhead: 1e-2,
        ..low
    };
    let big = morsel(&high, &key(), &inputs, total, 1e12, 4);
    assert!(big.rows > small.rows, "{} vs {}", big.rows, small.rows);
    assert_eq!(big.bound, Bound::Overhead);
    let huge = Linear {
        overhead: 1.0,
        ..low
    };
    assert_eq!(
        morsel(&huge, &key(), &inputs, total, 1e12, 4).rows,
        MAX_MORSEL_ROWS
    );

    // 128 bytes a row at the p95 and one fixed: 640,100 bytes hold 5,000 rows.
    let tight = morsel(&high, &key(), &inputs, total, 640_100.0, 4);
    assert_eq!(tight.bound, Bound::Memory);
    assert_eq!(tight.rows, 5_000);
    assert!(tight.rows < big.rows);
    // Every worker gets a batch; a small input is one morsel.
    let few = morsel(&high, &key(), &inputs, 40_000, 1e12, 4);
    assert_eq!(
        (few.rows, few.units, few.bound),
        (10_000, 4, Bound::Workers)
    );
    let one = morsel(&high, &key(), &inputs, 500, 1e12, 1);
    assert_eq!((one.rows, one.units, one.bound), (500, 1, Bound::Rows));
    // No overhead: the smallest morsel.
    let free = Linear {
        overhead: 0.0,
        ..low
    };
    assert_eq!(
        morsel(&free, &key(), &inputs, total, 1e12, 4).bound,
        Bound::Smallest
    );
}

/// A 50-task DAG: five sources, then each task takes the one before it and the one five before.
fn fifty(units: u32) -> Workload {
    let mut tasks = Vec::new();
    let mut edges = Vec::new();
    for i in 0..50usize {
        let mut u = unit(1e-3 * (1.0 + (i % 7) as f64), 4_096.0, 1_024.0, 0.0);
        u.duration.p95 = u.duration.p50 * 2.0;
        tasks.push(TaskLoad::new(format!("t{i}"), units, u));
        if i >= 5 {
            edges.push((i - 1, i));
            edges.push((i - 5, i));
        }
    }
    Workload {
        tasks,
        edges,
        checkpoint_interval_seconds: None,
    }
}

/// CM-T26 (CM-I18): the simulator's speed, a 50-task DAG over 10,000 batches (200 a task) and
/// over 10,000 batches a task, on 16 workers. It reports rather than asserts a time
/// (CONTRIBUTING: a figure that needs particular hardware reports); it is run on the reference
/// host in an optimised build:
/// `cargo test -p moruna-cost --release --test sim -- --ignored --nocapture cm_t26`.
#[test]
#[ignore = "a measurement for the reference host, in an optimised build"]
fn cm_t26_simulation_speed() {
    let m = Machine {
        workers: 16,
        memory_bytes: 64.0 * 1_048_576.0,
        disk_bytes_per_second: 500e6,
    };
    for units in [200u32, 10_000] {
        let mut sim = Simulator::new(&fifty(units), m).expect("a workload");
        let warm = sim.run(&mut Admission).expect("simulated").events;
        assert!(warm > 0);
        let mut times = Vec::new();
        for _ in 0..if units == 200 { 50 } else { 5 } {
            let at = std::time::Instant::now();
            let run = sim.run(&mut Admission).expect("simulated");
            times.push(at.elapsed().as_secs_f64());
            assert!(run.finish > 0.0 && run.peak_memory <= m.memory_bytes);
        }
        times.sort_by(f64::total_cmp);
        let median = times[times.len() / 2];
        let per_unit = median / f64::from(units * 50);
        eprintln!(
            "cm_t26: 50 tasks x {units} batches ({} units, {warm} events): median {:.3} ms, \
             {:.0} ns a unit",
            units * 50,
            median * 1e3,
            per_unit * 1e9
        );
    }
    let mut sim = Simulator::new(&fifty(200), m).expect("a workload");
    let at = std::time::Instant::now();
    let forecast = sim
        .forecast(
            &mut Admission,
            moruna_cost::sim::DRAWS,
            moruna_cost::sim::SEED,
        )
        .expect("forecast");
    eprintln!(
        "cm_t26: forecast of 10,000 units, the medians and {} draws: {:.3} ms",
        forecast.draws,
        at.elapsed().as_secs_f64() * 1e3
    );
    assert!(forecast.finish.p95 >= forecast.finish.p50);
}

/// Chooses the candidate of the first task its script names that is a candidate, the script
/// read once from its start; when none is, the first candidate.
struct Script(Vec<usize>, usize);

impl Policy for Script {
    fn name(&self) -> &str {
        "script"
    }

    fn reset(&mut self) {
        self.1 = 0;
    }

    fn choose(&mut self, view: &View<'_>) -> Option<usize> {
        while let Some(&task) = self.0.get(self.1) {
            self.1 += 1;
            if let Some(i) = view.candidates.iter().position(|c| c.task == task) {
                return Some(i);
            }
        }
        Some(0)
    }
}

/// Chooses at random, from its own seeded generator.
struct Random(Rng);

impl Policy for Random {
    fn name(&self) -> &str {
        "random"
    }

    fn choose(&mut self, view: &View<'_>) -> Option<usize> {
        Some((self.0.next_u64() % view.candidates.len() as u64) as usize)
    }
}

/// CM-T30 (CM-I13, CM-I14; `architecture/adaptive-runtime.md` section 12.5): under any policy,
/// on random workloads with random budgets, memory held never passes the budget, every unit runs,
/// the same seed gives the same run, and a workload either finishes or is refused as infeasible
/// by name; a unit that fits alone but not beside pinned state is infeasible at run time.
#[test]
fn cm_t30_the_hard_rule_holds_on_random_workloads() {
    let mut rng = Rng::new(0x5eed);
    let mut finished = 0;
    let mut spilled = 0;
    for round in 0..400 {
        let n = 2 + (rng.next_u64() % 6) as usize;
        let mut tasks = Vec::new();
        let mut edges = Vec::new();
        let mut most = 0.0f64;
        for i in 0..n {
            let mut u = unit(
                0.1 + rng.open01(),
                (rng.open01() * 200.0).round(),
                (rng.open01() * 100.0).round(),
                (rng.open01() * 500.0).round(),
            );
            u.duration.p95 = u.duration.p50 * (1.0 + rng.open01());
            u.peak_memory.p95 = u.peak_memory.p50 * (1.0 + rng.open01());
            let mut t = TaskLoad::new(format!("t{i}"), 1 + (rng.next_u64() % 6) as u32, u);
            t.stateful = rng.next_u64().is_multiple_of(5);
            if t.stateful {
                t.unit.state_growth = Estimate::exact((rng.open01() * 20.0).round(), Unit::Bytes);
            }
            t.scaling = Scaling::from_speedups(Some(1.0 + rng.open01()), None);
            t.start_seconds = rng.open01() * 0.2;
            t.checkpoint_seconds = rng.open01() * 0.05;
            t.checkpoint_bytes = (rng.open01() * 100.0).round();
            most = most.max(t.unit.peak_memory.p95);
            for j in 0..i {
                if rng.next_u64().is_multiple_of(3) {
                    edges.push((j, i));
                }
            }
            tasks.push(t);
        }
        let workload = Workload {
            tasks,
            edges,
            checkpoint_interval_seconds: (round % 2 == 0).then_some(0.7),
        };
        let m = Machine {
            workers: 1 + (rng.next_u64() % 4) as u32,
            memory_bytes: most + 1.0 + rng.open01() * 600.0,
            disk_bytes_per_second: 200.0 + rng.open01() * 2_000.0,
        };
        let mut sim = match Simulator::new(&workload, m) {
            Ok(sim) => sim,
            Err(CostError::Infeasible { task, .. }) => {
                assert!(task.starts_with('t'));
                continue;
            }
            Err(other) => panic!("round {round}: {other}"),
        };
        let seed = rng.next_u64();
        let mut policy = Random(Rng::new(seed));
        let first = match sim.run(&mut policy) {
            Ok(run) => run.clone(),
            Err(CostError::Infeasible { task, .. }) => {
                assert!(task.starts_with('t'));
                continue;
            }
            Err(other) => panic!("round {round}: {other}"),
        };
        assert!(
            first.peak_memory <= m.memory_bytes * (1.0 + 1e-9),
            "round {round}: held {} of {}",
            first.peak_memory,
            m.memory_bytes
        );
        for (t, task) in first.tasks.iter().enumerate() {
            assert!(task.end >= task.start, "round {round} task {t}");
            assert!(task.end <= first.finish + 1e-9);
            assert!(task.max_running >= 1);
        }
        let mut again = Random(Rng::new(seed));
        assert_eq!(
            sim.run(&mut again).expect("the same run"),
            &first,
            "round {round}"
        );
        let mut draws = Rng::new(seed);
        let sampled = sim.run_sampled(&mut Admission, &mut draws).expect("a draw");
        assert!(sampled.peak_memory <= m.memory_bytes * (1.0 + 1e-9));
        finished += 1;
        if first.spill_bytes > 0.0 {
            spilled += 1;
        }
    }
    assert!(finished > 200, "{finished} of 400 finished");
    assert!(spilled > 10, "only {spilled} spilled");

    // Fits alone, not beside the state another task pins: infeasible when nothing can move.
    let mut keeps = TaskLoad::new("keeps", 2, unit(1.0, 10.0, 0.0, 0.0));
    keeps.unit.state_growth = Estimate::exact(100.0, Unit::Bytes);
    keeps.stateful = true;
    let other = TaskLoad::new("other", 1, unit(1.0, 200.0, 0.0, 0.0));
    let workload = Workload {
        tasks: vec![keeps, other],
        edges: Vec::new(),
        checkpoint_interval_seconds: None,
    };
    let m = Machine {
        workers: 1,
        memory_bytes: 250.0,
        disk_bytes_per_second: 100.0,
    };
    let mut sim = Simulator::new(&workload, m).expect("each fits alone");
    match sim.run(&mut Script(vec![0, 1], 0)) {
        Err(CostError::Infeasible { task, reason, .. }) => {
            assert_eq!(task, "other");
            assert!(reason.contains("held by state"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert!(sim.run(&mut Script(vec![1, 0], 0)).is_ok());
}

/// CM-T31 (CM-I19): no unit of a task runs before its start-up ends. A task of eight 0.1 s
/// units with a 2 s cold start beside one of four 0.5 s units, on four workers: the first unit
/// starts the process at 0 and ends at 2.1; the others wait for the process, not for workers,
/// which run the other task meanwhile (it ends at 1.0); three more run 2.0 to 2.1 and the last
/// four 2.1 to 2.2. Were the others to start at once, it would forecast 2.1.
#[test]
fn cm_t31_units_wait_for_their_start_up() {
    let other = task("other", 4, 0.5);
    let mut cold = task("cold", 8, 0.1);
    cold.start_seconds = 2.0;
    let run = simulate(vec![other, cold], &[], machine(4));
    assert!(close(run.finish, 2.2), "{}", run.finish);
    assert!(close(run.tasks[0].end, 1.0), "{}", run.tasks[0].end);
    assert!(close(run.tasks[1].start, 0.0));
    assert_eq!(run.tasks[1].max_running, 4);
    assert!(
        close(run.worker_seconds, 2.0 + 0.8 + 2.0),
        "{}",
        run.worker_seconds
    );

    // The same cold task feeding a consumer, on two workers: its second unit starts when the
    // process is up (2.0) and both end at 2.1, where the consumer's two start.
    let mut cold = task("cold", 2, 0.1);
    cold.start_seconds = 2.0;
    let run = simulate(vec![cold, task("next", 2, 0.1)], &[(0, 1)], machine(2));
    assert!(close(run.tasks[1].start, 2.1), "{}", run.tasks[1].start);
    assert!(close(run.finish, 2.2), "{}", run.finish);
}

fn workload(tasks: Vec<TaskLoad>, edges: &[(usize, usize)]) -> Workload {
    Workload {
        tasks,
        edges: edges.to_vec(),
        checkpoint_interval_seconds: None,
    }
}

/// CM-T38 (CM-I23): a simulation starts where a live run is. A chain of four units a stage on
/// one worker, a's first three finished, its fourth under way with half its work left, b's first
/// finished and its second waiting: the rest is a's half second, then b's three seconds, 3.5 s
/// from the snapshot, and the critical path runs through both. An output on disk is read back
/// before its consumer runs. Everything finished is 0 s. A stateful task's state from its
/// finished units is held. Each malformed start is refused saying what is wrong, and a fresh
/// start is a fresh run.
#[test]
fn cm_t38_a_simulation_starts_from_a_live_run() {
    let chain = workload(vec![task("a", 4, 1.0), task("b", 4, 1.0)], &[(0, 1)]);
    let mut sim = Simulator::new(&chain, machine(1)).expect("a workload");
    let start = Start {
        tasks: vec![
            TaskStart {
                done: 3,
                running: vec![0.5],
                on_disk: Vec::new(),
            },
            TaskStart {
                done: 1,
                running: Vec::new(),
                on_disk: Vec::new(),
            },
        ],
    };
    let run = sim
        .run_from(&start, &mut Admission)
        .expect("simulated")
        .clone();
    assert!(close(run.finish, 3.5), "{}", run.finish);
    assert_eq!(run.critical_path, [0, 1]);
    assert!(close(run.worker_seconds, 3.5), "{}", run.worker_seconds);
    assert!(close(run.tasks[0].end, 0.5));
    assert_eq!(run.spill_bytes, 0.0);
    // The same simulator runs fresh again afterwards, as if nothing had started.
    let fresh = sim.run(&mut Admission).expect("simulated").clone();
    assert!(
        close(fresh.finish, 8.0),
        "eight units on one worker: {}",
        fresh.finish
    );
    assert_eq!(
        sim.run_from(&Start::fresh(2), &mut Admission)
            .expect("simulated")
            .finish,
        fresh.finish
    );

    // Two of a's outputs b still needs, one on disk: it is read back first (100 bytes at 100
    // bytes a second), and the memory held starts at the one in memory.
    let big = |name: &str| TaskLoad::new(name, 3, unit(1.0, 100.0, 100.0, 0.0));
    let pair = workload(vec![big("a"), big("b")], &[(0, 1)]);
    let m = Machine {
        workers: 1,
        memory_bytes: 1e6,
        disk_bytes_per_second: 100.0,
    };
    let mut sim = Simulator::new(&pair, m).expect("a workload");
    let start = Start {
        tasks: vec![
            TaskStart {
                done: 3,
                running: Vec::new(),
                on_disk: vec![1],
            },
            TaskStart {
                done: 1,
                running: Vec::new(),
                on_disk: Vec::new(),
            },
        ],
    };
    let run = sim
        .run_from(&start, &mut Admission)
        .expect("simulated")
        .clone();
    assert!(close(run.reload_bytes, 100.0), "{}", run.reload_bytes);
    assert!(
        close(run.finish, 3.0),
        "b's two units and one read back: {}",
        run.finish
    );
    assert!(run.peak_memory >= 200.0 && run.peak_memory <= m.memory_bytes);

    // Everything finished: nothing to go.
    let all = Start {
        tasks: vec![
            TaskStart {
                done: 3,
                ..TaskStart::default()
            },
            TaskStart {
                done: 3,
                ..TaskStart::default()
            },
        ],
    };
    assert_eq!(
        sim.run_from(&all, &mut Admission)
            .expect("simulated")
            .finish,
        0.0
    );

    // A stateful task's state from its finished units is held from the snapshot on.
    let mut grows = TaskLoad::new("s", 4, unit(1.0, 10.0, 0.0, 0.0));
    grows.unit.state_growth = Estimate::exact(50.0, Unit::Bytes);
    grows.stateful = true;
    let mut sim = Simulator::new(&workload(vec![grows], &[]), machine(1)).expect("a workload");
    let start = Start {
        tasks: vec![TaskStart {
            done: 2,
            ..TaskStart::default()
        }],
    };
    let run = sim
        .run_from(&start, &mut Admission)
        .expect("simulated")
        .clone();
    assert!(close(run.finish, 2.0));
    assert!(run.peak_memory >= 100.0 + 10.0, "{}", run.peak_memory);

    // Refusals, each saying what is wrong.
    let mut sim = Simulator::new(&chain, machine(1)).expect("a workload");
    let refused = |sim: &mut Simulator, start: Start| match sim.run_from(&start, &mut Admission) {
        Err(CostError::Simulation(why)) => why,
        other => panic!("refused, not {other:?}"),
    };
    let one = |done: u32, running: Vec<f64>, on_disk: Vec<u32>| TaskStart {
        done,
        running,
        on_disk,
    };
    let why = refused(&mut sim, Start::fresh(3));
    assert!(why.contains("3 tasks"), "{why}");
    let why = refused(
        &mut sim,
        Start {
            tasks: vec![one(4, vec![1.0], vec![]), one(0, vec![], vec![])],
        },
    );
    assert!(why.contains("4 units"), "{why}");
    let why = refused(
        &mut sim,
        Start {
            tasks: vec![one(1, vec![f64::NAN], vec![]), one(0, vec![], vec![])],
        },
    );
    assert!(why.contains("not a time"), "{why}");
    let why = refused(
        &mut sim,
        Start {
            tasks: vec![one(1, vec![], vec![2]), one(0, vec![], vec![])],
        },
    );
    assert!(why.contains("has not finished"), "{why}");
    let why = refused(
        &mut sim,
        Start {
            tasks: vec![one(1, vec![0.1], vec![]), one(0, vec![0.1], vec![])],
        },
    );
    assert!(why.contains("2 units under way on 1 workers"), "{why}");
    let why = refused(
        &mut sim,
        Start {
            tasks: vec![one(1, vec![], vec![]), one(1, vec![0.5], vec![])],
        },
    );
    assert!(why.contains("its input, unit 1"), "{why}");
    let mut sim = Simulator::new(&pair, m).expect("a workload");
    let why = refused(
        &mut sim,
        Start {
            tasks: vec![one(2, vec![], vec![1]), one(1, vec![0.5], vec![])],
        },
    );
    assert!(why.contains("a unit under way holds it"), "{why}");
}

/// CM-T39 (CM-I24): a simulator follows a resize. A fan-out of eight one-second units on two
/// workers finishes at 4 s, at 2 s after a resize to four, and at 4 s again back at two; a
/// stateful task stays one at a time whatever the workers. Memory shrunk under what a unit needs
/// is `Infeasible` by name and leaves the simulator as it was; a machine with no worker is
/// refused.
#[test]
fn cm_t39_a_simulator_follows_a_resize() {
    let wide = workload(vec![TaskLoad::new("w", 8, unit(1.0, 10.0, 0.0, 0.0))], &[]);
    let mut sim = Simulator::new(&wide, machine(2)).expect("a workload");
    assert!(close(sim.run(&mut Admission).expect("ran").finish, 4.0));
    sim.resize(machine(4)).expect("resized");
    assert_eq!(sim.machine().workers, 4);
    assert!(close(sim.run(&mut Admission).expect("ran").finish, 2.0));
    sim.resize(machine(2)).expect("resized");
    assert!(close(sim.run(&mut Admission).expect("ran").finish, 4.0));

    let mut one = TaskLoad::new("s", 3, unit(1.0, 10.0, 0.0, 0.0));
    one.stateful = true;
    let mut sim = Simulator::new(&workload(vec![one], &[]), machine(1)).expect("a workload");
    sim.resize(machine(8)).expect("resized");
    assert!(close(sim.run(&mut Admission).expect("ran").finish, 3.0));

    let mut sim = Simulator::new(&wide, machine(2)).expect("a workload");
    let small = Machine {
        memory_bytes: 5.0,
        ..machine(2)
    };
    match sim.resize(small) {
        Err(CostError::Infeasible { task, .. }) => assert_eq!(task, "w"),
        other => panic!("infeasible, not {other:?}"),
    }
    assert_eq!(sim.machine(), machine(2), "unchanged");
    assert!(matches!(
        sim.resize(Machine {
            workers: 0,
            ..machine(2)
        }),
        Err(CostError::Simulation(_))
    ));
    assert!(close(sim.run(&mut Admission).expect("ran").finish, 4.0));
}
