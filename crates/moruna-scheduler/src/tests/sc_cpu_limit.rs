//! The CPU limit bounds the active workers (MH 4.4): threads are created up to `workers_max` and parked, and the
//! workers that take tasks are `min(workers.active, cpu_limit)`. A lowered limit parks workers at
//! their next pick without a knob write; a raised one lets the knob take effect up to it.
//! SC-I7, G-I5.

use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::{CancelToken, Knob, Knobs, StatsSource};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::{RigBuilder, wait_for};

#[test]
fn cpu_limit_bounds_active() {
    let kernel = FakeKernel::new().latency(Duration::from_millis(20));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            // The elastic pool: sixteen threads, four of them the starting N.
            cfg.workers_max = 16;
            cfg.workers_active = 16;
            cfg.initial_morsel_target = 8;
            cfg.read_ahead = 32;
        })
        .source(FakeSource::new().splits(1, 4_000, 32_000))
        .kernel(Arc::new(kernel))
        .go();

    // Before any limit is set the bound is the pool, so the knob alone decides (a run whose
    // limits never move behaves as it did).
    assert_eq!(rig.scheduler.cpu_limit(), (16, 16));
    assert_eq!(rig.scheduler.set_cpu_limit(4), 4);
    assert_eq!(
        rig.scheduler.set_cpu_limit(0),
        1,
        "a run always has one worker"
    );
    assert_eq!(
        rig.scheduler.set_cpu_limit(999),
        16,
        "no thread beyond the pool"
    );
    rig.scheduler.set_cpu_limit(4);
    assert_eq!(
        rig.scheduler.snapshot().active_workers,
        4,
        "the knob takes effect up to the limit"
    );

    let cancel = CancelToken::new();
    let token = cancel.clone();
    let result = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        assert!(
            wait_for(Duration::from_secs(5), || {
                rig.scheduler.scheduler_stats().workers_busy >= 4
            }),
            "four workers never became busy"
        );
        for _ in 0..30 {
            let stats = rig.scheduler.scheduler_stats();
            assert!(
                stats.workers_busy <= 4,
                "{} busy under a limit of 4",
                stats.workers_busy
            );
            assert_eq!(stats.workers_active, 4);
            std::thread::sleep(Duration::from_millis(5));
        }

        // Lowered: the workers above the new limit park at their next pick, with no knob write.
        rig.scheduler.set_cpu_limit(2);
        assert!(
            wait_for(Duration::from_secs(5), || {
                rig.scheduler.scheduler_stats().workers_busy <= 2
            }),
            "lowering the limit did not park workers"
        );
        for _ in 0..30 {
            let busy = rig.scheduler.scheduler_stats().workers_busy;
            assert!(busy <= 2, "{busy} busy under a limit of 2");
            std::thread::sleep(Duration::from_millis(5));
        }

        // Raised: the knob (still 16) takes effect up to the new limit.
        rig.scheduler.set_cpu_limit(12);
        let raised = wait_for(Duration::from_secs(5), || {
            rig.scheduler.scheduler_stats().workers_busy > 4
        });
        // And a knob write below the limit is still the knob's (G-I5).
        rig.scheduler.set(Knob::ActiveWorkers(3));
        let knob_rules = wait_for(Duration::from_secs(5), || {
            rig.scheduler.scheduler_stats().workers_active == 3
        });
        cancel.cancel();
        (raised, knob_rules, handle.join())
    });
    assert!(
        result.0,
        "raising the limit did not put more workers to work"
    );
    assert!(result.1, "a knob below the limit is the knob");
    assert!(result.2.is_ok(), "the run thread panicked");
}
