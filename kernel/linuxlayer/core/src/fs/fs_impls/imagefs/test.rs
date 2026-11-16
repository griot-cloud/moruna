// SPDX-License-Identifier: MPL-2.0

//! The image root over an image held in memory, which answers the port as the image
//! filesystem server answers `moruna.imagefs` 3.0 (SDD 27 d.1, f.6).

use alloc::format;
use core::{
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    time::Duration,
};

use ostd::prelude::ktest;

use super::{
    ImageFs, ImageSource,
    fs::{ImageInode, MOUNT_POINTS},
    port::{ImageDirent, ImageError, ImageResult, ImageStat, READ_MAX, kind, open_flags},
};
use crate::{
    fs::{
        file::{InodeType, mkmod},
        utils::DirentVisitor,
        vfs::{
            file_system::{FileSystem, FsFlags},
            inode::{HardLinkability, Inode, MknodType, RenameMode, SymbolicLink},
            path::Dentry,
        },
    },
    prelude::*,
    process::{Gid, Uid},
};

enum Node {
    Dir { mode: u32 },
    File { mode: u32, data: Vec<u8> },
    Link { target: Vec<u8> },
}

/// What the image was asked.
#[derive(Default, Debug, Clone, Copy)]
pub(in crate::fs) struct Calls {
    pub(in crate::fs) stat: usize,
    pub(in crate::fs) open: usize,
    pub(in crate::fs) readdir: usize,
    pub(in crate::fs) read: usize,
    pub(in crate::fs) readlink: usize,
    pub(in crate::fs) close: usize,
}

/// An image in memory: absolute paths to nodes, inode numbers in path order.
pub(in crate::fs) struct MemImage {
    nodes: BTreeMap<Vec<u8>, (u64, Node)>,
    open: Mutex<BTreeMap<u32, Vec<u8>>>,
    next_file: AtomicU32,
    /// Entries one `readdir` answers.
    page: usize,
    /// `readdir` answers the same cookie forever.
    stuck: bool,
    calls: Mutex<Calls>,
    /// The next `read` finds its file closed, as a server that forgot it would.
    forget_once: AtomicBool,
}

impl MemImage {
    pub(in crate::fs) fn new() -> Self {
        let mut image = Self {
            nodes: BTreeMap::new(),
            open: Mutex::new(BTreeMap::new()),
            next_file: AtomicU32::new(1),
            page: 3,
            stuck: false,
            calls: Mutex::new(Calls::default()),
            forget_once: AtomicBool::new(false),
        };
        image.insert(b"/", Node::Dir { mode: 0o755 });
        image
    }

    fn insert(&mut self, path: &[u8], node: Node) {
        let ino = 100 + self.nodes.len() as u64;
        self.nodes.insert(path.to_vec(), (ino, node));
    }

    pub(in crate::fs) fn dir(mut self, path: &str) -> Self {
        self.insert(path.as_bytes(), Node::Dir { mode: 0o755 });
        self
    }

    pub(in crate::fs) fn file(mut self, path: &str, data: &[u8]) -> Self {
        self.insert(
            path.as_bytes(),
            Node::File {
                mode: 0o644,
                data: data.to_vec(),
            },
        );
        self
    }

    fn raw_file(mut self, path: &[u8], mode: u32) -> Self {
        self.insert(path, Node::File { mode, data: vec![] });
        self
    }

    pub(in crate::fs) fn link(mut self, path: &str, target: &[u8]) -> Self {
        self.insert(
            path.as_bytes(),
            Node::Link {
                target: target.to_vec(),
            },
        );
        self
    }

    fn paged(mut self, page: usize) -> Self {
        self.page = page;
        self
    }

    pub(in crate::fs) fn calls(&self) -> Calls {
        *self.calls.lock()
    }

    fn stat_of(&self, path: &[u8]) -> ImageResult<ImageStat> {
        let (ino, node) = self.nodes.get(path).ok_or(ImageError::NOT_FOUND)?;
        Ok(match node {
            Node::Dir { mode } => ImageStat {
                kind: kind::DIRECTORY,
                mode: *mode,
                size: 0,
                ino: *ino,
            },
            Node::File { mode, data } => ImageStat {
                kind: kind::REGULAR,
                mode: *mode,
                size: data.len() as u64,
                ino: *ino,
            },
            Node::Link { target } => ImageStat {
                kind: kind::SYMLINK,
                mode: 0o777,
                size: target.len() as u64,
                ino: *ino,
            },
        })
    }

