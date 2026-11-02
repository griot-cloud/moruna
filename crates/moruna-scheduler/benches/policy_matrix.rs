//! SC-B1 policy_matrix (adaptive-runtime 10, 12.4; RA 7, 9): every registered policy on RA 7's
//! workloads, run through the scheduler and the real placement engine, varying one factor at a
//! time around each workload's base: the memory-to-data ratio, batch size, branching, reuse,
//! the CPU-to-IO ratio, skew, selectivity and workers. Each run reports its finish time, the
//! worker time idle while a unit was ready, the memory unused while a unit was ready, the bytes
//! spilled, and whether it was correct (it completed, every sink got every batch once, and the
//! catalog never passed its budget). Every policy runs every cell twice, on the same workload and
//! seed: on the stages' measured ratios, and with `loglinear` learning from nothing during the
//! run (f.22).
//!
//! `MORUNA_SC_B1=smoke` (the default) runs each workload's base, small; `MORUNA_SC_B1=medium`
//! each base with units of milliseconds, where what adapting costs is seen at its true share;
//! `MORUNA_SC_B1=full` runs every cell at the proof's size. Each row reports the time to the
//! run returning, including its wait for the learning thread. Store IO and the final drain
//! wait are reported separately,
//! and the share of the units' time deciding and learning took (`adapt %`); a table at the end
//! puts each policy's ratio and model runs side by side. The disk is the fake reactor's: every IO resolves after the
//! cell's IO latency on a thread of its own, and a kernel spins for its CPU latency. Optional
//! `MORUNA_SC_B1_WORKLOAD` and `MORUNA_SC_B1_CELL` exact-name selectors partition the full
//! matrix into bounded runs; an unmatched selector fails. Each cell also requires identical
//! completed sink row counts across its policies/models. The exit
//! status is non-zero when any run was not correct.
//!
//! `cargo bench -p moruna-scheduler --bench policy_matrix`

#[path = "../tests/common/mod.rs"]
mod common;

use std::time::Duration;

use moruna_kernel::{Consumer, Link, Producer, Topology};

use common::{Case, Model, Outcome, floor_budget, run};

/// One of RA 7's workloads, with the knobs a cell may vary.
#[derive(Clone, Copy, Debug)]
struct Cell {
    workload: Workload,
    /// Data read over the memory budget.
    ratio: u64,
    /// Rows per source batch.
    rows: u64,
    /// Stages reading in parallel (shared-input branching).
    branching: u16,
    /// Of those, how many read the same source batch; the rest read a second source.
    reuse: u16,
    /// True for a kernel that costs more than its IO, false for the reverse.
    cpu_bound: bool,
    skewed: bool,
    /// Output rows per input row, in thousandths: the middle of the factor range.
    selectivity: u64,
    workers: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    /// Scan, filter, aggregate.
    Linear,
    /// One batch feeds several stages.
    Branching,
    /// A filter keeps a small part of what a sink also gets whole.
    Retention,
    /// Two sources meet in one stage.
    Converging,
    /// A stage makes far more than it takes, which the next one reduces back.
    SpillAggregate,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Workload::Linear => "linear",
            Workload::Branching => "branching",
            Workload::Retention => "retention",
            Workload::Converging => "converging",
            Workload::SpillAggregate => "spill_aggregate",
        }
    }
}

fn link(from: Producer, to: Consumer) -> Link {
    Link { from, to }
}

fn k(stage: u16, port: u16) -> Consumer {
    Consumer::Kernel { stage, port }
}

