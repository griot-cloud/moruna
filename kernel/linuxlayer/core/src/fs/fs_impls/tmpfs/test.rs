// SPDX-License-Identifier: MPL-2.0

//! A task's scratch: a tmpfs under a quota (`ramfs::quota`).

use alloc::format;

use ostd::prelude::ktest;

use super::TmpFs;
use crate::{
    fs::{
        file::{InodeType, mkmod},
        fs_impls::ramfs::quota::{ENTRY_COST, Quota},
        vfs::{
            file_system::FileSystem,
            inode::{FallocMode, HardLinkability, MknodType},
            path::Dentry,
        },
    },
    prelude::*,
};

fn errno<T>(result: Result<T>) -> Errno {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => error.error(),
    }
}

fn used(fs: &TmpFs) -> usize {
    fs.quota().unwrap().used()
}

#[ktest]
fn the_quota_takes_and_gives_back() {
    let quota = Quota::new(1000);
    quota.charge(600).unwrap();
    assert_eq!(errno(quota.charge(401)), Errno::ENOSPC);
    assert_eq!(quota.used(), 600);
    quota.charge(400).unwrap();
    assert_eq!(errno(quota.charge(usize::MAX)), Errno::ENOSPC);
    quota.release(2000);
    assert_eq!(quota.used(), 0);
    assert_eq!(quota.limit(), 1000);
}

#[ktest]
fn a_file_is_charged_its_pages_and_its_entry() {
    let fs = TmpFs::new_scratch(2 * ENTRY_COST + 3 * PAGE_SIZE).unwrap();
    let root = fs.root_inode();
    let dentry = Dentry::new_root(root.clone());
    let mode = mkmod!(u+rw);

    let file = root.create(&dentry, "a", InodeType::File, mode).unwrap();
    assert_eq!(used(&fs), ENTRY_COST);

    // A byte charges a page; filling the page charges nothing more.
    file.write_bytes_at(0, b"x").unwrap();
    assert_eq!(used(&fs), ENTRY_COST + PAGE_SIZE);
    file.write_bytes_at(1, &[1u8; PAGE_SIZE - 1]).unwrap();
    assert_eq!(used(&fs), ENTRY_COST + PAGE_SIZE);

    // A write past the quota changes nothing.
    let big = vec![2u8; 3 * PAGE_SIZE];
    assert_eq!(errno(file.write_bytes_at(PAGE_SIZE, &big)), Errno::ENOSPC);
    assert_eq!(file.size(), PAGE_SIZE);
    assert_eq!(used(&fs), ENTRY_COST + PAGE_SIZE);

    // Growing and cutting the file.
    file.resize(&dentry, 3 * PAGE_SIZE).unwrap();
    assert_eq!(used(&fs), ENTRY_COST + 3 * PAGE_SIZE);
    assert_eq!(errno(file.resize(&dentry, 4 * PAGE_SIZE)), Errno::ENOSPC);
    assert_eq!(file.size(), 3 * PAGE_SIZE);
    file.resize(&dentry, PAGE_SIZE + 1).unwrap();
    assert_eq!(used(&fs), ENTRY_COST + 2 * PAGE_SIZE);
    file.fallocate(FallocMode::Allocate, 0, 3 * PAGE_SIZE)
        .unwrap();
    assert_eq!(used(&fs), ENTRY_COST + 3 * PAGE_SIZE);

    // `statfs` sees the quota.
    let sb = fs.sb();
    assert_eq!(sb.blocks, (2 * ENTRY_COST + 3 * PAGE_SIZE) / PAGE_SIZE);
    assert_eq!(sb.bfree, ENTRY_COST / PAGE_SIZE);
    assert_eq!(sb.files, (2 * ENTRY_COST + 3 * PAGE_SIZE) / ENTRY_COST);
}

#[ktest]
fn every_kind_of_entry_is_charged() {
    let fs = TmpFs::new_scratch(8 * ENTRY_COST).unwrap();
    let root = fs.root_inode();
    let dentry = Dentry::new_root(root.clone());
    let mode = mkmod!(u+rw);

    let dir = root.create(&dentry, "d", InodeType::Dir, mode).unwrap();
    assert_eq!(used(&fs), ENTRY_COST);
    let link = root.create_symlink(&dentry, "l", "target", mode).unwrap();
    assert_eq!(used(&fs), 2 * ENTRY_COST + 6);
    let fifo = root
        .mknod(&dentry, "p", mode, MknodType::NamedPipe)
        .unwrap();
    assert_eq!(used(&fs), 3 * ENTRY_COST + 6);
    let tmp = root
        .create_tmpfile(&dentry, mode, HardLinkability::Linkable)
        .unwrap();
    assert_eq!(used(&fs), 4 * ENTRY_COST + 6);

    // Dropping a detached entry gives its charge back.
    drop(tmp);
    assert_eq!(used(&fs), 3 * ENTRY_COST + 6);
    assert_eq!(dir.type_(), InodeType::Dir);
    assert_eq!(link.type_(), InodeType::SymLink);
    assert_eq!(fifo.type_(), InodeType::NamedPipe);

    // Entries run out.
    let mut held = vec![];
    for i in 0..4 {
        held.push(
            root.create(&dentry, &format!("f{i}"), InodeType::File, mode)
                .unwrap(),
        );
    }
    assert_eq!(
        errno(root.create(&dentry, "full", InodeType::File, mode)),
        Errno::ENOSPC
    );
    assert_eq!(
        errno(root.mknod(&dentry, "full", mode, MknodType::NamedPipe)),
        Errno::ENOSPC
    );
    assert_eq!(
        errno(root.create_tmpfile(&dentry, mode, HardLinkability::Linkable)),
        Errno::ENOSPC
    );
}

#[ktest]
fn a_removed_open_file_keeps_its_charge_until_closed() {
    let fs = TmpFs::new_scratch(16 * PAGE_SIZE).unwrap();
    let root = fs.root_inode();
    let dentry = Dentry::new_root(root.clone());
    let file = dentry
        .as_dir_dentry_or_err()
        .unwrap()
        .create_child("a", || {
            root.create(&dentry, "a", InodeType::File, mkmod!(u+rw))
        })
        .unwrap()
        .inode()
        .clone();
    file.write_bytes_at(0, &[7u8; 2 * PAGE_SIZE]).unwrap();
    assert_eq!(used(&fs), ENTRY_COST + 2 * PAGE_SIZE);

    dentry.as_dir_dentry_or_err().unwrap().unlink("a").unwrap();
    assert_eq!(used(&fs), ENTRY_COST + 2 * PAGE_SIZE);
    drop(file);
    assert_eq!(used(&fs), 0);
}

#[ktest]
fn a_plain_tmpfs_has_no_quota() {
    let fs = TmpFs::new_tmpfs();
    assert!(fs.quota().is_none());
    let root = fs.root_inode();
    let dentry = Dentry::new_root(root.clone());
    let file = root
        .create(&dentry, "a", InodeType::File, mkmod!(u+rw))
        .unwrap();
    file.write_bytes_at(0, &[1u8; 4 * PAGE_SIZE]).unwrap();
    file.resize(&dentry, 0).unwrap();
}
