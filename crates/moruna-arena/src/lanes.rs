//! Per-thread lanes (02 f.3): what a buffer holds to find its way home, one per thread and
//! arena, so that eight threads allocating at once do not all write one reference count.
//!
//! Every `Buffer` carries an `Arc<dyn ArenaHandle>` (contracts d.3), and every allocation cloned
//! the arena's one `Arc<Inner>` for it, so every allocation and release wrote that count's word:
//! 4 ns a pair with one thread, 422 with eight. A buffer now clones its thread's [`Lane`], a word
//! no other thread writes unless it releases a buffer this thread allocated.
//!
//! A lane is kept by the arena's registry and found through a weak reference in the thread, so
//! nothing a thread keeps outlives the arena: dropping the `Arena` empties the registry, and the
//! lanes then live only as long as the buffers that hold them. A thread's lanes leave their
//! registries when the thread ends.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use moruna_kernel::{ArenaHandle, Tier};

use crate::Inner;

/// A thread's way into one arena: the handle its buffers hold.
pub(crate) struct Lane {
    inner: Arc<Inner>,
}

impl ArenaHandle for Lane {
    fn release(&self, ptr: *mut u8, len: usize, tier: Tier) {
        self.inner.release(ptr, len, tier);
    }
}

/// The lanes of one arena, each held for its thread.
#[derive(Default)]
pub(crate) struct Registry {
    lanes: Mutex<Vec<Arc<Lane>>>,
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Lane>>> {
        self.lanes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lanes registered now.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Lets every lane go: the arena is dropped. A lane lives on while a buffer holds it.
    pub(crate) fn clear(&self) {
        let lanes = std::mem::take(&mut *self.lock());
        drop(lanes);
    }

    fn remove(&self, lane: &Arc<Lane>) {
        self.lock().retain(|l| !Arc::ptr_eq(l, lane));
    }
}

/// An arena's identity, unlike its address never another's.
pub(crate) fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// This thread's lanes, by the arena's id. When the thread ends each leaves its registry.
#[derive(Default)]
struct ThreadLanes(Vec<(u64, Weak<Lane>)>);

impl Drop for ThreadLanes {
    fn drop(&mut self) {
        for (_, lane) in self.0.drain(..) {
            if let Some(lane) = lane.upgrade() {
                lane.inner.lanes.remove(&lane);
            }
        }
    }
}

thread_local! {
    static LANES: RefCell<ThreadLanes> = RefCell::default();
}

impl Inner {
    /// This thread's lane into the arena, made and registered the first time the thread asks.
    pub(crate) fn lane(self: &Arc<Self>) -> Arc<Lane> {
        let id = self.id;
        let found = LANES.with(|lanes| {
            lanes
                .borrow()
                .0
                .iter()
                .find(|(of, _)| *of == id)
                .and_then(|(_, lane)| lane.upgrade())
        });
        match found {
            Some(lane) => lane,
            None => self.new_lane(),
        }
    }

    fn new_lane(self: &Arc<Self>) -> Arc<Lane> {
        let lane = Arc::new(Lane {
            inner: Arc::clone(self),
        });
        self.lanes.lock().push(Arc::clone(&lane));
        let id = self.id;
        LANES.with(|lanes| {
            let mut lanes = lanes.borrow_mut();
            // A lane of an arena gone, or of this one gone before, goes first.
            lanes
                .0
                .retain(|(of, lane)| *of != id && lane.strong_count() > 0);
            lanes.0.push((id, Arc::downgrade(&lane)));
        });
        lane
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arena, ArenaConfig};
    use moruna_kernel::{Allocator, Guarantee};

    fn arena() -> Arc<Arena> {
        Arena::new(ArenaConfig {
            host_bytes: 4 << 20,
            page_bytes: 4096,
            huge_pages: Guarantee::Probed(false),
            memlock: Guarantee::Absent,
            ..ArenaConfig::default()
        })
        .expect("an arena")
    }

    #[test]
    fn a_thread_has_one_lane_an_arena_until_it_ends() {
        let a = arena();
        let other = arena();
        let first = a.alloc(64 << 10, Tier::Host).expect("room");
        let second = a.alloc(64 << 10, Tier::Host).expect("room");
        let elsewhere = other.alloc(64 << 10, Tier::Host).expect("room");
        assert_eq!(a.inner.lanes.len(), 1, "one lane for this thread");
        assert_eq!(other.inner.lanes.len(), 1);
        let held = {
            let a = Arc::clone(&a);
            std::thread::spawn(move || {
                let b = a.alloc(64 << 10, Tier::Host).expect("room");
                assert_eq!(a.inner.lanes.len(), 2, "a lane of its own");
                b
            })
            .join()
            .expect("a thread")
        };
        assert_eq!(
            a.inner.lanes.len(),
            1,
            "its lane left when the thread ended"
        );
        // A buffer of the thread that ended still goes home through its lane.
        drop(held);
        drop((first, second, elsewhere));
        assert_eq!(a.stats().host_in_use, 0);
    }

    #[test]
    fn the_arena_goes_with_its_last_buffer() {
        let a = arena();
        let inner = Arc::downgrade(&a.inner);
        let held = a.alloc(64 << 10, Tier::Host).expect("room");
        drop(a);
        assert!(inner.upgrade().is_some(), "a live buffer keeps the arena");
        drop(held);
        assert!(inner.upgrade().is_none(), "and nothing else does");
        // A thread that knew a dropped arena makes a new lane for the next one.
        let b = arena();
        let buffer = b.alloc(64 << 10, Tier::Host).expect("room");
        assert_eq!(b.inner.lanes.len(), 1);
        drop(buffer);
    }
}
