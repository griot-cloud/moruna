//! RC-T9 classification_table. Every row of f.6 in turn: the state that row describes is
//! given to the fakes, the tick is run, and the class and the one action the row names are what
//! come out. Proves f.6 and, row by row, RC-I8: no tick moves both the read-ahead and the
//! worker count.

mod common;

use common::{
    GIB, MIB, active_workers, config, high_waters, kernel, morsel_targets, probe, read_aheads,
    record, sample, staging_triggers, stateful_kernel,
};
use moruna_controller::Bottleneck;
use moruna_kernel::{KernelHints, Sample, SchedulerStats, StageStats, TierKind, TraceRecord};
use moruna_testkit::{FakeKnobs, FakeSampler};

const CEILING: u64 = 8 * GIB;
const BASELINE: u64 = 400 * MIB;

/// The anonymous figure that trips the two memory rows: the ceiling less half the reserve.
fn over_the_line() -> u64 {
    let reserve = (CEILING as f64 * 0.10) as u64;
    CEILING - reserve / 2 + MIB
}

fn scheduler_stats(active: u16, busy: u16) -> SchedulerStats {
    SchedulerStats {
        per_stage: vec![StageStats {
            stage: 1,
            tasks: 10,
            busy_ns: 1_000,
            errors: 0,
            skipped: 0,
            instances_live: 1,
        }],
        workers_active: active,
        workers_busy: busy,
        sink_concurrency: 2,
        ..SchedulerStats::default()
    }
}

fn row_rig(stats: SchedulerStats, samples: Vec<Sample>) -> common::Rig {
    let cfg = config(CEILING, 8);
    let probe_bytes = cfg.probe_bytes;
    common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new()
            .probe_result(1, probe(probe_bytes, 2.0))
            .stats(stats),
        FakeSampler::new().scripted(samples),
    )
}

/// The record that tells the controller how full Q0 and Qn are (f.6 reads both off the trace).
fn queued(seq: u64, q0: u64, qn: u64) -> TraceRecord {
    TraceRecord {
        q_bytes_before: vec![0, 0, q0, 0, 0],
        q_bytes_after: vec![0, 0, qn, 0, 0],
        ..record(seq, 1, 4 * MIB, 4 * MIB)
    }
}

fn last_class(rig: &common::Rig) -> Bottleneck {
    rig.controller
        .summary()
        .timeline
        .last()
        .map(|(_, class)| *class)
        .expect("the tick appended a class to the timeline")
}

#[test]
fn rc_t9_row1_state_growth() {
    let cfg = config(CEILING, 8);
    let probe_bytes = cfg.probe_bytes;
    let mut stats = scheduler_stats(8, 8);
    stats.per_stage[0].instances_live = 2;
    let samples = vec![
        sample(BASELINE, 1_000),
        sample(over_the_line(), 2_000),
        sample(over_the_line(), 3_000),
    ];
    let rig = common::Rig::new(
        cfg,
        vec![stateful_kernel(1, 2, KernelHints::default())],
        FakeKnobs::new()
            .probe_result(1, probe(probe_bytes, 2.0))
            .stats(stats),
        FakeSampler::new().scripted(samples),
    );
    rig.run_up();
    let budgets_before = common::last_budgets(&rig.placement).expect("prepare set budgets");

    let mut growing = record(1, 1, 4 * MIB, 4 * MIB);
    growing.instance = 0;
    growing.state_bytes = 256 * MIB;
    rig.feed(&growing);
    rig.controller.tick_once();

    assert_eq!(last_class(&rig), Bottleneck::StateGrowth, "f.6 row 1");
    let budgets_after = common::last_budgets(&rig.placement).expect("the row set budgets");
    assert!(
        common::host_pool(&budgets_after) < common::host_pool(&budgets_before),
        "f.6 row 1: the placement half shrinks to fund the state"
    );
    rig.controller.stop();
}

