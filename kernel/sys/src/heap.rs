//! The global allocator of a program on the Moruna microkernel (feature `heap`, SDD 23
//! f.6): memory objects mapped into the process, so a program built on `moruna-sys`
//! (and so every server written with `moruna-svc`) has `alloc`.
//!
//! Small blocks (up to 2 KiB) come from eight power-of-two size classes, each a free
//! list threaded through its free blocks, carved from 64 KiB chunks that are never
//! returned. A larger block is its own memory object, mapped on allocation and
//! unmapped on free, so a server that handles one large message gives its pages back.
//! Alignment up to a page is honoured; a larger alignment is refused (a null pointer,
//! which `alloc` reports as an allocation failure).
//!
//! A program may give the heap another source of memory, once, before its first
//! allocation ([`set_source`], stream B2).

use core::{
    alloc::{GlobalAlloc, Layout},
    cell::UnsafeCell,
    ptr::null_mut,
    sync::atomic::{AtomicBool, Ordering},
};

use moruna_abi::{Error, PAGE_SIZE, RawHandle, map, nr};

use crate::raw;

/// The block sizes of the small classes; a class's blocks are aligned to its size.
const CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];
/// The memory the small classes are carved from, one chunk at a time.
const CHUNK: usize = 64 * 1024;
const PAGE: usize = PAGE_SIZE as usize;

struct State {
    /// The first free block of each class, zero when none; a free block's first eight
    /// bytes hold the address of the next.
    free: [usize; CLASSES.len()],
    /// The unused part of the current chunk.
    next: usize,
    end: usize,
    // stream B2 (data-os-streams.md): where the memory comes from, and whether that is
    // settled.
    source: Source,
    /// A program gave the source.
    given: bool,
    /// Something was allocated: the source stays.
    sealed: bool,
    // stream B2 end
}

// stream B2 (data-os-streams.md): the heap's source of memory. The Linux personality's
// heap must be at one address in every process of its task, so it maps its own
// (`ostd::backend::shared_map`); every other program keeps memory objects mapped into
// its process.

/// Where the heap's memory comes from.
#[derive(Clone, Copy)]
struct Source {
    /// Maps `len` fresh zeroed bytes (whole pages), readable and writable; their address.
    map: fn(usize) -> Option<usize>,
    /// Gives back `len` bytes at `addr` that `map` mapped.
    unmap: fn(usize, usize),
}

/// The core's: memory objects mapped into the process.
const CORE: Source = Source {
    map: map_pages,
    unmap: unmap_pages,
};

/// Why [`set_source`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// A source was given already.
    AlreadySet,
    /// The heap has allocated already, from its first source.
    AlreadyAllocated,
}

/// Gives the heap its memory from `map` (`len` fresh zeroed bytes, whole pages, readable
/// and writable; their address) and back to `unmap` (`addr`, `len`), in place of memory
/// objects mapped into the process. Once only, and only before anything is allocated: a
/// second call, or one after an allocation, changes nothing, so a block never goes back
/// through a source that did not map it.
pub fn set_source(
    map: fn(usize) -> Option<usize>,
    unmap: fn(usize, usize),
) -> Result<(), SourceError> {
    HEAP.set_source(Source { map, unmap })
}

fn unmap_pages(addr: usize, len: usize) {
    raw::memory_unmap(addr as u64, len as u64);
}
// stream B2 end

struct Heap {
    locked: AtomicBool,
    state: UnsafeCell<State>,
}

// SAFETY: `state` is only reached between a successful acquire of `locked` and its
// release (`Heap::with`), so one thread at a time touches it.
unsafe impl Sync for Heap {}

// The crate's own host tests make heaps of their own, whose sources make no kernel call.
#[cfg_attr(not(test), global_allocator)]
static HEAP: Heap = Heap::new(CORE);

fn class_of(layout: Layout) -> Option<usize> {
    let need = layout.size().max(layout.align());
    CLASSES.iter().position(|&size| size >= need)
}

