// SPDX-License-Identifier: MPL-2.0

//! Concrete file system implementations.
//!
//! This module contains all the specific file system implementations supported by the kernel.

pub(crate) mod cgroupfs;
pub mod devtmpfs;
// Moruna: no program mounts (data-os.md section 3a), and the root is the image's layers
// through `imagefs`, so these block filesystems are not registered. Their code stays until
// the page cache's write-back, which only they drive, is cut from `vm/` (stream B2-vm's).
#[expect(dead_code, reason = "no block filesystem is registered")]
pub(crate) mod exfat;
#[expect(dead_code, reason = "no block filesystem is registered")]
pub(crate) mod ext2;
pub(crate) mod imagefs;
pub(crate) mod procfs;
pub(crate) mod pseudofs;
pub(crate) mod ramfs;
pub(crate) mod sysfs;
pub(crate) mod tmpfs;

pub(super) fn init() {
    sysfs::init();
    procfs::init();
    cgroupfs::init();
    ramfs::init();
    tmpfs::init();
    devtmpfs::init();
    pseudofs::init();
}

pub(super) fn init_on_each_cpu() {
    procfs::init_on_each_cpu();
}

pub(super) fn init_in_first_kthread() {
    devtmpfs::init_in_first_kthread();
}
