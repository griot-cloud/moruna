//! The controller under a budget that follows the machine (MH 4.4):
//! ceiling_from_the_sample, cpu_limit_bounds_workers, host_budget_follows_arena.

mod common;

use common::{GIB, MIB, active_workers, config, kernel, morsel_targets, probe, staging_triggers};
use moruna_kernel::{KernelHints, Sample};
use moruna_testkit::{FakeKnobs, FakeSampler};

/// A sample carrying the limits in force, as discovery's sampler produces it once a watcher
/// publishes into its cell.
fn with_limits(anon: u64, at: u64, ceiling: u64, cpu: f64) -> Sample {
    Sample {
        ceiling_bytes: ceiling,
        cpu_limit: cpu,
        ..common::sample(anon, at)
    }
}

fn run(anon: u64, ceiling: u64, cpu: f64, from: u64, count: u64) -> Vec<Sample> {
    (0..count)
        .map(|i| with_limits(anon, from + i * 1_000_000, ceiling, cpu))
        .collect()
}

fn rig(ceiling: u64, workers: u16, samples: Vec<Sample>) -> common::Rig {
    let cfg = config(ceiling, workers);
    let knobs = FakeKnobs::new().probe_result(1, probe(cfg.probe_bytes, 1.0));
    common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        knobs,
        FakeSampler::new().scripted(samples),
    )
}

/// ceiling_from_the_sample. The ceiling the classifier works to is the one the sample
/// carries, read every tick. Lowered, it takes effect within the tick that saw it: the
/// plan is re-solved inside it, the largest target comes down, the last queue stages, and a note
/// says so. Raised, the plan grows back. A sample that carries no ceiling changes nothing.
#[test]
fn ceiling_from_the_sample() {
    let rig = rig(8 * GIB, 4, run(200 * MIB, 8 * GIB, 4.0, 1_000_000, 64));
    rig.run_up();
    assert_eq!(rig.controller.limits_in_force().0, 8 * GIB);
    let before = *morsel_targets(&rig.writes()).last().expect("a target");
    let writes_before = rig.writes().len();

    // The machine shrinks to a quarter: one tick.
    rig.sampler
        .clone()
        .scripted(run(200 * MIB, 2 * GIB, 4.0, 100_000_000, 64));
    rig.controller.tick_once();
    assert_eq!(
        rig.controller.limits_in_force().0,
        2 * GIB,
        "the ceiling followed within one tick"
    );
    let writes = rig.writes()[writes_before..].to_vec();
    let after = *morsel_targets(&writes)
        .last()
        .expect("the target was rewritten");
    assert!(
        after.1 < before.1,
        "the shrink target is taken at once: {} then {}",
        before.1,
        after.1
    );
    assert!(
        staging_triggers(&writes).iter().any(|(_, on)| *on),
        "spill engages under the lowered ceiling"
    );
    assert!(
        rig.controller
            .summary()
            .notes
            .iter()
            .any(|note| note.contains("memory ceiling moved")),
        "the change is noted for the report"
    );

    // A second tick at the same ceiling is no change at all.
    let settled = rig.writes().len();
    rig.controller.tick_once();
    assert!(
        morsel_targets(&rig.writes()[settled..]).is_empty(),
        "nothing moved, so nothing is re-planned"
    );

    // The machine grows back: the plan follows it up.
    rig.sampler
        .clone()
        .scripted(run(200 * MIB, 8 * GIB, 4.0, 200_000_000, 64));
    let grown_from = rig.writes().len();
    rig.controller.tick_once();
    assert_eq!(rig.controller.limits_in_force().0, 8 * GIB);
    let regrown = morsel_targets(&rig.writes()[grown_from..]);
    assert!(
        regrown.last().is_some_and(|(_, bytes)| *bytes > after.1),
        "the plan grew back with the ceiling: {regrown:?}"
    );

    // A sampler that knows no limits (a fake) leaves them alone.
    rig.sampler
        .clone()
        .scripted(run(200 * MIB, 0, 0.0, 300_000_000, 8));
    rig.controller.tick_once();
    assert_eq!(rig.controller.limits_in_force().0, 8 * GIB);
    rig.controller.stop();
}

/// cpu_limit_bounds_workers. The pool is the thread count the scheduler created (`workers_max`,
/// the elastic `cpu_max`), and the worker count any rule may choose is bounded by the CPU limit
/// the sample carries: at start, after a lowering (parked down within the tick that saw
/// it) and after a raise (the solve uses every worker the new limit allows that memory can feed).
#[test]
fn cpu_limit_bounds_workers() {
    let rig = rig(16 * GIB, 16, run(200 * MIB, 16 * GIB, 4.0, 1_000_000, 64));
    rig.run_up();
    let started = *active_workers(&rig.writes())
        .last()
        .expect("a worker count");
    assert!(
        started <= 4,
        "sixteen threads exist but the limit is four: {started} active"
    );
    assert_eq!(rig.controller.limits_in_force().1, 4);

    rig.sampler
        .clone()
        .scripted(run(200 * MIB, 16 * GIB, 2.0, 100_000_000, 8));
    let from = rig.writes().len();
    rig.controller.tick_once();
    let lowered = active_workers(&rig.writes()[from..]);
    assert_eq!(
        lowered.last().copied(),
        Some(2),
        "parked down to the limit within one tick"
    );

    rig.sampler
        .clone()
        .scripted(run(200 * MIB, 16 * GIB, 12.0, 200_000_000, 8));
    let from = rig.writes().len();
    rig.controller.tick_once();
    let raised = *active_workers(&rig.writes()[from..])
        .last()
        .expect("the raise wrote a worker count");
    assert!(
        raised > 4 && raised <= 12,
        "more workers than the starting four and no more than the limit: {raised}"
    );
    assert_eq!(rig.controller.limits_in_force().1, 12);
    assert!(
        rig.controller
            .summary()
            .notes
            .iter()
            .any(|note| note.contains("CPU limit moved"))
    );

    // The facade's direct path does the same, and a limit above the pool is the pool.
    rig.controller.follow_limits(0, 64.0);
    assert_eq!(rig.controller.limits_in_force().1, 16);
    rig.controller.follow_limits(0, 1.0);
    assert_eq!(rig.controller.limits_in_force().1, 1);
    assert_eq!(active_workers(&rig.writes()).last().copied(), Some(1));
    rig.controller.stop();
}

