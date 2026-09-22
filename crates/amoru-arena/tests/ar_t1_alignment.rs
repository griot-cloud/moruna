//! AR-T1 alignment (02 k): 10,000 random-size allocations across tiers (host only in CI);
//! every pointer is a multiple of 64 and of `page_bytes`. AR-I1.

mod common;

use amoru_kernel::{ALIGNMENT, Allocator, Tier};

#[test]
fn ar_t1_alignment() {
    let arena = common::host_arena(256 << 20);
    let page = arena.page_bytes() as u64;
    let mut rng = common::Rng::new(0xA111_6A7E);
    // A bounded live set keeps the region from filling: the invariant is about the address
    // every allocation returns, not about how many fit at once.
    let mut live: Vec<amoru_kernel::Buffer> = Vec::new();
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
