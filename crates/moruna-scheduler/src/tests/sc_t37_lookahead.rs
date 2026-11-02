//! SC-T37 lookahead (f.24; adaptive-runtime 9.4 steps 3 and 4, G12; SDD 24 CM-I23, CM-I24).
//!
//! The snapshot of a run is the workload, start and machine its state says; the lookahead plays
//! the best units forward and runs the one whose predicted finish is earliest, where the greedy
//! order would run another; with no budget it simulates nothing and says it went shallower; a
//! drifting key makes it one step deep; a resize replays the kept simulator on the new machine; a
//! snapshot the simulator refuses is counted with its reason and the greedy order answers.

use moruna_kernel::{
    Action, BufferId, Consumer, Head, Link, Machine, PortView, PredictedBy, Prediction, Producer,
    Ready, SchedulerState, SchedulingPolicy, Spread, StageView, Topology,
};
use moruna_testkit::FakePlacement;

use super::common::decided;
use crate::policies::{LOOKAHEAD, Lookahead, ResidencyGreedy};

const MS: u64 = 1_000_000;

/// Two sources; stage 1 (1 ms) feeds stage 2 (10 ms) and a sink; stage 3 (1 ms) reads the second
/// source and feeds a sink.
fn two_lines() -> Topology {
    let link = |from, to| Link { from, to };
    match Topology::new(
        2,
        3,
        2,
        vec![
            link(Producer::Source(0), Consumer::Kernel { stage: 1, port: 0 }),
            link(Producer::Kernel(1), Consumer::Kernel { stage: 2, port: 0 }),
            link(Producer::Kernel(2), Consumer::Sink(0)),
            link(Producer::Source(1), Consumer::Kernel { stage: 3, port: 0 }),
            link(Producer::Kernel(3), Consumer::Sink(1)),
        ],
    ) {
        Ok(t) => t,
        Err(e) => panic!("{e}"),
    }
}

fn predicted(ns: u64, out: u64) -> Prediction {
    Prediction {
        out_bytes: Spread::exact(out),
        duration_ns: Spread::exact(ns),
        by: PredictedBy::Model,
        drifting: false,
        observations: 20,
    }
}

fn port(queue: u16, batches: u64) -> PortView {
    PortView {
        queue,
        batches,
        memory_bytes: batches * 100,
        disk_bytes: 0,
    }
}

fn head(stage: u16, queue: u16) -> Ready {
    Ready {
        stage,
        port: 0,
        queue,
        head: Head {
            id: BufferId(u64::from(queue)),
            seq: 0,
            bytes: 100,
            last: true,
            arrived: 0,
        },
        queue_bytes: 100,
        out_bytes: 0,
        predicted: predicted(MS, 100),
        critical_ns: 0,
        waited_ns: 0,
    }
}

/// Stage 1 has a batch waiting, stage 3 three and one unit under way: the free worker either
/// starts the line to stage 2 (and the run ends at 11 ms) or another unit of stage 3 (12 ms).
fn stages() -> Vec<StageView> {
    vec![
        StageView {
            stage: 1,
            ports: vec![port(0, 1)],
            running_ns: Vec::new(),
            cap: 2,
            unit: predicted(MS, 100),
        },
        StageView {
            stage: 2,
            ports: vec![port(1, 0)],
            running_ns: Vec::new(),
            cap: 2,
            unit: predicted(10 * MS, 100),
        },
        StageView {
            stage: 3,
            ports: vec![port(3, 3)],
            running_ns: vec![0],
            cap: 2,
            unit: predicted(MS, 100),
        },
    ]
}

fn machine(workers: u16) -> Machine {
    // Memory is full, so the greedy order runs the unit that frees most (stage 3, whose output a
    // sink takes) first.
    Machine {
        workers,
        budget_bytes: 1 << 20,
        held_bytes: 1 << 20,
        disk_bytes_per_second: 1 << 30,
        claim_cap_bytes: 1 << 20,
    }
}

fn state<'a>(
    topology: &'a Topology,
    ready: &'a [Ready],
    views: &'a [StageView],
    fake: &'a FakePlacement,
    budget_ns: u64,
    workers: u16,
) -> SchedulerState<'a> {
    SchedulerState {
        worker: 0,
        ready,
        topology,
        residency: fake,
        budget_ns,
        machine: machine(workers),
        stages: views,
    }
}

fn first(actions: &[Action]) -> u16 {
    match actions.first() {
        Some(Action::Run { stage, .. }) => *stage,
        other => panic!("a run first, not {other:?}"),
    }
}

#[test]
fn sc_t37_the_snapshot_is_what_the_state_says() {
    let topology = two_lines();
    let views = stages();
    let ready = [head(1, 0), head(3, 3)];
    let fake = FakePlacement::new();
    let s = state(&topology, &ready, &views, &fake, u64::MAX, 2);
    let snapshot = crate::policies::snapshot_of(&s).expect("a snapshot");
    // Sources first, then the stages: source 0 has made stage 1's one batch, source 1 stage 3's
    // running unit and its three; stage 1 has one unit, stage 2 the one it makes, stage 3 four.
    let units: Vec<u32> = snapshot.workload.tasks.iter().map(|t| t.units).collect();
    assert_eq!(units, vec![1, 4, 1, 1, 4]);
    let done: Vec<u32> = snapshot.start.tasks.iter().map(|t| t.done).collect();
    assert_eq!(done, vec![1, 4, 0, 0, 0]);
    assert_eq!(snapshot.start.tasks[4].running, vec![1e-3]);
    assert_eq!(snapshot.workload.edges, vec![(0, 2), (2, 3), (1, 4)]);
    assert_eq!(snapshot.stage_task, vec![Some(2), Some(3), Some(4)]);
    assert_eq!(snapshot.machine.workers, 2);

    // A backlog on disk puts its last batches on disk, never one a unit under way holds.
    let mut spilled = stages();
    spilled[2].ports[0].disk_bytes = 200;
    spilled[2].ports[0].memory_bytes = 100;
    let s = state(&topology, &ready, &spilled, &fake, u64::MAX, 2);
    let snapshot = crate::policies::snapshot_of(&s).expect("a snapshot");
    assert_eq!(snapshot.start.tasks[1].on_disk, vec![2, 3]);
    // A backlog past the horizon is cut to it.
    let mut long = stages();
    long[2].ports[0].batches = 1_000;
    let s = state(&topology, &ready, &long, &fake, u64::MAX, 2);
    let snapshot = crate::policies::snapshot_of(&s).expect("a snapshot");
    assert_eq!(
        u64::from(snapshot.workload.tasks[4].units),
        1 + crate::policies::HORIZON_BATCHES
    );
    // A state that does not describe the run's stages is no snapshot.
    let s = state(&topology, &ready, &views[..2], &fake, u64::MAX, 2);
    assert!(crate::policies::snapshot_of(&s).is_err());
}

