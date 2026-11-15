//! PL-T38 segment_outlives_its_writes (f.7, PL-I8, 2026-11-14): a segment is not unlinked while a
//! record it holds is still being written. A record is the segment's from the moment its bytes
//! are reserved; counted only when it landed, a segment that had rolled while a write was in
//! flight looked reclaimable as soon as its landed records were released, and was unlinked under
//! the write. On the benchmark VM the late write then found the file gone ("io open") or the other
//! records read back as zeros ("record magic is not MORUNSEG").

mod common;

use std::time::Duration;

use moruna_kernel::{Locality, Placement, Residency, TierKind};
use moruna_placement::state::State;
use moruna_testkit::{FakeAllocator, FakeReactor};

#[test]
fn pl_t38_a_segment_outlives_the_writes_into_it() {
    let scratch = common::Scratch::new("t38");
    let alloc = FakeAllocator::new();
    let page = 4096u64;
    // How many bytes one batch's record takes, measured on an engine of its own: the first
    // record starts at 0, its body one page in, and runs to the end of its last page.
    let record = {
        let probe_dir = common::Scratch::new("t38probe");
        let reactor = FakeReactor::new();
        let engine = common::engine(
            common::config(
                1,
                Some(probe_dir.path().to_path_buf()),
                common::budgets(1 << 30, 0, 1 << 30),
            ),
            &alloc,
            &reactor,
        );
        engine.set_staging(0, true);
        engine.set_promotion_window(0, 1);
        for seq in 0..2u64 {
            engine
                .push(0, common::table_morsel(&alloc, seq, 0, 256))
                .expect("push");
        }
        // Batch 1, behind the head, so its resident copy goes once the record lands.
        engine
            .spill(engine.buffer_of(0, 1).expect("a batch"))
            .expect("spill");
        common::settle(&reactor);
        match engine
            .entry_states(0)
            .into_iter()
            .find(|(seq, _)| *seq == 1)
        {
            Some((_, State::OnDisk(seg))) => seg.offset + seg.len.div_ceil(page) * page,
            other => panic!("the probe's batch is not on disk: {other:?}"),
        }
    };
    let reactor = FakeReactor::new().with_latency(Duration::from_millis(400));
    // Two records fit a segment, and a third does not.
    let mut cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    cfg.segment_bytes = record * 2 + record / 2;
    cfg.checkpoint_enabled = false;
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    engine.set_water(0, TierKind::Host, 1 << 30, 1 << 30);
    for seq in 0..5u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 256))
            .expect("push");
    }
    let id = |seq| engine.buffer_of(0, seq).expect("a batch");

    // Batch 1 lands in the first segment and is read back: resident, with its record kept.
    engine.spill(id(1)).expect("spill 1");
    common::settle(&reactor);
    engine.reload(id(1)).expect("reload 1");
    common::settle(&reactor);
    assert!(matches!(
        engine
            .entry_states(0)
            .into_iter()
            .find(|(seq, _)| *seq == 1)
            .map(|(_, state)| state),
        Some(State::ResidentOnDisk(_))
    ));

    // Batch 3 is being written into the same segment when batch 4 rolls to a second one.
    // Batch 2, never written, stays ahead of it, so batch 3 is not at the head when it lands.
    engine.spill(id(3)).expect("spill 3");
    engine.spill(id(4)).expect("spill 4");
    assert_eq!(
        engine.detailed_stats().segments_live,
        2,
        "setup: batch 4 rolled to a second segment"
    );
    assert!(
        reactor.in_flight() > 0,
        "setup: batch 3 is still being written"
    );

    // Batches 0 and 1 are taken; batch 1's record, the first segment's only landed one, is
    // released while batch 3's write is still in flight.
    for expected in [0u64, 1] {
        let (morsel, _) = engine
            .pop_blocking(0, common::want_host(), Locality::Any)
            .expect("pop")
            .expect("a morsel");
        assert_eq!(morsel.seq, expected);
        engine.release(0, morsel.seq).expect("release");
    }
    common::settle(&reactor);

    // Every record on disk names a file that is there (PL-I8).
    let run_dir = scratch.path().join(format!("moruna-{}", "07".repeat(16)));
    for (seq, state) in engine.entry_states(0) {
        if let State::OnDisk(seg) = state {
            let path = run_dir.join(format!("seg-{:06}.seg", seg.segment));
            assert!(
                path.is_file(),
                "batch {seq} is on disk in {}, which was unlinked under its write",
                path.display()
            );
        }
    }

    // And the rest comes back whole, after which every segment goes.
    engine.close(0);
    let mut rest = Vec::new();
    while let Some((morsel, _)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop")
    {
        rest.push(morsel.seq);
        engine.release(0, morsel.seq).expect("release");
    }
    assert_eq!(rest, vec![2, 3, 4]);
    common::settle(&reactor);
    assert_eq!(engine.detailed_stats().segments_live, 0);
}
