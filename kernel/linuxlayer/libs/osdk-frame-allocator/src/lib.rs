// SPDX-License-Identifier: MPL-2.0

//! OSDK's frame allocator, for the layer in user space. The personality's frames come from
//! `ostd::mm::FrameAllocOptions`; what is free is its memory budget less what it holds.

#![no_std]

/// OSDK's buddy frame allocator: unused in user space.
pub struct FrameAllocator;

/// The bytes the personality may still allocate.
pub fn load_total_free_size() -> usize {
    ostd::mm::frame::free_bytes()
}
