//! AR-T10 small_budget_mode (02 k): a region of 256 MiB serves classes 0..=12 by splitting;
//! class 13 fails with `Alloc`. e.1.

mod common;

use amoru_kernel::{Allocator, AmoruError, Buffer, Tier};

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
    assert!(matches!(e, AmoruError::Alloc { budget, .. } if budget == region));
    drop(live);
    let e = arena
        .alloc((512 << 20) as usize, Tier::Host)
        .expect_err("class 13 still does not fit an empty 256 MiB region");
    assert!(matches!(e, AmoruError::Alloc { tier, .. } if tier == Tier::Host));
}
