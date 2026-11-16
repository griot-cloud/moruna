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
pub mod program_page;

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
    /// What starts the name of every slot the personality takes for itself. Every other name
    /// on its start page is a handle granted to its program (`run_granted`, `run_plane`), which
    /// the program's own start page names; a program's names never start with it.
    pub const PREFIX: &str = "lx.";
    /// The process's own handle, with `READ` and `WRITE`; the launcher adds it.
    pub const SELF: &str = "lx.self";
    /// The core's console: the personality's log, and a task's text with no log ring.
    pub const CONSOLE: &str = "lx.console";
    /// A booted test's initramfs, a memory object.
    pub const INITRAMFS: &str = "lx.initramfs";
    /// The command line, a memory object: `init=PROGRAM`, then `--` and its arguments.
    pub const CMDLINE: &str = "lx.cmdline";
    /// A task's own connection to the image filesystem (`moruna.imagefs` 3.0), its hello
    /// not yet said: the task's root.
    pub const FILESYSTEM: &str = "lx.filesystem";
    /// A task's scratch account: its limit is what `/tmp` holds.
    pub const SCRATCH: &str = "lx.scratch";
    /// A task's input slot, to wait on: attached as the plane's slot 0.
    pub const INPUT: &str = "lx.input";
    /// A task's output slot, to wait on: attached after the input.
    pub const OUTPUT: &str = "lx.output";
    /// The engine's log ring, a memory object.
    pub const LOG: &str = "lx.log";
    /// A process resource with `WRITE`: what `fork` clones the task's processes from.
    pub const PROCESSES: &str = "lx.processes";
    /// The task's command, a memory object in [`command`]'s encoding: the program's path,
    /// its arguments and its environment, exactly as given.
    pub const COMMAND: &str = "lx.command";
    /// A memory object of [`report::LEN`] bytes, writable, where the personality says how
    /// the task ended ([`report`]).
    pub const REPORT: &str = "lx.report";
    /// Whether `name` names a handle granted to the program, not one of the personality's.
    pub fn is_program(name: &str) -> bool {
        !name.starts_with(PREFIX)
    }

    /// Whether `name` may name a handle granted to a program: not empty, at most
    /// `limits::NAME_MAX` bytes, and not the personality's.
    pub fn program_name_ok(name: &str) -> bool {
        !name.is_empty() && name.len() <= moruna_abi::limits::NAME_MAX && is_program(name)
    }
}

/// How a task ended, as the personality writes it for its launcher when its first program
/// ends: linux-protocol's `Ended` fields the launcher cannot see itself. Little endian:
/// `0` the magic (written last), `4` how, `8` the code, `16` the calls served, `24` the
/// number of refused calls listed, then from `32` each refused call's number and count.
pub mod report {
    use alloc::vec::Vec;

    /// The report's object's length.
    pub const LEN: usize = 4096;
    /// "MLR1": a whole report.
    pub const MAGIC: u32 = u32::from_le_bytes(*b"MLR1");
    /// The program exited: the code is its exit status.
    pub const EXITED: u8 = 1;
    /// A signal ended it: the code is the signal.
    pub const SIGNALED: u8 = 2;
    /// The program could not start: the code is the Linux error.
    pub const NOT_STARTED: u8 = 0x10;
    /// The most refused calls listed.
    pub const REFUSED_MAX: usize = (LEN - 32) / 16;

    /// What the personality reports.
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Report {
        pub how: u8,
        pub code: i64,
        pub served: u64,
        /// Each refused call's number and count, at most [`REFUSED_MAX`].
        pub refused: Vec<(u64, u64)>,
    }

    /// The report's bytes, [`LEN`] of them; refused calls past [`REFUSED_MAX`] left out.
    pub fn encode(report: &Report) -> Vec<u8> {
        let mut out = alloc::vec![0u8; LEN];
        out[4] = report.how;
        out[8..16].copy_from_slice(&report.code.to_le_bytes());
        out[16..24].copy_from_slice(&report.served.to_le_bytes());
        let listed = &report.refused[..report.refused.len().min(REFUSED_MAX)];
        out[24..28].copy_from_slice(&(listed.len() as u32).to_le_bytes());
        for (i, (number, count)) in listed.iter().enumerate() {
            let at = 32 + 16 * i;
            out[at..at + 8].copy_from_slice(&number.to_le_bytes());
            out[at + 8..at + 16].copy_from_slice(&count.to_le_bytes());
        }
        out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        out
    }

    /// The report in `bytes`; `None` when the personality wrote none.
    pub fn decode(bytes: &[u8]) -> Option<Report> {
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        if bytes.len() < 32 || bytes[0..4] != MAGIC.to_le_bytes() {
            return None;
        }
        let listed = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as usize;
        if listed > REFUSED_MAX || bytes.len() < 32 + 16 * listed {
            return None;
        }
        Some(Report {
            how: bytes[4],
            code: u64_at(8) as i64,
            served: u64_at(16),
            refused: (0..listed)
                .map(|i| (u64_at(32 + 16 * i), u64_at(40 + 16 * i)))
                .collect(),
        })
    }
}

/// A task's command as the launcher hands it over: each string ends with a zero byte, the
/// path first, then the arguments (`argv[0]` included), an empty string, then the
/// environment. Bytes after the environment's end are zero.
pub mod command {
    use alloc::vec::Vec;

    /// A program to run: its path, arguments and environment.
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Command {
        pub path: Vec<u8>,
        pub argv: Vec<Vec<u8>>,
        pub env: Vec<Vec<u8>>,
    }

    /// Why a command cannot be encoded: a string holds a zero byte, or an argument is
    /// empty (which would end the arguments).
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct BadCommand;

    /// The command's encoding.
    pub fn encode(command: &Command) -> Result<Vec<u8>, BadCommand> {
        let mut out = Vec::new();
        let mut put = |s: &[u8], may_be_empty: bool| {
            if s.contains(&0) || (s.is_empty() && !may_be_empty) {
                return Err(BadCommand);
            }
            out.extend_from_slice(s);
            out.push(0);
            Ok(())
        };
        put(&command.path, false)?;
        for arg in &command.argv {
            put(arg, false)?;
        }
        put(b"", true)?;
        for var in &command.env {
            put(var, false)?;
        }
        put(b"", true)?;
        Ok(out)
    }

    /// The command `bytes` encode; `None` for bytes that are not an encoding.
    pub fn decode(bytes: &[u8]) -> Option<Command> {
        let mut strings = bytes.split(|&b| b == 0);
        let path = strings.next().filter(|p| !p.is_empty())?.to_vec();
        let take = |strings: &mut core::slice::Split<'_, u8, _>| {
            let mut list = Vec::new();
            loop {
                match strings.next() {
                    None => return None,
                    Some([]) => return Some(list),
                    Some(s) => list.push(s.to_vec()),
                }
            }
        };
        let argv = take(&mut strings)?;
        let env = take(&mut strings)?;
        Some(Command { path, argv, env })
    }
}

/// Why an image cannot be launched.
#[derive(Clone, Copy, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
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
