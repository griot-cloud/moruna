// SPDX-License-Identifier: MPL-2.0

//! How the Linux personality is launched into a program's process
//! (`kernel-is-the-machine.md` §4.1 a, `architecture/data-os.md` §4).
//!
//! The personality's image is linked inside its region, above where the core places its own
//! mappings, because a Linux program links where every Moruna program image does. A launcher
//! creates the process, maps the image's segments at their addresses, maps the first
//! thread's stack, grants the personality its handles, gives the process its region with
//! the copy routines the image names, and launches it. This module holds what both sides
//! agree on, and the launcher (feature `launcher`).

#![no_std]

extern crate alloc;

#[cfg(all(feature = "launcher", target_os = "none"))]
pub mod launcher;

use alloc::vec::Vec;

use moruna_loader::elf::{self, Segment};

/// The page size.
pub const PAGE: u64 = moruna_abi::PAGE_SIZE;

/// The personality's region: from the core's map base (the end of `IMAGE_RANGE`, where the
/// core places what it maps itself) for `REGION_LEN` bytes. The program lives below.
pub const REGION_BASE: u64 = moruna_abi::IMAGE_RANGE.end;
/// The region's length.
pub const REGION_LEN: u64 = 0x6000_0000_0000;

/// Where the personality's image is linked: inside the region, far above the core's own
/// mappings, which it places upwards from `REGION_BASE`.
pub const IMAGE_BASE: u64 = 0x5000_0000_0000;

/// The task's heap: addresses the personality's heap takes, mapped in every process of
/// its task (`ostd::backend::start_task_memory`), above the core's own mappings.
pub const HEAP: core::ops::Range<u64> = 0x2000_0000_0000..0x3000_0000_0000;

/// The task's shared memory: addresses the personality picks for objects it maps in every
/// process of its task (`ostd::backend::shared_map`), between the heap and the image.
pub const SHARED: core::ops::Range<u64> = HEAP.end..IMAGE_BASE;

/// A personality thread's stack: its size and alignment. The thread's block sits at its
/// base (`ostd::backend`).
pub const STACK_SIZE: u64 = 256 * 1024;

/// The table at the image's base, three `u64`s the linker fills: the copy routines' start
/// and end, and their recovery point (`ostd::backend::user_copy_range`).
pub mod info {
    /// The copy routines' first byte.
    pub const COPY_START: usize = 0;
    /// One past their last byte.
    pub const COPY_END: usize = 8;
    /// Where a fault in them resumes.
    pub const COPY_RECOVERY: usize = 16;
    /// The table's length.
    pub const LEN: usize = 24;
}

/// What the first thread's stack pointer points at: the start page's address, then zero.
pub const STACK_ARGS: u64 = 16;

/// The slots the personality reads from its start page.
pub mod slot {
    /// The process's own handle, with `READ` and `WRITE`; the launcher adds it.
    pub const SELF: &str = "self";
    /// The core's console: the personality's log, and a task's text with no log ring.
    pub const CONSOLE: &str = "console";
    /// A booted test's initramfs, a memory object.
    pub const INITRAMFS: &str = "initramfs";
    /// The command line, a memory object: `init=PROGRAM`, then `--` and its arguments.
    pub const CMDLINE: &str = "cmdline";
    /// A task's own connection to the image filesystem (`moruna.imagefs` 3.0), its hello
    /// not yet said: the task's root.
    pub const FILESYSTEM: &str = "filesystem";
    /// A task's scratch account: its limit is what `/tmp` holds.
    pub const SCRATCH: &str = "scratch";
    /// A task's input slot, to wait on: attached as the plane's slot 0.
    pub const INPUT: &str = "input";
    /// A task's output slot, to wait on: attached after the input.
    pub const OUTPUT: &str = "output";
    /// The engine's log ring, a memory object.
    pub const LOG: &str = "log";
    /// A process resource with `WRITE`: what `fork` clones the task's processes from.
    pub const PROCESSES: &str = "processes";
}

/// Why an image cannot be launched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The ELF is not one the loader takes, or a segment lies outside the region.
    Elf(elf::ElfError),
    /// The image has no info table at `IMAGE_BASE`.
    NoInfo,
    /// The copy routines or their recovery point lie outside the image.
    BadInfo,
    /// The image reaches past where the first stack goes.
    TooLarge,
}

/// Where the launcher puts each part of the personality.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The first thread's entry.
    pub entry: u64,
    /// The image's segments.
    pub segments: Vec<Segment>,
    /// The copy routines' range.
    pub copy: core::ops::Range<u64>,
    /// Where a fault in them resumes.
    pub recovery: u64,
    /// The first thread's stack: its base, aligned to `STACK_SIZE`, and `STACK_SIZE` long.
    pub stack: u64,
}

impl Plan {
    /// Plans the launch of the personality image `bytes`.
    pub fn new(bytes: &[u8]) -> Result<Plan, PlanError> {
        let image =
            elf::parse(bytes, REGION_BASE..REGION_BASE + REGION_LEN).map_err(PlanError::Elf)?;
        let table = image
            .segments
            .iter()
            .find(|s| s.vaddr <= IMAGE_BASE && IMAGE_BASE + info::LEN as u64 <= s.vaddr + s.filesz)
            .ok_or(PlanError::NoInfo)?;
        let at = (table.offset + (IMAGE_BASE - table.vaddr)) as usize;
        let word = |i: usize| {
            let b = &bytes[at + i..at + i + 8];
            u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        };
        let (start, end, recovery) = (
            word(info::COPY_START),
            word(info::COPY_END),
            word(info::COPY_RECOVERY),
        );
        let in_code = |a: u64, b: u64| {
            image
                .segments
                .iter()
                .any(|s| s.execute && s.vaddr <= a && b <= s.vaddr + s.memsz)
        };
        if start >= end || !in_code(start, end) || !in_code(recovery, recovery + 1) {
            return Err(PlanError::BadInfo);
        }
        let image_end = image
            .segments
            .iter()
            .map(Segment::page_end)
            .max()
            .unwrap_or(IMAGE_BASE);
        // The stack: the next `STACK_SIZE` boundary past the image and a guard of one stack.
        let stack = image_end.div_ceil(STACK_SIZE) * STACK_SIZE + STACK_SIZE;
        if stack + STACK_SIZE > REGION_BASE + REGION_LEN {
            return Err(PlanError::TooLarge);
        }
        Ok(Plan {
            entry: image.entry,
            segments: image.segments,
            copy: start..end,
            recovery,
            stack,
        })
    }

    /// The first thread's stack pointer: the arguments' address, at the stack's top.
    pub fn stack_pointer(&self) -> u64 {
        self.stack + STACK_SIZE - STACK_ARGS
    }
}

/// The map flags a segment's permissions ask for.
pub fn segment_flags(s: &Segment) -> u64 {
    use moruna_abi::map;
    let mut flags = 0;
    if s.read {
        flags |= map::READ;
    }
    if s.write {
        flags |= map::WRITE;
    }
    if s.execute {
        flags |= map::EXECUTE;
    }
    flags
}

#[cfg(test)]
mod test;
