//! PL-T20 completions_through_then (PL-I14, g): submission returns at once, the state change
//! happens on the thread that resolved the completion, the engine has no thread of its own,
//! and `Completion::wait` is never called.

mod common;

use moruna_kernel::Reactor;
use moruna_kernel::{Locality, Placement, TierKind};
use moruna_placement::state::State;
use moruna_testkit::{FakeAllocator, FakeReactor};
use std::time::{Duration, Instant};

#[test]
fn pl_t20_push_does_not_wait_for_the_move() {
    let scratch = common::Scratch::new("t20");
    let alloc = FakeAllocator::new().pinned(true);
    let latency = Duration::from_millis(100);
    let reactor = FakeReactor::new().with_latency(latency);
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 1 << 30, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_consumer(0, common::want_device());

    let started = Instant::now();
    engine
        .push(0, common::tensor_morsel(&alloc, 0, 0, 1024))
        .expect("push");
    let inside = started.elapsed();
    // The SDD's figure is a millisecond. Under coverage instrumentation the same work takes
    // a few, so what is asserted is the claim itself: the call returned long before the move
    // it issued could have completed (PL-I14, RE-I6).
    assert!(
        inside < latency / 5,
        "push took {inside:?} of the move's {latency:?}; it must not wait on it (PL-I14)"
    );
    assert_eq!(reactor.in_flight(), 1, "the move is in flight");
    assert!(
        matches!(engine.head_state(0), Some((0, State::Promoting(_, _)))),
        "and the entry says so"
    );

    common::settle(&reactor);
    assert!(
        matches!(engine.head_state(0), Some((0, State::Resident(_)))),
        "the state change happened in the completion's callback"
    );
    assert_eq!(reactor.in_flight(), 0);
}

#[test]
fn pl_t20_shutdown_mid_move_leaves_the_engine_responsive() {
    // A `FakeReactor` built with `cancel_on_shutdown(true)` and shut down mid-move leaves the
    // engine responsive: it never parked on a completion, so there is nothing to unwind.
    let scratch = common::Scratch::new("t20b");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new()
        .with_latency(Duration::from_millis(50))
        .cancel_on_shutdown(true);
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 128);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes, bytes);
    for seq in 0..8u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 128))
            .expect("push");
    }
    assert!(reactor.in_flight() > 0, "moves are in flight");

    let started = Instant::now();
    engine.shutdown();
    reactor.shutdown();
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "shutdown returned in {:?}: it must not wait on a completion",
        started.elapsed()
    );
    let started = Instant::now();
    assert!(matches!(
        engine.pop_blocking(0, common::want_host(), Locality::Any),
        Err(moruna_kernel::MorunaError::Cancelled)
    ));
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "pop after shutdown returned at once"
    );
}

#[test]
fn pl_t20_the_completion_runs_on_the_resolving_thread() {
    let scratch = common::Scratch::new("t20c");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(Duration::from_millis(10));
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 128);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes, bytes);
    for seq in 0..4u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 128))
            .expect("push");
    }
    // The pushing thread returned with the entries still in flight, which is only possible
    // if the state change is left to the resolving thread (PL-I14).
    let in_flight = engine
        .entry_states(0)
        .into_iter()
        .filter(|(_, state)| state.in_flight())
        .count();
    assert!(
        in_flight > 0,
        "the pushing thread did not wait for the moves"
    );
    common::settle(&reactor);
    assert_eq!(
        engine
            .entry_states(0)
            .into_iter()
            .filter(|(_, state)| state.in_flight())
            .count(),
        0,
        "and the resolving threads finished them"
    );
}
