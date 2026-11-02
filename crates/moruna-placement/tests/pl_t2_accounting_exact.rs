//! PL-T2 accounting_exact (PL-I3, PL-I21): resident plus reserved per tier equals the model
//! after every event and never exceeds the budget; the catalog's totals and each queue's figure
//! follow the batches exactly, the `Disk` slot counting what is only on disk.

mod common;

use moruna_kernel::{Locality, Placement, TIER_COUNT, TierKind};
use moruna_testkit::{FakeAllocator, FakeReactor};
use std::time::Duration;

#[test]
fn pl_t2_accounting_exact() {
    let scratch = common::Scratch::new("t2");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(Duration::from_millis(1));
    let host_budget = 1u64 << 20;
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(host_budget, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 32);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes * 4, bytes * 8);

    let budgets = [0u64, host_budget, host_budget, 1 << 30, 0];
    let check = |label: &str| {
        let stats = engine.stats();
        for (slot, budget) in budgets.iter().enumerate() {
            // One snapshot, taken under the catalog lock: the two figures are read together.
            let resident = stats.catalog.resident_by_tier[slot];
            let reserved = stats.catalog.reserved_by_tier[slot];
            assert!(
                resident + reserved <= *budget,
                "{label}: tier slot {slot} holds {resident} + {reserved} reserved, budget {budget}"
            );
        }
    };
    // The model: with nothing in flight, the tier counters hold exactly the bytes of the
    // entries that have resident bytes, in the tiers they are resident in (PL-I3). It is
    // checked at quiescent points, because a counter and a queue cannot be read in one
    // atomic step and a completion may land between the two reads.
    let model = |label: &str| {
        let mut expected = [0u64; TIER_COUNT];
        for (_, state) in engine.entry_states(0) {
            if let Some(tier) = state.resident_tier() {
                expected[tier.index()] += bytes;
            } else if matches!(state, moruna_placement::state::State::OnDisk(_)) {
                expected[TierKind::Disk.index()] += bytes;
            }
        }
        assert_eq!(
            engine.detailed_stats().per_queue[0].bytes_by_tier,
            expected,
            "{label}: the queue's figure does not match its entries"
        );
        // One queue, every consumer released: the catalog holds exactly the queue's batches.
        assert_eq!(
            engine.stats().catalog.resident_by_tier,
            expected,
            "{label}: the catalog's totals do not match the batches"
        );
    };

    let mut rng = common::Rng::new(0xACC0);
    let mut next = 0u64;
    let mut out = 0u64;
    for _ in 0..300 {
        if rng.below(3) < 2 {
            engine
                .push(0, common::table_morsel(&alloc, next, 0, 32))
                .expect("push");
            next += 1;
            check("after push");
        } else if let Some(morsel) = engine
            .pop(0, common::want_host(), Locality::Any)
            .expect("pop")
        {
            check("after pop, held");
            engine.release(0, morsel.seq).expect("release");
            out += 1;
            check("after release");
        }
    }
    common::settle(&reactor);
    check("after the moves settled");
    model("after the moves settled");

    engine.close(0);
    while let Some((morsel, _)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        engine.release(0, morsel.seq).expect("release");
        out += 1;
        check("while draining");
    }
    assert_eq!(out, next, "every morsel came back");
    model("after the queue drained");
    let detail = engine.detailed_stats();
    assert_eq!(
        detail.reservations, [0; TIER_COUNT],
        "every reservation is released once its move settles (f.9)"
    );
    assert_eq!(moruna_placement::locks::violations(), 0, "lock order (4.2)");
}
