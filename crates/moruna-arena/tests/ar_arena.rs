//! The AR tests that need no process-wide state and little memory: AR-T1, AR-T2, AR-T5,
//! AR-T7, AR-T8, AR-T10 and AR-T12 of `architecture/sdd/02-arena.md` section k.
//!
//! They share one test binary because each one links `arrow` statically and the quality
//! job builds the workspace twice, once for coverage; twelve binaries for twelve tests put
//! the CI runner out of disk. AR-T3 and AR-T6 keep binaries of their own because the state
//! they set (the reservation counters, `RLIMIT_MEMLOCK`) is process wide, and the
//! memory-heavy tests keep another.

mod common;

use moruna_arena::{Arena, ArenaConfig};
use moruna_kernel::{
    ALIGNMENT, Allocator, MorunaError, Buffer, BufferView, Guarantee, Tier, TierKind,
};

// AR-T1 alignment (02 k): 10,000 random-size allocations across tiers (host only in CI);
// every pointer is a multiple of 64 and of `page_bytes`. AR-I1.

#[test]
fn ar_t1_alignment() {
    let arena = common::host_arena(256 << 20);
    let page = arena.page_bytes() as u64;
    let mut rng = common::Rng::new(0xA111_6A7E);
    // A bounded live set keeps the region from filling: the invariant is about the address
    // every allocation returns, not about how many fit at once.
    let mut live: Vec<moruna_kernel::Buffer> = Vec::new();
    for i in 0..10_000u32 {
        let bytes = rng.upto(1 << 20);
        let b = arena
            .alloc(bytes as usize, Tier::Host)
            .expect("the region serves a buffer of at most 1 MiB");
        let ptr = b.host_ptr().expect("a host buffer has a host pointer") as usize as u64;
        assert_eq!(ptr % ALIGNMENT as u64, 0, "allocation {i} of {bytes} bytes");
        // AR-I1: page alignment is required when the class size is at least the page size,
        // which is every class here because class 0 is 64 KiB.
        assert!(common::charged(bytes) >= page);
        assert_eq!(ptr % page, 0, "allocation {i} of {bytes} bytes");
        live.push(b);
        if live.len() == 16 {
            live.clear();
        }
    }
    assert_eq!(arena.stats().allocations_total, 10_000);
}

// AR-T2 budget_enforced (02 k): allocate until failure; the sum of charged sizes stays at
// or below the region, and the failure is `Alloc` with the correct `in_use`. AR-I2.

#[test]
fn ar_t2_budget_enforced() {
    let region = 256u64 << 20;
    let arena = common::host_arena(region);
    assert_eq!(arena.region_bytes(Tier::Host), region);

    let mut live: Vec<Buffer> = Vec::new();
    let mut model = 0u64;
    let err = loop {
        match arena.alloc(4 << 20, Tier::Host) {
            Ok(b) => {
                model += common::charged(4 << 20);
                live.push(b);
                assert!(
                    model <= region,
                    "charged {model} exceeds the region {region}"
                );
                assert_eq!(arena.stats().host_in_use, model);
            }
            Err(e) => break e,
        }
    };
    assert_eq!(live.len(), (region / (4 << 20)) as usize);
    match err {
        MorunaError::Alloc {
            bytes,
            tier,
            budget,
            in_use,
        } => {
            assert_eq!(bytes, 4 << 20);
            assert_eq!(tier, Tier::Host);
            assert_eq!(budget, region);
            assert_eq!(in_use, model);
            assert_eq!(in_use, arena.stats().host_in_use);
        }
        other => panic!("expected Alloc, got {other:?}"),
    }
    // Nothing the arena refused was charged, and giving the bytes back gives the budget back.
    drop(live);
    assert_eq!(arena.stats().host_in_use, 0);
}

// AR-T5 stats_exact (02 k): a random alloc/release sequence against a model; `AllocStats`
// equals the model at every step. AR-I5.

#[test]
fn ar_t5_stats_exact() {
    let arena = common::host_arena(256 << 20);
    let mut rng = common::Rng::new(0x5_7A75);
    let mut live: Vec<(Buffer, u64)> = Vec::new();
    let mut model_in_use = 0u64;
    let mut model_allocations = 0u64;

    for step in 0..5_000u32 {
        let take = live.is_empty() || !rng.next().is_multiple_of(3);
        if take {
            let bytes = rng.upto(512 << 10);
            match arena.alloc(bytes as usize, Tier::Host) {
                Ok(b) => {
                    model_in_use += common::charged(bytes);
                    model_allocations += 1;
                    live.push((b, common::charged(bytes)));
                }
                Err(_) => {
                    // A refusal changes nothing: AR-I5 is about live buffers.
                }
            }
        } else {
            let i = (rng.next() % live.len() as u64) as usize;
            let (b, charged) = live.swap_remove(i);
            drop(b);
            model_in_use -= charged;
        }
        let stats = arena.stats();
        assert_eq!(stats.host_in_use, model_in_use, "step {step}");
        assert_eq!(stats.pinned_in_use, 0, "step {step}");
        assert_eq!(stats.device_in_use, [0u64; 8], "step {step}");
        assert_eq!(stats.allocations_total, model_allocations, "step {step}");
        assert_eq!(stats.payload_copies_total, 0, "step {step}");
        assert_eq!(stats.boundary_copies_total, 0, "step {step}");
    }

    drop(live);
    assert_eq!(arena.stats().host_in_use, 0);
    assert_eq!(arena.arena_stats().double_release, 0);
}

