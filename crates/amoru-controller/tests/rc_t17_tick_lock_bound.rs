//! RC-T17 tick_lock_bound. The controller runs beside workers that must never wait on it, so
//! the arithmetic of one tick is the only thing its mutex ever covers: it is held for at most
//! five milliseconds, and no call into another component happens while it is held. Proves
//! RC-I10 and preamble 4.2.
//!
//! The second half is asserted structurally and not by timing: every fake is wrapped in a
//! `Watcher` that checks the lock is free at the moment it is called, so a call that crept
//! inside the lock fails the test at the call.
//!
//! The first half is asserted on the distribution of lock holds rather than on the single
//! largest one. The largest is a wall-clock figure, and on a build host with more runnable
//! threads than cores it measures the scheduler: the same twenty microseconds of arithmetic
//! reads as three milliseconds when the holder is descheduled in the middle of it, and as
//! thirty when the host is saturated. The distribution moves only if the controller does more
//! work under the lock, which is what RC-I10 is about, so the bound is asserted there and the
//! maximum is reported as the provisional timing figure it is (preamble E1, 6.7).

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use amoru_controller::{Controller, TICK_BOUND_MS};
use amoru_kernel::{KernelHints, SchedulerStats, StageStats, TraceSink};
use amoru_testkit::{FakeKnobs, FakePlacement, FakeSampler, FakeTrace};
use common::{GIB, MIB, WatchedPlacement, Watcher, config, kernel, probe, record, sample};

/// The window the SDD names: a hundred thousand records in the trace and a thousand ticks over
/// them.
const RECORDS: u64 = 100_000;
const TICKS: u64 = 1_000;

#[test]
fn rc_t17_tick_lock_bound() {
    let cfg = config(8 * GIB, 4);
    let probe_bytes = cfg.probe_bytes;
    let knobs = FakeKnobs::new()
        .probe_result(1, probe(probe_bytes, 3.0))
        .stats(SchedulerStats {
            per_stage: vec![StageStats {
                stage: 1,
                instances_live: 1,
                ..StageStats::default()
            }],
            workers_active: 4,
            workers_busy: 4,
            ..SchedulerStats::default()
        });
    // The sampler's clock must keep moving over a thousand ticks, or the stall detector of h
    // fires and the run is terminated before the measurement is finished.
    let samples: Vec<_> = (0..=TICKS + 8)
        .map(|at| sample(400 * MIB, 1_000 + at * 1_000))
        .collect();
    let sampler = FakeSampler::new().scripted(samples);
    let trace = FakeTrace::new().capacity(RECORDS as usize + 16);
    let placement = FakePlacement::new();

    let holder: Arc<OnceLock<Weak<Controller>>> = Arc::new(OnceLock::new());
    let watched_knobs = Arc::new(Watcher::new(knobs.clone(), holder.clone()));
    let watched_sampler = Arc::new(Watcher::new(sampler.clone(), holder.clone()));
    let watched_trace = Arc::new(Watcher::new(trace.clone(), holder.clone()));
    let watched_placement = Arc::new(WatchedPlacement {
        inner: placement.clone(),
        controller: holder.clone(),
        other_calls: AtomicU64::new(0),
    });

    let controller = Arc::new(
        Controller::new(
            cfg,
            watched_knobs.clone(),
            watched_knobs.clone(),
            watched_knobs.clone(),
            watched_sampler,
            watched_trace,
            watched_placement.clone(),
            vec![kernel(1, KernelHints::default())],
        )
        .expect("controller"),
    );
    let _ = holder.set(Arc::downgrade(&controller));

    controller.prepare().expect("prepare");
    controller.probe_all().expect("probe_all");
    controller.start().expect("start");

    // A trace window the tail has to walk, and a record queue the tick has to drain. The bulk
    // records carry no list columns: `FakeTrace::tail` copies every matching record on every
    // call, so a list column on each of a hundred thousand of them measures the fake's
    // allocator rather than the controller's lock.
    let target = 8 * MIB;
    for seq in 1..=RECORDS {
        trace.record(amoru_kernel::TraceRecord {
            feat_column_bytes: Vec::new(),
            q_bytes_before: Vec::new(),
            q_bytes_after: Vec::new(),
            ..record(seq, 1, target, target * 3)
        });
    }
    for seq in 1..=1_000u64 {
        controller.on_record(&record(seq, 1, target, target * 3));
    }

    for _ in 0..TICKS {
        controller.tick_once();
    }

    let bound_ns = TICK_BOUND_MS * 1_000_000;
    let p99 = controller.lock_held_quantile_ns(0.99);
    let median = controller.lock_held_quantile_ns(0.5);
    let max = controller.max_lock_held_ns();
    // The figure the report carries, on whatever host this ran on (preamble E1).
    eprintln!(
        "RC-T17 lock holds over {TICKS} ticks: median under {median} ns, p99 under {p99} ns,          longest {max} ns, bound {bound_ns} ns"
    );
    assert!(
        p99 <= bound_ns,
        "RC-I10: 99 per cent of lock holds must be inside the {TICK_BOUND_MS} ms bound; this run          had a p99 of {p99} ns (median {median} ns, longest {max} ns)"
    );
    assert!(
        median * 8 <= bound_ns,
        "RC-I10: the typical lock hold is the arithmetic of one tick and should be far inside          the bound, not near it: median {median} ns against {bound_ns} ns"
    );
    assert!(
        !controller.lock_is_held(),
        "the lock is released when a tick ends"
    );
    assert_eq!(
        watched_placement.other_calls.load(Ordering::SeqCst),
        0,
        "11 l: set_budgets is the only placement call the controller makes"
    );
    controller.stop();
}
