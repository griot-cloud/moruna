//! PL-T11 move_table (e.4, contracts e.1): every row of the move table, with the operations
//! the engine issues for it; the host tier is the run's one host tier in every row and no
//! operation ever joins `Host` and `PinnedHost`.
//!
//! The device rows are exercised against the fake, which accepts any tier pair; they are
//! tagged "(reference host, E1)" for the real reactor and are run here with tensor payloads.
//! A table payload cannot take a device row at all: rebuilding a `RecordBatch` over buffers
//! the engine allocated needs either `unsafe` in this crate, which section l forbids, or a
//! safe constructor the contracts do not have. That is reported as an E10 item; the disk
//! rows are unaffected, because a record goes through `ipc::encode_framing` and `ipc::decode`.

mod common;

use moruna_kernel::{DeviceId, Locality, MorunaError, Placement, Tier, TierKind};
use moruna_placement::state::State;
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};

const DEVICE: Tier = Tier::Device(DeviceId(0));

fn engine_with(
    pinned: bool,
    scratch: &common::Scratch,
) -> (
    std::sync::Arc<moruna_placement::PlacementEngine>,
    FakeAllocator,
    FakeReactor,
) {
    let alloc = FakeAllocator::new().pinned(pinned);
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 1 << 30, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    (engine, alloc, reactor)
}

#[test]
fn pl_t11_host_to_device_and_back() {
    for pinned in [false, true] {
        let scratch = common::Scratch::new("t11a");
        let (engine, alloc, reactor) = engine_with(pinned, &scratch);
        let host = alloc.host_tier();
        assert_eq!(engine.host_tier(), host, "the run has one host tier (e.1)");
        engine.set_consumer(0, common::want_device());
        engine
            .push(0, common::tensor_morsel(&alloc, 0, 0, 1024))
            .expect("push");
        common::settle(&reactor);

        // host tier -> Device(d): one copy per payload (e.4).
        let ops = reactor.ops();
        assert_eq!(ops.len(), 1, "one operation for one tensor");
        assert_eq!(ops[0].kind, OpKind::Copy);
        assert_eq!(ops[0].src_tier, Some(host));
        assert_eq!(ops[0].dst_tier, Some(DEVICE));
        assert!(matches!(
            engine.head_state(0),
            Some((0, State::Resident(DEVICE)))
        ));

        // Device(d) -> host tier: the engine calls it a promotion, whichever way the rank
        // goes, because it moves toward the consumer's tier (h).
        engine.set_consumer(0, common::want_host());
        common::settle(&reactor);
        let ops = reactor.ops();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[1].kind, OpKind::Copy);
        assert_eq!(ops[1].src_tier, Some(DEVICE));
        assert_eq!(ops[1].dst_tier, Some(host));

        // No operation ever names the other host tier (contracts e.1).
        let other = if host == Tier::Host {
            Tier::PinnedHost
        } else {
            Tier::Host
        };
        for op in reactor.ops() {
            assert_ne!(op.src_tier, Some(other), "no Host/PinnedHost pair (e.1)");
            assert_ne!(op.dst_tier, Some(other), "no Host/PinnedHost pair (e.1)");
        }
    }
}

#[test]
fn pl_t11_host_to_disk_and_back() {
    for pinned in [false, true] {
        let scratch = common::Scratch::new("t11b");
        let (engine, alloc, reactor) = engine_with(pinned, &scratch);
        let host = alloc.host_tier();
        engine.set_staging(0, true);
        engine.set_promotion_window(0, 1);
        let sample = common::tensor_morsel(&alloc, 0, 0, 1024);
        let bytes = sample.bytes;
        drop(sample);
        engine.set_water(0, TierKind::PinnedHost, bytes, bytes);
        engine.set_water(0, TierKind::Host, bytes, bytes);
        for seq in 0..3u64 {
            engine
                .push(0, common::tensor_morsel(&alloc, seq, 0, 1024))
                .expect("push");
        }
        common::settle(&reactor);

        // host tier -> Disk: one `write_file` per piece of the record (e.4, f.5).
        let writes: Vec<_> = reactor
            .ops()
            .into_iter()
            .filter(|op| op.kind == OpKind::WriteFile)
            .collect();
        assert!(!writes.is_empty(), "a record was written");
        for op in &writes {
            assert_eq!(op.src_tier, Some(host), "written from the host tier (e.4)");
        }

        // Disk -> host tier: one `read_file` into one arena buffer (e.4, f.6).
        engine.close(0);
        while engine
            .pop_blocking(0, common::want_host(), Locality::Any)
            .expect("pop_blocking")
            .is_some()
        {}
        let reads: Vec<_> = reactor
            .ops()
            .into_iter()
            .filter(|op| op.kind == OpKind::ReadFile)
            .collect();
        assert!(!reads.is_empty(), "a record was read back");
        for op in &reads {
            assert_eq!(op.dst_tier, Some(host), "read into the host tier (e.4)");
        }
    }
}