    fn children(&self, dir: &[u8]) -> Vec<ImageDirent> {
        self.nodes
            .iter()
            .filter_map(|(path, (ino, node))| {
                let (parent, name) = path.rsplit_split()?;
                let parent: &[u8] = if parent.is_empty() { b"/" } else { parent };
                (parent == dir).then(|| {
                    let kind = match node {
                        Node::Dir { .. } => kind::DIRECTORY,
                        Node::File { .. } => kind::REGULAR,
                        Node::Link { .. } => kind::SYMLINK,
                    };
                    (name.to_vec(), kind, *ino)
                })
            })
            .collect()
    }

    fn opened(&self, file: u32) -> ImageResult<Vec<u8>> {
        self.open
            .lock()
            .get(&file)
            .cloned()
            .ok_or(ImageError::BAD_FILE)
    }
}

trait SplitPath {
    fn rsplit_split(&self) -> Option<(&[u8], &[u8])>;
}

impl SplitPath for Vec<u8> {
    /// The parent and the last part of an absolute path; `None` for the root.
    fn rsplit_split(&self) -> Option<(&[u8], &[u8])> {
        if self.as_slice() == b"/" {
            return None;
        }
        let at = self.iter().rposition(|byte| *byte == b'/')?;
        Some((&self[..at], &self[at + 1..]))
    }
}

impl ImageSource for MemImage {
    fn stat(&self, path: &[u8], _follow: bool) -> ImageResult<ImageStat> {
        self.calls.lock().stat += 1;
        self.stat_of(path)
    }

    fn open(&self, path: &[u8], flags: u32) -> ImageResult<(u32, ImageStat)> {
        self.calls.lock().open += 1;
        let stat = self.stat_of(path)?;
        if flags & open_flags::DIRECTORY != 0 && stat.kind != kind::DIRECTORY {
            return Err(ImageError::NOT_DIRECTORY);
        }
        if flags & open_flags::NOFOLLOW != 0 && stat.kind == kind::SYMLINK {
            return Err(ImageError::LOOP);
        }
        let file = self.next_file.fetch_add(1, Ordering::Relaxed);
        self.open.lock().insert(file, path.to_vec());
        Ok((file, stat))
    }

    fn readdir(&self, file: u32, cookie: u32) -> ImageResult<(Vec<ImageDirent>, u32)> {
        self.calls.lock().readdir += 1;
        let path = self.opened(file)?;
        let children = self.children(&path);
        let from = cookie as usize;
        if from >= children.len() {
            return Ok((vec![], cookie));
        }
        let to = (from + self.page).min(children.len());
        let next = if self.stuck { cookie } else { to as u32 };
        Ok((children[from..to].to_vec(), next))
    }

    fn readlink(&self, path: &[u8]) -> ImageResult<Vec<u8>> {
        self.calls.lock().readlink += 1;
        match self.nodes.get(path) {
            Some((_, Node::Link { target })) => Ok(target.clone()),
            Some(_) => Err(ImageError::BAD_PATH),
            None => Err(ImageError::NOT_FOUND),
        }
    }

    fn read(&self, file: u32, offset: u64, len: u32) -> ImageResult<Vec<u8>> {
        self.calls.lock().read += 1;
        if self.forget_once.swap(false, Ordering::Relaxed) {
            self.open.lock().clear();
        }
        if len > READ_MAX {
            return Err(ImageError::BAD_RANGE);
        }
        let path = self.opened(file)?;
        match &self.nodes[&path].1 {
            Node::File { data, .. } => {
                let from = (offset as usize).min(data.len());
                let to = (from + len as usize).min(data.len());
                Ok(data[from..to].to_vec())
            }
            Node::Dir { .. } => Err(ImageError::IS_DIRECTORY),
            Node::Link { .. } => Err(ImageError::LOOP),
        }
    }

    fn close(&self, file: u32) -> ImageResult<()> {
        self.calls.lock().close += 1;
        self.open
            .lock()
            .remove(&file)
            .map(|_| ())
            .ok_or(ImageError::BAD_FILE)
    }
}

/// A directory's entries as `(name, ino, type)`.
#[derive(Default)]
struct Listing(Vec<(String, u64, InodeType)>);

impl DirentVisitor for Listing {
    fn visit(&mut self, name: &str, ino: u64, type_: InodeType, _offset: usize) -> Result<()> {
        self.0.push((name.to_string(), ino, type_));
        Ok(())
    }
}

