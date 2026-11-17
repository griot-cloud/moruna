//! moruna: std's allocator over the process's heap (`moruna_sys::rt::memory`).

pub use moruna_sys::rt::memory::{alloc, alloc_zeroed, dealloc, realloc};