// AR-T8 split_at (02 k): split, release one half, allocate again: the slot is not reused
// until both halves have been released. e.3.

#[test]
fn ar_t8_split_at() {
    let arena = common::host_arena(4 << 20);
    let b = arena
        .alloc(64 * 1024, Tier::Host)
        .expect("one class-0 slot");
    let slot = b.host_ptr().expect("host pointer");
    assert_eq!(arena.stats().host_in_use, 64 * 1024);

    let (head, tail) = b.split_at(16 * 1024);
    assert_eq!((head.len(), tail.len()), (16 * 1024, 48 * 1024));
    assert_eq!(
        arena.stats().host_in_use,
        64 * 1024,
        "one slot, two buffers"
    );

    drop(head);
    assert_eq!(
        arena.stats().host_in_use,
        64 * 1024,
        "half the slot is still live"
    );
    let other = arena.alloc(64 * 1024, Tier::Host).expect("another slot");
    assert_ne!(
        other.host_ptr().expect("host pointer"),
        slot,
        "the slot must not be reused while a half of it is live (e.3)"
    );
    drop(other);

    drop(tail);
    assert_eq!(arena.stats().host_in_use, 0, "both halves are back");
    let again = arena.alloc(64 * 1024, Tier::Host).expect("the slot again");
    assert_eq!(
        again.host_ptr().expect("host pointer"),
        slot,
        "the slot is reusable once every byte of it is back"
    );
    assert_eq!(arena.arena_stats().double_release, 0);
}

// AR-T10 small_budget_mode (02 k): a region of 256 MiB serves classes 0..=12 by splitting;
// class 13 fails with `Alloc`. e.1.

#[test]
fn ar_t10_small_budget_mode() {
    let region = 256u64 << 20;
    let arena = common::host_arena(region);

    // Class 12 is the whole region: the small-budget mode seeds it as one block (e.1).
    let whole = arena
        .alloc((256 << 20) as usize, Tier::Host)
        .expect("class 12 is the region");
    assert_eq!(arena.stats().host_in_use, region);
    drop(whole);

    // Classes 0..=11 come out of one buddy split and are live at the same time: their sum
    // is 256 MiB minus one granule, so the region serves every one of them.
    let mut live: Vec<Buffer> = Vec::new();
    let mut expected = 0u64;
    for c in 0..=11u32 {
        let bytes = (64u64 << 10) << c;
        let b = arena
            .alloc(bytes as usize, Tier::Host)
            .unwrap_or_else(|e| panic!("class {c} of {bytes} bytes: {e}"));
        assert_eq!(b.len() as u64, bytes);
        expected += common::charged(bytes);
        assert_eq!(arena.stats().host_in_use, expected);
        live.push(b);
    }
    assert_eq!(expected, region - (64 << 10));

    // Class 13 is larger than the region and fails whatever else is live (AR-I2).
    let e = arena
        .alloc((512 << 20) as usize, Tier::Host)
        .expect_err("class 13 does not fit a 256 MiB region");
    assert!(matches!(e, MorunaError::Alloc { budget, .. } if budget == region));
    drop(live);
    let e = arena
        .alloc((512 << 20) as usize, Tier::Host)
        .expect_err("class 13 still does not fit an empty 256 MiB region");
    assert!(matches!(e, MorunaError::Alloc { tier, .. } if tier == Tier::Host));
}

// AR-T12 contains_and_tier_of (02 k): for every live buffer, `contains(ptr)` is true for
// its first and last byte and false one byte past its end and for a heap pointer;
// `tier_of` equals the buffer's tier; on a pinned arena every host `tier_of` is
// `PinnedHost`, on an unpinned one `Host`; `BufferView::of_arrow` over
// `into_arrow_buffer` of an arena buffer, and over a slice of it, reports that tier.
// f.7, AR-I6.

