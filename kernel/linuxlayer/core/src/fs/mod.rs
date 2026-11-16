// SPDX-License-Identifier: MPL-2.0

pub mod file;
mod fs_impls;
pub(crate) mod initramfs;
pub(crate) mod pipe;
pub(crate) mod rootfs;
pub(crate) mod task_streams;
pub(crate) mod thread_info;
pub mod utils;
pub mod vfs;

pub use fs_impls::devtmpfs;
pub(crate) use fs_impls::{cgroupfs, exfat, ext2, procfs, pseudofs, ramfs, tmpfs};

use crate::{
    fs::{
        file::{InodeType, file_table::FdFlags, mkmod},
        task_streams::{ConsoleText, TaskStreams},
        vfs::{
            path::{FsPath, Path, PathResolver, PerMountFlags},
            registry::FsAndRoot,
        },
    },
    init,
    prelude::*,
};

pub(crate) fn init() {
    vfs::init();
    fs_impls::init();
}

pub(crate) fn init_on_each_cpu() {
    fs_impls::init_on_each_cpu();
}

pub(crate) fn init_in_first_kthread(path_resolver: &PathResolver) {
    fs_impls::init_in_first_kthread();
    initramfs::init_in_first_kthread(path_resolver).unwrap();
}

pub(crate) fn init_in_first_process(ctx: &Context) {
    let fs = ctx.thread_local.borrow_fs();
    let path_resolver = fs.resolver().read();

    // The image's root has its `/dev` (`rootfs::mount_task_root`); an initramfs gets one.
    if !init::booted_from_rootfs() {
        let dev_path = lookup_or_create_dev(&path_resolver).unwrap();
        dev_path
            .mount(
                FsAndRoot::new(devtmpfs::singleton().clone()),
                PerMountFlags::default(),
                Some("devtmpfs".to_string()),
                ctx,
            )
            .unwrap();
    }

    // Descriptors 0, 1 and 2 are the task's streams (SDD 29 f.7, f.8); there is no
    // terminal (data-os.md section 3a).
    let streams = match rootfs::task_fs() {
        Some(task_fs) => task_fs.streams.clone(),
        None => TaskStreams::text_only(Arc::new(ConsoleText)),
    };

    let mut file_table_ref = ctx.thread_local.borrow_file_table_mut();
    let mut file_table = file_table_ref.unwrap().write();
    for file in streams.files() {
        file_table
            .insert(file, FdFlags::empty())
            .map_err(|(err, _)| err)
            .unwrap();
    }
}

fn lookup_or_create_dev(path_resolver: &PathResolver) -> Result<Path> {
    match path_resolver.lookup(&FsPath::try_from("/dev")?) {
        Err(error) if error.error() == Errno::ENOENT => {
            path_resolver
                .root()
                .new_child("dev", InodeType::Dir, mkmod!(a+rx, u+w))
        }
        result => result,
    }
}
