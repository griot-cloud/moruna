//! PL-T22 shutdown (f.15, preamble 4.3): `shutdown` returns at once with moves in flight,
//! releases every reservation, cancels the callers, unlinks nothing, and is idempotent.

mod common;

use moruna_kernel::{Locality, MorunaError, Placement, Reactor, TIER_COUNT, TierKind};
use moruna_placement::state::State;
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};
use std::time::{Duration, Instant};

#[test]
fn pl_t22_shutdown() {
    let scratch = common::Scratch::new("t22");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(Duration::from_millis(50));
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 128);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes, bytes);
    for seq in 0..50u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 128))
            .expect("push");
    }
    assert!(reactor.in_flight() > 0, "moves are in flight");
    let segments_before = engine.detailed_stats().segments_live;

    let started = Instant::now();
    engine.shutdown();
    let took = started.elapsed();
    assert!(
        took < Duration::from_millis(5),
        "shutdown took {took:?}: it must not wait on a completion (f.15)"
    );

    // Every reservation is released and every entry is back in its source state (f.15).
    let detail = engine.detailed_stats();
    assert_eq!(
        detail.reservations, [0; TIER_COUNT],
        "reservations released"
    );
    assert_eq!(detail.moves_in_flight, 0, "the moves map is empty");
    for (seq, state) in engine.entry_states(0) {
        assert!(
            !state.in_flight(),
            "entry {seq} is still {state:?} after shutdown"
        );
    }
    let resident: u64 = engine.stats().queues[0].bytes_by_tier.iter().sum::<u64>();
    let accounted: u64 = engine
        .entry_states(0)
        .into_iter()
        .filter(|(_, state)| state.resident_tier().is_some())
        .count() as u64
        * bytes;
    assert_eq!(
        resident, accounted,
        "resident plus reserved is the entries' bytes"
    );

    // Callers are cancelled.
    assert!(matches!(
        engine.pop_blocking(0, common::want_host(), Locality::Any),
        Err(MorunaError::Cancelled)
    ));
    assert!(matches!(
        engine.push(0, common::table_morsel(&alloc, 99, 0, 128)),
        Err(MorunaError::Cancelled)
    ));

    // No segment was unlinked: a manifest may reference them (f.15).
    assert_eq!(
        engine.detailed_stats().segments_live,
        segments_before,
        "shutdown unlinks nothing"
    );
    let ops_before = reactor.ops().len();
    assert!(
        !reactor.segments().is_empty(),
        "every segment is still registered with the reactor"
    );

    // The late completions change nothing.
    let snapshot = format!("{:?}", engine.detailed_stats());
    reactor.shutdown();
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(
        format!("{:?}", engine.detailed_stats()),
        snapshot,
        "a completion that arrives after shutdown finds no move id (f.15)"
    );
    assert!(
        reactor
            .ops()
            .iter()
            .skip(ops_before)
            .all(|op| op.kind != OpKind::WriteFile),
        "the engine issued nothing after shutdown"
    );

    // And a second shutdown is a no-op.
    engine.shutdown();
    assert_eq!(format!("{:?}", engine.detailed_stats()), snapshot);
    assert!(!engine.any_in_flight());
}

#[test]
fn pl_t22_an_entry_promoting_from_disk_keeps_its_record() {
    let scratch = common::Scratch::new("t22b");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(Duration::from_millis(60));
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::table_morsel(&alloc, 0, 0, 128);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes, bytes);
    for seq in 0..4u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 128))
            .expect("push");
    }
    common::settle(&reactor);
    engine.shutdown();
    for (seq, state) in engine.entry_states(0) {
        assert!(
            matches!(
                state,
                State::Resident(_) | State::ResidentOnDisk(_) | State::OnDisk(_)
            ),
            "entry {seq} is {state:?}: a move in flight goes back to where its bytes were"
        );
    }
}

#[test]
fn pl_t22_the_source_state_of_every_move_is_where_its_bytes_were() {
    // f.15 restores each in-flight entry to its source state, which is what `State` computes;
    // the cases are the four moves of e.4 with and without a valid record.
    use moruna_kernel::{NodeId, RemoteRef, SegmentRef, Tier};
    use moruna_placement::shutdown::is_settled;
    let seg = SegmentRef {
        segment: 3,
        offset: 4096,
        len: 128,
    };
    let promoting = State::Promoting(Tier::Host, Tier::Device(moruna_kernel::DeviceId(0)));
    assert!(!is_settled(&promoting));
    assert!(matches!(
        promoting.source_state(None),
        State::Resident(Tier::Host)
    ));
    assert!(matches!(
        promoting.source_state(Some(seg)),
        State::ResidentOnDisk(Tier::Host)
    ));
    let from_disk = State::Promoting(Tier::Disk(seg), Tier::Host);
    assert!(matches!(
        from_disk.source_state(Some(seg)),
        State::OnDisk(_)
    ));
    assert!(matches!(from_disk.source_state(None), State::Evicted));
    let demoting = State::Demoting(Tier::Host, Tier::Disk(seg));
    assert!(matches!(
        demoting.source_state(None),
        State::Resident(Tier::Host)
    ));
    // A settled state is its own source state.
    for state in [
        State::Resident(Tier::Host),
        State::ResidentOnDisk(Tier::Host),
        State::OnDisk(seg),
        State::Evicted,
        State::Consumed,
        State::OnRemote(
            NodeId(1),
            RemoteRef {
                addr: 1,
                rkey: 2,
                len: 3,
            },
        ),
    ] {
        assert!(is_settled(&state));
        assert_eq!(
            state.source_state(None).index(),
            state.index(),
            "{state:?} is already where its bytes are"
        );
        assert_eq!(state.accounted_tier(), state.resident_tier());
    }
}
