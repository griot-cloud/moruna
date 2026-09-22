//! AR-T11 flat_footprint (02 k): after `new`, resident memory is within 2% of the region
//! size and does not grow during the AR-T5 sequence. f.1.
//!
//! Resident memory is read from `/proc/self/statm`, which is the Linux path the document
//! names; the four CI jobs run there, so the assertion is exercised on every push. On a
//! host without `/proc` the test still runs the same sequence and holds the arena to what
//! it can observe there: the region does not change size and the accounting returns to
//! zero, which is the other half of "the footprint is flat".

mod common;

use amoru_kernel::{Allocator, Buffer, Tier};

/// Resident bytes of this process, or `None` where `/proc/self/statm` does not exist.
fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: test-only; `sysconf` takes no pointers.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    Some(pages * page)
}

#[test]
fn ar_t11_flat_footprint() {
    let region = 256u64 << 20;
    let before = resident_bytes();
    let arena = common::host_arena(region);
    let after_new = resident_bytes();
    assert_eq!(arena.region_bytes(Tier::Host), region);

    if let (Some(before), Some(after_new)) = (before, after_new) {
        let charged = after_new.saturating_sub(before);
        let slack = region / 50;
        assert!(
            charged.abs_diff(region) <= slack,
            "after Arena::new the process is resident by {charged} bytes for a {region} byte \
             region, more than 2% off (f.1)"
        );
    } else {
        println!(
            "AR-T11: /proc/self/statm is the Linux path this document names and this host has \
             none; the resident-size half runs in the CI Linux jobs"
        );
    }

    // The AR-T5 sequence: nothing here may grow the footprint, because every byte it uses
    // was committed by `new`.
    let mut rng = common::Rng::new(0xF1A7);
    let mut live: Vec<Buffer> = Vec::new();
    for _ in 0..5_000u32 {
        if live.is_empty() || !rng.next().is_multiple_of(3) {
            if let Ok(b) = arena.alloc(rng.upto(512 << 10) as usize, Tier::Host) {
                live.push(b);
            }
        } else {
            let i = (rng.next() % live.len() as u64) as usize;
            live.swap_remove(i);
        }
    }
    let during = resident_bytes();
    if let (Some(after_new), Some(during)) = (after_new, during) {
        let slack = region / 50;
        assert!(
            during <= after_new + slack,
            "resident memory grew from {after_new} to {during} during the run (f.1)"
        );
    }
    drop(live);
    assert_eq!(arena.stats().host_in_use, 0);
    assert_eq!(
        arena.region_bytes(Tier::Host),
        region,
        "the region never grows"
    );
}