/// Maps `len` fresh zeroed bytes (whole pages), readable and writable, and returns
/// their address. The memory object's handle is closed: the mapping keeps the pages.
fn map_pages(len: usize) -> Option<usize> {
    let handle = Error::from_raw(raw::call1(nr::MEMORY_CREATE, len as u64).0).ok()? as RawHandle;
    let mapped = Error::from_raw(raw::memory_map(
        handle,
        0,
        len as u64,
        map::READ | map::WRITE,
    ));
    raw::handle_close(handle);
    mapped.ok().map(|addr| addr as usize)
}

impl Heap {
    const fn new(source: Source) -> Heap {
        Heap {
            locked: AtomicBool::new(false),
            state: UnsafeCell::new(State {
                free: [0; CLASSES.len()],
                next: 0,
                end: 0,
                source,
                given: false,
                sealed: false,
            }),
        }
    }

    // stream B2 (data-os-streams.md)
    fn set_source(&self, source: Source) -> Result<(), SourceError> {
        self.with(|state| {
            if state.given {
                Err(SourceError::AlreadySet)
            } else if state.sealed {
                Err(SourceError::AlreadyAllocated)
            } else {
                state.source = source;
                state.given = true;
                Ok(())
            }
        })
    }

    /// The source, sealed: whatever is allocated now comes from it.
    fn sealed_source(&self) -> Source {
        self.with(|state| {
            state.sealed = true;
            state.source
        })
    }
    // stream B2 end

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: the lock above is held until the release below, so this is the only
        // reference to the state.
        let result = f(unsafe { &mut *self.state.get() });
        self.locked.store(false, Ordering::Release);
        result
    }
}

fn take_small(state: &mut State, class: usize) -> *mut u8 {
    let size = CLASSES[class];
    let head = state.free[class];
    if head != 0 {
        // SAFETY: `head` is a free block of this class, which `give_small` wrote the
        // next free block's address into; the heap owns it until it is handed out.
        state.free[class] = unsafe { (head as *const usize).read() };
        return head as *mut u8;
    }
    let mut at = state.next.next_multiple_of(size);
    if state.next == 0 || at + size > state.end {
        let Some(chunk) = (state.source.map)(CHUNK) else {
            return null_mut();
        };
        state.next = chunk;
        state.end = chunk + CHUNK;
        at = chunk;
    }
    state.next = at + size;
    at as *mut u8
}

fn give_small(state: &mut State, class: usize, ptr: *mut u8) {
    // SAFETY: `ptr` is a block of this class the caller no longer uses (the contract
    // of `dealloc`); it is at least 16 bytes and 16-byte aligned, so it holds the link.
    unsafe { (ptr as *mut usize).write(state.free[class]) };
    state.free[class] = ptr as usize;
}

// SAFETY: blocks are handed out once and taken back only through `dealloc` with the
// layout they were allocated with: a small block returns to its class's free list, a
// large one is unmapped, exactly the mapping `alloc` made for it.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match class_of(layout) {
            Some(class) => self.with(|state| {
                state.sealed = true;
                take_small(state, class)
            }),
            None if layout.align() <= PAGE => {
                (self.sealed_source().map)(layout.size().next_multiple_of(PAGE))
                    .map_or(null_mut(), |a| a as *mut u8)
            }
            None => null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        match class_of(layout) {
            Some(class) => self.with(|state| give_small(state, class, ptr)),
            None => {
                let source = self.with(|state| state.source);
                (source.unmap)(ptr as usize, layout.size().next_multiple_of(PAGE));
            }
        }
    }
}