/// The DAG of a cell, and how many sources it has.
fn topology(cell: &Cell) -> Topology {
    let built = match cell.workload {
        Workload::Linear | Workload::SpillAggregate => Topology::new(
            1,
            2,
            1,
            vec![
                link(Producer::Source(0), k(1, 0)),
                link(Producer::Kernel(1), k(2, 0)),
                link(Producer::Kernel(2), Consumer::Sink(0)),
            ],
        ),
        Workload::Branching => {
            let stages = cell.branching.max(1);
            let reuse = cell.reuse.clamp(1, stages);
            let sources = if reuse < stages { 2 } else { 1 };
            let mut links = Vec::new();
            for stage in 1..=stages {
                let source = if stage <= reuse { 0 } else { 1 };
                links.push(link(Producer::Source(source), k(stage, 0)));
            }
            for stage in 1..=stages {
                links.push(link(Producer::Kernel(stage), Consumer::Sink(stage - 1)));
            }
            Topology::new(sources, stages, stages, links)
        }
        Workload::Retention => Topology::new(
            1,
            1,
            2,
            vec![
                link(Producer::Source(0), k(1, 0)),
                link(Producer::Source(0), Consumer::Sink(0)),
                link(Producer::Kernel(1), Consumer::Sink(1)),
            ],
        ),
        Workload::Converging => Topology::new(
            2,
            1,
            1,
            vec![
                link(Producer::Source(0), k(1, 0)),
                link(Producer::Source(1), k(1, 1)),
                link(Producer::Kernel(1), Consumer::Sink(0)),
            ],
        ),
    };
    match built {
        Ok(topology) => topology,
        Err(e) => panic!("{:?}: {e}", cell.workload),
    }
}

/// The base of each workload: the factors its own nature fixes.
fn base(workload: Workload) -> Cell {
    let mut cell = Cell {
        workload,
        ratio: 3,
        rows: 128,
        branching: 3,
        reuse: 3,
        cpu_bound: true,
        skewed: false,
        selectivity: 1000,
        workers: 2,
    };
    match workload {
        Workload::Linear => cell.selectivity = 400,
        Workload::Retention => cell.selectivity = 60,
        Workload::SpillAggregate => cell.selectivity = 3000,
        Workload::Branching | Workload::Converging => {}
    }
    cell
}

/// Each workload's base, then one factor at a time.
fn cells(full: bool) -> Vec<(String, Cell)> {
    let workloads = [
        Workload::Linear,
        Workload::Branching,
        Workload::Retention,
        Workload::Converging,
        Workload::SpillAggregate,
    ];
    let mut out = Vec::new();
    for workload in workloads {
        let b = base(workload);
        out.push(("base".to_string(), b));
        if !full {
            continue;
        }
        out.push(("ratio=1".into(), Cell { ratio: 1, ..b }));
        out.push(("ratio=6".into(), Cell { ratio: 6, ..b }));
        out.push(("rows=32".into(), Cell { rows: 32, ..b }));
        out.push(("rows=512".into(), Cell { rows: 512, ..b }));
        if workload == Workload::Branching {
            out.push((
                "branching=5".into(),
                Cell {
                    branching: 5,
                    reuse: 5,
                    ..b
                },
            ));
            out.push(("reuse=1".into(), Cell { reuse: 1, ..b }));
        }
        out.push((
            "io_bound".into(),
            Cell {
                cpu_bound: false,
                ..b
            },
        ));
        out.push(("skewed".into(), Cell { skewed: true, ..b }));
        out.push((
            "selectivity/4".into(),
            Cell {
                selectivity: (b.selectivity / 4).max(10),
                ..b
            },
        ));
        out.push(("workers=1".into(), Cell { workers: 1, ..b }));
        out.push(("workers=4".into(), Cell { workers: 4, ..b }));
    }
    out
}

/// How big a matrix run is: its batches, its kernel and IO latencies, and which cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Size {
    /// Each workload's base, 24 batches a source, units of a few hundred microseconds.
    Smoke,
    /// Each workload's base, 64 batches a source, units of milliseconds: what a real kernel's
    /// unit costs, so what deciding and learning cost is seen at its true share.
    Medium,
    /// Every cell, 400 batches a source, the smoke's latencies: the proof.
    Full,
}

impl Size {
    fn name(self) -> &'static str {
        match self {
            Size::Smoke => "smoke",
            Size::Medium => "medium",
            Size::Full => "full",
        }
    }

    fn batches(self) -> u32 {
        match self {
            Size::Smoke => 24,
            Size::Medium => 64,
            Size::Full => 400,
        }
    }

    /// Kernel and IO latency, for a kernel that costs more than its IO and for the reverse.
    fn latencies(self, cpu_bound: bool) -> (Duration, Duration) {
        let scale = if self == Size::Medium { 10 } else { 1 };
        if cpu_bound {
            (
                Duration::from_micros(200 * scale),
                Duration::from_micros(20 * scale),
            )
        } else {
            (
                Duration::from_micros(10 * scale),
                Duration::from_micros(200 * scale),
            )
        }
    }
}