#[test]
fn pl_t11_disk_to_device_is_two_rows_in_sequence() {
    let scratch = common::Scratch::new("t11c");
    let (engine, alloc, reactor) = engine_with(true, &scratch);
    let host = alloc.host_tier();
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let sample = common::tensor_morsel(&alloc, 0, 0, 1024);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::PinnedHost, bytes, bytes);
    for seq in 0..3u64 {
        engine
            .push(0, common::tensor_morsel(&alloc, seq, 0, 1024))
            .expect("push");
    }
    common::settle(&reactor);
    assert!(
        engine
            .entry_states(0)
            .iter()
            .any(|(_, state)| matches!(state, State::OnDisk(_))),
        "an entry is on disk for this row"
    );

    // The consumer wants the device; the resident head takes the host-to-device row.
    engine.set_consumer(0, common::want_device());
    common::settle(&reactor);
    // Taking it off makes the next head an entry whose only bytes are a segment record, and
    // its promotion is then the two rows in sequence.
    let before = reactor.ops().len();
    engine
        .pop(0, common::want_device(), Locality::Any)
        .expect("pop")
        .expect("the head reached the device");
    common::settle(&reactor);
    let after: Vec<_> = reactor.ops().into_iter().skip(before).collect();
    let read = after
        .iter()
        .position(|op| op.kind == OpKind::ReadFile)
        .expect("the record is read into the host tier first");
    let copy = after
        .iter()
        .position(|op| op.kind == OpKind::Copy)
        .expect("then copied to the device");
    assert!(read < copy, "the copy is issued from the read's completion");
    assert_eq!(after[read].dst_tier, Some(host));
    assert_eq!(after[copy].src_tier, Some(host));
    assert_eq!(after[copy].dst_tier, Some(DEVICE));
    assert!(
        after[read]
            .t_resolve
            .is_some_and(|at| at <= after[copy].t_submit),
        "the second row is submitted after the first resolved"
    );
}

#[test]
fn pl_t11_device_to_disk_goes_through_the_host_tier() {
    let scratch = common::Scratch::new("t11d");
    let (engine, alloc, reactor) = engine_with(true, &scratch);
    let host = alloc.host_tier();
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 4);
    engine.set_consumer(0, common::want_device());
    let sample = common::tensor_morsel(&alloc, 0, 0, 1024);
    let bytes = sample.bytes;
    drop(sample);
    for seq in 0..4u64 {
        engine
            .push(0, common::tensor_morsel(&alloc, seq, 0, 1024))
            .expect("push");
        common::settle(&reactor);
    }
    assert_eq!(
        engine
            .entry_states(0)
            .iter()
            .filter(|(_, state)| matches!(state, State::Resident(DEVICE)))
            .count(),
        4,
        "every entry reached the device"
    );
    // Device is over its high water: demotion is one tier down, to the host tier (e.4).
    engine.set_promotion_window(0, 1);
    engine.set_water(0, TierKind::Device, bytes, bytes);
    common::settle(&reactor);
    let down: Vec<_> = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::Copy && op.src_tier == Some(DEVICE))
        .collect();
    assert!(!down.is_empty(), "Device demotes to the host tier first");
    for op in &down {
        assert_eq!(op.dst_tier, Some(host), "never two tiers at once (e.4)");
    }
    // And then the host tier is over its own high water and the record is written.
    engine.set_water(0, TierKind::PinnedHost, bytes, bytes);
    common::settle(&reactor);
    assert!(
        reactor.ops().iter().any(|op| op.kind == OpKind::WriteFile),
        "host tier -> Disk completes the Device -> Disk ladder (e.4)"
    );
}

#[test]
fn pl_t11_a_table_cannot_take_a_device_row() {
    // The E10 item this crate reports: a device row for a table needs a safe constructor for
    // a `RecordBatch` over buffers the engine allocated. The refusal is explicit, named and
    // surfaced through `pop`, never a silent wrong answer.
    let scratch = common::Scratch::new("t11e");
    let (engine, alloc, reactor) = engine_with(true, &scratch);
    engine.set_consumer(
        0,
        moruna_kernel::PayloadSpec {
            kind: moruna_kernel::PayloadKind::Either,
            tier: moruna_kernel::TierPref::Device,
        },
    );
    engine
        .push(0, common::table_morsel(&alloc, 0, 0, 64))
        .expect("push");
    common::settle(&reactor);
    let error = engine
        .pop(0, common::want_device(), Locality::Any)
        .expect_err("the refusal reaches the caller");
    let text = error.to_string();
    assert!(matches!(error, MorunaError::Staging(_)), "got {text}");
    assert!(text.contains("E10"), "the message names the item: {text}");
}