#[test]
fn sc_t37_the_earliest_predicted_finish_runs() {
    let topology = two_lines();
    let views = stages();
    let ready = [head(1, 0), head(3, 3)];
    let fake = FakePlacement::new();
    let s = state(&topology, &ready, &views, &fake, 10_000_000_000, 2);
    let greedy = decided(&mut ResidencyGreedy::default(), &s);
    assert_eq!(first(&greedy), 3, "the greedy order frees memory first");
    let mut lookahead = Lookahead::default();
    assert_eq!(lookahead.id().name, LOOKAHEAD);
    let chosen = decided(&mut lookahead, &s);
    assert_eq!(
        first(&chosen),
        1,
        "the line to the 10 ms stage finishes first"
    );
    assert_eq!(chosen.len(), 2, "every ready unit stays in the order");
    let counters = lookahead.counters();
    // Two first steps, then the best two each with both second steps.
    assert_eq!(counters.simulations, 6, "{counters:?}");
    assert_eq!((counters.shallower, counters.simulation_errors), (0, 0));
    assert!(format!("{lookahead:?}").contains("simulations: 6"));

    // A ready unit whose key drifts: one step deep.
    let mut drifting = ready;
    drifting[1].predicted.drifting = true;
    let s = state(&topology, &drifting, &views, &fake, 10_000_000_000, 2);
    let mut shallow = Lookahead::default();
    assert_eq!(first(&decided(&mut shallow, &s)), 1);
    assert_eq!(shallow.counters().simulations, 2);

    // One ready unit: nothing to weigh.
    let s = state(&topology, &ready[..1], &views, &fake, 10_000_000_000, 2);
    let mut alone = Lookahead::default();
    assert_eq!(first(&decided(&mut alone, &s)), 1);
    assert_eq!(alone.counters().simulations, 0);
}

#[test]
fn sc_t37_the_budget_bounds_it() {
    let topology = two_lines();
    let views = stages();
    let ready = [head(1, 0), head(3, 3)];
    let fake = FakePlacement::new();
    // No budget: no simulation, the greedy order, and it says so.
    let s = state(&topology, &ready, &views, &fake, 0, 2);
    let mut lookahead = Lookahead::default();
    assert_eq!(first(&decided(&mut lookahead, &s)), 3);
    let c = lookahead.counters();
    assert_eq!((c.simulations, c.shallower), (0, 1));
}

#[test]
fn sc_t37_a_resize_replays_the_kept_simulator() {
    let topology = two_lines();
    let views = stages();
    let ready = [head(1, 0), head(3, 3)];
    let fake = FakePlacement::new();
    let mut lookahead = Lookahead::default();
    let s = state(&topology, &ready, &views, &fake, 10_000_000_000, 2);
    assert_eq!(first(&decided(&mut lookahead, &s)), 1);
    assert_eq!(lookahead.counters().resizes, 0);
    // The same run, resized to four workers: the kept simulator plays the new machine, and with
    // a worker for every line the order no longer changes the finish, so the greedy order's first
    // unit, the earliest by rank among equals, runs.
    let s = state(&topology, &ready, &views, &fake, 10_000_000_000, 4);
    assert_eq!(first(&decided(&mut lookahead, &s)), 3);
    assert_eq!(lookahead.counters().resizes, 1);
    // And back.
    let s = state(&topology, &ready, &views, &fake, 10_000_000_000, 2);
    assert_eq!(first(&decided(&mut lookahead, &s)), 1);
    assert_eq!(lookahead.counters().resizes, 2);
}

#[test]
fn sc_t37_a_refused_snapshot_is_counted_and_greedy_answers() {
    let topology = two_lines();
    let views = stages();
    let ready = [head(1, 0), head(3, 3)];
    let fake = FakePlacement::new();
    // A budget no unit's reservation fits: the simulator refuses it by name.
    let mut s = state(&topology, &ready, &views, &fake, 10_000_000_000, 2);
    s.machine.budget_bytes = 10;
    let mut lookahead = Lookahead::default();
    assert_eq!(first(&decided(&mut lookahead, &s)), 3, "the greedy order");
    let c = lookahead.counters();
    assert_eq!(c.simulation_errors, 1);
    assert!(
        c.simulation_error
            .as_deref()
            .is_some_and(|why| why.contains("infeasible")),
        "{c:?}"
    );
    // A state that does not describe the run is counted too.
    let s = state(&topology, &ready, &views[..1], &fake, 10_000_000_000, 2);
    assert_eq!(first(&decided(&mut lookahead, &s)), 3);
    assert_eq!(lookahead.counters().simulation_errors, 2);
}
