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
}

struct Heap {
    locked: AtomicBool,
    state: UnsafeCell<State>,
}

// SAFETY: `state` is only reached between a successful acquire of `locked` and its
// release (`Heap::with`), so one thread at a time touches it.
unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap {
    locked: AtomicBool::new(false),
    state: UnsafeCell::new(State {
        free: [0; CLASSES.len()],
        next: 0,
        end: 0,
    }),
};

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
        let Some(chunk) = map_pages(CHUNK) else {
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
            Some(class) => self.with(|state| take_small(state, class)),
            None if layout.align() <= PAGE => {
                map_pages(layout.size().next_multiple_of(PAGE)).map_or(null_mut(), |a| a as *mut u8)
            }
            None => null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        match class_of(layout) {
            Some(class) => self.with(|state| give_small(state, class, ptr)),
            None => {
                raw::memory_unmap(ptr as u64, layout.size().next_multiple_of(PAGE) as u64);
            }
        }
    }
}
