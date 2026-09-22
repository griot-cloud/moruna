//! PL-T24 water_by_tier_kind (d.1, e.2): a queue's marks are keyed by `TierKind`, the mark
//! for the other host tier is stored and has no effect, and the index the marks refer to is
//! `TierKind::index()`.

mod common;

use amoru_kernel::{DeviceId, Locality, Placement, Tier, TierKind};
use amoru_testkit::{FakeAllocator, FakeReactor, OpKind};

fn demotions_under(pinned: bool, mark: TierKind) -> (u64, u64) {
    let scratch = common::Scratch::new("t24");
    let alloc = FakeAllocator::new().pinned(pinned);
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
    engine.set_water(0, mark, bytes, bytes);
    for seq in 0..6u64 {
        engine
            .push(0, common::table_morsel(&alloc, seq, 0, 64))
            .expect("push");
    }
    common::settle(&reactor);
    let writes = reactor
        .ops()
        .iter()
        .filter(|op| op.kind == OpKind::WriteFile)
        .count() as u64;
    (engine.stats().queues[0].demotions, writes)
}

#[test]
fn pl_t24_the_mark_for_the_run_s_host_tier_governs() {
    // Unpinned: `Host` is the run's host tier, so its mark is the one that demotes.
    let (demotions, writes) = demotions_under(false, TierKind::Host);
    assert!(
        demotions > 0 && writes > 0,
        "the Host mark governs an unpinned run"
    );
    let (demotions, writes) = demotions_under(false, TierKind::PinnedHost);
    assert_eq!(
        (demotions, writes),
        (0, 0),
        "the mark for the other host tier is stored and has no effect (contracts e.1)"
    );

    // Pinned: `PinnedHost` is the run's host tier, and the roles swap.
    let (demotions, writes) = demotions_under(true, TierKind::PinnedHost);
    assert!(
        demotions > 0 && writes > 0,
        "the PinnedHost mark governs a pinned run"
    );
    let (demotions, writes) = demotions_under(true, TierKind::Host);
    assert_eq!((demotions, writes), (0, 0), "and the Host mark does not");
}

#[test]
fn pl_t24_the_device_mark_governs_the_one_device_slot() {
    let scratch = common::Scratch::new("t24b");
    let alloc = FakeAllocator::new().pinned(true);
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 1 << 30, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_consumer(0, common::want_device());
    engine.set_promotion_window(0, 4);
    let sample = common::tensor_morsel(&alloc, 0, 0, 512);
    let bytes = sample.bytes;
    drop(sample);
    for seq in 0..4u64 {
        engine
            .push(0, common::tensor_morsel(&alloc, seq, 0, 512))
            .expect("push");
        common::settle(&reactor);
    }
    let device = Tier::Device(DeviceId(0));
    assert_eq!(
        engine.stats().queues[0].bytes_by_tier[device.index()],
        bytes * 4,
        "the Device slot of `bytes_by_tier` is `TierKind::index()` (d.1)"
    );
    assert_eq!(device.index(), TierKind::Device.index());

    engine.set_promotion_window(0, 1);
    engine.set_water(0, TierKind::Device, bytes, bytes);
    common::settle(&reactor);
    assert!(
        engine.stats().queues[0].bytes_by_tier[device.index()] <= bytes * 2,
        "the Device mark governs Device(devices[0]) (b, d.1)"
    );
    // A mark for the reserved tier is accepted and stored, and never consulted in v1 (h).
    engine.set_water(0, TierKind::Remote, 1, 2);
    common::settle(&reactor);
    assert_eq!(
        engine.stats().queues[0].bytes_by_tier[TierKind::Remote.index()],
        0
    );
    assert!(engine.pop(0, common::want_device(), Locality::Any).is_ok());
}

