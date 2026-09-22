//! PL-T13 slow_sink_throughput (S10, S15). Tagged "(reference host, E1)": the figure is only
//! comparable on the reference hardware with a real NVMe staging path and the real reactor,
//! so it is ignored here. Run it with `--ignored` to record a provisional figure, which the
//! report labels with this host's name.

mod common;

use amoru_kernel::{Locality, Placement, TierKind};
use amoru_testkit::{FakeAllocator, FakeReactor};
use std::time::Instant;

#[test]
#[ignore = "reference host, E1: the throughput ratio is only comparable on the reference hardware with the real reactor and NVMe"]
fn pl_t13_slow_sink_throughput() {
    let scratch = common::Scratch::new("t13");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 2);
    let rows = 4096usize;
    let sample = common::table_morsel(&alloc, 0, 0, rows);
    let bytes = sample.bytes;
    drop(sample);

    // Before staging engages: the queue is under its high water throughout.
    let count = 400u64;
    engine.set_water(0, TierKind::Host, bytes * count, bytes * count);
    let started = Instant::now();
    for seq in 0..count {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, rows))
            .expect("push");
        engine
            .pop_blocking(0, common::want_host(), Locality::Any)
            .expect("pop_blocking")
            .expect("the head is resident");
    }
    let before = started.elapsed();

    // After staging engages: the consumer drains at a tenth of the producer's rate, so the
    // queue sits above its high water and records are written and read back.
    engine.set_water(0, TierKind::Host, bytes * 4, bytes * 8);
    let started = Instant::now();
    let mut popped = 0u64;
    for seq in 0..count {
        engine
            .push(0, common::table_morsel(&alloc, count + seq, 0, rows))
            .expect("push");
        if seq % 10 == 9 {
            // One pop per ten pushes, and never a blocking one: a drained queue that is not
            // closed would park for ever.
            if engine
                .pop(0, common::want_host(), Locality::Any)
                .expect("pop")
                .is_some()
            {
                popped += 1;
            }
        }
    }
    engine.close(0);
    while engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
        .is_some()
    {
        popped += 1;
    }
    let after = started.elapsed();

    let host = hostname::get()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ratio = before.as_secs_f64() / after.as_secs_f64();
    println!(
        "PL-T13 provisional on {host}: {count} morsels of {bytes} bytes; \
         before staging {:?}, after staging {:?}, ratio {ratio:.3}",
        before, after
    );
    assert!(popped > 0);
    // S15's 70% is a claim about NVMe sequential bandwidth against the sink's rate, measured
    // on the reference hardware with the real reactor (E1). On any other host the figure
    // above is provisional and is reported, not asserted: the fake moves bytes with memcpy
    // on the calling thread, so the ratio says nothing about the criterion.
    match std::env::var("AMORU_REFERENCE_HOST") {
        Ok(reference) if reference == host => assert!(
            ratio >= 0.70,
            "throughput after staging engaged must be at least 70% of before (S15); got {ratio:.3}"
        ),
        Ok(_) | Err(_) => {
            println!("PL-T13 not asserted on {host}: it is not the reference host named by E1")
        }
    }
}
