// SPDX-License-Identifier: MPL-2.0

//! Temporary file system (tmpfs) based on ramfs.

pub(crate) use fs::TmpFs;
use fs::TmpFsType;

mod fs;

pub(super) fn init() {
    crate::fs::vfs::registry::register(&TmpFsType).unwrap();
}

#[cfg(ktest)]
mod test;
