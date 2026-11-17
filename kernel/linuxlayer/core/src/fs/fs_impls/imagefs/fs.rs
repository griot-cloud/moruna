// SPDX-License-Identifier: MPL-2.0

//! The image's root filesystem: Moruna's verified layers, read through the image
//! filesystem server's protocol (SDD 27).
//!
//! The server merges the layers and checks every page it reads against its SHA-256
//! tree, and its page cache is shared by every task. This side asks it a path at a
//! time, as the VFS walks, so mounts over the image (`/tmp`, `/proc`, `/sys`, `/dev`)
//! and its links resolve the way Linux resolves them. Nothing here writes: every
//! change is `EROFS`.
//!
//! A directory is listed once and kept, since the image never changes. A read is
//! a run of `read` requests of at most `READ_MAX` bytes. A file mapped by a program
//! is backed by a page cache whose pages are filled from the same reads, a chunk at
//! a time.

use core::time::Duration;

use align_ext::AlignExt;
use device_id::DeviceId;
use io_util::batch::IoBatch;
use ostd::mm::{VmIo, VmIoFill};

use super::port::{ImageError, ImageResult, ImageSource, ImageStat, READ_MAX, kind, open_flags};
use crate::{
    fs::{
        file::{InodeMode, InodeType, StatusFlags},
        pseudofs::AnonDeviceId,
        utils::{DirentVisitor, NAME_MAX},
        vfs::{
            file_system::{FileSystem, FsEventSubscriberStats, FsFlags, SuperBlock},
            inode::{
                Extension, FileOps, HardLinkability, Inode, Metadata, MknodType, RenameMode,
                SymbolicLink,
            },
            path::Dentry,
        },
    },
    prelude::*,
    process::{Gid, Uid},
    vm::page_cache::{LockedCachePage, PageCache, PageCacheBackend, Vmo},
};

/// `statfs`'s magic for the image: "MRNL".
const IMAGEFS_MAGIC: u64 = 0x4d52_4e4c;

/// The mount points the personality puts over the image. The server adds `dev` and
/// `proc` and answers `tmp` (SDD 27 f.6); one the image lacks shows as an empty
/// directory at the root, so it can be mounted on.
pub(crate) const MOUNT_POINTS: [&str; 4] = ["dev", "proc", "sys", "tmp"];

/// Inode numbers of the mount points this side adds, apart from the image's.
const SYNTHETIC_INO: u64 = 1 << 63;

/// Files this client keeps open on its connection. The server allows 1024.
const OPEN_MAX: usize = 256;

/// Entries a directory may hold (the server's own bound, SDD 27 i).
const ENTRIES_MAX: usize = 1 << 20;

/// The image's root filesystem, one task's connection to the image filesystem.
pub(crate) struct ImageFs {
    source: Arc<dyn ImageSource>,
    sb: SuperBlock,
    _anon_device_id: AnonDeviceId,
    root: Arc<ImageInode>,
    open: Mutex<OpenFiles>,
    fs_event_subscriber_stats: FsEventSubscriberStats,
}

/// Regular files open on the connection, by inode number, least recently used first
/// to go.
#[derive(Default)]
struct OpenFiles {
    files: BTreeMap<u64, OpenFile>,
    tick: u64,
}

struct OpenFile {
    file: u32,
    used: u64,
}

impl ImageFs {
    /// Connects the image's root through `source`.
    pub(crate) fn new(source: Arc<dyn ImageSource>) -> Result<Arc<Self>> {
        let root_stat = source.stat(b"/", false)?;
        if root_stat.kind != kind::DIRECTORY {
            return_errno_with_message!(Errno::ENOTDIR, "the image's root is not a directory");
        }
        let anon_device_id = AnonDeviceId::acquire()
            .ok_or_else(|| Error::with_message(Errno::ENOMEM, "no device ID for the image"))?;
        let mut sb = SuperBlock::new(IMAGEFS_MAGIC, PAGE_SIZE, NAME_MAX, anon_device_id.id());
        sb.flags = FsFlags::RDONLY.bits() as u64;
        let dev_id = anon_device_id.id();
        Ok(Arc::new_cyclic(|weak_fs| Self {
            source,
            sb,
            _anon_device_id: anon_device_id,
            root: ImageInode::new(
                weak_fs.clone(),
                dev_id,
                b"/".to_vec(),
                root_stat,
                root_stat.ino,
                false,
            ),
            open: Mutex::new(OpenFiles::default()),
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
        }))
    }

