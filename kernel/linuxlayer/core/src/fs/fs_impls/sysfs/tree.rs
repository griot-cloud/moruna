// SPDX-License-Identifier: MPL-2.0

//! The fixed tree behind `/sys`.

use alloc::format;
use core::time::Duration;

use device_id::DeviceId;

use crate::{
    fs::{
        file::{InodeMode, InodeType, StatusFlags, mkmod},
        pseudofs::AnonDeviceId,
        utils::{DirentVisitor, NAME_MAX},
        vfs::{
            file_system::{FileSystem, FsEventSubscriberStats, FsFlags, SuperBlock},
            inode::{Extension, FileOps, Inode, Metadata},
            path::Dentry,
            registry::{FsProperties, FsType},
        },
    },
    prelude::*,
    process::{Gid, Uid},
};

/// `statfs`'s magic for sysfs.
const SYSFS_MAGIC: u64 = 0x6265_6572;

/// The size Linux states for a sysfs file, whatever it holds.
const ATTR_SIZE: usize = PAGE_SIZE;

const ROOT_INO: u64 = 1;

/// A name of the tree: a directory of names, or a file whose text is made when read.
pub(super) enum Spec {
    Dir(&'static [(&'static str, Spec)]),
    File(fn() -> String),
}

/// The tree a task sees.
pub(super) static TREE: Spec = Spec::Dir(&[
    (
        "devices",
        Spec::Dir(&[(
            "system",
            Spec::Dir(&[(
                "cpu",
                Spec::Dir(&[
                    ("online", Spec::File(cpu_list)),
                    ("possible", Spec::File(cpu_list)),
                    ("present", Spec::File(cpu_list)),
                ]),
            )]),
        )]),
    ),
    (
        "kernel",
        Spec::Dir(&[(
            "mm",
            Spec::Dir(&[(
                "transparent_hugepage",
                Spec::Dir(&[("enabled", Spec::File(transparent_hugepage))]),
            )]),
        )]),
    ),
]);

/// The CPUs the task may run on, as Linux lists a range: `0`, or `0-N`.
pub(super) fn cpu_list() -> String {
    match ostd::cpu::num_cpus() {
        0 | 1 => "0\n".to_string(),
        n => format!("0-{}\n", n - 1),
    }
}

/// The personality maps no huge page behind a program's back.
fn transparent_hugepage() -> String {
    "always madvise [never]\n".to_string()
}

pub(crate) struct SysFs {
    _anon_device_id: AnonDeviceId,
    sb: SuperBlock,
    root: Arc<SysInode>,
    fs_event_subscriber_stats: FsEventSubscriberStats,
}

impl SysFs {
    pub(crate) fn new() -> Result<Arc<Self>> {
        Self::with_tree(&TREE)
    }

    pub(super) fn with_tree(tree: &'static Spec) -> Result<Arc<Self>> {
        let anon_device_id = AnonDeviceId::acquire()
            .ok_or_else(|| Error::with_message(Errno::ENOMEM, "no device ID for sysfs"))?;
        let mut sb = SuperBlock::new(SYSFS_MAGIC, PAGE_SIZE, NAME_MAX, anon_device_id.id());
        sb.flags = FsFlags::RDONLY.bits() as u64;
        let dev_id = anon_device_id.id();
        Ok(Arc::new_cyclic(|weak_fs| {
            let mut next_ino = ROOT_INO;
            Self {
                _anon_device_id: anon_device_id,
                sb,
                root: SysInode::build(tree, &mut next_ino, ROOT_INO, dev_id, weak_fs),
                fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            }
        }))
    }
}

impl FileSystem for SysFs {
    fn name(&self) -> &'static str {
        "sysfs"
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn sb(&self) -> SuperBlock {
        self.sb.clone()
    }

    fn flags(&self) -> FsFlags {
        FsFlags::RDONLY
    }

    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.fs_event_subscriber_stats
    }
}

pub(super) struct SysFsType;

impl FsType for SysFsType {
    fn name(&self) -> &'static str {
        "sysfs"
    }

    fn properties(&self) -> FsProperties {
        FsProperties::empty()
    }

    fn sysnode(&self) -> Option<Arc<dyn aster_systree::SysNode>> {
        None
    }
}