// stream B2 (data-os-streams.md): the source's refusals and a given source, on heaps whose
// sources make no kernel call.
#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::AtomicUsize;
    use std::{alloc::Layout as StdLayout, sync::Mutex, vec::Vec};

    use super::*;

    /// What a test source mapped and unmapped: address and length.
    static MAPPED: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
    static UNMAPPED: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
    /// Maps from the other test source, which stands for the core's; only the refusal
    /// test uses it.
    static FIRST_MAPS: AtomicUsize = AtomicUsize::new(0);

    fn host_map(len: usize) -> Option<usize> {
        let layout = StdLayout::from_size_align(len, PAGE).ok()?;
        // SAFETY: the layout is not zero sized: every request is whole pages.
        let addr = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
        (addr != 0).then_some(addr)
    }

    fn host_unmap(addr: usize, len: usize) {
        let layout = StdLayout::from_size_align(len, PAGE).unwrap();
        // SAFETY: `addr` came from `host_map` with this length.
        unsafe { std::alloc::dealloc(addr as *mut u8, layout) };
    }

    fn given_map(len: usize) -> Option<usize> {
        let addr = host_map(len)?;
        MAPPED.lock().unwrap().push((addr, len));
        Some(addr)
    }

    fn given_unmap(addr: usize, len: usize) {
        UNMAPPED.lock().unwrap().push((addr, len));
        host_unmap(addr, len);
    }

    fn first_map(len: usize) -> Option<usize> {
        FIRST_MAPS.fetch_add(1, Ordering::Relaxed);
        host_map(len)
    }

    const FIRST: Source = Source {
        map: first_map,
        unmap: host_unmap,
    };

    #[test]
    fn a_given_source_maps_chunks_and_large_blocks_and_takes_them_back() {
        let heap = Heap::new(FIRST);
        let given = Source {
            map: given_map,
            unmap: given_unmap,
        };
        assert_eq!(heap.set_source(given), Ok(()));
        assert_eq!(heap.set_source(given), Err(SourceError::AlreadySet));
        let small = Layout::from_size_align(24, 8).unwrap();
        let large = Layout::from_size_align(3 * PAGE + 1, 8).unwrap();
        // SAFETY: the layouts are not zero sized, and each block goes back with its own.
        unsafe {
            let a = heap.alloc(small);
            let b = heap.alloc(small);
            assert!(!a.is_null() && b as usize == a as usize + 32);
            let big = heap.alloc(large);
            assert!(!big.is_null());
            assert_eq!(
                *MAPPED.lock().unwrap(),
                [(a as usize, CHUNK), (big as usize, 4 * PAGE)]
            );
            heap.dealloc(big, large);
            assert_eq!(*UNMAPPED.lock().unwrap(), [(big as usize, 4 * PAGE)]);
            heap.dealloc(b, small);
            assert_eq!(heap.alloc(small), b);
            assert!(
                heap.alloc(Layout::from_size_align(8, 2 * PAGE).unwrap())
                    .is_null()
            );
        }
    }

    #[test]
    fn a_source_after_an_allocation_is_refused_and_changes_nothing() {
        let heap = Heap::new(FIRST);
        let before = FIRST_MAPS.load(Ordering::Relaxed);
        let large = Layout::from_size_align(2 * PAGE, 8).unwrap();
        // SAFETY: as above.
        let block = unsafe { heap.alloc(large) };
        assert!(!block.is_null());
        let other = Source {
            map: |_| None,
            unmap: |_, _| panic!("a block went back through a source that did not map it"),
        };
        assert_eq!(heap.set_source(other), Err(SourceError::AlreadyAllocated));
        // SAFETY: as above.
        unsafe {
            heap.dealloc(block, large);
            assert!(
                !heap
                    .alloc(Layout::from_size_align(16, 16).unwrap())
                    .is_null()
            );
        }
        assert_eq!(FIRST_MAPS.load(Ordering::Relaxed), before + 2);
        // A small block seals the source as well.
        let fresh = Heap::new(FIRST);
        // SAFETY: as above.
        assert!(!unsafe { fresh.alloc(Layout::from_size_align(16, 16).unwrap()) }.is_null());
        assert_eq!(fresh.set_source(other), Err(SourceError::AlreadyAllocated));
    }
}
// stream B2 end
