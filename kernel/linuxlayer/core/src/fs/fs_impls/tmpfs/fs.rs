// SPDX-License-Identifier: MPL-2.0

use crate::{
    fs::{
        fs_impls::ramfs::{self, RamFs, quota::Quota},
        pseudofs::AnonDeviceId,
        vfs::{
            file_system::SuperBlock,
            inode::RevalidationPolicy,
            registry::{FsProperties, FsType},
        },
    },
    prelude::*,
};

const TMPFS_MAGIC: u64 = 0x0102_1994;

/// The temporary file system (tmpfs) structure.
//
// TODO: `TmpFs` currently aliases `RamFs` and relies on `RamFs::new_tmpfs()`
// to create tmpfs-flavored ramfs instances. In the future we need to
// implement a dedicated tmpfs with tmpfs-specific features such as memory
// limits and swap support.
pub(crate) type TmpFs = RamFs;

impl TmpFs {
    /// Creates a tmpfs filesystem instance.
    pub(in crate::fs) fn new_tmpfs() -> Arc<Self> {
        Self::new_tmpfs_backing("tmpfs", RevalidationPolicy::empty())
    }

    /// Creates a tmpfs-backed filesystem instance with a custom filesystem name
    /// and directory-entry revalidation policy.
    pub(in crate::fs) fn new_tmpfs_backing(
        name: &'static str,
        revalidation_policy: RevalidationPolicy,
    ) -> Arc<Self> {
        let anon_device_id = AnonDeviceId::acquire().expect("no device ID is available for tmpfs");
        let sb = {
            let mut super_block = SuperBlock::new(
                TMPFS_MAGIC,
                ramfs::BLOCK_SIZE,
                ramfs::NAME_MAX,
                anon_device_id.id(),
            );
            let max_blocks = default_max_blocks();
            let max_inodes = default_max_inodes();
            super_block.blocks = max_blocks;
            super_block.bfree = max_blocks;
            super_block.bavail = max_blocks;
            super_block.files = max_inodes;
            super_block.ffree = max_inodes;
            super_block
        };
        Self::new_with_sb(name, anon_device_id, sb, revalidation_policy)
    }

    /// Creates a task's scratch: a tmpfs holding at most `quota` bytes, the limit of
    /// the task's scratch account.
    pub(in crate::fs) fn new_scratch(quota: usize) -> Result<Arc<Self>> {
        let anon_device_id = AnonDeviceId::acquire()
            .ok_or_else(|| Error::with_message(Errno::ENOMEM, "no device ID for the scratch"))?;
        let mut sb = SuperBlock::new(
            TMPFS_MAGIC,
            ramfs::BLOCK_SIZE,
            ramfs::NAME_MAX,
            anon_device_id.id(),
        );
        let max_inodes = quota / ramfs::quota::ENTRY_COST;
        sb.files = max_inodes;
        sb.ffree = max_inodes;
        Ok(Self::new_with_quota(
            "tmpfs",
            anon_device_id,
            sb,
            Quota::new(quota),
        ))
    }
}

// FIXME: These defaults are only a rough approximation for tmpfs-over-ramfs.
// A dedicated tmpfs implementation should replace them with real tmpfs limit
// and accounting semantics.
fn default_max_blocks() -> usize {
    crate::vm::mem_total() / PAGE_SIZE / 2
}
fn default_max_inodes() -> usize {
    crate::vm::mem_total() / PAGE_SIZE / 2
}

pub(super) struct TmpFsType;

impl FsType for TmpFsType {
    fn name(&self) -> &'static str {
        "tmpfs"
    }

    fn properties(&self) -> FsProperties {
        FsProperties::empty()
    }

    fn sysnode(&self) -> Option<Arc<dyn aster_systree::SysNode>> {
        None
    }
}