    /// Runs `op` on the connection's number for regular file `inode`, opening it if
    /// it is not open. A number the server no longer knows is opened again once.
    fn with_file<T>(&self, inode: &ImageInode, op: impl Fn(u32) -> ImageResult<T>) -> Result<T> {
        let file = self.file_of(inode)?;
        match op(file) {
            Err(ImageError::BAD_FILE) => {
                self.open.lock().files.remove(&inode.stat.ino);
                let file = self.file_of(inode)?;
                op(file).map_err(Into::into)
            }
            result => result.map_err(Into::into),
        }
    }

    fn file_of(&self, inode: &ImageInode) -> Result<u32> {
        let mut open = self.open.lock();
        open.tick += 1;
        let tick = open.tick;
        if let Some(entry) = open.files.get_mut(&inode.stat.ino) {
            entry.used = tick;
            return Ok(entry.file);
        }
        if open.files.len() >= OPEN_MAX
            && let Some((&ino, _)) = open.files.iter().min_by_key(|(_, entry)| entry.used)
        {
            let evicted = open.files.remove(&ino).unwrap();
            let _ = self.source.close(evicted.file);
        }
        let (file, _) = self.source.open(&inode.path, open_flags::NOFOLLOW)?;
        open.files
            .insert(inode.stat.ino, OpenFile { file, used: tick });
        Ok(file)
    }

    /// Closes regular file `ino` if it is open.
    fn forget(&self, ino: u64) {
        if let Some(entry) = self.open.lock().files.remove(&ino) {
            let _ = self.source.close(entry.file);
        }
    }

    /// The files open on the connection.
    #[cfg(ktest)]
    pub(super) fn open_files(&self) -> usize {
        self.open.lock().files.len()
    }
}

impl Drop for ImageFs {
    fn drop(&mut self) {
        let open = core::mem::take(&mut self.open.lock().files);
        for (_, entry) in open {
            let _ = self.source.close(entry.file);
        }
    }
}

