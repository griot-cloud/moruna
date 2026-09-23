//! RC-T8 one_action_per_tick. Whatever the tick decides, it never moves both the read-ahead and
//! the worker count: two knobs chasing the same symptom is how a controller oscillates, and
//! RC-I8 is the rule that stops it. Every tick of every scenario here is checked.

mod common;

use moruna_kernel::{KernelHints, Sample, SchedulerStats, StageStats, TraceRecord};
use moruna_testkit::{FakeKnobs, FakeSampler};
use common::{GIB, MIB, active_workers, config, kernel, probe, read_aheads, record, sample};

const CEILING: u64 = 8 * GIB;
const BASELINE: u64 = 400 * MIB;

fn stats(active: u16, busy: u16, reads: u16, writes: u16) -> SchedulerStats {
    SchedulerStats {
        per_stage: vec![StageStats {
            stage: 1,
            instances_live: 1,
            ..StageStats::default()
        }],
        workers_active: active,
        workers_busy: busy,
        reads_in_flight: reads,
        writes_in_flight: writes,
        sink_concurrency: 2,
        ..SchedulerStats::default()
    }
}

fn queued(seq: u64, q0: u64, qn: u64) -> TraceRecord {
    TraceRecord {
        q_bytes_before: vec![0, 0, q0, 0, 0],
        q_bytes_after: vec![0, 0, qn, 0, 0],
        ..record(seq, 1, 4 * MIB, 8 * MIB)
    }
}

fn drive(name: &str, stats: SchedulerStats, samples: Vec<Sample>, q0: u64, qn_step: u64) {
    let cfg = config(CEILING, 8);
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new()
            .probe_result(1, probe(probe_bytes, 3.0))
            .stats(stats),
        FakeSampler::new().scripted(samples),
    );
    rig.run_up();

    for tick in 1..=12u64 {
        rig.feed(&queued(tick, q0, tick * qn_step));
        let mark = rig.writes().len();
        rig.controller.tick_once();
        let fresh = &rig.writes()[mark..];
        let moved_read_ahead = !read_aheads(fresh).is_empty();
        let moved_workers = !active_workers(fresh).is_empty();
        assert!(
            !(moved_read_ahead && moved_workers),
            "RC-I8: {name} tick {tick} moved both the read-ahead and the worker count: {fresh:?}"
        );
    }
    rig.controller.stop();
}

#[test]
fn rc_t8_one_action_per_tick() {
    let steady: Vec<Sample> = (0..64)
        .map(|at| sample(BASELINE, 1_000 + at * 1_000))
        .collect();
    let mut throttled: Vec<Sample> = steady.clone();
    for slot in throttled.iter_mut().skip(1) {
        slot.throttled_us = 10_000_000;
    }
    let mut tight: Vec<Sample> = steady.clone();
    let reserve = (CEILING as f64 * 0.10) as u64;
    for slot in tight.iter_mut().skip(1) {
        slot.anon_bytes = CEILING - reserve / 2 + MIB;
    }

    // Idle workers with an empty Q0: the read-ahead row.
    drive("io read", stats(8, 1, 2, 0), steady.clone(), 0, 0);
    // Idle workers with a full Q0: the parked-by-budget row, which may add a worker.
    drive(
        "memory parked",
        stats(8, 1, 0, 0),
        steady.clone(),
        64 * GIB,
        0,
    );
    // A busy pipeline with a rising last queue: the sink row, which lowers the read-ahead.
    drive("sink", stats(8, 8, 0, 2), steady.clone(), 0, 64 * MIB);
    // A throttled process: the CPU quota row, which lowers the worker count.
    drive("cpu quota", stats(8, 8, 0, 0), throttled, 0, 0);
    // Memory pressure: the memory row, which moves neither.
    drive("memory", stats(8, 8, 0, 0), tight, 0, 0);
}
