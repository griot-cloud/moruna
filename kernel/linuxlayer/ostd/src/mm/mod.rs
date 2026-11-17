// SPDX-License-Identifier: MPL-2.0

//! Memory, from a user process.
//!
//! The personality's own memory is "kernel space" to the carried code, and the program's
//! lower region is "user space" (`kernel-is-the-machine.md` §4.1 a). Readers and writers
//! over the program's memory are fallible: they go through the fault-tolerant copy.

pub mod dma;
pub mod fault;
pub mod frame;
pub mod heap;
pub mod io;
pub(crate) mod kspace;
mod mem_obj;
pub mod page_prop;
pub mod tlb;
pub mod vm_space;

pub use self::{
    frame::{
        AnyFrameMeta, AnyUFrameMeta, Frame, FrameAllocOptions, FrameRef, Segment, UFrame, USegment,
    },
    io::{
        Fallible, FallibleVmRead, FallibleVmWrite, Infallible, PodAtomic, PodOnce, VmIo, VmIoFill,
        VmIoOnce, VmReader, VmWriter,
    },
    kspace::{KERNEL_VADDR_RANGE, MAX_USERSPACE_VADDR},
    mem_obj::{HasDaddr, HasPaddr, HasPaddrRange, HasSize, Split},
    page_prop::{CachePolicy, PageFlags, PageProperty},
    vm_space::VmSpace,
};

/// A virtual address.
pub type Vaddr = usize;
/// A "physical" address: in user space, an address in the personality's own memory.
pub type Paddr = usize;
/// A device address.
pub type Daddr = usize;

/// The page size.
pub const PAGE_SIZE: usize = 4096;

/// Whether `vaddr..vaddr + len` lies in the program's memory.
pub fn is_in_user_space(vaddr: Vaddr, len: usize) -> bool {
    vaddr.saturating_add(len) <= MAX_USERSPACE_VADDR
}