impl FileSystem for ImageFs {
    fn name(&self) -> &'static str {
        "imagefs"
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

/// One name of the image, as the server stated it.
pub(super) struct ImageInode {
    /// Absolute in the image, `/` for the root.
    path: Vec<u8>,
    stat: ImageStat,
    type_: InodeType,
    parent_ino: u64,
    /// A mount point this side added: an empty directory.
    synthetic: bool,
    container_dev_id: DeviceId,
    fs: Weak<ImageFs>,
    this: Weak<ImageInode>,
    listing: Mutex<Option<Arc<Vec<Child>>>>,
    target: Mutex<Option<String>>,
    page_cache: Mutex<Option<PageCache>>,
    /// The last chunk read to fill the page cache: its offset and bytes.
    chunk: Mutex<Option<(usize, Vec<u8>)>>,
    extension: Extension,
}

struct Child {
    name: String,
    ino: u64,
    type_: InodeType,
}

fn type_of(kind: u8) -> InodeType {
    match kind {
        kind::DIRECTORY => InodeType::Dir,
        kind::REGULAR => InodeType::File,
        kind::SYMLINK => InodeType::SymLink,
        _ => InodeType::Unknown,
    }
}

fn read_only() -> Error {
    Error::with_message(Errno::EROFS, "the image is read only")
}

impl ImageInode {
    fn new(
        fs: Weak<ImageFs>,
        container_dev_id: DeviceId,
        path: Vec<u8>,
        stat: ImageStat,
        parent_ino: u64,
        synthetic: bool,
    ) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            path,
            stat,
            type_: type_of(stat.kind),
            parent_ino,
            synthetic,
            container_dev_id,
            fs,
            this: this.clone(),
            listing: Mutex::new(None),
            target: Mutex::new(None),
            page_cache: Mutex::new(None),
            chunk: Mutex::new(None),
            extension: Extension::new(),
        })
    }

    fn image_fs(&self) -> Result<Arc<ImageFs>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "the image is gone"))
    }

    fn is_root(&self) -> bool {
        self.path == b"/"
    }

    fn child_path(&self, name: &str) -> Vec<u8> {
        let mut path = self.path.clone();
        if !self.is_root() {
            path.push(b'/');
        }
        path.extend_from_slice(name.as_bytes());
        path
    }

    /// The directory's entries in name order, read once.
    fn listing(&self) -> Result<Arc<Vec<Child>>> {
        let mut guard = self.listing.lock();
        if let Some(listing) = guard.as_ref() {
            return Ok(listing.clone());
        }
        let mut children = Vec::new();
        if !self.synthetic {
            let fs = self.image_fs()?;
            let flags = open_flags::DIRECTORY | open_flags::NOFOLLOW;
            let (file, _) = fs.source.open(&self.path, flags)?;
            let listed = list(fs.source.as_ref(), file, &mut children);
            let _ = fs.source.close(file);
            listed?;
        }
        if self.is_root() {
            for (index, name) in MOUNT_POINTS.iter().enumerate() {
                if !children.iter().any(|child| child.name == *name) {
                    children.push(Child {
                        name: name.to_string(),
                        ino: SYNTHETIC_INO + index as u64,
                        type_: InodeType::Dir,
                    });
                }
            }
            children.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        }
        let listing = Arc::new(children);
        *guard = Some(listing.clone());
        Ok(listing)
    }

    /// The bytes of page `idx` from the chunk that holds it, read if it is not the
    /// last chunk read.
    fn page_bytes(&self, idx: usize) -> Result<Vec<u8>> {
        let chunk_len = READ_MAX as usize;
        let at = idx * PAGE_SIZE;
        let start = at / chunk_len * chunk_len;
        let mut chunk = self.chunk.lock();
        if chunk.as_ref().is_none_or(|(offset, _)| *offset != start) {
            let fs = self.image_fs()?;
            let bytes = fs.with_file(self, |file| fs.source.read(file, start as u64, READ_MAX))?;
            *chunk = Some((start, bytes));
        }
        let (_, bytes) = chunk.as_ref().unwrap();
        let from = (at - start).min(bytes.len());
        let to = (from + PAGE_SIZE).min(bytes.len());
        Ok(bytes[from..to].to_vec())
    }
}

/// Every entry of open directory `file`, its names that are not UTF-8 left out (the
/// VFS's names are strings).
fn list(source: &dyn ImageSource, file: u32, children: &mut Vec<Child>) -> Result<()> {
    let mut cookie = 0;
    loop {
        let (entries, next) = source.readdir(file, cookie)?;
        if entries.is_empty() {
            return Ok(());
        }
        if next <= cookie || children.len() + entries.len() > ENTRIES_MAX {
            return_errno_with_message!(Errno::EIO, "the image's listing does not end");
        }
        for (name, kind, ino) in entries {
            if let Ok(name) = String::from_utf8(name) {
                children.push(Child {
                    name,
                    ino,
                    type_: type_of(kind),
                });
            }
        }
        cookie = next;
    }
}

impl Drop for ImageInode {
    fn drop(&mut self) {
        if self.type_ == InodeType::File
            && let Some(fs) = self.fs.upgrade()
        {
            fs.forget(self.stat.ino);
        }
    }
}

