//! PL-T5 q0_evicts (PL-I6, f.16): with staging off, Q0 pressure drops recomputable entries
//! rather than writing them; `evicted` lists them oldest first, `replace` restores them in
//! their original position, and turning staging on makes the same pressure write.

mod common;

use moruna_kernel::{Locality, Placement, TierKind};
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};

#[test]
fn pl_t5_q0_evicts() {
    let scratch = common::Scratch::new("t5");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 32);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes * 2, bytes * 3);

    for seq in 0..8u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 32))
            .expect("push");
    }
    common::settle(&reactor);

    // Staging is off for stage 0 by default (b), so pressure evicted instead of writing.
    assert!(
        !reactor.ops().iter().any(|op| op.kind == OpKind::WriteFile),
        "Q0 must not write source morsels to disk (PL-I6)"
    );
    let evicted = engine.evicted(0);
    assert!(!evicted.is_empty(), "pressure must have evicted something");
    let seqs: Vec<u64> = evicted.iter().map(|(seq, _)| *seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "`evicted` lists them oldest first (f.16)");
    assert_eq!(
        engine.detailed_stats().per_queue[0].evictions,
        evicted.len() as u64
    );

    // `replace` puts the bytes back in the entry's original position (f.16, PL-I5). The
    // pressure is lifted first, or the planner would evict them again at once.
    engine.set_water(0, TierKind::Host, bytes * 16, bytes * 32);
    for (seq, origin) in &evicted {
        let mut morsel = common::table_morsel(&alloc, *seq, 0, 32);
        morsel.origin = origin.clone();
        engine.replace(0, morsel).expect("replace");
    }
    assert!(engine.evicted(0).is_empty(), "nothing is evicted any more");
    assert_eq!(
        engine.detailed_stats().per_queue[0].evictions_replaced,
        evicted.len() as u64
    );
    // A `replace` for a sequence number that is not evicted is refused.
    assert!(
        engine
            .replace(0, common::table_morsel(&alloc, 4242, 0, 32))
            .is_err(),
        "replace: no evicted entry"
    );

    engine.close(0);
    let mut order = Vec::new();
    while let Some((morsel, _)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        order.push(morsel.seq);
    }
    assert_eq!(
        order,
        (0..8u64).collect::<Vec<_>>(),
        "FIFO survives replace"
    );
}

#[test]
fn pl_t5_staging_on_writes_instead() {
    let scratch = common::Scratch::new("t5b");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_promotion_window(0, 1);
    engine.set_staging(0, true);
    let sample = common::table_morsel(&alloc, 0, 0, 32);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes * 2, bytes * 3);
    for seq in 0..8u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 32))
            .expect("push");
    }
    common::settle(&reactor);
    assert!(
        reactor.ops().iter().any(|op| op.kind == OpKind::WriteFile),
        "with staging on the same pressure writes (PL-I6)"
    );
    assert!(engine.evicted(0).is_empty(), "and evicts nothing");
}
