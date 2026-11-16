// SPDX-License-Identifier: MPL-2.0

//! OSDK's heap allocator, for the layer in user space: the per-CPU boxes aster-util uses,
//! and the names the layer's `vm` module gives its allocator. The allocator itself is
//! `moruna-sys`'s.

#![no_std]

use core::alloc::Layout;

pub use ostd::{CpuLocalBox, alloc_cpu_local};

/// OSDK's slab heap allocator: unused in user space.
pub struct HeapAllocator;

/// The slab slot a layout would take: none, since no slab allocator runs.
pub const fn type_from_layout(_layout: Layout) -> Option<ostd::mm::heap::SlotInfo> {
    None
}

/// The memory held in slab caches: none.
pub fn load_total_slab_size() -> usize {
    0
}
