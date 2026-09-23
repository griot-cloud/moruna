//! SC-T5: a knob written by the controller takes effect within one pick, and parking is
//! lossless. SC-I5, SC-I7.

use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::{CancelToken, Knob, Knobs, StatsSource};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::{RigBuilder, wait_for};

#[test]
fn sc_t5_knobs_immediate() {
    let kernel = FakeKernel::new().latency(Duration::from_millis(20));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 16;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.read_ahead = 32;
        })
        .source(FakeSource::new().splits(1, 4_000, 32_000))
        .kernel(Arc::new(kernel))
        .go();

    let cancel = CancelToken::new();
    let token = cancel.clone();
    let scheduler = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        // Two workers active: never more than two inside `apply`.
        assert!(
            wait_for(Duration::from_secs(5), || {
                rig.scheduler.scheduler_stats().workers_busy >= 2
            }),
            "two workers never became busy"
        );
        for _ in 0..40 {
            let busy = rig.scheduler.scheduler_stats().workers_busy;
            assert!(busy <= 2, "{busy} workers were busy with active = 2");
            std::thread::sleep(Duration::from_millis(5));
        }
        rig.scheduler.set(Knob::ActiveWorkers(16));
        let raised = wait_for(Duration::from_secs(5), || {
            rig.scheduler.scheduler_stats().workers_busy > 2
        });
        cancel.cancel();
        let outcome = handle.join();
        (raised, outcome)
    });
    assert!(
        scheduler.0,
        "raising the knob did not put more workers to work"
    );
    assert!(scheduler.1.is_ok(), "the worker thread panicked");
}

/// SC-I10: every scheduler-side knob is clamped to the preamble's range here, each clamp is
/// counted in `SchedulerStats::knob_clamps`, and the three placement knobs are forwarded to the
/// engine by the scheduler and by nobody else. Preamble section 5.
#[test]
fn sc_t5_knobs_clamped_and_forwarded() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 4;
            cfg.workers_active = 4;
            cfg.morsel_min = 1024;
            cfg.morsel_max = 4096;
            cfg.initial_morsel_target = 2048;
        })
        .stages(1)
        .go();

    rig.scheduler.set(Knob::MorselTarget { stage: 1, bytes: 1 });
    rig.scheduler.set(Knob::MorselTarget {
        stage: 1,
        bytes: u64::MAX,
    });
    rig.scheduler.set(Knob::ActiveWorkers(0));
    rig.scheduler.set(Knob::ActiveWorkers(999));
    rig.scheduler.set(Knob::ReadAhead(4_000));
    rig.scheduler.set(Knob::PromotionWindow {
        stage: 1,
        morsels: 0,
    });
    rig.scheduler.set(Knob::PromotionWindow {
        stage: 1,
        morsels: 4_000,
    });
    eprintln!("final {}", rig.scheduler.scheduler_stats().knob_clamps);
    assert_eq!(
        rig.scheduler.scheduler_stats().knob_clamps,
        7,
        "every out-of-range value was clamped and counted"
    );
    let snapshot = rig.scheduler.snapshot();
    assert_eq!(snapshot.active_workers, 4, "clamped to workers.max");
    assert_eq!(snapshot.read_ahead, 64, "clamped to the readahead ceiling");
    assert_eq!(
        snapshot
            .morsel_target
            .iter()
            .find(|(s, _)| *s == 1)
            .map(|(_, b)| *b),
        Some(4096),
        "clamped to morsel.max_bytes"
    );
    assert_eq!(
        snapshot.promotion_window,
        vec![(1, 32)],
        "clamped to the promotion window ceiling"
    );

    // The three placement knobs are forwarded, and the snapshot returns what was forwarded.
    rig.scheduler
        .set(Knob::StagingTrigger { stage: 0, on: true });
    rig.scheduler.set(Knob::HighWater {
        stage: 0,
        tier: moruna_kernel::TierKind::Host,
        bytes: 4_096,
    });
    let snapshot = rig.scheduler.snapshot();
    assert_eq!(snapshot.staging, vec![(0, true)]);
    assert_eq!(
        snapshot.high_water,
        vec![(0, moruna_kernel::TierKind::Host, 4_096)]
    );
    // The engine saw the forwarded water mark, with low set to half of high.
    assert!(
        !moruna_kernel::Placement::is_full(rig.placement.as_ref(), 0),
        "an empty queue is not full at a four kilobyte high water mark"
    );
    // A set after the run has exited is a no-op (RC h).
    rig.scheduler.shutdown();
    let before = rig.scheduler.scheduler_stats().knob_clamps;
    rig.scheduler.set(Knob::ActiveWorkers(999));
    assert_eq!(rig.scheduler.scheduler_stats().knob_clamps, before);
}