/// host_budget_follows_arena. When the facade resizes the arena it hands the controller the
/// new capacity, which is the whole host allowance of f.1: the plan is re-solved inside it and the
/// placement engine is given its new tier budgets. The same figure again is no change.
#[test]
fn host_budget_follows_arena() {
    let rig = rig(8 * GIB, 4, run(200 * MIB, 8 * GIB, 4.0, 1_000_000, 64));
    rig.run_up();
    let host = rig.controller.limits_in_force().2;
    let budgets = rig.placement.budgets_set().len();
    let before = *morsel_targets(&rig.writes()).last().expect("a target");

    rig.controller.set_arena(host / 8, 0);
    assert_eq!(rig.controller.limits_in_force().2, host / 8);
    let set = rig.placement.budgets_set();
    assert_eq!(
        set.len(),
        budgets + 1,
        "the placement engine got its new budgets"
    );
    let placed = set.last().expect("budgets");
    assert!(placed.host + placed.pinned_host <= host / 8);
    let after = *morsel_targets(&rig.writes()).last().expect("a target");
    assert!(after.1 <= before.1);

    let count = rig.placement.budgets_set().len();
    rig.controller.set_arena(host / 8, 0);
    assert_eq!(
        rig.placement.budgets_set().len(),
        count,
        "no change, no call"
    );

    rig.controller.set_arena(host, 0);
    assert_eq!(rig.controller.limits_in_force().2, host);
    rig.controller.stop();

    // Before the run is going, the budget is recorded and nothing is re-planned.
    let idle = self::rig(8 * GIB, 4, run(200 * MIB, 8 * GIB, 4.0, 1_000_000, 8));
    idle.controller.set_arena(GIB, 0);
    idle.controller.follow_limits(4 * GIB, 2.0);
    assert!(idle.writes().is_empty());
    assert_eq!(idle.controller.limits_in_force(), (4 * GIB, 2, GIB));
}

/// Nanoseconds since the epoch, the clock trace records carry.
fn epoch_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// A record far over any line, whose `apply` started at `t_start_ns`.
fn over(seq: u64, t_start_ns: u64) -> moruna_kernel::TraceRecord {
    let mut r = common::record(seq, 1, 16 * MIB, 64 * MIB);
    r.t_start_ns = t_start_ns;
    r.t_end_ns = t_start_ns + 1_000;
    r.mem_anon_before = 3 * GIB;
    r.mem_anon_peak = 3 * GIB + 64 * MIB;
    r
}

/// A record measured before the limits or the arena last moved says nothing about the run as it
/// is now: it is neither fitted nor a breach. While the arena drains after the machine shrank, a
/// breach sheds but does not terminate the run; once the drain is complete the ordinary rule
/// applies again and a run that cannot fit is diagnosed.
#[test]
fn stale_records_and_drains_are_not_breaches() {
    let rig = rig(GIB, 1, run(200 * MIB, GIB, 1.0, 1_000_000, 64));
    rig.run_up();
    let host = rig.controller.limits_in_force().2;
    rig.controller.follow_limits(GIB - 64 * MIB, 0.0);

    // Stale: `apply` started before the ceiling moved.
    let from = rig.writes().len();
    rig.feed(&over(1, 1_000));
    assert!(
        morsel_targets(&rig.writes()[from..]).is_empty(),
        "a record from before the change is not a breach"
    );
    assert!(rig.knobs.terminated().is_none());

    // Current, while the arena drains: a breach, which sheds.
    rig.controller.set_arena(host, 64 * MIB);
    let now = epoch_ns() + 1_000_000_000;
    rig.feed(&over(2, now));
    assert!(
        !morsel_targets(&rig.writes()[from..]).is_empty(),
        "a record from after the change is a breach"
    );
    assert!(rig.knobs.terminated().is_none(), "{:?}", rig.knobs.terminated());

    // Draining: breaches at the floor on one worker shed and do not terminate.
    let now = epoch_ns() + 2_000_000_000;
    for seq in 3..40 {
        rig.feed(&over(seq, now + seq));
    }
    assert!(
        rig.knobs.terminated().is_none(),
        "a drain in progress is not a kernel that does not fit: {:?}",
        rig.knobs.terminated()
    );

    // The drain completes: the same evidence now ends the run with a diagnostic (G-I8).
    rig.controller.set_arena(host, 0);
    let now = epoch_ns() + 3_000_000_000;
    for seq in 40..80 {
        rig.feed(&over(seq, now + seq));
    }
    assert!(
        rig.knobs.terminated().is_some(),
        "after the drain a run that cannot fit is diagnosed"
    );
    rig.controller.stop();
}
