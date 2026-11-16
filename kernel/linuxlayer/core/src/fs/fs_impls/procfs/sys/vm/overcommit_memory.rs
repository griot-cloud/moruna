// SPDX-License-Identifier: MPL-2.0

//! `/proc/sys/vm/overcommit_memory`, which the allocators read at their start:
//! jemalloc in pyarrow and mimalloc in free-threaded CPython and pyarrow. It says 0,
//! Linux's default, so they reserve address space as they do on Linux; the task's
//! account still bounds what it commits.

use aster_util::printer::VmPrinter;

use crate::{
    fs::{
        file::mkmod,
        procfs::template::{ProcFile, ProcFileOps},
        vfs::inode::Inode,
    },
    prelude::*,
};

/// The heuristic mode, Linux's default.
const OVERCOMMIT_GUESS: u32 = 0;

pub(super) struct OvercommitMemoryFileOps;

impl OvercommitMemoryFileOps {
    pub(super) fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        // Reference: <https://elixir.bootlin.com/linux/v6.16.5/source/mm/util.c#L800>
        ProcFile::new(Self, parent, mkmod!(a+r, u+w))
    }
}

impl ProcFileOps for OvercommitMemoryFileOps {
    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        let mut printer = VmPrinter::new_skip(writer, offset);
        writeln!(printer, "{}", OVERCOMMIT_GUESS)?;
        Ok(printer.bytes_written())
    }

    fn write_at(&self, _offset: usize, _reader: &mut VmReader) -> Result<usize> {
        return_errno_with_message!(Errno::EPERM, "the overcommit mode is fixed")
    }
}
