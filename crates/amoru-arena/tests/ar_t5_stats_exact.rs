//! AR-T5 stats_exact (02 k): a random alloc/release sequence against a model; `AllocStats`
//! equals the model at every step. AR-I5.

mod common;

use amoru_kernel::{Allocator, Buffer, Tier};

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
