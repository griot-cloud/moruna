//! PL-T12 q0_staging_on (E8 precondition): with `set_staging(0, true)` Q0 behaves like any
//! other queue and holds a dataset far larger than its resident budget on disk, with a
//! resident window of `k`; pops proceed in order and nothing is evicted.

mod common;

use moruna_kernel::{Locality, Placement, TierKind};
use moruna_placement::state::State;
use moruna_testkit::{FakeAllocator, FakeReactor};

#[test]
fn pl_t12_q0_staging_on() {
    let scratch = common::Scratch::new("t12");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    let window = 3u16;
    engine.set_promotion_window(0, window);

    let sample = common::table_morsel(&alloc, 0, 0, 64);
    let bytes = sample.bytes;
    drop(sample);
    // The dataset is ten times what the queue may hold resident.
    let entries = 200u64;
    let resident = 20u64;
    engine.set_water(0, TierKind::Host, bytes * resident / 2, bytes * resident);

    for seq in 0..entries {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 64))
            .expect("push");
        common::settle(&reactor);
        let held: u64 = engine.stats().queues[0].bytes_by_tier.iter().sum();
        assert!(
            held <= bytes * (resident + u64::from(window)),
            "the resident set stayed near the window: {held} bytes"
        );
    }
    common::settle(&reactor);

    let detail = engine.detailed_stats();
    assert_eq!(detail.per_queue[0].evictions, 0, "nothing is evicted (E8)");
    assert!(
        detail.per_queue[0].entries_by_state[4] > entries / 2,
        "most of the dataset is on disk"
    );
    let promotable = engine
        .entry_states(0)
        .into_iter()
        .take(usize::from(window))
        .filter(|(_, state)| {
            matches!(
                state,
                State::Resident(_) | State::ResidentOnDisk(_) | State::Promoting(_, _)
            )
        })
        .count();
    assert_eq!(
        promotable,
        usize::from(window),
        "the window ahead of the head is resident or on its way (PL-I1)"
    );

    engine.close(0);
    let mut order = Vec::with_capacity(entries as usize);
    while let Some((morsel, _)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        order.push(morsel.seq);
    }
    assert_eq!(order, (0..entries).collect::<Vec<_>>(), "in order (PL-I5)");
    assert_eq!(engine.detailed_stats().per_queue[0].evictions, 0);
}
