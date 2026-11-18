//! A count of the bytes a piece of the run holds, measured, never estimated (system design 3A).
//!
//! One counter, [`Held`], two ways of moving it, one for each allocator the run has:
//!
//! - **The arena:** [`crate::Tally`] wraps an [`crate::Allocator`] and moves its `Held` when a
//!   buffer it handed out is made and released: what one read in flight has taken (6.3c).
//! - **The heap:** a thread enters a `Held` ([`Held::enter_heap`]) and, until the guard drops,
//!   every Rust heap allocation and free it makes moves that `Held`, through the process's global
//!   allocator ([`heap_counted`]): what a Parquet sink's writer holds (6.3d).
//!
//! Why two feeds into one counter: each allocator sees only its own bytes. The arena's
//! allocator hands out the buffers reads land in, from memory the global allocator never sees;
//! the global allocator hands out the `Vec`s and maps a library such as `parquet` grows inside
//! its calls, which no arena wrapper sees. A piece of the run holds bytes of both kinds, so each
//! allocator moves the same kind of count where it sees them, and a reader asks one `Held` what
//! is held, whichever allocator holds it. One counter type, one way to read it, two feeds
//! because there are two allocators.
//!
//! The heap side works only where the program's global allocator calls [`heap_counted`]: the
//! engine binary and the Python extension install `moruna_runtime`'s counting allocator, which
//! does. [`heap_counting`] says whether one has; where none has (a library that embeds the engine
//! with its own allocator, most tests), a `Held` entered on the heap stays at nothing, and a
//! component that needs the figure falls back to an estimate and says so.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// The counter behind a [`Held`]: what is held now, and the most since the last reset.
#[derive(Debug, Default)]
struct Counter {
    now: AtomicI64,
    peak: AtomicI64,
}

impl Counter {
    fn moved(&self, delta: i64) {
        let now = self.now.fetch_add(delta, Ordering::AcqRel) + delta;
        if delta > 0 {
            self.peak.fetch_max(now, Ordering::AcqRel);
        }
    }
}

/// A count of bytes held, moved by an allocator wrapper ([`crate::Tally`]) or by the heap a
/// thread allocates while it has entered it ([`Held::enter_heap`]). Clones count into the same
/// figure.
#[derive(Clone, Debug, Default)]
pub struct Held {
    counter: Arc<Counter>,
}

impl Held {
    /// A count of nothing yet.
    pub fn new() -> Held {
        Held::default()
    }

    /// The bytes held now: what was counted in less what was counted out, never below
    /// nothing (a free of what was allocated before the count began counts out).
    pub fn held(&self) -> u64 {
        self.counter.now.load(Ordering::Acquire).max(0) as u64
    }

    /// The most held since the count began or [`Held::reset_peak`].
    pub fn peak(&self) -> u64 {
        self.counter.peak.load(Ordering::Acquire).max(0) as u64
    }

    /// Start the peak again from what is held now.
    pub fn reset_peak(&self) {
        let now = self.counter.now.load(Ordering::Acquire);
        self.counter.peak.store(now, Ordering::Release);
    }

    /// Count `bytes` in.
    pub(crate) fn add(&self, bytes: u64) {
        self.counter.moved(bytes as i64);
    }

    /// Count `bytes` out.
    pub(crate) fn sub(&self, bytes: u64) {
        self.counter.moved(-(bytes as i64));
    }

    /// Count the calling thread's Rust heap into this until the guard drops: every allocation
    /// in, every free out, whatever was allocated where. Entering another inside it counts into
    /// that one until its guard drops, then into this again.
    pub fn enter_heap(&self) -> HeapEntered<'_> {
        let previous = ENTERED
            .try_with(|entered| entered.replace(Arc::as_ptr(&self.counter)))
            .unwrap_or(std::ptr::null());
        HeapEntered {
            previous,
            _held: PhantomData,
            _thread: PhantomData,
        }
    }
}

/// While it lives, the calling thread's heap is counted into the [`Held`] it was made from.
/// It stays on its thread.
#[must_use = "the heap is counted only while the guard lives"]
pub struct HeapEntered<'a> {
    previous: *const Counter,
    _held: PhantomData<&'a Held>,
    _thread: PhantomData<*const ()>,
}

impl Drop for HeapEntered<'_> {
    fn drop(&mut self) {
        let _ = ENTERED.try_with(|entered| entered.set(self.previous));
    }
}

thread_local! {
    /// The counter the calling thread's heap is counted into, if any. A plain pointer in a cell
    /// with no destructor, so the allocator reads it without allocating.
    static ENTERED: Cell<*const Counter> = const { Cell::new(std::ptr::null()) };
}

/// Whether a global allocator has called [`heap_counted`].
static COUNTING: AtomicBool = AtomicBool::new(false);

/// What a counting global allocator calls for every allocation (`delta` > 0) and free
/// (`delta` < 0): the bytes move the [`Held`] the calling thread has entered, if any. It does
/// not allocate. On a thread that has entered none, which is every allocation but a Parquet
/// writer's, it costs a relaxed load of a flag stored once, one thread-local read and a null
/// test: no atomic write and no shared cache line written.
pub fn heap_counted(delta: i64) {
    if !COUNTING.load(Ordering::Relaxed) {
        COUNTING.store(true, Ordering::Relaxed);
    }
    let _ = ENTERED.try_with(|entered| {
        let counter = entered.get();
        if !counter.is_null() {
            // SAFETY: the pointer is set only by `Held::enter_heap`, from an `Arc<Counter>`
            // the guard borrows for as long as it lives, and the guard resets it when it drops,
            // on this thread, before the `Held` can go.
            unsafe { (*counter).moved(delta) };
        }
    });
}

/// Whether the program's global allocator counts the heap ([`heap_counted`]): true in the
/// engine binary and the Python extension, which install `moruna_runtime`'s counting allocator.
pub fn heap_counting() -> bool {
    COUNTING.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_moves_and_keeps_its_peak() {
        let held = Held::new();
        held.add(100);
        held.sub(40);
        assert_eq!((held.held(), held.peak()), (60, 100));
        held.reset_peak();
        assert_eq!(held.peak(), 60);
        held.sub(100);
        assert_eq!(held.held(), 0, "never below nothing");
        let clone = held.clone();
        clone.add(1_000);
        assert_eq!(held.held(), 960);
    }

    /// Without a counting allocator installed in this test binary, the heap is counted only
    /// by what calls `heap_counted` itself, into the `Held` the thread entered, and nesting
    /// counts into the inner one until it ends.
    #[test]
    fn the_heap_is_counted_into_what_the_thread_entered() {
        let outer = Held::new();
        let inner = Held::new();
        heap_counted(7);
        assert_eq!(outer.held(), 0, "nothing entered");
        {
            let _out = outer.enter_heap();
            heap_counted(100);
            {
                let _in = inner.enter_heap();
                heap_counted(30);
                heap_counted(-10);
            }
            heap_counted(-20);
        }
        heap_counted(5);
        assert_eq!((outer.held(), outer.peak()), (80, 100));
        assert_eq!((inner.held(), inner.peak()), (20, 30));
        assert!(heap_counting());
        // Another thread's heap is never this thread's count.
        let _out = outer.enter_heap();
        std::thread::spawn(|| heap_counted(1_000_000))
            .join()
            .expect("a thread");
        assert_eq!(outer.held(), 80);
    }
}