fn case(cell: &Cell, size: Size, policy: &str, model: Model) -> Case {
    let topology = topology(cell);
    let sources = usize::from(topology.sources());
    let batches = size.batches();
    // The factor range around a selectivity: a third either side.
    let around = |mid: u64| (mid * 2 / 3, mid * 4 / 3);
    let factors = around(cell.selectivity);
    // A spill aggregate's first stage makes `selectivity` thousandths of its input, and the
    // second reduces it by as much: it makes `1,000,000 / selectivity` thousandths of what it
    // takes, so the data comes out the size it went in.
    let stage_factors = if cell.workload == Workload::SpillAggregate {
        vec![
            around(cell.selectivity),
            around((1_000_000 / cell.selectivity.max(1)).max(3)),
        ]
    } else {
        Vec::new()
    };
    let (kernel_latency, io_latency) = size.latencies(cell.cpu_bound);
    let mut case = Case {
        topology,
        splits: vec![(batches, cell.rows); sources],
        factors,
        stage_factors,
        skewed: cell.skewed,
        kernel_latency,
        io_latency,
        workers: cell.workers,
        budget: 0,
        window: 2,
        policy: policy.to_string(),
        model,
    };
    let data = u64::from(batches) * cell.rows * 8 * sources as u64;
    case.budget = (data / cell.ratio.max(1)).max(floor_budget(&case));
    case
}

/// The measured full run duration, including its observation-store drain.
fn finish_ms(o: &Outcome) -> f64 {
    o.finished.as_secs_f64() * 1e3
}

fn selected_cells(full: bool) -> Vec<(String, Cell)> {
    let selector = |name| match std::env::var(name) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => panic!("{name}: {error}"),
    };
    let workload = selector("MORUNA_SC_B1_WORKLOAD");
    let label = selector("MORUNA_SC_B1_CELL");
    let cells: Vec<_> = cells(full)
        .into_iter()
        .filter(|(name, cell)| {
            workload
                .as_deref()
                .is_none_or(|wanted| wanted == cell.workload.name())
                && label
                    .as_deref()
                    .is_none_or(|wanted| wanted == name.as_str())
        })
        .collect();
    assert!(
        !cells.is_empty(),
        "no matrix cell matches workload={workload:?}, cell={label:?}"
    );
    cells
}

