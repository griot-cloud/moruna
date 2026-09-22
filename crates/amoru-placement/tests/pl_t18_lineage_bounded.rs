//! PL-T18 lineage_bounded (PL-I11): the lineage index holds exactly the morsels above the
//! committed watermark, grows with admission when the watermark is held, and empties at the
//! end of a run.

mod common;

use amoru_kernel::{Locality, Placement, TierKind};
use amoru_testkit::{FakeAllocator, FakeReactor};

#[test]
fn pl_t18_lineage_bounded() {
    let scratch = common::Scratch::new("t18");
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
    let sample = common::table_morsel(&alloc, 0, 0, 64);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes * 2, bytes * 4);

    // With the watermark held, the lineage grows exactly as fast as admission.
    for seq in 0..50u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 64))
            .expect("push");
        assert_eq!(
            engine.detailed_stats().lineage_len,
            seq + 1,
            "one lineage record per admitted morsel (PL-I11)"
        );
    }
    common::settle(&reactor);

    // A morsel that has been popped keeps its lineage: it is inside a kernel or the sink.
    for _ in 0..10 {
        engine
            .pop_blocking(0, common::want_host(), Locality::Any)
            .expect("pop_blocking")
            .expect("a morsel");
    }
    assert_eq!(
        engine.detailed_stats().lineage_len,
        50,
        "a popped morsel keeps its lineage until it is committed (PL-I11)"
    );

    // The watermark advancing at the sink's rate bounds it.
    for committed in 0..10u64 {
        engine.set_committed(committed);
        assert_eq!(
            engine.detailed_stats().lineage_len,
            50 - (committed + 1),
            "the lineage never exceeds what is above the watermark"
        );
    }

    // A watermark below the current one is a `debug_assert` in f.11, so a test build cannot
    // exercise the ignoring branch without aborting; the watermark's monotonicity is what
    // the loop above asserts.
    assert_eq!(engine.detailed_stats().committed_seq, Some(9));

    engine.close(0);
    while engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
        .is_some()
    {}
    engine.set_committed(49);
    assert_eq!(
        engine.detailed_stats().lineage_len,
        0,
        "the lineage is empty at the end of a run (PL-I11)"
    );
    assert_eq!(engine.detailed_stats().committed_seq, Some(49));
}
