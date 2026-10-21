//! RC-T21 a waiting stage (MH 4.10). A stage whose call waits on a port is neither idle nor
//! computing: with the rest of the chain unsaturated the tick says `Waiting` and moves no knob;
//! under memory pressure the stage halved is another one; and a waiting stage's state is not what
//! a memory row ends the run for.

mod common;

use common::{
    GIB, MIB, active_workers, config, kernel, morsel_targets, probe, read_aheads, sample,
    stateful_kernel,
};
use moruna_controller::Bottleneck;
use moruna_kernel::{KernelHints, Sample, SchedulerStats, StageStats};
use moruna_testkit::{FakeKnobs, FakeSampler};

const CEILING: u64 = 8 * GIB;
const BASELINE: u64 = 400 * MIB;

fn over_the_line() -> u64 {
    let reserve = (CEILING as f64 * 0.10) as u64;
    CEILING - reserve / 2 + MIB
}

fn stats(stages: &[u16], busy: u16, waiting: &[u16]) -> SchedulerStats {
    SchedulerStats {
        per_stage: stages
            .iter()
            .map(|stage| StageStats {
                stage: *stage,
                tasks: 10,
                busy_ns: 1_000,
                instances_live: 1,
                ..StageStats::default()
            })
            .collect(),
        workers_active: 8,
        workers_busy: busy,
        workers_waiting: waiting.len() as u16,
        stages_waiting: waiting.to_vec(),
        sink_concurrency: 2,
        ..SchedulerStats::default()
    }
}

fn rig(stages: &[u16], stats: SchedulerStats, samples: Vec<Sample>) -> common::Rig {
    let cfg = config(CEILING, 8);
    let probe_bytes = cfg.probe_bytes;
    let mut knobs = FakeKnobs::new().stats(stats);
    for stage in stages {
        knobs = knobs.probe_result(*stage, probe(probe_bytes, 2.0));
    }
    common::Rig::new(
        cfg,
        stages
            .iter()
            .map(|stage| kernel(*stage, KernelHints::default()))
            .collect(),
        knobs,
        FakeSampler::new().scripted(samples),
    )
}

fn last_class(rig: &common::Rig) -> Bottleneck {
    rig.controller
        .summary()
        .timeline
        .last()
        .map(|(_, class)| *class)
        .expect("a class")
}

/// The one worker in `apply` waits on a port, every other is free: the stage waits, and the
/// tick moves no knob for it.
#[test]
fn rc_t21_a_waiting_stage_is_waiting_and_moves_nothing() {
    let rig = rig(
        &[1],
        stats(&[1], 1, &[1]),
        vec![
            sample(BASELINE, 1_000),
            sample(BASELINE, 2_000),
            sample(BASELINE, 3_000),
        ],
    );
    rig.run_up();
    let mark = rig.writes().len();
    for _ in 0..5 {
        rig.controller.tick_once();
        assert_eq!(last_class(&rig), Bottleneck::Waiting);
    }
    let fresh = &rig.writes()[mark..];
    assert!(morsel_targets(fresh).is_empty(), "{fresh:?}");
    assert!(active_workers(fresh).is_empty(), "{fresh:?}");
    assert!(read_aheads(fresh).is_empty(), "{fresh:?}");
    rig.controller.stop();
}

/// Under memory pressure the Memory row halves the largest stage that is not waiting; the
/// waiting one keeps its target. The two start equal, and the row would take the later one.
#[test]
fn rc_t21_memory_pressure_halves_another_stage() {
    let rig = rig(
        &[1, 2],
        stats(&[1, 2], 2, &[2]),
        vec![
            sample(BASELINE, 1_000),
            sample(over_the_line(), 2_000),
            sample(over_the_line(), 3_000),
        ],
    );
    rig.run_up();
    let mark = rig.writes().len();
    rig.controller.tick_once();
    assert_eq!(last_class(&rig), Bottleneck::Memory);
    let fresh = &rig.writes()[mark..];
    let halved = morsel_targets(fresh);
    assert!(!halved.is_empty(), "the row halved a stage");
    assert!(
        halved.iter().all(|(stage, _)| *stage == 1),
        "the waiting stage 2 is not taken down: {halved:?}"
    );
    rig.controller.stop();
}

/// The only stage waits, under memory pressure: nothing of it is halved.
#[test]
fn rc_t21_the_only_stage_waiting_is_not_halved() {
    let rig = rig(
        &[1],
        stats(&[1], 1, &[1]),
        vec![
            sample(BASELINE, 1_000),
            sample(over_the_line(), 2_000),
            sample(over_the_line(), 3_000),
        ],
    );
    rig.run_up();
    let mark = rig.writes().len();
    rig.controller.tick_once();
    let fresh = &rig.writes()[mark..];
    assert!(morsel_targets(fresh).is_empty(), "{fresh:?}");
    rig.controller.stop();
}

/// A waiting stage's state past half the budget does not end the run while it waits.
#[test]
fn rc_t21_a_waiting_stages_state_does_not_end_the_run() {
    let cfg = config(CEILING, 8);
    let probe_bytes = cfg.probe_bytes;
    let mut stats = stats(&[1], 1, &[1]);
    stats.per_stage[0].instances_live = 1;
    let rig = common::Rig::new(
        cfg,
        vec![stateful_kernel(1, 1, KernelHints::default())],
        FakeKnobs::new()
            .probe_result(1, probe(probe_bytes, 2.0))
            .stats(stats),
        FakeSampler::new().scripted(vec![
            sample(BASELINE, 1_000),
            sample(over_the_line(), 2_000),
            sample(over_the_line(), 3_000),
        ]),
    );
    rig.run_up();
    let mut growing = common::record(1, 1, 4 * MIB, 4 * MIB);
    growing.instance = 0;
    growing.state_bytes = 6 * GIB;
    rig.feed(&growing);
    rig.controller.tick_once();
    assert!(
        rig.knobs.terminated().is_none(),
        "the waiting stage's state ended the run: {:?}",
        rig.knobs.terminated()
    );
    rig.controller.stop();
}
