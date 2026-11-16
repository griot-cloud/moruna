// SPDX-License-Identifier: MPL-2.0

//! `/sys`, cut to what a task's programs read (as `/proc` is, `procfs/mod.rs`).
//!
//! Asterinas's sysfs shows its whole system tree (devices, buses, classes, the
//! filesystem registry). A task needs four files of it, so this sysfs is a fixed,
//! read-only tree of those files and nothing else:
//!
//! - `devices/system/cpu/{online,possible,present}`: glibc's `get_nprocs` and
//!   `get_nprocs_conf` (CPython's `os.cpu_count`, OpenBLAS in NumPy, Arrow's thread
//!   pools), and the engine's discovery;
//! - `kernel/mm/transparent_hugepage/enabled`: jemalloc in pyarrow and mimalloc in
//!   CPython, which tune their use of huge pages by it.
//!
//! The system tree itself stays (the filesystem registry and the device model add
//! to it), but nothing mounts it.

mod tree;

pub(crate) use aster_systree::primary_tree as systree_singleton;
pub(crate) use tree::SysFs;

use crate::fs::vfs::registry;

pub(crate) fn init() {
    registry::register(&tree::SysFsType).unwrap();
}

#[cfg(ktest)]
mod test;