#[test]
fn rc_t9_row2_memory() {
    let rig = row_rig(
        scheduler_stats(8, 8),
        vec![
            sample(BASELINE, 1_000),
            sample(over_the_line(), 2_000),
            sample(over_the_line(), 3_000),
        ],
    );
    rig.run_up();
    let before = morsel_targets(&rig.writes())
        .last()
        .map(|(_, bytes)| *bytes)
        .expect("a target");
    let mark = rig.writes().len();
    rig.controller.tick_once();

    assert_eq!(last_class(&rig), Bottleneck::Memory, "f.6 row 2");
    let fresh = &rig.writes()[mark..];
    assert_eq!(
        morsel_targets(fresh).last().map(|(_, bytes)| *bytes),
        Some(before / 2),
        "f.6 row 2: the largest stage is halved"
    );
    assert_eq!(
        staging_triggers(fresh),
        vec![(1, true)],
        "f.6 row 2: staging is turned on for the last queue"
    );
    rig.controller.stop();
}

#[test]
fn rc_t9_row3_cpu_quota() {
    let mut throttled = sample(BASELINE, 2_000);
    throttled.throttled_us = 10_000_000;
    let rig = row_rig(
        scheduler_stats(8, 8),
        vec![sample(BASELINE, 1_000), throttled, throttled],
    );
    rig.run_up();
    let before = *active_workers(&rig.writes())
        .last()
        .expect("a worker count");
    let mark = rig.writes().len();
    rig.controller.tick_once();

    assert_eq!(last_class(&rig), Bottleneck::CpuQuota, "f.6 row 3");
    let fresh = &rig.writes()[mark..];
    assert_eq!(
        active_workers(fresh),
        vec![before - 1],
        "f.6 row 3: one worker fewer, and nothing else"
    );
    assert!(
        read_aheads(fresh).is_empty(),
        "RC-I8: not the read-ahead as well"
    );
    rig.controller.stop();
}

#[test]
fn rc_t9_row4_io_read() {
    let mut stats = scheduler_stats(8, 1);
    stats.reads_in_flight = 2;
    let rig = row_rig(
        stats,
        vec![
            sample(BASELINE, 1_000),
            sample(BASELINE, 2_000),
            sample(BASELINE, 3_000),
        ],
    );
    rig.run_up();
    rig.feed(&queued(1, 0, 0));
    let mark = rig.writes().len();
    rig.controller.tick_once();

    assert_eq!(last_class(&rig), Bottleneck::IoRead, "f.6 row 4");
    let fresh = &rig.writes()[mark..];
    // The initial solution spends the whole queue half, so there is nothing spare to fund
    // another split in flight with: the row's second branch shrinks the high waters to pay for
    // it, which is the write f.6 names for rows 4 and 5.
    assert!(
        !high_waters(fresh).is_empty() || !read_aheads(fresh).is_empty(),
        "f.6 row 4: either a deeper read-ahead or the bytes to fund one"
    );
    assert!(
        active_workers(fresh).is_empty(),
        "RC-I8: the worker count does not move in the same tick as the read-ahead"
    );
    rig.controller.stop();
}

#[test]
fn rc_t9_row5_memory_parked() {
    let rig = row_rig(
        scheduler_stats(8, 1),
        vec![
            sample(BASELINE, 1_000),
            sample(BASELINE, 2_000),
            sample(BASELINE, 3_000),
        ],
    );
    rig.run_up();
    let high_water = high_waters(&rig.writes())
        .into_iter()
        .filter(|(_, tier, _)| *tier == TierKind::Host)
        .map(|(_, _, bytes)| bytes)
        .max()
        .expect("a high water");
    // Q0 above its low water with idle workers: what parks them is the budget, not the source.
    rig.feed(&queued(1, high_water, 0));
    let mark = rig.writes().len();
    rig.controller.tick_once();

    assert_eq!(last_class(&rig), Bottleneck::Memory, "f.6 row 5");
    let fresh = &rig.writes()[mark..];
    let shrunk = high_waters(fresh);
    assert!(!shrunk.is_empty(), "f.6 row 5: the high waters shrink");
    assert!(
        shrunk.iter().all(|(_, _, bytes)| *bytes < high_water),
        "f.6 row 5: by a tenth: {shrunk:?}"
    );
    assert!(
        read_aheads(fresh).is_empty(),
        "RC-I8: not the read-ahead as well"
    );
    rig.controller.stop();
}