fn main() {
    let size = match std::env::var("MORUNA_SC_B1").as_deref() {
        Ok("full") => Size::Full,
        Ok("medium") => Size::Medium,
        Ok("smoke") | Err(std::env::VarError::NotPresent) => Size::Smoke,
        Ok(other) => panic!("MORUNA_SC_B1={other:?}: smoke, medium or full"),
        Err(e) => panic!("MORUNA_SC_B1: {e}"),
    };
    let policies = moruna_scheduler::policies::standard();
    let names: Vec<String> = policies.names().iter().map(|n| n.to_string()).collect();
    let (k, io) = size.latencies(true);
    println!(
        "SC-B1 policy_matrix ({}): {} source batches per source, a unit {} us of kernel and {} us \
         of IO (the IO-bound cell the reverse), policies {}",
        size.name(),
        size.batches(),
        k.as_micros(),
        io.as_micros(),
        names.join(", ")
    );
    println!(
        "{:<16} {:<14} {:<17} {:<10} {:>10} {:>9} {:>11} {:>12} {:>9} {:>9} {:>9} {:>9} {:>11} {:>13} {:>16}  correct",
        "workload",
        "cell",
        "policy",
        "model",
        "finish ms",
        "idle ms",
        "unused MB*s",
        "spilled B",
        "budget",
        "adapt %",
        "store ms",
        "drain ms",
        "asked/hit",
        "over/fallback",
        "sims/shallow/err"
    );
    let mut wrong = 0u64;
    let mut summary: Vec<String> = Vec::new();
    for (label, cell) in selected_cells(size == Size::Full) {
        let mut expected_rows: Option<Vec<u64>> = None;
        let mut results: Vec<(String, Model, Outcome)> = Vec::new();
        for (name, model) in names
            .iter()
            .flat_map(|n| [(n, Model::None), (n, Model::LogLinear)])
        {
            let case = case(&cell, size, name, model);
            let mut outcome = run(
                &case,
                &format!(
                    "b1-{}-{label}-{name}-{}",
                    cell.workload.name(),
                    model.name()
                ),
            );
            if outcome.correct.is_ok() {
                match &expected_rows {
                    Some(rows) if *rows != outcome.output_rows => {
                        outcome.correct = Err(format!(
                            "sink rows {:?} differ from this cell's reference {rows:?}",
                            outcome.output_rows
                        ));
                    }
                    None => expected_rows = Some(outcome.output_rows.clone()),
                    _ => {}
                }
            }
            let correct = match &outcome.correct {
                Ok(()) => "yes".to_string(),
                Err(e) => {
                    wrong += 1;
                    format!("NO: {e}")
                }
            };
            let o = &outcome.policy.overhead;
            println!(
                "{:<16} {:<14} {:<17} {:<10} {:>10.1} {:>9.1} {:>11.3} {:>12} {:>9} {:>9.3} {:>9.1} {:>9.1} {:>11} {:>13} {:>16}  {}",
                cell.workload.name(),
                label,
                name,
                model.name(),
                finish_ms(&outcome),
                outcome.policy.idle_ready_ns as f64 / 1e6,
                outcome.policy.unused_ready_byte_ns as f64 / 1e9 / 1e6,
                outcome.catalog.spilled_bytes,
                case.budget,
                o.share() * 100.0,
                (o.store_ns + o.save_ns) as f64 / 1e6,
                o.drain_ns as f64 / 1e6,
                format!("{}/{}", o.predictions, o.cached),
                format!(
                    "{}/{}",
                    outcome.policy.over_budget, outcome.policy.fallbacks
                ),
                format!(
                    "{}/{}/{}",
                    outcome.policy.counters.simulations,
                    outcome.policy.counters.shallower,
                    outcome.policy.counters.simulation_errors
                ),
                correct
            );
            if let Some(why) = &outcome.policy.counters.simulation_error {
                println!("{:<16} {:<14} first simulation error: {why}", "", label);
            }
            results.push((name.clone(), model, outcome));
        }
        println!(
            "{:<16} {:<14} reference sink rows: {expected_rows:?}",
            "", label
        );
        if let Some((best, model, _)) = results
            .iter()
            .filter(|(_, _, o)| o.correct.is_ok())
            .min_by(|a, b| finish_ms(&a.2).total_cmp(&finish_ms(&b.2)))
        {
            println!(
                "{:<16} {:<14} earliest finish: {best} on {}",
                "",
                label,
                model.name()
            );
        }
        for name in &names {
            let of = |m: Model| {
                results
                    .iter()
                    .find(|(n, model, _)| n == name && *model == m)
                    .map(|(_, _, o)| o)
            };
            if let (Some(ratio), Some(model)) = (of(Model::None), of(Model::LogLinear)) {
                let (r, m) = (finish_ms(ratio), finish_ms(model));
                summary.push(format!(
                    "{:<16} {:<14} {:<17} {:>10.1} {:>10.1} {:>+8.1}% {:>9.3} {:>9.3} {:>11} {:>11}",
                    cell.workload.name(),
                    label,
                    name,
                    r,
                    m,
                    (m / r - 1.0) * 100.0,
                    ratio.policy.overhead.share() * 100.0,
                    model.policy.overhead.share() * 100.0,
                    ratio.catalog.spilled_bytes,
                    model.catalog.spilled_bytes
                ));
            }
        }
    }
    println!();
    println!(
        "each policy on measured ratios and with loglinear, side by side (full run finish, including store drain):"
    );
    println!(
        "{:<16} {:<14} {:<17} {:>10} {:>10} {:>9} {:>9} {:>9} {:>11} {:>11}",
        "workload",
        "cell",
        "policy",
        "ratio ms",
        "model ms",
        "model",
        "adapt% r",
        "adapt% m",
        "spilled r",
        "spilled m"
    );
    for line in summary {
        println!("{line}");
    }
    if wrong > 0 {
        eprintln!("SC-B1: {wrong} runs were not correct");
        std::process::exit(1);
    }
}
