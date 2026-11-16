// SPDX-License-Identifier: MPL-2.0

#![short_vis_path::add(procfs)]

use crate::{
    fs::{
        file::{InodeType, mkmod},
        procfs::{
            ProcDir, StaticEntry,
            sys::kernel::yama::YamaDirOps,
            template::{
                ListedEntry, ProcDirOps, ReaddirEntry, listed_entries_from_table,
                lookup_child_from_table, visit_listed_entries,
            },
        },
        vfs::inode::Inode,
    },
    prelude::*,
    security::lsm::is_yama_enabled,
};

// Moruna: cut from `/proc` (`procfs/mod.rs`). These stay until the
// credentials and names only they read are cut from `process/` and `net/`.
#[expect(dead_code, reason = "cut from /proc")]
mod overflow_gid;
#[expect(dead_code, reason = "cut from /proc")]
mod overflow_uid;
#[expect(dead_code, reason = "cut from /proc")]
mod uts;
mod yama;

/// Represents the inode at `/proc/sys/kernel`.
pub(in procfs) struct KernelDirOps;

impl KernelDirOps {
    pub(in procfs) fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        // Reference:
        // <https://elixir.bootlin.com/linux/v6.16.5/source/kernel/sysctl.c#L1765>
        // <https://elixir.bootlin.com/linux/v6.16.5/source/fs/proc/proc_sysctl.c#L978>
        ProcDir::new(Self, parent, mkmod!(a+rx))
    }

    // Moruna: only `yama`, whose scope the security module's calls read and set; no
    // program here reads the rest (`procfs/mod.rs`).
    const STATIC_ENTRIES: &'static [StaticEntry] = &[];
}

impl ProcDirOps for KernelDirOps {
    fn lookup_child(&self, this_dir: &ProcDir<Self>, name: &str) -> Result<Arc<dyn Inode>> {
        if let Some(child) = lookup_child_from_table(name, Self::STATIC_ENTRIES, |f| {
            (f)(this_dir.this_weak().clone())
        }) {
            return Ok(child);
        }

        if name == "yama" && is_yama_enabled() {
            return Ok(YamaDirOps::new_inode(this_dir.this_weak().clone()));
        }

        return_errno_with_message!(Errno::ENOENT, "the file does not exist");
    }

    fn visit_entries_from_offset<'a, F>(&'a self, offset: usize, visit_fn: F) -> Result<()>
    where
        F: FnMut(ReaddirEntry<'a>) -> Result<()>,
    {
        let yama_entry = is_yama_enabled().then(|| ListedEntry::new("yama", InodeType::Dir));

        visit_listed_entries(
            offset,
            listed_entries_from_table(Self::STATIC_ENTRIES).chain(yama_entry),
            visit_fn,
        )
    }
}