impl FileOps for ImageInode {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        match self.type_ {
            InodeType::File => {}
            InodeType::Dir => return_errno_with_message!(Errno::EISDIR, "a directory is not read"),
            _ => return_errno_with_message!(Errno::EINVAL, "not a regular file"),
        }
        let size = self.stat.size as usize;
        if offset >= size {
            return Ok(0);
        }
        let want = writer.avail().min(size - offset);
        let fs = self.image_fs()?;
        let mut done = 0;
        while done < want {
            let len = (want - done).min(READ_MAX as usize);
            let at = (offset + done) as u64;
            let bytes = fs.with_file(self, |file| fs.source.read(file, at, len as u32))?;
            if bytes.len() > len {
                return_errno_with_message!(Errno::EIO, "the image answered more than asked");
            }
            writer.write_fallible(&mut bytes.as_slice().into())?;
            done += bytes.len();
            if bytes.len() < len {
                break;
            }
        }
        Ok(done)
    }

    fn write_at(
        &self,
        _offset: usize,
        _reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno_with_message!(Errno::EISDIR, "a directory is not written");
        }
        Err(read_only())
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        if self.type_ != InodeType::Dir {
            return_errno_with_message!(Errno::ENOTDIR, "not a directory");
        }
        let listing = self.listing()?;
        let mut idx = offset;
        let visited = (|| -> Result<()> {
            if idx == 0 {
                visitor.visit(".", self.stat.ino, InodeType::Dir, 1)?;
                idx = 1;
            }
            if idx == 1 {
                visitor.visit("..", self.parent_ino, InodeType::Dir, 2)?;
                idx = 2;
            }
            for child in listing.iter().skip(idx - 2) {
                visitor.visit(&child.name, child.ino, child.type_, idx + 1)?;
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

impl PageCacheBackend for ImageInode {
    fn read_page_async(
        &self,
        idx: usize,
        locked_page: LockedCachePage,
        _io_batch: &mut IoBatch,
    ) -> Result<()> {
        if idx * PAGE_SIZE >= (self.stat.size as usize).align_up(PAGE_SIZE) {
            return_errno_with_message!(Errno::EINVAL, "the page is past the file's end");
        }
        let bytes = self.page_bytes(idx)?;
        locked_page.write_bytes(0, &bytes)?;
        if bytes.len() < PAGE_SIZE {
            locked_page.fill_zeros(bytes.len(), PAGE_SIZE - bytes.len())?;
        }
        locked_page.set_up_to_date();
        Ok(())
    }

    fn write_page_async(
        &self,
        _idx: usize,
        _locked_page: LockedCachePage,
        _io_batch: &mut IoBatch,
    ) -> Result<()> {
        Err(read_only())
    }
}

impl Inode for ImageInode {
    fn size(&self) -> usize {
        self.stat.size as usize
    }

    fn resize(&self, _self_dentry: &Dentry, _new_size: usize) -> Result<()> {
        Err(read_only())
    }

    fn metadata(&self) -> Result<Metadata> {
        let size = self.stat.size as usize;
        Ok(Metadata {
            ino: self.stat.ino,
            size,
            optimal_block_size: PAGE_SIZE,
            nr_sectors_allocated: size.div_ceil(512),
            last_access_at: Duration::ZERO,
            last_modify_at: Duration::ZERO,
            last_meta_change_at: Duration::ZERO,
            type_: self.type_,
            mode: self.mode()?,
            nr_hard_links: if self.type_ == InodeType::Dir { 2 } else { 1 },
            uid: Uid::new_root(),
            gid: Gid::new_root(),
            container_dev_id: self.container_dev_id,
            self_dev_id: None,
            birth_at: None,
        })
    }

    fn ino(&self) -> u64 {
        self.stat.ino
    }

    fn type_(&self) -> InodeType {
        self.type_
    }

    fn mode(&self) -> Result<InodeMode> {
        Ok(InodeMode::from_bits_truncate(
            (self.stat.mode & 0o7777) as u16,
        ))
    }

    fn set_mode(&self, _self_dentry: &Dentry, _mode: InodeMode) -> Result<()> {
        Err(read_only())
    }

    fn owner(&self) -> Result<Uid> {
        Ok(Uid::new_root())
    }

    fn set_owner(&self, _self_dentry: &Dentry, _uid: Uid) -> Result<()> {
        Err(read_only())
    }

    fn group(&self) -> Result<Gid> {
        Ok(Gid::new_root())
    }

    fn set_group(&self, _self_dentry: &Dentry, _gid: Gid) -> Result<()> {
        Err(read_only())
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

    fn page_cache(&self) -> Option<Arc<Vmo>> {
        if self.type_ != InodeType::File {
            return None;
        }
        let mut page_cache = self.page_cache.lock();
        if page_cache.is_none() {
            let backend: Weak<dyn PageCacheBackend> = self.this.clone();
            *page_cache = PageCache::new_with_backend(self.stat.size as usize, backend).ok();
        }
        page_cache.as_ref().map(|cache| cache.as_vmo().clone())
    }

    fn create(
        &self,
        _self_dentry: &Dentry,
        _name: &str,
        _type_: InodeType,
        _mode: InodeMode,
    ) -> Result<Arc<dyn Inode>> {
        Err(read_only())
    }

    fn create_symlink(
        &self,
        _self_dentry: &Dentry,
        _name: &str,
        _target: &str,
        _mode: InodeMode,
    ) -> Result<Arc<dyn Inode>> {
        Err(read_only())
    }

    fn create_tmpfile(
        &self,
        _self_dentry: &Dentry,
        _mode: InodeMode,
        _hard_linkability: HardLinkability,
    ) -> Result<Arc<dyn Inode>> {
        Err(read_only())
    }

    fn mknod(
        &self,
        _self_dentry: &Dentry,
        _name: &str,
        _mode: InodeMode,
        _type_: MknodType,
    ) -> Result<Arc<dyn Inode>> {
        Err(read_only())
    }

    fn link(&self, _self_dentry: &Dentry, _old_dentry: &Dentry, _name: &str) -> Result<()> {
        Err(read_only())
    }

    fn unlink(&self, _child_dentry: &Dentry) -> Result<()> {
        Err(read_only())
    }

    fn rmdir(&self, _child_dentry: &Dentry) -> Result<()> {
        Err(read_only())
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        if self.type_ != InodeType::Dir {
            return_errno_with_message!(Errno::ENOTDIR, "not a directory");
        }
        if self.synthetic {
            return_errno_with_message!(Errno::ENOENT, "a mount point is empty");
        }
        // A listing already read answers a name that is not there.
        if let Some(listing) = self.listing.lock().as_ref()
            && !listing.iter().any(|child| child.name == name)
        {
            return_errno_with_message!(Errno::ENOENT, "the image has no such name");
        }
        let fs = self.image_fs()?;
        let path = self.child_path(name);
        let mount_point = MOUNT_POINTS.iter().position(|point| *point == name);
        let (stat, synthetic) = match fs.source.stat(&path, false) {
            Ok(stat) => (stat, false),
            Err(ImageError::NOT_FOUND) if self.is_root() && mount_point.is_some() => {
                let stat = ImageStat {
                    kind: kind::DIRECTORY,
                    mode: 0o755,
                    size: 0,
                    ino: SYNTHETIC_INO + mount_point.unwrap() as u64,
                };
                (stat, true)
            }
            Err(error) => return Err(error.into()),
        };
        Ok(ImageInode::new(
            self.fs.clone(),
            self.container_dev_id,
            path,
            stat,
            self.stat.ino,
            synthetic,
        ))
    }

    fn rename(
        &self,
        _old_child_dentry: &Dentry,
        _new_dir_dentry: &Dentry,
        _new_name: &str,
        _replaced_inode: Option<&Arc<dyn Inode>>,
        _mode: RenameMode,
    ) -> Result<()> {
        Err(read_only())
    }

    fn read_link(&self) -> Result<SymbolicLink> {
        if self.type_ != InodeType::SymLink {
            return_errno_with_message!(Errno::EINVAL, "not a symbolic link");
        }
        let mut target = self.target.lock();
        if target.is_none() {
            let fs = self.image_fs()?;
            let bytes = fs.source.readlink(&self.path)?;
            let text = String::from_utf8(bytes)
                .map_err(|_| Error::with_message(Errno::EIO, "a link's target is not UTF-8"))?;
            *target = Some(text);
        }
        Ok(SymbolicLink::Plain(target.clone().unwrap()))
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.upgrade().unwrap()
    }

    fn extension(&self) -> &Extension {
        &self.extension
    }
}
