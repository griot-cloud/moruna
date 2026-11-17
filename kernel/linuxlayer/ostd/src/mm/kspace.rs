// SPDX-License-Identifier: MPL-2.0

//! Where the program's memory ends.
//!
//! The personality's region sits above the program's (`kernel-is-the-machine.md`
//! §4.1 a): `personality_set` gives it `[MAP_BASE, ...)`, with `MAP_BASE` the end of
//! `moruna_abi::IMAGE_RANGE`, and the program lives below. A pointer from a program is
//! accepted only below `MAX_USERSPACE_VADDR`, so a program can never make the personality
//! copy to or from the personality's own region. Host tests keep OSTD's bound, since a
//! test's "program memory" is the host's heap.

use core::ops::Range;

use super::Vaddr;

/// The end of the program's memory: the base of the personality's region.
#[cfg(target_os = "none")]
pub const MAX_USERSPACE_VADDR: Vaddr = moruna_abi::IMAGE_RANGE.end as Vaddr;

/// The end of the program's memory: OSTD's bound, Linux's 47-bit user half less its last
/// page.
#[cfg(not(target_os = "none"))]
pub const MAX_USERSPACE_VADDR: Vaddr = 0x0000_8000_0000_0000 - super::PAGE_SIZE;

/// The personality's own addresses, as the carried code names "kernel space".
pub const KERNEL_VADDR_RANGE: Range<Vaddr> = 0..usize::MAX;
