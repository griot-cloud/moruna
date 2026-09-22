//! PL-T8 misses_counted (PL-I9): every `pop_blocking` that waits on a move is counted, the
//! wait comes back in the return value, and the sum of the returned waits is `miss_wait_us`.

mod common;

use amoru_kernel::{Locality, Placement, TierKind};
use amoru_testkit::{FakeAllocator, FakeReactor};
use std::time::{Duration, Instant};

#[test]
fn pl_t8_misses_counted() {
    let scratch = common::Scratch::new("t8");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(Duration::from_millis(15));
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 32);
    let bytes = sample.bytes;
    drop(sample);
    // Room for one entry only, so every pop after the first waits on a read from disk.
    engine.set_water(0, TierKind::Host, bytes, bytes);

    for seq in 0..6u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 32))
            .expect("push");
    }
    engine.close(0);

    let started = Instant::now();
    let mut returned = 0u64;
    let mut waited = 0u64;
    while let Some((morsel, wait_us)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        returned += wait_us;
        if wait_us > 0 {
            waited += 1;
        }
        let _ = morsel;
    }
    let measured = started.elapsed().as_micros() as u64;

    let stats = engine.stats().queues[0].clone();
    assert!(waited > 0, "some pop must have waited on a move");
    assert_eq!(stats.misses, waited, "every waiting pop is counted (PL-I9)");
    assert_eq!(
        stats.miss_wait_us, returned,
        "the waits the calls returned sum to `miss_wait_us`"
    );
    assert!(
        returned <= measured,
        "the waits {returned} cannot exceed the {measured} the whole drain took"
    );
    assert!(
        measured - returned <= measured / 2 + 50_000,
        "the waits must be within reach of the measured time"
    );
}

#[test]
fn pl_t8_a_pop_that_did_not_wait_returns_zero() {
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(1, None, common::budgets(1 << 30, 0, 0));
    let engine = common::engine(cfg, &alloc, &reactor);
    engine
        .push(0, common::table_morsel(&alloc, 0, 0, 16))
        .expect("push");
    let (morsel, wait) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
        .expect("the head is resident");
    assert_eq!(morsel.seq, 0);
    assert_eq!(wait, 0, "a pop that did not wait returns 0 (PL-I9)");
    assert_eq!(engine.stats().queues[0].misses, 0);
}