fn check_bounds(arena: &Arena, tier: Tier) {
    let sizes = [1usize, 4096, 64 * 1024, 3 * 1024 * 1024];
    let mut live = Vec::new();
    for bytes in sizes {
        let b = arena.alloc(bytes, tier).expect("a buffer");
        let ptr = b.host_ptr().expect("a host buffer has a host pointer");
        assert!(arena.contains(ptr.cast_const()));
        // SAFETY: test-only; the last byte of a live buffer and the address one past it,
        // which is only compared, never read.
        let last = unsafe { ptr.add(bytes - 1) };
        assert!(arena.contains(last.cast_const()));
        assert_eq!(arena.tier_of(ptr.cast_const()), Some(tier));
        assert_eq!(arena.tier_of(last.cast_const()), Some(tier));
        live.push(b);
    }
    // One byte past the region is not in it, and neither is anything on the heap.
    let region = arena.region_bytes(tier);
    let base = live[0].host_ptr().expect("host pointer");
    // SAFETY: test-only; the address is computed and compared, never read.
    let past = unsafe { base.add(region as usize) };
    assert!(!arena.contains(past.cast_const()));
    assert_eq!(arena.tier_of(past.cast_const()), None);
    let heap = [0u8; 64];
    assert!(!arena.contains(heap.as_ptr()));
    assert_eq!(arena.tier_of(heap.as_ptr()), None);
    // The other host tier does not exist in this run (AR-I6, contracts e.1).
    let other = if tier == Tier::Host {
        Tier::PinnedHost
    } else {
        Tier::Host
    };
    assert_eq!(arena.region_bytes(other), 0);
}

fn check_arrow(arena: &Arena, tier: Tier) {
    let b = arena.alloc(8192, tier).expect("a buffer to hand to Arrow");
    let arrow_buffer = b.into_arrow_buffer().expect("a host buffer converts");
    let view = BufferView::of_arrow(&arrow_buffer, arena).expect("an arena-owned allocation");
    assert_eq!(view.tier(), tier);
    assert_eq!(view.len(), 8192);
    let sliced = arrow_buffer.slice(64);
    let view = BufferView::of_arrow(&sliced, arena).expect("a slice shares the allocation");
    assert_eq!(view.tier(), tier, "a slice keeps the arena's tier (d.3)");
}

#[test]
fn ar_t12_contains_and_tier_of() {
    let arena = common::host_arena(64 << 20);
    assert!(!arena.is_pinned());
    check_bounds(&arena, Tier::Host);
    check_arrow(&arena, Tier::Host);

    // The pinned half of the invariant. A host that refuses `mlock` says so through
    // `is_pinned()`; AR-T6 forces both outcomes deterministically, so this half asserts
    // whichever tier this host's arena actually has.
    let pinned = Arena::new(ArenaConfig {
        host_bytes: 1 << 20,
        host_tier: TierKind::PinnedHost,
        device_bytes: Vec::new(),
        page_bytes: 4096,
        huge_pages: Guarantee::Probed(false),
        memlock: Guarantee::Probed(true),
        register_rdma: false,
    })
    .expect("a pinned arena, or its fallback");
    let tier = if pinned.is_pinned() {
        Tier::PinnedHost
    } else {
        println!("AR-T12: this host refused mlock; the arena fell back to Tier::Host (f.1)");
        Tier::Host
    };
    let b = pinned.alloc(4096, tier).expect("the run's host tier");
    let ptr = b.host_ptr().expect("host pointer");
    assert_eq!(pinned.tier_of(ptr.cast_const()), Some(tier));
    assert_eq!(b.tier(), tier);
    check_arrow(&pinned, tier);
}

#[cfg(feature = "cuda")]
mod ar_t7 {
    // AR-T7 device_no_host_ptr (02 k, feature `cuda`, skipped without a device and listed as
    // skipped): `host_ptr()` is None and `as_ref` panics with "Device". AR-I7.
    //
    // No GPU host exists (preamble E1), so this test is ignored with its reason and listed in
    // the pull request. It is compiled whenever the `cuda` feature is on, so it cannot rot.
    use moruna_arena::{Arena, ArenaConfig};
    use moruna_kernel::{Allocator, DeviceId, Guarantee, Tier, TierKind};

    #[test]
    #[ignore = "reference host, E1: needs a CUDA device; no GPU host is named"]
    fn ar_t7_device_no_host_ptr() {
        let arena = Arena::new(ArenaConfig {
            host_bytes: 4 << 20,
            host_tier: TierKind::Host,
            device_bytes: vec![(DeviceId(0), 64 << 20)],
            page_bytes: 4096,
            huge_pages: Guarantee::Probed(false),
            memlock: Guarantee::Absent,
            register_rdma: false,
        })
        .expect("a device arena");
        let b = arena
            .alloc(4096, Tier::Device(DeviceId(0)))
            .expect("a device buffer");
        assert!(b.host_ptr().is_none());
        assert!(b.device_ptr().is_some());
        assert_eq!(arena.stats().device_in_use[0], 64 * 1024);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = b.as_ref();
        }))
        .expect_err("as_ref panics on a device buffer");
        let msg = panicked
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(msg.contains("Device"), "panic message was {msg:?}");
    }
}
