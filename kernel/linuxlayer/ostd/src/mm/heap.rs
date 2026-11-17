// SPDX-License-Identifier: MPL-2.0

//! The heap: `moruna-sys`'s allocator, over the core's memory objects. OSDK's slab
//! allocator is not used, so its slot information is never asked for.

/// The slot a slab allocator would serve a layout from.
#[derive(Clone, Copy, Debug)]
pub enum SlotInfo {
    /// A slab slot of this size.
    SlabSlot(usize),
    /// A large slot of this size.
    LargeSlot(usize),
}