#[test]
fn rc_t9_row6_sink() {
    let mut stats = scheduler_stats(8, 8);
    stats.writes_in_flight = 2;
    stats.sink_concurrency = 2;
    let rig = row_rig(
        stats,
        (0..8)
            .map(|at| sample(BASELINE, 1_000 + at * 1_000))
            .collect(),
    );
    rig.run_up();
    let read_ahead = *read_aheads(&rig.writes()).last().expect("a read-ahead");

    // The last queue rising over four consecutive ticks is what tells the controller the sink
    // is the one that cannot keep up.
    let mut mark = rig.writes().len();
    for tick in 1..=6u64 {
        rig.feed(&queued(tick, 0, tick * 64 * MIB));
        mark = rig.writes().len();
        rig.controller.tick_once();
        if last_class(&rig) == Bottleneck::Sink {
            break;
        }
    }

    assert_eq!(last_class(&rig), Bottleneck::Sink, "f.6 row 6");
    let fresh = &rig.writes()[mark..];
    assert_eq!(
        staging_triggers(fresh),
        vec![(1, true)],
        "f.6 row 6: staging on for the last queue"
    );
    assert_eq!(
        read_aheads(fresh),
        vec![read_ahead - 1],
        "f.6 row 6: one split fewer in flight"
    );
    assert!(
        active_workers(fresh).is_empty(),
        "RC-I8: not the worker count as well"
    );
    rig.controller.stop();
}

#[test]
fn rc_t9_row7_compute_and_row8_idle() {
    let rig = row_rig(
        scheduler_stats(8, 8),
        vec![sample(BASELINE, 1_000), sample(BASELINE, 2_000)],
    );
    rig.run_up();
    let mark = rig.writes().len();
    rig.controller.tick_once();
    assert_eq!(last_class(&rig), Bottleneck::Compute, "f.6 row 7");
    assert_eq!(
        rig.writes().len(),
        mark,
        "f.6 row 7: compute bound is where a tuned run wants to be; nothing moves"
    );
    rig.controller.stop();

    // Busy between the two thresholds is neither idle workers nor a full pipeline.
    let idle = row_rig(
        scheduler_stats(10, 8),
        vec![sample(BASELINE, 1_000), sample(BASELINE, 2_000)],
    );
    idle.run_up();
    let mark = idle.writes().len();
    idle.controller.tick_once();
    assert_eq!(last_class(&idle), Bottleneck::Idle, "f.6 row 8");
    assert_eq!(idle.writes().len(), mark, "f.6 row 8: nothing moves");
    idle.controller.stop();
}

/// The bound at the head of 11 f: `TraceTail::tail` answers out of the trace writer's
/// in-memory chunks only (04 f.3), so a window can come back empty. An empty window is an
/// absence of evidence about Q0, and the two rows that read Q0 must not fire on it. Without
/// this, every roll-over of the trace window would look like an idle source and raise the
/// read-ahead against a queue that was in fact full.
#[test]
fn rc_t9_row4_short_tail_is_not_evidence() {
    let mut stats = scheduler_stats(8, 1);
    stats.reads_in_flight = 2;
    let rig = row_rig(
        stats,
        vec![
            sample(BASELINE, 1_000),
            sample(BASELINE, 2_000),
            sample(BASELINE, 3_000),
        ],
    );
    rig.run_up();

    // No record has reached the trace, so the tail has nothing to say about Q0.
    let mark = rig.writes().len();
    rig.controller.tick_once();
    assert_ne!(
        last_class(&rig),
        Bottleneck::IoRead,
        "11 f: an empty trace window is not an empty queue"
    );
    assert_eq!(
        rig.writes().len(),
        mark,
        "11 f: and nothing is moved on the strength of it"
    );

    // The same tick, once the window can answer, is the read-ahead row.
    rig.feed(&queued(1, 0, 0));
    rig.controller.tick_once();
    assert_eq!(
        last_class(&rig),
        Bottleneck::IoRead,
        "f.6 row 4, with evidence"
    );
    rig.controller.stop();
}
