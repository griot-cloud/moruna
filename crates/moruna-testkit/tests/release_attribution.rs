//! `FakeAllocator` attributes a release to the region it came from, not to whatever the
//! address belongs to now (d.15, h).
//!
//! The fake is the more dangerous of the two allocators to get this wrong in, because unlike
//! the real arena it hands memory back to the system: `dealloc` runs, the address can be
//! given out again as part of a later and possibly larger allocation, and a release credited
//! by address can then free a region a live `Buffer` still points into. That is a
//! use-after-free in every test that uses the fake, and a SIGSEGV is what it looks like when
//! it surfaces.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_kernel::{Allocator, Buffer, NO_TOKEN, Tier};
use moruna_testkit::FakeAllocator;

/// Allocate, split, convert to Arrow and drop from several threads until the system has
/// recycled addresses many times over, then check that the allocator refused nothing and
/// leaked nothing. Every release in this run is a correct one, so a refusal means the
/// accounting is wrong, and a region left over means a release went missing.
#[test]
fn addresses_recycle_without_a_refused_or_misattributed_release() {
    let alloc = FakeAllocator::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    std::thread::scope(|scope| {
        for thread in 0..6u64 {
            let alloc = alloc.clone();
            scope.spawn(move || {
                let mut seed = thread * 2_654_435_761 + 1;
                let mut live: Vec<Buffer> = Vec::new();
                while Instant::now() < deadline {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let bytes = 1 + (seed % 9000) as usize;
                    let Ok(buffer) = alloc.alloc(bytes, Tier::Host) else {
                        continue;
                    };
                    assert_ne!(buffer.token(), NO_TOKEN, "the fake tags what it hands out");
                    match seed % 4 {
                        // Split, so two owners share one region and it goes back only when
                        // both have (d.3).
                        0 => {
                            let (head, tail) = buffer.split_at(bytes / 2);
                            live.push(head);
                            live.push(tail);
                        }
                        // A zero-length half, which owns nothing and can outlive its region:
                        // the case that used to be credited a byte it did not own.
                        1 => {
                            let (empty, whole) = buffer.split_at(0);
                            live.push(whole);
                            drop(empty);
                        }
                        // Hand the bytes to Arrow, which releases them from its own owner
                        // when the last slice goes.
                        2 => {
                            let arrow = buffer.into_arrow_buffer().expect("a host buffer");
                            let _slice = arrow.slice_with_length(0, bytes.min(arrow.len()));
                        }
                        _ => live.push(buffer),
                    }
                    if live.len() > 32 {
                        live.remove((seed % live.len() as u64) as usize);
                    }
                }
            });
        }
    });
    assert!(
        alloc.allocations_total() > 1_000,
        "the soak has to actually allocate; it made {}",
        alloc.allocations_total()
    );
    assert_eq!(
        alloc.refused_releases(),
        0,
        "every release in a correct run is attributable"
    );
    assert_eq!(alloc.live_regions(), 0, "and every region came back");
    assert_eq!(alloc.in_use(Tier::Host), 0);
}

/// A release that arrives after its region has gone back to the system is refused, even when
/// the address has since been handed out again.
///
/// Attributed by address, as this was, the release below would have found the region that now
/// covers the address and credited it, freeing it under a live `Buffer` and leaving every
/// later read of those bytes a use-after-free.
#[test]
fn a_stale_release_is_refused_rather_than_credited_to_the_next_region() {
    let alloc = FakeAllocator::new();
    // Take a region, remember what named it, and give it back.
    let victim = alloc.buffer(4096, Tier::Host);
    let base = victim.host_ptr().expect("a host buffer");
    let token = victim.token();
    let handle: Arc<dyn moruna_kernel::ArenaHandle> = Arc::clone(victim.arena());
    drop(victim);
    assert_eq!(alloc.live_regions(), 0);

    // Whatever the system gives out next may well cover that address; hold a spread of
    // regions so that it probably does.
    let mut heirs: BTreeMap<usize, Buffer> = BTreeMap::new();
    for _ in 0..64 {
        let buffer = alloc.buffer(4096, Tier::Host);
        heirs.insert(buffer.host_ptr().expect("a host buffer") as usize, buffer);
    }
    let live_before = alloc.live_regions();
    let in_use_before = alloc.in_use(Tier::Host);

    handle.release_token(base, 4096, Tier::Host, token);
    assert_eq!(alloc.refused_releases(), 1, "the stale release is counted");
    assert_eq!(
        alloc.live_regions(),
        live_before,
        "and freed nothing: no heir lost its region to it"
    );
    assert_eq!(alloc.in_use(Tier::Host), in_use_before);

    // The same for a release carrying no token at all: nothing says which region it belongs
    // to, and this allocator will not guess.
    handle.release(base, 4096, Tier::Host);
    assert_eq!(alloc.refused_releases(), 2);
    assert_eq!(alloc.live_regions(), live_before);

    // Releasing one region twice over is refused the second time rather than double-freeing.
    let (address, buffer) = heirs.pop_first().expect("an heir");
    let heir_token = buffer.token();
    let heir_ptr = buffer.host_ptr().expect("a host buffer");
    assert_eq!(heir_ptr as usize, address);
    drop(buffer);
    handle.release_token(heir_ptr, 4096, Tier::Host, heir_token);
    assert_eq!(alloc.refused_releases(), 3);

    heirs.clear();
    assert_eq!(alloc.live_regions(), 0, "and the rest came back normally");
    assert_eq!(alloc.in_use(Tier::Host), 0);
}