/// Takes at most `left` entries, then refuses.
struct Few {
    left: usize,
    names: Vec<String>,
}

impl DirentVisitor for Few {
    fn visit(&mut self, name: &str, _ino: u64, _type: InodeType, _offset: usize) -> Result<()> {
        if self.left == 0 {
            return_errno!(Errno::EINVAL);
        }
        self.left -= 1;
        self.names.push(name.to_string());
        Ok(())
    }
}

fn names(inode: &dyn Inode) -> Vec<String> {
    let mut listing = Listing::default();
    inode.readdir_at(0, &mut listing).unwrap();
    listing.0.into_iter().map(|(name, _, _)| name).collect()
}

fn mount(image: MemImage) -> (Arc<MemImage>, Arc<ImageFs>) {
    let image = Arc::new(image);
    let fs = ImageFs::new(image.clone()).unwrap();
    (image, fs)
}

fn errno<T>(result: Result<T>) -> Errno {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => error.error(),
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

#[ktest]
fn root_lists_the_image_and_the_mount_points() {
    let (_, fs) = mount(
        MemImage::new()
            .dir("/bin")
            .dir("/etc")
            .dir("/proc")
            .dir("/tmp"),
    );
    let root = fs.root_inode();
    assert_eq!(
        names(root.as_ref()),
        [".", "..", "bin", "dev", "etc", "proc", "sys", "tmp"]
    );
    assert_eq!(MOUNT_POINTS, ["dev", "proc", "sys", "tmp"]);

    // `sys` is not in the image: an empty directory stands for it.
    let sys = root.lookup("sys").unwrap();
    assert_eq!(sys.type_(), InodeType::Dir);
    assert_eq!(sys.ino(), (1 << 63) + 2);
    assert_eq!(names(sys.as_ref()), [".", ".."]);
    assert_eq!(errno(sys.lookup("anything")), Errno::ENOENT);

    // `proc` is the image's own.
    assert_eq!(root.lookup("proc").unwrap().ino(), 103);

    assert_eq!(fs.name(), "imagefs");
    assert_eq!(fs.flags(), FsFlags::RDONLY);
    assert!(fs.sync().is_ok());
    assert_eq!(fs.sb().flags, FsFlags::RDONLY.bits() as u64);
    assert!(Arc::ptr_eq(
        &root.fs(),
        &(fs.clone() as Arc<dyn FileSystem>)
    ));
}

#[ktest]
fn a_listing_is_paged_once_and_answers_lookups() {
    let (image, fs) = mount(
        MemImage::new()
            .dir("/etc")
            .file("/etc/a", b"1")
            .file("/etc/b", b"2")
            .file("/etc/c", b"3")
            .file("/etc/d", b"4")
            .raw_file(b"/etc/\xff", 0o600)
            .paged(2),
    );
    let etc = fs.root_inode().lookup("etc").unwrap();
    assert_eq!(names(etc.as_ref()), [".", "..", "a", "b", "c", "d"]);
    let readdirs = image.calls().readdir;
    // Five entries two at a time, and the empty answer.
    assert_eq!(readdirs, 4);
    assert_eq!(names(etc.as_ref()).len(), 6);
    assert_eq!(image.calls().readdir, readdirs);

    // A name the listing lacks is not asked for.
    let stats = image.calls().stat;
    assert_eq!(errno(etc.lookup("zzz")), Errno::ENOENT);
    assert_eq!(image.calls().stat, stats);
    assert_eq!(etc.lookup("c").unwrap().size(), 1);

    // From an offset, and a visitor that stops part of the way.
    let mut listing = Listing::default();
    assert_eq!(etc.readdir_at(4, &mut listing).unwrap(), 2);
    assert_eq!(listing.0[0].0, "c");
    let mut few = Few {
        left: 3,
        names: vec![],
    };
    assert_eq!(etc.readdir_at(0, &mut few).unwrap(), 3);
    assert_eq!(few.names, [".", "..", "a"]);
    let mut none = Few {
        left: 0,
        names: vec![],
    };
    assert_eq!(errno(etc.readdir_at(0, &mut none)), Errno::EINVAL);
    assert_eq!(
        errno(etc.lookup("a").unwrap().readdir_at(0, &mut none)),
        Errno::ENOTDIR
    );
}

#[ktest]
fn a_listing_that_does_not_end_is_refused() {
    let mut image = MemImage::new().dir("/d").file("/d/a", b"").paged(1);
    image.stuck = true;
    let (_, fs) = mount(image);
    let dir = fs.root_inode().lookup("d").unwrap();
    assert_eq!(
        errno(dir.readdir_at(0, &mut Listing::default())),
        Errno::EIO
    );
}

#[ktest]
fn a_read_is_runs_of_read_max() {
    let data = pattern(100_000);
    let (image, fs) = mount(MemImage::new().file("/big", &data));
    let big = fs.root_inode().lookup("big").unwrap();
    assert_eq!(big.size(), 100_000);

    let mut buf = vec![0u8; 100_000];
    assert_eq!(big.read_bytes_at(0, &mut buf).unwrap(), 100_000);
    assert_eq!(buf, data);
    assert_eq!(image.calls().read, 100_000usize.div_ceil(READ_MAX as usize));
    assert_eq!(image.calls().open, 1);

    let mut tail = vec![0u8; 100];
    assert_eq!(big.read_bytes_at(99_990, &mut tail).unwrap(), 10);
    assert_eq!(&tail[..10], &data[99_990..]);
    assert_eq!(big.read_bytes_at(100_000, &mut tail).unwrap(), 0);
    assert_eq!(big.read_bytes_at(200_000, &mut tail).unwrap(), 0);
    // Still the one open file.
    assert_eq!(image.calls().open, 1);
}

#[ktest]
fn a_file_the_server_forgot_is_opened_again() {
    let (image, fs) = mount(MemImage::new().file("/f", b"hello"));
    let file = fs.root_inode().lookup("f").unwrap();
    let mut buf = [0u8; 5];
    file.read_bytes_at(0, &mut buf).unwrap();
    image.forget_once.store(true, Ordering::Relaxed);
    assert_eq!(file.read_bytes_at(0, &mut buf).unwrap(), 5);
    assert_eq!(&buf, b"hello");
    assert_eq!(image.calls().open, 2);
}

#[ktest]
fn open_files_are_bounded_and_closed() {
    let mut image = MemImage::new();
    for i in 0..300 {
        image = image.file(&format!("/f{i:03}"), b"x");
    }
    let (image, fs) = mount(image);
    let root = fs.root_inode();
    let mut held = vec![];
    for i in 0..300 {
        let file = root.lookup(&format!("f{i:03}")).unwrap();
        let mut buf = [0u8; 1];
        file.read_bytes_at(0, &mut buf).unwrap();
        held.push(file);
    }
    assert_eq!(fs.open_files(), 256);
    assert_eq!(image.calls().close, 300 - 256);

    // The least recently used went first: the first file opens again.
    let mut buf = [0u8; 1];
    held[0].read_bytes_at(0, &mut buf).unwrap();
    assert_eq!(image.calls().open, 301);

    // Dropping an inode closes its file; dropping the filesystem closes the rest.
    drop(held.pop());
    assert_eq!(fs.open_files(), 255);
    drop(root);
    let closes = image.calls().close;
    drop(fs);
    drop(held);
    assert_eq!(image.calls().close, closes + 255);
    assert!(image.open.lock().is_empty());
}

#[ktest]
fn metadata_says_the_image() {
    let (_, fs) = mount(
        MemImage::new()
            .dir("/d")
            .raw_file(b"/x", 0o4755)
            .file("/f", &pattern(1000)),
    );
    let root = fs.root_inode();
    let file = root.lookup("f").unwrap();
    let meta = file.metadata().unwrap();
    assert_eq!(meta.size, 1000);
    assert_eq!(meta.nr_sectors_allocated, 2);
    assert_eq!(meta.type_, InodeType::File);
    assert_eq!(meta.mode, mkmod!(u+rw, g+r, o+r));
    assert_eq!(meta.nr_hard_links, 1);
    assert_eq!(meta.ino, file.ino());
    assert_eq!(meta.uid, Uid::new_root());
    assert_eq!(meta.last_modify_at, Duration::ZERO);
    assert_eq!(file.owner().unwrap(), Uid::new_root());
    assert_eq!(file.group().unwrap(), Gid::new_root());

    // Setuid is kept as the image says it.
    let setuid = root.lookup("x").unwrap();
    assert_eq!(setuid.mode().unwrap().bits(), 0o4755);

    let dir = root.lookup("d").unwrap();
    assert_eq!(dir.metadata().unwrap().nr_hard_links, 2);
    assert!(dir.page_cache().is_none());
    assert_eq!(errno(file.lookup("y")), Errno::ENOTDIR);
}

#[ktest]
fn every_change_is_refused() {
    let (_, fs) = mount(MemImage::new().dir("/d").file("/f", b"abc"));
    let root = fs.root_inode();
    let dentry = Dentry::new_root(root.clone());
    let file = root.lookup("f").unwrap();
    let dir = root.lookup("d").unwrap();
    let mode = mkmod!(u+rw);

    assert_eq!(errno(file.write_bytes_at(0, b"x")), Errno::EROFS);
    assert_eq!(errno(dir.write_bytes_at(0, b"x")), Errno::EISDIR);
    assert_eq!(errno(dir.read_bytes_at(0, &mut [0u8; 1])), Errno::EISDIR);
    assert_eq!(errno(file.resize(&dentry, 0)), Errno::EROFS);
    assert_eq!(
        errno(root.create(&dentry, "n", InodeType::File, mode)),
        Errno::EROFS
    );
    assert_eq!(
        errno(root.create_symlink(&dentry, "n", "t", mode)),
        Errno::EROFS
    );
    assert_eq!(
        errno(root.create_tmpfile(&dentry, mode, HardLinkability::Linkable)),
        Errno::EROFS
    );
    assert_eq!(
        errno(root.mknod(&dentry, "n", mode, MknodType::NamedPipe)),
        Errno::EROFS
    );
    assert_eq!(errno(root.link(&dentry, &dentry, "n")), Errno::EROFS);
    assert_eq!(errno(root.unlink(&dentry)), Errno::EROFS);
    assert_eq!(errno(root.rmdir(&dentry)), Errno::EROFS);
    assert_eq!(
        errno(root.rename(&dentry, &dentry, "n", None, RenameMode::Replace)),
        Errno::EROFS
    );
    assert_eq!(errno(file.set_mode(&dentry, mode)), Errno::EROFS);
    assert_eq!(
        errno(file.set_owner(&dentry, Uid::new_root())),
        Errno::EROFS
    );
    assert_eq!(
        errno(file.set_group(&dentry, Gid::new_root())),
        Errno::EROFS
    );
    // Times do not change, and say so by staying zero.
    file.set_atime(&dentry, Duration::from_secs(5));
    file.set_mtime(&dentry, Duration::from_secs(5));
    file.set_ctime(&dentry, Duration::from_secs(5));
    assert_eq!(file.metadata().unwrap().last_modify_at, Duration::ZERO);
}

#[ktest]
fn links_read_their_targets() {
    let (image, fs) = mount(
        MemImage::new()
            .link("/lib", b"usr/lib")
            .link("/bad", b"\xff\xfe")
            .file("/f", b""),
    );
    let root = fs.root_inode();
    let lib = root.lookup("lib").unwrap();
    assert_eq!(lib.type_(), InodeType::SymLink);
    assert_eq!(lib.size(), 7);
    for _ in 0..2 {
        match lib.read_link().unwrap() {
            SymbolicLink::Plain(target) => assert_eq!(target, "usr/lib"),
            SymbolicLink::Path(_) => panic!("a plain link"),
        }
    }
    // The target is read once.
    assert_eq!(image.calls().readlink, 1);
    assert_eq!(errno(root.lookup("bad").unwrap().read_link()), Errno::EIO);
    assert_eq!(errno(root.lookup("f").unwrap().read_link()), Errno::EINVAL);
    assert_eq!(errno(lib.read_bytes_at(0, &mut [0u8; 1])), Errno::EINVAL);
}

#[ktest]
fn mapped_pages_are_filled_a_chunk_at_a_time() {
    let data = pattern(3 * PAGE_SIZE + 100);
    let (image, fs) = mount(MemImage::new().file("/lib.so", &data));
    let lib = fs.root_inode().lookup("lib.so").unwrap();
    let vmo = lib.page_cache().unwrap();
    // The same cache for every mapping.
    assert!(Arc::ptr_eq(&vmo, &lib.page_cache().unwrap()));

    let mut buf = vec![0u8; 4 * PAGE_SIZE];
    vmo.read(0, &mut VmWriter::from(buf.as_mut_slice()).to_fallible())
        .unwrap();
    assert_eq!(&buf[..data.len()], data.as_slice());
    assert!(buf[data.len()..].iter().all(|byte| *byte == 0));
    // Four pages from one chunk.
    assert_eq!(image.calls().read, 1);
}

#[ktest]
fn the_root_must_be_a_directory() {
    let mut image = MemImage::new();
    image.nodes.insert(
        b"/".to_vec(),
        (
            1,
            Node::File {
                mode: 0o644,
                data: vec![],
            },
        ),
    );
    assert_eq!(errno(ImageFs::new(Arc::new(image))), Errno::ENOTDIR);

    let mut empty = MemImage::new();
    empty.nodes.clear();
    assert_eq!(errno(ImageFs::new(Arc::new(empty))), Errno::ENOENT);
}

#[ktest]
fn refusals_are_linux_errors() {
    let table = [
        (ImageError::TRANSPORT, Errno::EIO),
        (ImageError::NOT_FOUND, Errno::ENOENT),
        (ImageError::NOT_DIRECTORY, Errno::ENOTDIR),
        (ImageError::IS_DIRECTORY, Errno::EISDIR),
        (ImageError::READ_ONLY, Errno::EROFS),
        (ImageError::NO_SPACE, Errno::ENOSPC),
        (ImageError::EXISTS, Errno::EEXIST),
        (ImageError::NOT_EMPTY, Errno::ENOTEMPTY),
        (ImageError::BAD_PATH, Errno::EINVAL),
        (ImageError::LOOP, Errno::ELOOP),
        (ImageError::BAD_FILE, Errno::EBADF),
        (ImageError::BAD_RANGE, Errno::EINVAL),
        (ImageError::TOO_LARGE, Errno::EFBIG),
        (ImageError::CORRUPT, Errno::EIO),
        (ImageError::IO, Errno::EIO),
        (ImageError::TOO_MANY_FILES, Errno::ENFILE),
        (ImageError::UNSUPPORTED, Errno::EOPNOTSUPP),
        (ImageError(99), Errno::EIO),
    ];
    for (refusal, expected) in table {
        assert_eq!(refusal.errno(), expected);
        assert_eq!(Error::from(refusal).error(), expected);
    }
}

#[ktest]
fn a_corrupt_page_is_an_io_error() {
    struct Corrupt;
    impl ImageSource for Corrupt {
        fn stat(&self, path: &[u8], _follow: bool) -> ImageResult<ImageStat> {
            Ok(ImageStat {
                kind: if path == b"/" {
                    kind::DIRECTORY
                } else {
                    kind::REGULAR
                },
                mode: 0o644,
                size: 10,
                ino: 7,
            })
        }
        fn open(&self, _path: &[u8], _flags: u32) -> ImageResult<(u32, ImageStat)> {
            Ok((1, self.stat(b"/f", false)?))
        }
        fn readdir(&self, _file: u32, _cookie: u32) -> ImageResult<(Vec<ImageDirent>, u32)> {
            Err(ImageError::CORRUPT)
        }
        fn readlink(&self, _path: &[u8]) -> ImageResult<Vec<u8>> {
            Err(ImageError::IO)
        }
        fn read(&self, _file: u32, _offset: u64, len: u32) -> ImageResult<Vec<u8>> {
            // More than asked: refused.
            Ok(vec![0; len as usize + 1])
        }
        fn close(&self, _file: u32) -> ImageResult<()> {
            Err(ImageError::TRANSPORT)
        }
    }
    let fs = ImageFs::new(Arc::new(Corrupt)).unwrap();
    let root = fs.root_inode();
    assert_eq!(
        errno(root.readdir_at(0, &mut Listing::default())),
        Errno::EIO
    );
    let file = root.lookup("f").unwrap();
    assert_eq!(errno(file.read_bytes_at(0, &mut [0u8; 4])), Errno::EIO);
    let _ = file.downcast_ref::<ImageInode>().unwrap();
}

#[ktest]
fn a_dropped_filesystem_leaves_its_inodes_failing() {
    let (_, fs) = mount(MemImage::new().file("/f", b"abc").dir("/d"));
    let file = fs.root_inode().lookup("f").unwrap();
    let dir = fs.root_inode().lookup("d").unwrap();
    drop(fs);
    assert_eq!(errno(file.read_bytes_at(0, &mut [0u8; 3])), Errno::EIO);
    assert_eq!(
        errno(dir.readdir_at(0, &mut Listing::default())),
        Errno::EIO
    );
    assert_eq!(errno(dir.lookup("x")), Errno::EIO);
}
