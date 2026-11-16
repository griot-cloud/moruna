// SPDX-License-Identifier: MPL-2.0

//! Where the program's memory ends.
//!
//! The personality's region sits above the program's (`kernel-is-the-machine.md`
//! §4.1 a); a pointer from a program is accepted only below `MAX_USERSPACE_VADDR`, so a
//! program can never make the personality copy to or from the personality's own region.
//! The region's base is stream B1's to fix with `personality_set`; until then this is
//! OSTD's bound, Linux's 47-bit user half less its last page.

use core::ops::Range;

use super::Vaddr;

/// The end of the program's memory.
pub const MAX_USERSPACE_VADDR: Vaddr = 0x0000_8000_0000_0000 - super::PAGE_SIZE;

/// The personality's own addresses, as the carried code names "kernel space".
pub const KERNEL_VADDR_RANGE: Range<Vaddr> = 0..usize::MAX;

pub(crate) const KERNEL_BASE_VADDR: Vaddr = KERNEL_VADDR_RANGE.start;
pub(crate) const KERNEL_END_VADDR: Vaddr = KERNEL_VADDR_RANGE.end;
