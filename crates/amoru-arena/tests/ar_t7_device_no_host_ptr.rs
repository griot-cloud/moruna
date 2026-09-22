//! AR-T7 device_no_host_ptr (02 k, feature `cuda`, skipped without a device and listed as
//! skipped): `host_ptr()` is None and `as_ref` panics with "Device". AR-I7.
//!
//! No GPU host exists (preamble E1), so this test is ignored with its reason and listed in
//! the pull request. It is compiled whenever the `cuda` feature is on, so it cannot rot.

#![cfg(feature = "cuda")]

use amoru_arena::{Arena, ArenaConfig};
use amoru_kernel::{Allocator, DeviceId, Guarantee, Tier, TierKind};

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
