//! std's allocator (`System`, std's `sys::alloc`) over the process's heap: the same heap a
//! program without std has (`heap`), whose memory is charged to the process's account. Under
//! std the heap is not the global allocator itself; std's `System` is, and calls these.

use core::alloc::{GlobalAlloc, Layout};

use crate::heap::HEAP;

/// A block for `layout`, or null.
///
/// # Safety
///
/// `GlobalAlloc::alloc`'s contract: `layout` has a non-zero size.
pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    // SAFETY: the caller's contract.
    unsafe { HEAP.alloc(layout) }
}

/// A zeroed block for `layout`, or null. A block may be one given back before.
///
/// # Safety
///
/// As for [`alloc`].
pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    // SAFETY: the caller's contract.
    unsafe { HEAP.alloc_zeroed(layout) }
}

/// Gives a block back.
///
/// # Safety
///
/// `GlobalAlloc::dealloc`'s contract: `ptr` came from [`alloc`] with `layout`.
pub unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
    // SAFETY: the caller's contract.
    unsafe { HEAP.dealloc(ptr, layout) }
}

/// The block at `ptr` grown or shrunk to `new_size`: a new block, the bytes copied.
///
/// # Safety
///
/// `GlobalAlloc::realloc`'s contract.
pub unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    // SAFETY: the caller's contract.
    unsafe { HEAP.realloc(ptr, layout, new_size) }
}
