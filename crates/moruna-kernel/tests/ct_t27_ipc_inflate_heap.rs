//! CT-T27 ipc_bombs_cost_the_heap_little: the refusals of CT-T24 measured on the process's own
//! heap as well as the arena's, because a decoder's working memory is not the arena's. Every
//! bomb of CT-T24 is refused while the heap, counted by a global allocator, never rises by a
//! megabyte over what the stream itself holds.
//!
//! This binary holds one test: its counters are the process's, so a second test running beside
//! it would be counted too.

// The contract error is large by design (contracts d.14).
#![allow(clippy::result_large_err)]

mod bombs;
mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use arrow::buffer::Buffer;
use bombs::bombs;
use common::FakeAllocator;
use moruna_kernel::ipc::{Inflate, read_stream};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is the system allocator's own, with the layout it was given; the counters
// only add and subtract the sizes of the blocks the system allocator hands out and takes back.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's layout is passed through unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        // SAFETY: `ptr` came from `alloc` above with this layout.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr` came from `alloc` above with this layout.
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            if new_size >= layout.size() {
                let live = LIVE.fetch_add(new_size - layout.size(), Ordering::SeqCst)
                    + (new_size - layout.size());
                PEAK.fetch_max(live, Ordering::SeqCst);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::SeqCst);
            }
        }
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

#[test]
fn ct_t27_ipc_bombs_cost_the_heap_little() {
    let mut worst = (0usize, String::new());
    for bomb in bombs() {
        let alloc = FakeAllocator::new();
        let buffer = Buffer::from_vec(bomb.bytes.clone());
        let inflate = Inflate::new(&alloc, bomb.ceiling);
        let before = LIVE.load(Ordering::SeqCst);
        PEAK.store(before, Ordering::SeqCst);
        let refused = read_stream(&buffer, &inflate);
        let rise = PEAK.load(Ordering::SeqCst).saturating_sub(before);
        assert!(refused.is_err(), "{}", bomb.label);
        if rise > worst.0 {
            worst = (rise, bomb.label.clone());
        }
        assert!(
            rise < 1 << 20,
            "{}: the heap rose by {rise} bytes while the batch was refused",
            bomb.label
        );
    }
    println!(
        "CT-T27 worst heap rise during a refusal: {} bytes ({})",
        worst.0, worst.1
    );
}
