// SPDX-License-Identifier: MPL-2.0

//! A task's root filesystem (Moruna: rewritten from Asterinas's block-device root).
//!
//! The root is the image's verified layers, read only, through the image filesystem
//! (SDD 27, [`ImageFs`]). Over it the start mounts, before the task's first process:
//!
//! - `/tmp`: the task's scratch, a tmpfs holding at most the scratch account's limit,
//!   the only place a task writes;
//! - `/proc` and `/sys`, cut to what a task's programs read;
//! - `/dev`: `null`, `zero` and `urandom`.
//!
//! No program mounts anything (data-os.md section 3a): the start's own mounts are the
//! only ones. Asterinas's root on a block device (ext2, `root=`, `rootfstype=`, `ro`,
//! `rw`) is gone with ext2.

use spin::once::Once;

use super::{
    devtmpfs,
    fs_impls::{
        imagefs::{ImageFs, ImageSource},
        procfs,
        sysfs::SysFs,
        tmpfs::TmpFs,
    },
    task_streams::TaskStreams,
    vfs::{
        path::{FsPath, Mount, MountNamespace, Path, PathResolver, PerMountFlags},
        registry::FsAndRoot,
    },
};
use crate::prelude::*;

/// What the start gives a task's filesystems.
pub(crate) struct TaskFs {
    /// The task's connection to the image filesystem.
    pub(crate) image: Arc<dyn ImageSource>,
    /// The bytes the scratch may hold: the scratch account's limit.
    pub(crate) scratch_quota: usize,
    /// Descriptors 0, 1 and 2.
    pub(crate) streams: TaskStreams,
}

static TASK_FS: Once<TaskFs> = Once::new();

/// Gives the task's filesystems, once, before the first process starts.
#[cfg_attr(
    not(ktest),
    expect(dead_code, reason = "the launcher (stream B2) calls it")
)]
pub(crate) fn set_task_fs(task_fs: TaskFs) -> Result<()> {
    let mut given = Some(task_fs);
    TASK_FS.call_once(|| given.take().unwrap());
    if given.is_some() {
        return_errno_with_message!(Errno::EBUSY, "the task's filesystems were given already");
    }
    Ok(())
}

/// The task's filesystems, if the start gave them.
pub(crate) fn task_fs() -> Option<&'static TaskFs> {
    TASK_FS.get()
}

/// Mounts the task's root and switches to it: it becomes the resolver's root and working
/// directory, and the bootstrap root is detached.
pub(crate) fn switch_to_rootfs(path_resolver: &mut PathResolver) -> Result<()> {
    let task_fs = task_fs().ok_or_else(|| {
        Error::with_message(Errno::EINVAL, "the start gave no image to make the root of")
    })?;
    let root = mount_task_root(
        task_fs.image.clone(),
        task_fs.scratch_quota,
        Arc::downgrade(MountNamespace::get_init_singleton()),
    )?;
    path_resolver.switch_root_for_boot(root);
    ostd::early_println!("[kernel] the image's root is ready");
    Ok(())
}

/// Makes the task's root: the image, and the scratch, `/proc`, `/sys` and `/dev` over
/// it. The mounts are not attached to a namespace's tree yet.
pub(in crate::fs) fn mount_task_root(
    image: Arc<dyn ImageSource>,
    scratch_quota: usize,
    mnt_ns: Weak<MountNamespace>,
) -> Result<Arc<Mount>> {
    let image_fs = ImageFs::new(image)?;
    let root = Mount::new_detached(
        FsAndRoot::new(image_fs),
        PerMountFlags::RDONLY,
        mnt_ns,
        Some("image".to_string()),
    )?;
    let nothing_runs = PerMountFlags::NOSUID | PerMountFlags::NODEV;
    root.mount_child_for_boot(
        "tmp",
        FsAndRoot::new(TmpFs::new_scratch(scratch_quota)?),
        nothing_runs,
        Some("scratch".to_string()),
    )?;
    let fixed = nothing_runs | PerMountFlags::NOEXEC;
    root.mount_child_for_boot(
        "proc",
        FsAndRoot::new(procfs::new_procfs()),
        fixed,
        Some("proc".to_string()),
    )?;
    root.mount_child_for_boot(
        "sys",
        FsAndRoot::new(SysFs::new()?),
        fixed | PerMountFlags::RDONLY,
        Some("sysfs".to_string()),
    )?;
    root.mount_child_for_boot(
        "dev",
        FsAndRoot::new(devtmpfs::singleton().clone()),
        PerMountFlags::NOSUID | PerMountFlags::NOEXEC,
        Some("devtmpfs".to_string()),
    )?;
    Ok(root)
}

/// Finds the init program the `init` parameter names.
///
/// Returns `Ok(None)` when `init` is not given, and an error if the pathname is invalid
/// or cannot be resolved.
pub(crate) fn find_init(path_resolver: &PathResolver) -> Result<Option<(Path, &'static str)>> {
    let Some(init_path) = INIT_PATH.get().map(String::as_str) else {
        return Ok(None);
    };

    let path = path_resolver.lookup(&FsPath::try_from(init_path)?)?;
    Ok(Some((path, init_path)))
}

