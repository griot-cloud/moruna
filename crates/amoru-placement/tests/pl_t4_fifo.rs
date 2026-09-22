//! PL-T4 fifo (PL-I5): pops are in push order however much demotion pressure there is;
//! demotion changes where an entry's bytes are, never its position.

mod common;

use amoru_kernel::{Locality, Placement, TierKind};
use amoru_testkit::{FakeAllocator, FakeReactor};

#[test]
fn pl_t4_fifo() {
    let entries = 10_000usize;
    let scratch = common::Scratch::new("t4");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);

    let sample = common::table_morsel(&alloc, 0, 0, 16);
    let bytes = sample.bytes;
    drop(sample);
    // Pressure that lets a handful of entries stay resident and pushes the rest to disk.
    engine.set_water(0, TierKind::Host, bytes * 2, bytes * 4);

    let mut pushed = Vec::with_capacity(entries);
    for seq in 0..entries as u64 {
        let morsel = common::table_morsel(&alloc, seq, 0, 16);
        pushed.push(seq);
        engine.push(0, morsel).expect("push");
    }
    engine.close(0);

    let mut popped = Vec::with_capacity(entries);
    while popped.len() < entries {
        match engine
            .pop_blocking(0, common::want_host(), Locality::Any)
            .expect("pop_blocking")
        {
            Some((morsel, _)) => popped.push(morsel.seq),
            None => break,
        }
    }
    assert_eq!(popped, pushed, "pops must be in push order (PL-I5)");
    assert!(
        engine
            .pop(0, common::want_host(), Locality::Any)
            .expect("pop")
            .is_none(),
        "a drained, closed queue returns None"
    );
    assert!(
        engine.stats().queues[0].demotions > 0,
        "the test must actually have exercised demotion"
    );
}
