//! AR-T2 budget_enforced (02 k): allocate until failure; the sum of charged sizes stays at
//! or below the region, and the failure is `Alloc` with the correct `in_use`. AR-I2.

mod common;

use amoru_kernel::{Allocator, AmoruError, Buffer, Tier};

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
        AmoruError::Alloc {
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
