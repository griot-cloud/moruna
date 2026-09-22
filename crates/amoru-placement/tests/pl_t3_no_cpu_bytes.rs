//! PL-T3 no_cpu_bytes (PL-I4): the engine never copies payload bytes; every byte movement is
//! a reactor operation whose tiers match the move table of e.4, and the bytes that land on
//! disk are the payload's own.

mod common;

use amoru_kernel::{Allocator, Locality, Placement, Tier, TierKind};
use amoru_testkit::{FakeAllocator, FakeReactor, OpKind};

#[test]
fn pl_t3_no_cpu_bytes() {
    let scratch = common::Scratch::new("t3");
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

    // A tensor payload is one contiguous range with no framing at all, so a run over it is
    // the strict case: nothing is copied by the CPU anywhere in the engine.
    let elements = 1024usize;
    let sample = common::tensor_morsel(&alloc, 0, 0, elements);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes, bytes * 2);
    let before = alloc.stats().payload_copies_total;

    for seq in 0..8u64 {
        engine
            .push(0, common::tensor_morsel(&alloc, seq, 0, elements))
            .expect("push");
    }
    common::settle(&reactor);
    assert!(
        engine.stats().queues[0].demotions > 0,
        "the run must have demoted"
    );

    engine.close(0);
    let mut seen = 0u64;
    while let Some((morsel, _)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        assert_eq!(morsel.bytes, bytes, "the payload came back whole");
        seen += 1;
    }
    assert_eq!(seen, 8);
    assert!(
        engine.stats().queues[0].promotions > 0,
        "the run must have promoted from disk"
    );

    // `FakeAllocator::stats` reports `payload_copies_total` as a constant zero: the fake of
    // contracts d.15 has no counter behind `Allocator::note_payload_copy`, so this reads the
    // figure the SDD names and the structural checks below are what actually prove PL-I4.
    // Reported as a finding against d.15.
    assert_eq!(
        alloc.stats().payload_copies_total,
        before,
        "the engine copied payload bytes with the CPU (PL-I4)"
    );

    // Every byte movement is a reactor operation, and its tiers are a row of e.4.
    let ops = reactor.ops();
    assert!(!ops.is_empty(), "the moves went through the reactor");
    for op in &ops {
        match op.kind {
            OpKind::WriteFile => {
                assert_eq!(
                    op.src_tier,
                    Some(Tier::Host),
                    "a record is written from the run's host tier (e.4)"
                );
                assert_eq!(
                    op.offset % common::PAGE as u64,
                    0,
                    "pieces are page aligned"
                );
            }
            OpKind::ReadFile => {
                assert_eq!(
                    op.dst_tier,
                    Some(Tier::Host),
                    "a record is read into the run's host tier (e.4)"
                );
                assert_eq!(op.offset % common::PAGE as u64, 0);
            }
            OpKind::Copy
            | OpKind::ReadObject
            | OpKind::WriteObject
            | OpKind::HeadObject
            | OpKind::ListPrefix
            | OpKind::DeleteObject
            | OpKind::AbortMultipart => {
                panic!(
                    "a host-only run issued {:?}, which is not a row of e.4",
                    op.kind
                )
            }
        }
    }
    assert_eq!(amoru_placement::locks::violations(), 0, "lock order (4.2)");
}

#[test]
fn pl_t3_table_pieces_are_the_batch_buffers() {
    // For a table the record's pieces are two metadata pages and the batch's own buffers,
    // with one exception the engine declares: arrow's IPC encoder allocates its own validity
    // bitmap, which has no arena provenance and so cannot become a `BufferView` (contracts
    // d.3); those bytes are brought into the arena once per record and declared through
    // `Allocator::note_payload_copy`. Reported as a finding against 09 e.3.
    let scratch = common::Scratch::new("t3b");
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
    let rows = 1024usize;
    let sample = common::table_morsel(&alloc, 0, 0, rows);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes, bytes * 2);
    for seq in 0..6u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, rows))
            .expect("push");
    }
    common::settle(&reactor);
    let demotions = engine.stats().queues[0].demotions;
    assert!(demotions > 0, "the run must have demoted");
    let writes: Vec<_> = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::WriteFile)
        .collect();
    assert_eq!(
        writes.len() as u64,
        demotions * 4,
        "a record is the header page, the framing page, the encoder's bitmap and the data"
    );
    let page = common::PAGE as u64;
    let bitmap = rows.div_ceil(8) as u64;
    let data = rows as u64 * 4;
    for record in writes.chunks(4) {
        assert_eq!(record[0].len, page, "the header is one page");
        assert_eq!(record[1].len, page, "the framing is page rounded");
        assert_eq!(record[2].len, bitmap, "the encoder's validity bitmap");
        assert_eq!(record[3].len, data, "the batch's own data buffer, as is");
        for piece in record {
            assert_eq!(piece.offset % page, 0, "every piece is page aligned (e.3)");
        }
    }
}
