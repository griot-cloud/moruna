// SPDX-License-Identifier: MPL-2.0

use aster_systree::{
    AttrLessBranchNodeFields, SysNode, SysObj, SysPerms, SysStr, inherit_sys_branch_node,
};
use spin::Once;

use crate::{
    fs::{
        fs_impls::sysfs,
        vfs::{file_system::FileSystem, path::Dentry},
    },
    prelude::*,
};

/// A type of file system.
pub(crate) trait FsType: Send + Sync + 'static {
    /// Gets the name of this FS type such as `"ext4"` or `"sysfs"`.
    fn name(&self) -> &'static str;

    /// Gets the properties of this FS type.
    #[expect(dead_code, reason = "only the mount calls, refused, read it")]
    fn properties(&self) -> FsProperties;

    /// Returns a `SysTree` node that represents the FS type.
    ///
    /// If a FS type is not intended to appear under SysFs,
    /// then this method returns a `None`.
    ///
    /// The same result will be returned by this method
    /// when it is called multiple times.
    fn sysnode(&self) -> Option<Arc<dyn SysNode>>;
}

/// Object-safe view of [`FsType`] used by the registry, mount syscall, and procfs.
///
/// Implemented automatically for every [`FsType`] via a blanket impl, so FS
/// authors only implement [`FsType`].
pub(crate) trait DynFsType: Send + Sync + 'static {
    /// Gets the name of this FS type such as `"ext4"` or `"sysfs"`.
    fn name(&self) -> &'static str;

    /// Gets the properties of this FS type.
    #[expect(dead_code, reason = "only the mount calls, refused, read it")]
    fn properties(&self) -> FsProperties;

    /// Returns a `SysTree` node that represents the FS type.
    fn sysnode(&self) -> Option<Arc<dyn SysNode>>;
}

impl<T: FsType> DynFsType for T {
    fn name(&self) -> &'static str {
        <T as FsType>::name(self)
    }

    fn properties(&self) -> FsProperties {
        <T as FsType>::properties(self)
    }

    fn sysnode(&self) -> Option<Arc<dyn SysNode>> {
        <T as FsType>::sysnode(self)
    }
}

bitflags! {
    /// The properties common to all FS instances.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct FsProperties: u32 {
        /// Whether a FS needs to be backed by a disk.
        ///
        /// Most persistent FSes such as ext2 require disks.
        /// But a volatile FS such as ramfs or
        /// a pseudo FS such as sysfs does not.
        const NEED_DISK = 1 << 1;
    }
}

/// Registers a new FS type.
//
// TODO: Figure out what should happen when unregistering the FS type.
pub(crate) fn register(new_type: &'static dyn DynFsType) -> Result<()> {
    FS_REGISTRY.get().unwrap().register(new_type)
}

/// Executes a user-provided operation with an iterator that can access each
/// and every FS type.
#[expect(dead_code, reason = "only /proc/filesystems, cut, used it")]
pub(crate) fn with_iter<F, R>(f: F) -> R
where
    F: FnOnce(&mut dyn Iterator<Item = (&str, &dyn DynFsType)>) -> R,
{
    let guard = FS_REGISTRY.get().unwrap().fs_table.lock();

    let mut iter = guard.iter().map(|(name, fs_type)| (*name, *fs_type));
    f(&mut iter)
}

/// Initializes the FS registry module.
pub(crate) fn init() {
    // This object will appear at the `/sys/fs` path
    FS_REGISTRY.call_once(|| {
        let singleton = FsRegistry::new();
        sysfs::systree_singleton()
            .root()
            .add_child(singleton.clone())
            .unwrap();
        singleton
    });
}

static FS_REGISTRY: Once<Arc<FsRegistry>> = Once::new();

/// A file system paired with its root [`Dentry`].
#[derive(Clone)]
pub(crate) struct FsAndRoot {
    fs: Arc<dyn FileSystem>,
    root_dentry: Arc<Dentry>,
}

impl FsAndRoot {
    /// Creates an `FsAndRoot` by deriving the root dentry from the file system.
    ///
    /// This root is the file system root created from [`FileSystem::root_inode`].
    pub(crate) fn new(fs: Arc<dyn FileSystem>) -> Self {
        let root_dentry = Dentry::new_root(fs.root_inode());
        Self { fs, root_dentry }
    }

    /// Consumes the `FsAndRoot` and returns the file system and its root dentry.
    pub(in crate::fs) fn into_parts(self) -> (Arc<dyn FileSystem>, Arc<Dentry>) {
        (self.fs, self.root_dentry)
    }
}

struct FsRegistry {
    fs_table: Mutex<BTreeMap<&'static str, &'static dyn DynFsType>>,
    systree_fields: AttrLessBranchNodeFields<dyn SysObj, Self>,
}

impl Debug for FsRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FsRegistry")
            .field("systree_fields", &self.systree_fields)
            .finish()
    }
}

impl FsRegistry {
    fn new() -> Arc<Self> {
        let name = SysStr::from("fs");
        Arc::new_cyclic(|weak_self| {
            let fs_table = Mutex::new(BTreeMap::new());
            let systree_fields = AttrLessBranchNodeFields::new(name, weak_self.clone());
            Self {
                fs_table,
                systree_fields,
            }
        })
    }

    /// Registers a file system control interface.
    fn register(&self, new_type: &'static dyn DynFsType) -> Result<()> {
        let mut fs_table = self.fs_table.lock();
        if fs_table.contains_key(new_type.name()) {
            return_errno_with_message!(Errno::EEXIST, "the file system type already exists");
        }

        if let Some(node) = new_type.sysnode() {
            self.systree_fields.add_child(node)?;
        }

        fs_table.insert(new_type.name(), new_type);
        Ok(())
    }
}

inherit_sys_branch_node!(FsRegistry, systree_fields, {
    fn perms(&self) -> SysPerms {
        SysPerms::DEFAULT_RW_PERMS
    }
});
