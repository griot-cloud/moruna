//! PL-T14 move_failure_paths (f.10): a failed move is re-issued once, identically; a second
//! failure leaves the entry where its bytes are, surfaces through `pop` for the head, and
//! never loses a byte.

mod common;

use moruna_kernel::{Locality, MorunaError, Placement, TierKind};
use moruna_placement::state::State;
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};

#[test]
fn pl_t14_one_failure_is_retried_identically() {
    let scratch = common::Scratch::new("t14a");
    let alloc = FakeAllocator::new().pinned(true);
    let reactor = FakeReactor::new().fail_next(OpKind::Copy, 1);
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 1 << 30, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_consumer(0, common::want_device());
    engine
        .push(0, common::tensor_morsel(&alloc, 0, 0, 1024))
        .expect("push");
    common::settle(&reactor);

    let copies: Vec<_> = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::Copy)
        .collect();
    assert_eq!(
        copies.len(),
        2,
        "the identical operation is issued again (f.10)"
    );
    assert_eq!(
        copies[0].src_tier, copies[1].src_tier,
        "the same source tier"
    );
    assert_eq!(
        copies[0].dst_tier, copies[1].dst_tier,
        "the same destination"
    );
    assert_eq!(copies[0].len, copies[1].len, "the same bytes");
    assert!(
        matches!(
            engine.head_state(0),
            Some((0, State::Resident(moruna_kernel::Tier::Device(_))))
        ),
        "the second attempt succeeded and the entry landed"
    );
    assert_eq!(engine.detailed_stats().per_queue[0].move_retries, 1);
    assert!(!engine.is_full(0), "one failure is not a full queue");
}

#[test]
fn pl_t14_two_failures_surface_through_pop() {
    let scratch = common::Scratch::new("t14b");
    let alloc = FakeAllocator::new().pinned(true);
    let reactor = FakeReactor::new().fail_next(OpKind::Copy, 2);
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 1 << 30, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_consumer(0, common::want_device());
    engine
        .push(0, common::tensor_morsel(&alloc, 7, 0, 1024))
        .expect("push");
    common::settle(&reactor);

    assert!(
        engine.is_full(0),
        "a failed head makes the queue full (f.14)"
    );
    let error = engine
        .pop(0, common::want_device(), Locality::Any)
        .expect_err("the failure reaches the caller");
    let text = error.to_string();
    assert!(matches!(error, MorunaError::Staging(_)), "got {text}");
    assert!(text.contains('7'), "the message names the morsel: {text}");
    assert!(
        !engine.is_full(0),
        "the error is taken by the pop that returned it (f.10)"
    );
    // The entry stayed where its bytes were: the reactor was given a view, not the buffer.
    assert!(matches!(
        engine.head_state(0),
        Some((7, State::Resident(_)))
    ));
    assert_eq!(
        engine.detailed_stats().reservations,
        [0; moruna_kernel::TIER_COUNT],
        "the reservation of a failed move is released (f.9)"
    );
}

#[test]
fn pl_t14_a_failed_write_leaves_the_bytes_resident() {
    let scratch = common::Scratch::new("t14c");
    let alloc = FakeAllocator::new();
    // A record of a table is four pieces (e.3), so two whole attempts are eight failures.
    let reactor = FakeReactor::new().fail_next(OpKind::WriteFile, 8);
    let cfg = common::config(
        2,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_promotion_window(1, 1);
    let sample = common::table_morsel(&alloc, 0, 1, 128);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(1, TierKind::Host, bytes, bytes);
    let before = alloc.in_use(moruna_kernel::Tier::Host);
    for seq in 0..2u64 {
        engine
            .push(1, common::table_morsel(&alloc, seq, 1, 128))
            .expect("push");
    }
    common::settle(&reactor);

    // The demotion failed twice: the entry stays resident, with its bytes intact (RE-I1).
    let states = engine.entry_states(1);
    assert!(
        states
            .iter()
            .all(|(_, state)| matches!(state, State::Resident(_))),
        "no entry went to disk: {states:?}"
    );
    assert!(
        alloc.in_use(moruna_kernel::Tier::Host) > before,
        "the payload bytes are still held"
    );
    let attempts: Vec<_> = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::WriteFile)
        .collect();
    assert_eq!(
        attempts.len(),
        8,
        "one record, two attempts of four pieces (f.10)"
    );
    let abandoned_end = attempts
        .iter()
        .map(|op| op.offset + op.len)
        .max()
        .expect("a write was attempted");
    assert_eq!(engine.detailed_stats().per_queue[1].move_retries, 1);
    // Pressure remains and the queue is not full: a demotion that failed is not a bound.
    assert!(
        !engine.is_full(1),
        "a failed demotion is not a full queue (f.14)"
    );

    // The next record is laid past the abandoned pages, so the segment stays append only.
    engine
        .push(1, common::table_morsel(&alloc, 2, 1, 128))
        .expect("push");
    common::settle(&reactor);
    let landed = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::WriteFile)
        .skip(8)
        .map(|op| op.offset)
        .min()
        .expect("a further record was written");
    assert!(
        landed >= abandoned_end,
        "a later record must not reuse the abandoned pages: {landed} < {abandoned_end}"
    );

    // And every entry still pops with its rows.
    engine.close(1);
    let mut seen = 0;
    while let Some((morsel, _)) = engine
        .pop_blocking(1, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        assert_eq!(morsel.features.rows, 128);
        seen += 1;
    }
    assert_eq!(seen, 3, "nothing was lost");
}
