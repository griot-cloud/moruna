//! RE-T23 (item 3.5b): an object read lands in the arena buffer it was charged to, and holds
//! no more than a piece of its range outside the arena, whatever the range's size.
//!
//! The defect: `read_object` takes the whole range from the store as one body on the heap
//! (`ObjectBackend::get_range`, then `runtime::read_object_body` copies it into `dst`), so for
//! the length of the read the range is held twice: once in the arena, where the drive charged it,
//! and once outside it, where nothing charged it. The range is the source's, a Parquet row
//! group's whole projected span, so what a read takes above the arena grows with the file's row
//! groups and never with the budget. It spends the room the arena sizing leaves above the arena
//! for a copy's decode (3A: a read's bytes held outside the arena never exceed its own bytes in
//! flight). Found by 3.5's lock turn: the NYC 2024-01 month copied store to store peaked at 0.96
//! of a 342 MB job limit on Blob, and one S3 run was killed at it.
//!
//! The test reads ranges of 4, 16 and 64 MiB of a `file://` object through the real store
//! backend, each into an arena that holds exactly that range (the smallest budget the read
//! fits), and counts the heap with a global allocator. The arena buffer is taken before the
//! count starts, so what the count sees is what the read holds outside the arena. The bound is
//! one mebibyte for every range: a piece of the body on its way into the arena.
//!
//! This binary holds one test: its counters are the process's, so a second test running beside
//! it would be counted too.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moruna_kernel::Reactor as _;
use moruna_kernel::{Allocator, Tier};
use moruna_reactor::{ObjectStoreConfig, Reactor, ReactorConfig};
use moruna_testkit::FakeAllocator;

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

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's layout is passed through unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
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
                let grown = new_size - layout.size();
                let live = LIVE.fetch_add(grown, Ordering::SeqCst) + grown;
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

const MIB: usize = 1 << 20;
/// The ranges read: each larger than the last, so a heap that grows with the range shows.
const RANGES: [usize; 3] = [4 * MIB, 16 * MIB, 64 * MIB];
/// What a read may hold outside the arena at once: a piece of the body on its way in.
const OUTSIDE_BOUND: usize = MIB;

/// The byte at `offset` of the object: a pattern a misplaced piece would break.
fn byte_at(offset: usize) -> u8 {
    (offset.wrapping_mul(31) ^ (offset >> 12)) as u8
}

#[test]
fn re_t23_an_object_read_holds_a_piece_of_its_range_outside_the_arena() {
    let dir = std::env::temp_dir().join(format!("re-t23-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let largest = *RANGES.iter().max().expect("ranges");
    {
        let mut file = std::fs::File::create(dir.join("object.bin")).expect("the object");
        let mut piece = vec![0u8; MIB];
        for start in (0..largest + MIB).step_by(MIB) {
            for (i, b) in piece.iter_mut().enumerate() {
                *b = byte_at(start + i);
            }
            file.write_all(&piece).expect("the object's bytes");
        }
    }

    let mut held = Vec::new();
    for &len in &RANGES {
        // The arena holds exactly this read: the smallest budget it fits.
        let alloc = FakeAllocator::new().with_limit(Tier::Host, len as u64);
        let reactor = Reactor::new(
            ReactorConfig {
                object_store: ObjectStoreConfig {
                    local_root: Some(dir.clone()),
                    ..ObjectStoreConfig::default()
                },
                ..ReactorConfig::default()
            },
            Arc::new(alloc.clone()),
        )
        .expect("the reactor builds");
        // An offset off the page, as a column chunk's is.
        let offset = MIB / 2 + 7;
        let dst = alloc
            .alloc(len, Tier::Host)
            .expect("the arena has the range");
        let before = LIVE.load(Ordering::SeqCst);
        PEAK.store(before, Ordering::SeqCst);
        let read = reactor
            .read_object("file:///object.bin", offset as u64, dst)
            .wait()
            .expect("the range is read");
        let outside = PEAK.load(Ordering::SeqCst).saturating_sub(before);
        let bytes = read.as_ref();
        for at in [0, len / 3, len - 1] {
            assert_eq!(
                bytes[at],
                byte_at(offset + at),
                "byte {at} of the {len}-byte range"
            );
        }
        drop(read);
        reactor.shutdown();
        held.push((len, outside));
    }
    std::fs::remove_dir_all(&dir).ok();

    for (len, outside) in &held {
        println!("RE-T23: a read of {len} bytes held {outside} bytes outside the arena");
    }
    for (len, outside) in held {
        assert!(
            outside <= OUTSIDE_BOUND,
            "a read of {len} bytes held {outside} bytes outside the arena, beside the {len} \
             charged to it in the arena: more than the {OUTSIDE_BOUND} a piece of a body may \
             take, so a range's size, which is the file's, sets what a read takes above the arena"
        );
    }
}
