//! The lock order of preamble 4.2, checked at run time.
//!
//! The engine's locks are, in order: a queue (position 2 of the preamble's list), the lineage
//! index (2b), the roll guard and the moves map and the segments map (3). A lock is taken
//! only when every lock this thread already holds sits earlier in that order; anything else
//! is counted and, in a debug build, asserted. PL-T23 and PL-T15 read the counter.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// A queue (preamble 4.2 position 2).
pub const QUEUE: u32 = 1;
/// The lineage index (position 2b).
pub const LINEAGE: u32 = 2;
/// The guard that serialises opening a segment (position 3).
pub const ROLL: u32 = 3;
/// The moves map (position 3).
pub const MOVES: u32 = 4;
/// The segments map (position 3).
pub const SEGMENTS: u32 = 5;

static VIOLATIONS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static HELD: Cell<u32> = const { Cell::new(0) };
}

/// How many times a lock was taken out of order since the process started; zero is the
/// only acceptable value (preamble 4.2).
pub fn violations() -> u64 {
    VIOLATIONS.load(Ordering::Acquire)
}

/// A lock this thread holds, released when the guard drops.
pub struct Held {
    position: u32,
    previous: u32,
}

impl Held {
    /// Record that this thread is taking the lock at `position`.
    pub fn enter(position: u32) -> Held {
        let previous = HELD.with(|held| held.get());
        if previous >= position {
            VIOLATIONS.fetch_add(1, Ordering::AcqRel);
            debug_assert!(
                previous < position,
                "lock order (preamble 4.2): taking position {position} while holding {previous}"
            );
        }
        HELD.with(|held| held.set(position.max(previous)));
        Held { position, previous }
    }

    /// The position this guard records.
    pub fn position(&self) -> u32 {
        self.position
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        HELD.with(|held| held.set(self.previous));
    }
}

/// True when this thread holds no engine lock; what `checkpoint`'s manifest write and every
/// reactor call must be able to say (PL-I14, f.12).
pub fn none_held() -> bool {
    HELD.with(|held| held.get()) == 0
}