enum Content {
    Dir(Vec<(&'static str, Arc<SysInode>)>),
    File(fn() -> String),
}

struct SysInode {
    ino: u64,
    parent_ino: u64,
    content: Content,
    container_dev_id: DeviceId,
    fs: Weak<SysFs>,
    extension: Extension,
}

impl SysInode {
    fn build(
        spec: &'static Spec,
        next_ino: &mut u64,
        parent_ino: u64,
        container_dev_id: DeviceId,
        fs: &Weak<SysFs>,
    ) -> Arc<Self> {
        let ino = *next_ino;
        *next_ino += 1;
        let content = match spec {
            Spec::Dir(children) => Content::Dir(
                children
                    .iter()
                    .map(|(name, child)| {
                        let child = Self::build(child, next_ino, ino, container_dev_id, fs);
                        (*name, child)
                    })
                    .collect(),
            ),
            Spec::File(text) => Content::File(*text),
        };
        Arc::new(Self {
            ino,
            parent_ino,
            content,
            container_dev_id,
            fs: fs.clone(),
            extension: Extension::new(),
        })
    }

    fn read_only() -> Error {
        Error::with_message(Errno::EACCES, "sysfs is read only")
    }
}

impl FileOps for SysInode {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        let Content::File(text) = &self.content else {
            return_errno_with_message!(Errno::EISDIR, "a directory is not read");
        };
        let text = text();
        let bytes = text.as_bytes();
        let from = offset.min(bytes.len());
        let to = bytes.len().min(from + writer.avail());
        writer.write_fallible(&mut bytes[from..to].into())?;
        Ok(to - from)
    }

    fn write_at(
        &self,
        _offset: usize,
        _reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        Err(Self::read_only())
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        let Content::Dir(children) = &self.content else {
            return_errno_with_message!(Errno::ENOTDIR, "not a directory");
        };
        let mut idx = offset;
        let visited = (|| -> Result<()> {
            if idx == 0 {
                visitor.visit(".", self.ino, InodeType::Dir, 1)?;
                idx = 1;
            }
            if idx == 1 {
                visitor.visit("..", self.parent_ino, InodeType::Dir, 2)?;
                idx = 2;
            }
            for (name, child) in children.iter().skip(idx - 2) {
                visitor.visit(name, child.ino, child.type_(), idx + 1)?;
                idx += 1;
            }
            Ok(())
        })();
        match visited {
            Err(error) if idx == offset => Err(error),
            _ => Ok(idx - offset),
        }
    }
}

impl Inode for SysInode {
    fn size(&self) -> usize {
        match self.content {
            Content::Dir(_) => 0,
            Content::File(_) => ATTR_SIZE,
        }
    }

    fn resize(&self, _self_dentry: &Dentry, _new_size: usize) -> Result<()> {
        Err(Self::read_only())
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            ino: self.ino,
            size: self.size(),
            optimal_block_size: PAGE_SIZE,
            nr_sectors_allocated: 0,
            last_access_at: Duration::ZERO,
            last_modify_at: Duration::ZERO,
            last_meta_change_at: Duration::ZERO,
            type_: self.type_(),
            mode: self.mode()?,
            nr_hard_links: 1,
            uid: Uid::new_root(),
            gid: Gid::new_root(),
            container_dev_id: self.container_dev_id,
            self_dev_id: None,
            birth_at: None,
        })
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn type_(&self) -> InodeType {
        match self.content {
            Content::Dir(_) => InodeType::Dir,
            Content::File(_) => InodeType::File,
        }
    }

    fn mode(&self) -> Result<InodeMode> {
        Ok(match self.content {
            Content::Dir(_) => mkmod!(a+rx),
            Content::File(_) => mkmod!(a+r),
        })
    }

    fn set_mode(&self, _self_dentry: &Dentry, _mode: InodeMode) -> Result<()> {
        Err(Self::read_only())
    }

    fn owner(&self) -> Result<Uid> {
        Ok(Uid::new_root())
    }

    fn set_owner(&self, _self_dentry: &Dentry, _uid: Uid) -> Result<()> {
        Err(Self::read_only())
    }

    fn group(&self) -> Result<Gid> {
        Ok(Gid::new_root())
    }

    fn set_group(&self, _self_dentry: &Dentry, _gid: Gid) -> Result<()> {
        Err(Self::read_only())
    }

    fn atime(&self) -> Duration {
        Duration::ZERO
    }

    fn set_atime(&self, _self_dentry: &Dentry, _time: Duration) {}

    fn mtime(&self) -> Duration {
        Duration::ZERO
    }

    fn set_mtime(&self, _self_dentry: &Dentry, _time: Duration) {}

    fn ctime(&self) -> Duration {
        Duration::ZERO
    }

    fn set_ctime(&self, _self_dentry: &Dentry, _time: Duration) {}

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        let Content::Dir(children) = &self.content else {
            return_errno_with_message!(Errno::ENOTDIR, "not a directory");
        };
        children
            .iter()
            .find(|(child_name, _)| *child_name == name)
            .map(|(_, child)| child.clone() as Arc<dyn Inode>)
            .ok_or_else(|| Error::with_message(Errno::ENOENT, "sysfs has no such name"))
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.upgrade().unwrap()
    }

    fn extension(&self) -> &Extension {
        &self.extension
    }
}
