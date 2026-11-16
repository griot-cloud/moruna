// SPDX-License-Identifier: MPL-2.0

use alloc::format;

use ostd::prelude::ktest;

use super::{
    SysFs,
    tree::{Spec, SysFsType, cpu_list},
};
use crate::{
    fs::{
        file::{InodeType, mkmod},
        utils::DirentVisitor,
        vfs::{
            file_system::{FileSystem, FsFlags},
            inode::Inode,
            path::Dentry,
            registry::FsType,
        },
    },
    prelude::*,
    process::{Gid, Uid},
};

fn walk(fs: &SysFs, path: &str) -> Result<Arc<dyn Inode>> {
    let mut inode = fs.root_inode();
    for name in path.split('/').filter(|name| !name.is_empty()) {
        inode = inode.lookup(name)?;
    }
    Ok(inode)
}

fn text(fs: &SysFs, path: &str) -> String {
    let inode = walk(fs, path).unwrap();
    let mut buf = vec![0u8; 256];
    let n = inode.read_bytes_at(0, &mut buf).unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

#[derive(Default)]
struct Names(Vec<(String, u64, InodeType)>);

impl DirentVisitor for Names {
    fn visit(&mut self, name: &str, ino: u64, type_: InodeType, _offset: usize) -> Result<()> {
        if self.0.len() == 3 && name == "stop" {
            return_errno!(Errno::EINVAL);
        }
        self.0.push((name.to_string(), ino, type_));
        Ok(())
    }
}

fn errno<T>(result: Result<T>) -> Errno {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => error.error(),
    }
}

#[ktest]
fn the_tree_is_what_programs_read() {
    let fs = SysFs::new().unwrap();
    let cpus = cpu_list();
    assert!(cpus == "0\n" || cpus.starts_with("0-"));
    for file in ["online", "possible", "present"] {
        assert_eq!(text(&fs, &format!("devices/system/cpu/{file}")), cpus);
    }
    assert_eq!(
        text(&fs, "kernel/mm/transparent_hugepage/enabled"),
        "always madvise [never]\n"
    );
    assert_eq!(errno(walk(&fs, "devices/virtual")), Errno::ENOENT);
    assert_eq!(errno(walk(&fs, "fs/cgroup")), Errno::ENOENT);
    assert_eq!(
        errno(walk(&fs, "devices/system/cpu/online/x")),
        Errno::ENOTDIR
    );

    // A read from an offset, and past the end.
    let online = walk(&fs, "devices/system/cpu/online").unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(online.read_bytes_at(1, &mut buf).unwrap(), cpus.len() - 1);
    assert_eq!(online.read_bytes_at(100, &mut buf).unwrap(), 0);
}

#[ktest]
fn the_tree_is_read_only() {
    let fs = SysFs::new().unwrap();
    let root = fs.root_inode();
    let dentry = Dentry::new_root(root.clone());
    let online = walk(&fs, "devices/system/cpu/online").unwrap();
    assert_eq!(errno(online.write_bytes_at(0, b"0-3")), Errno::EACCES);
    assert_eq!(errno(online.resize(&dentry, 0)), Errno::EACCES);
    assert_eq!(errno(online.set_mode(&dentry, mkmod!(a+rw))), Errno::EACCES);
    assert_eq!(
        errno(online.set_owner(&dentry, Uid::new_root())),
        Errno::EACCES
    );
    assert_eq!(
        errno(online.set_group(&dentry, Gid::new_root())),
        Errno::EACCES
    );
    assert_eq!(errno(root.read_bytes_at(0, &mut [0u8; 1])), Errno::EISDIR);
    assert_eq!(
        errno(online.readdir_at(0, &mut Names::default())),
        Errno::ENOTDIR
    );

    let meta = online.metadata().unwrap();
    assert_eq!(meta.size, PAGE_SIZE);
    assert_eq!(meta.mode, mkmod!(a+r));
    assert_eq!(meta.type_, InodeType::File);
    assert_eq!(online.owner().unwrap(), Uid::new_root());
    assert_eq!(online.group().unwrap(), Gid::new_root());
    let dir = root.metadata().unwrap();
    assert_eq!(dir.mode, mkmod!(a+rx));
    assert_eq!(dir.size, 0);
    assert_eq!(dir.ino, 1);
    online.set_atime(&dentry, Default::default());
    online.set_mtime(&dentry, Default::default());
    online.set_ctime(&dentry, Default::default());

    assert_eq!(fs.name(), "sysfs");
    assert_eq!(fs.flags(), FsFlags::RDONLY);
    assert!(fs.sync().is_ok());
    assert_eq!(fs.sb().magic, 0x6265_6572);
    assert!(Arc::ptr_eq(
        &online.fs(),
        &(fs.clone() as Arc<dyn FileSystem>)
    ));
    let _ = online.extension();
}

static STOPPING: Spec = Spec::Dir(&[
    ("a", Spec::File(cpu_list)),
    ("stop", Spec::File(cpu_list)),
    ("z", Spec::Dir(&[])),
]);

#[ktest]
fn directories_list_their_names() {
    let fs = SysFs::new().unwrap();
    let mut names = Names::default();
    let root = fs.root_inode();
    assert_eq!(root.readdir_at(0, &mut names).unwrap(), 4);
    let listed: Vec<_> = names.0.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(listed, [".", "..", "devices", "kernel"]);
    assert_eq!(names.0[2].2, InodeType::Dir);

    // A child's `..` is its parent.
    let cpu = walk(&fs, "devices/system/cpu").unwrap();
    let mut names = Names::default();
    cpu.readdir_at(1, &mut names).unwrap();
    assert_eq!(names.0[0].0, "..");
    assert_eq!(names.0[0].1, walk(&fs, "devices/system").unwrap().ino());
    assert_eq!(names.0[1].2, InodeType::File);

    // A visitor that stops part of the way, and one that stops at once.
    let stopping = SysFs::with_tree(&STOPPING).unwrap();
    let mut names = Names::default();
    assert_eq!(stopping.root_inode().readdir_at(0, &mut names).unwrap(), 3);
    let mut names = Names(vec![(String::new(), 0, InodeType::Dir); 3]);
    assert_eq!(
        errno(stopping.root_inode().readdir_at(3, &mut names)),
        Errno::EINVAL
    );
}

#[ktest]
fn the_type_names_sysfs() {
    let fs_type = SysFsType;
    assert_eq!(FsType::name(&fs_type), "sysfs");
    assert!(FsType::sysnode(&fs_type).is_none());
}