static INIT_PATH: Once<String> = Once::new();
aster_cmdline::define_kv_param!("init", INIT_PATH);

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;
    use crate::fs::{
        file::{InodeType, mkmod},
        fs_impls::imagefs::test::MemImage,
        task_streams::ConsoleText,
    };

    fn image() -> MemImage {
        MemImage::new()
            .dir("/dev")
            .dir("/etc")
            .file("/etc/hostname", b"moruna\n")
            .link("/lib", b"usr/lib")
            .dir("/proc")
            .dir("/tmp")
            .dir("/usr")
            .dir("/usr/lib")
            .file("/usr/lib/libc.so", b"\x7fELF")
    }

    fn read(resolver: &PathResolver, path: &str) -> String {
        let path = resolver.lookup(&FsPath::try_from(path).unwrap()).unwrap();
        let mut buf = vec![0u8; 256];
        let n = path.inode().read_bytes_at(0, &mut buf).unwrap();
        String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    fn errno<T>(result: Result<T>) -> Errno {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => error.error(),
        }
    }

    #[ktest]
    fn the_root_is_the_image_with_the_scratch_and_the_synthetic_filesystems_over_it() {
        let root = mount_task_root(Arc::new(image()), 4 * PAGE_SIZE, Weak::new()).unwrap();
        let resolver = PathResolver::new_at(root);

        // The image, its links followed.
        assert_eq!(read(&resolver, "/etc/hostname"), "moruna\n");
        assert_eq!(read(&resolver, "/lib/libc.so"), "\x7fELF");
        let etc = resolver.lookup(&FsPath::try_from("/etc").unwrap()).unwrap();
        assert_eq!(
            errno(etc.new_child("new", InodeType::File, mkmod!(u+rw))),
            Errno::EROFS
        );

        // `/proc` and `/sys`, as the allocators and glibc read them.
        assert_eq!(read(&resolver, "/proc/sys/vm/overcommit_memory"), "0\n");
        let overcommit = resolver
            .lookup(&FsPath::try_from("/proc/sys/vm/overcommit_memory").unwrap())
            .unwrap();
        assert_eq!(
            errno(overcommit.inode().write_bytes_at(0, b"2")),
            Errno::EPERM
        );
        let cpus = read(&resolver, "/sys/devices/system/cpu/online");
        assert!(cpus.starts_with('0'));
        assert_eq!(
            read(&resolver, "/sys/kernel/mm/transparent_hugepage/enabled"),
            "always madvise [never]\n"
        );
        let dev = resolver.lookup(&FsPath::try_from("/dev").unwrap()).unwrap();
        assert_eq!(dev.type_(), InodeType::Dir);
        assert_eq!(dev.mount_node().fs().name(), "devtmpfs");

        // The scratch takes what its quota allows and no more.
        let tmp = resolver.lookup(&FsPath::try_from("/tmp").unwrap()).unwrap();
        let file = tmp.new_child("out", InodeType::File, mkmod!(u+rw)).unwrap();
        file.inode()
            .write_bytes_at(0, &[1u8; 3 * PAGE_SIZE])
            .unwrap();
        assert_eq!(
            errno(
                file.inode()
                    .write_bytes_at(3 * PAGE_SIZE, &[1u8; PAGE_SIZE])
            ),
            Errno::ENOSPC
        );
        assert_eq!(read(&resolver, "/tmp/out").len(), 256);
    }

    #[ktest]
    fn an_image_that_cannot_be_read_makes_no_root() {
        let broken = MemImage::new().file("/tmp", b"not a directory");
        assert_eq!(
            errno(mount_task_root(Arc::new(broken), PAGE_SIZE, Weak::new())),
            Errno::ENOTDIR
        );
    }

    #[ktest]
    fn the_start_gives_the_filesystems_once_and_the_root_switches_to_them() {
        // No other test reads the first namespace, whose root this replaces.
        let mut resolver = MountNamespace::get_init_singleton().new_path_resolver();
        if task_fs().is_none() {
            assert_eq!(errno(switch_to_rootfs(&mut resolver)), Errno::EINVAL);
        }

        let task = || TaskFs {
            image: Arc::new(image()),
            scratch_quota: PAGE_SIZE,
            streams: TaskStreams::text_only(Arc::new(ConsoleText)),
        };
        let _ = set_task_fs(task());
        assert_eq!(errno(set_task_fs(task())), Errno::EBUSY);
        assert_eq!(task_fs().unwrap().scratch_quota, PAGE_SIZE);

        switch_to_rootfs(&mut resolver).unwrap();
        assert_eq!(read(&resolver, "/etc/hostname"), "moruna\n");
        assert!(find_init(&resolver).unwrap().is_none());
    }
}
