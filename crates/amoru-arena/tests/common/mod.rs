//! Test-only helpers shared by the AR tests: a host arena at a given budget, the size-class
//! model of e.2, and a deterministic random number generator (no crate is added for it, as
//! the preamble's dependency table would have to name one).

#![allow(dead_code)]

use std::sync::Arc;

use amoru_arena::{Arena, ArenaConfig};
use amoru_kernel::{Guarantee, TierKind};

/// Class 0 and the granularity of every offset (e.2).
pub const GRANULE: u64 = 64 * 1024;
/// Class 13, and the largest class (e.2).
pub const MAX_CLASS: u64 = 512 * 1024 * 1024;

/// A host arena of `host_bytes`, unpinned, with no huge pages: what every test that does
/// not exercise f.1's guarantee rules wants.
pub fn host_arena(host_bytes: u64) -> Arc<Arena> {
    Arena::new(ArenaConfig {
        host_bytes,
        host_tier: TierKind::Host,
        device_bytes: Vec::new(),
        page_bytes: 4096,
        huge_pages: Guarantee::Probed(false),
        memlock: Guarantee::Absent,
        register_rdma: false,
    })
    .expect("a host arena")
}

/// What a request of `bytes` costs against the budget (e.2): the size class, or the
/// granule-rounded size for a large allocation. This is the model the AR tests check the
/// arena's accounting against, written from the document rather than from the code.
pub fn charged(bytes: u64) -> u64 {
    if bytes == 0 {
        return 0;
    }
    if bytes > MAX_CLASS {
        return bytes.div_ceil(GRANULE) * GRANULE;
    }
    let mut size = GRANULE;
    while size < bytes {
        size *= 2;
    }
    size
}

/// Taken by every test that reserves a large region, so no two of them are resident at
/// once: `Arena::new` touches every page of its region (f.1), and the tests in one binary
/// otherwise run in parallel threads.
pub fn big_region_gate() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// xorshift64*, so a test's "random" sizes are the same on every host and in every run.
pub struct Rng(u64);

impl Rng {
    /// Seed it; any non-zero seed will do.
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    /// The next value.
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `1..=max`.
    pub fn upto(&mut self, max: u64) -> u64 {
        1 + self.next() % max
    }
}
