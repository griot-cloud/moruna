//! PL-T7 segment_deleted_when_empty (PL-I8, PL-I12, PL-I15): a segment is unlinked only when
//! every record in it is released and no manifest references it, `unregister_segment` comes
//! first, and no live entry ever names a file that is gone.

mod common;

use amoru_kernel::{CheckpointExtras, Locality, Placement, TierKind};
use amoru_placement::state::State;
use amoru_testkit::{FakeAllocator, FakeReactor};

fn drive(
    engine: &amoru_placement::PlacementEngine,
    alloc: &FakeAllocator,
    count: u64,
    rows: usize,
) {
    for seq in 0..count {
        engine
            .push(0, common::table_morsel(alloc, seq, 0, rows))
            .expect("push");
    }
}

#[test]
fn pl_t7_unlinked_at_once_without_checkpoints() {
    let segment = 64 * 1024u64;
    let scratch = common::Scratch::new("t7a");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    cfg.segment_bytes = segment;
    cfg.checkpoint_enabled = false;
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 256);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes, bytes);

    drive(&engine, &alloc, 40, 256);
    common::settle(&reactor);
    let opened = engine.detailed_stats().segments_live;
    assert!(opened > 1, "the run must have rolled at least once");
    let charge = engine.detailed_stats().disk_bytes;
    assert_eq!(charge, opened * segment, "a segment is charged in full");

    engine.close(0);
    let mut before_each_pop = Vec::new();
    while let Some((morsel, _)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        before_each_pop.push(morsel.seq);
        // No live entry ever references a file that is gone (PL-I8).
        for (seq, state) in engine.entry_states(0) {
            if let State::OnDisk(seg) = &state {
                let path = scratch
                    .path()
                    .join(format!("amoru-{}", "07".repeat(16)))
                    .join(format!("seg-{:06}.seg", seg.segment));
                assert!(
                    path.is_file(),
                    "entry {seq} names {} which is gone",
                    path.display()
                );
            }
        }
    }
    assert_eq!(before_each_pop.len(), 40);
    let detail = engine.detailed_stats();
    assert_eq!(detail.segments_live, 0, "every segment was unlinked (f.7)");
    assert_eq!(detail.disk_bytes, 0, "and the charge came back (PL-I7)");
    assert!(
        reactor.segments().is_empty(),
        "`unregister_segment` precedes the unlink (PL-I15)"
    );
    let run_dir = scratch.path().join(format!("amoru-{}", "07".repeat(16)));
    let left: Vec<_> = std::fs::read_dir(&run_dir)
        .expect("run directory")
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(left.is_empty(), "the preallocated files are gone: {left:?}");
}

#[test]
fn pl_t7_a_referenced_segment_survives_until_the_next_manifest() {
    let segment = 64 * 1024u64;
    let scratch = common::Scratch::new("t7b");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let mut cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    cfg.segment_bytes = segment;
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 256);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes, bytes);
    drive(&engine, &alloc, 30, 256);
    common::settle(&reactor);

    let path = engine
        .checkpoint(&CheckpointExtras::default())
        .expect("checkpoint");
    let referenced = amoru_placement::manifest::read_manifest(&path).expect("manifest");
    assert!(
        !referenced.segments.is_empty(),
        "the manifest must reference segments for this test"
    );
    for row in &referenced.segments {
        let file = path
            .parent()
            .expect("run directory")
            .join(format!("seg-{:06}.seg", row.segment));
        assert!(file.is_file(), "a referenced segment is on disk (PL-I12)");
    }

    // Consume everything: the records are released, but the manifest still names them.
    engine.close(0);
    while engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
        .is_some()
    {}
    for row in &referenced.segments {
        let file = path
            .parent()
            .expect("run directory")
            .join(format!("seg-{:06}.seg", row.segment));
        assert!(
            file.is_file(),
            "a segment the current manifest references is not unlinked (PL-I8, PL-I12)"
        );
    }
    assert!(engine.detailed_stats().segments_reclaimable > 0);

    // The next manifest no longer references them, and they go.
    engine
        .checkpoint(&CheckpointExtras::default())
        .expect("checkpoint");
    let detail = engine.detailed_stats();
    assert_eq!(detail.segments_live, 0, "reclaimed at the next write (f.7)");
    assert_eq!(detail.disk_bytes, 0);
    assert!(reactor.segments().is_empty(), "unregistered first (PL-I15)");
}