#[test]
fn pl_t24_the_engine_refuses_a_configuration_it_cannot_honour() {
    use amoru_kernel::{Allocator, AmoruError, Reactor};
    use std::sync::Arc;
    let allocator: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    let io: Arc<dyn Reactor> = Arc::new(FakeReactor::new());
    let base = common::config(1, None, common::budgets(1 << 30, 0, 0));

    let mut none = base.clone();
    none.stages = 0;
    match amoru_placement::PlacementEngine::new(none, allocator.clone(), io.clone()) {
        Err(AmoruError::Config { name, .. }) => assert_eq!(name, "stages"),
        Err(other) => panic!("expected Config, got {other}"),
        Ok(_) => panic!("expected Config, got an engine"),
    }
    let mut pageless = base.clone();
    pageless.page_bytes = 0;
    match amoru_placement::PlacementEngine::new(pageless, allocator.clone(), io.clone()) {
        Err(AmoruError::Config { name, .. }) => assert_eq!(name, "page.bytes"),
        Err(other) => panic!("expected Config, got {other}"),
        Ok(_) => panic!("expected Config, got an engine"),
    }
    let mut segmentless = base.clone();
    segmentless.segment_bytes = 0;
    match amoru_placement::PlacementEngine::new(segmentless, allocator, io) {
        Err(AmoruError::Config { name, .. }) => assert_eq!(name, "staging.segment_bytes"),
        Err(other) => panic!("expected Config, got {other}"),
        Ok(_) => panic!("expected Config, got an engine"),
    }
    assert_eq!(
        amoru_placement::PlacementConfig::default().stages,
        1,
        "the default configuration is one queue and no staging directory"
    );
    assert!(
        amoru_placement::PlacementConfig::default()
            .staging_dir
            .is_none()
    );
}

#[test]
fn pl_t24_a_knob_for_a_stage_outside_the_run_is_ignored() {
    // Every setter of d.1 takes a stage; one outside the run changes nothing and does not
    // panic, because the controller writes knobs without holding the pipeline's shape.
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(1, None, common::budgets(1 << 30, 0, 0));
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_consumer(9, common::want_device());
    engine.set_water(9, TierKind::Host, 1, 2);
    engine.set_staging(9, true);
    engine.set_promotion_window(9, 4);
    engine.close(9);
    assert!(
        engine
            .replace(9, common::table_morsel(&alloc, 0, 0, 8))
            .is_err()
    );
    assert_eq!(
        engine.stats().queues.len(),
        1,
        "the run still has one queue"
    );

    // A consumer wanting a device on a run with no device falls back to the host tier (b).
    let mut deviceless = common::config(1, None, common::budgets(1 << 30, 0, 0));
    deviceless.devices.clear();
    let engine = common::engine(deviceless, &alloc, &reactor);
    engine.set_consumer(0, common::want_device());
    engine
        .push(0, common::tensor_morsel(&alloc, 0, 0, 64))
        .expect("push");
    common::settle(&reactor);
    assert!(
        engine.peek_resident(0, common::want_host(), Locality::Any),
        "with no device the target is the host tier (b)"
    );
    assert!(reactor.ops().is_empty(), "and no move was needed");
}

#[test]
fn pl_t24_set_budgets_replaces_every_tier() {
    use amoru_kernel::TIER_COUNT;
    let scratch = common::Scratch::new("t24c");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    let sample = common::table_morsel(&alloc, 0, 0, 64);
    let bytes = sample.bytes;
    drop(sample);
    // A budget too small for one morsel: the head cannot be promoted and says so (h).
    engine.set_budgets(common::budgets(bytes / 2, 0, 1 << 30));
    engine.set_consumer(0, common::want_device());
    engine
        .push(0, common::table_morsel(&alloc, 0, 0, 64))
        .expect("push");
    common::settle(&reactor);
    let error = engine
        .pop(0, common::want_device(), Locality::Any)
        .expect_err("the head cannot fit");
    let text = error.to_string();
    assert!(text.contains("head cannot fit"), "{text}");
    assert_eq!(
        engine.detailed_stats().reservations,
        [0; TIER_COUNT],
        "a reservation that failed leaves nothing behind (f.9)"
    );
    // And raising the disk budget is what `set_budgets` carries to the staging bound.
    engine.set_budgets(common::budgets(1 << 30, 0, 4096));
    assert_eq!(engine.detailed_stats().disk_budget_effective, 4096);
}
