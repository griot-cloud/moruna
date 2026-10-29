//! `moruna.imagefs` 1.0 (SDD 27, d.1): a task's root filesystem. The image is read
//! only and merged from its layers; `/tmp` is the task's scratch directory, the only
//! path a write, a creation or a removal reaches. A connection is one task: its open
//! files and its scratch directory are its own and end with it. File reads are small
//! (`read`, at most `READ_MAX` bytes an answer); a file's pages are handed over whole
//! by `map`, a read-only memory object the task maps, with no copy of the bytes
//! through a message. The Linux personality (KC1) turns `openat`, `read`, `pread`,
//! `getdents`, `readlink`, `fstat` and `mmap` of a file into these requests.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::ToString, vec, vec::Vec};

use moruna_svc::{
    Decoder, Encoder, Memory, ProtocolError, Rights, Wire, WireError,
    conformance::{Case, Fail, Probe, Suite},
};

/// The most bytes one `read` answers with.
pub const READ_MAX: u32 = 32 * 1024;
/// The most bytes one `map` covers: a memory object's largest size.
pub const MAP_MAX: u64 = 256 << 20;
/// The most files one connection holds open.
pub const FILES_MAX: usize = 1024;
/// The body bytes one `readdir` answer fills at most, leaving room for the frame.
pub const READDIR_BYTES: usize = 48 * 1024;

/// `open`'s flags.
pub mod open {
    /// Open for writing (scratch files only).
    pub const WRITE: u32 = 1;
    /// Make the file if it is not there (in the scratch directory only).
    pub const CREATE: u32 = 2;
    /// With `CREATE`: refuse a file that is there.
    pub const EXCLUSIVE: u32 = 4;
    /// Cut a scratch file to nothing.
    pub const TRUNCATE: u32 = 8;
    /// Refuse anything but a directory.
    pub const DIRECTORY: u32 = 16;
    /// Do not follow a symbolic link at the path's end.
    pub const NOFOLLOW: u32 = 32;
    pub const ALL: u32 = 63;
}

/// What a name is, in `Stat` and `readdir`.
pub mod kind {
    pub const DIRECTORY: u8 = 1;
    pub const REGULAR: u8 = 2;
    pub const SYMLINK: u8 = 3;
}

/// Bytes, carried as one run rather than byte by byte; the same bytes on the wire as
/// a `Vec<u8>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl Wire for Bytes {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_len(self.0.len())?;
        e.put_raw(&self.0);
        Ok(())
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let n = d.take_len()?;
        Ok(Bytes(d.take_raw(n)?.to_vec()))
    }
}

impl From<&[u8]> for Bytes {
    fn from(b: &[u8]) -> Bytes {
        Bytes(b.to_vec())
    }
}

/// What `stat`, `fstat` and `open` say of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: u8,
    pub mode: u32,
    pub size: u64,
    pub ino: u64,
}

impl Wire for Stat {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.kind.encode(e)?;
        self.mode.encode(e)?;
        self.size.encode(e)?;
        self.ino.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Stat {
            kind: u8::decode(d)?,
            mode: u32::decode(d)?,
            size: u64::decode(d)?,
            ino: u64::decode(d)?,
        })
    }
}

/// The server's counts: its page cache, its devices and its mappings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Page reads the cache answered.
    pub hits: u64,
    /// Page reads it did not, each a page read from a device.
    pub misses: u64,
    /// Pages read from the layers' devices, headers and mappings included.
    pub pages_read: u64,
    /// Requests sent to the layers' devices.
    pub block_requests: u64,
    pub evictions: u64,
    /// Pages refused for not matching their hash.
    pub corrupt: u64,
    /// The pages the cache holds, and can hold.
    pub resident: u64,
    pub capacity: u64,
    /// `map` requests answered with a memory object made before.
    pub map_hits: u64,
    /// Pages of mappings read from a device straight into the mapping.
    pub map_pages_read: u64,
    /// Pages of mappings copied from the cache.
    pub map_pages_copied: u64,
}

impl Wire for Stats {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        for v in [
            self.hits,
            self.misses,
            self.pages_read,
            self.block_requests,
            self.evictions,
            self.corrupt,
            self.resident,
            self.capacity,
            self.map_hits,
            self.map_pages_read,
            self.map_pages_copied,
        ] {
            v.encode(e)?;
        }
        Ok(())
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let mut v = [0u64; 11];
        for x in &mut v {
            *x = u64::decode(d)?;
        }
        Ok(Stats {
            hits: v[0],
            misses: v[1],
            pages_read: v[2],
            block_requests: v[3],
            evictions: v[4],
            corrupt: v[5],
            resident: v[6],
            capacity: v[7],
            map_hits: v[8],
            map_pages_read: v[9],
            map_pages_copied: v[10],
        })
    }
}

/// One directory entry: its name, its kind and its inode number.
pub type Dirent = (Bytes, u8, u64);

moruna_svc::protocol! {
    /// A task's root filesystem: the image, read only, and its scratch directory.
    pub protocol imagefs {
        name = "moruna.imagefs";
        major = 1;
        minor = 0;
        requests {
            /// Opens `path` (absolute) with `open` flags; `mode` is a new file's. The
            /// file's number on this connection and what it is.
            1 => fn open(path: Bytes, flags: u32, mode: u32) -> (u32, Stat);
            /// What `path` is; a symbolic link at its end is followed when `follow`.
            2 => fn stat(path: Bytes, follow: bool) -> Stat;
            /// What open file `file` is.
            3 => fn fstat(file: u32) -> Stat;
            /// Directory `file`'s entries from `cookie` (0 first), in name order, and
            /// the cookie of the next; no entries at the end.
            4 => fn readdir(file: u32, cookie: u32) -> (Vec<Dirent>, u32);
            /// A symbolic link's target.
            5 => fn readlink(path: Bytes) -> Bytes;
            /// Up to `len` (at most `READ_MAX`) bytes of `file` from `offset`; fewer
            /// at its end, none past it.
            6 => fn read(file: u32, offset: u64, len: u32) -> Bytes;
            /// `len` bytes of image file `file` from `offset` (on a page), as a
            /// read-only memory object: the file's bytes, zero past its end.
            7 => fn map(file: u32, offset: u64, len: u64) -> Memory;
            /// Writes `data` at `offset` of a scratch file opened for writing; the
            /// bytes written.
            8 => fn write(file: u32, offset: u64, data: Bytes) -> u32;
            /// Sets a scratch file's length.
            9 => fn truncate(file: u32, size: u64) -> ();
            10 => fn close(file: u32) -> ();
            /// Makes a directory in the scratch directory.
            11 => fn mkdir(path: Bytes, mode: u32) -> ();
            /// Removes a file or an empty directory of the scratch directory.
            12 => fn remove(path: Bytes) -> ();
            /// Sets the scratch directory's quota in bytes, before anything is in it.
            13 => fn quota(bytes: u64) -> ();
            /// The server's counts.
            14 => fn stats() -> Stats;
        }
        events {}
        errors {
            1 => NotFound;
            2 => NotDirectory;
            3 => IsDirectory;
            /// A write, a creation or a removal outside the scratch directory.
            4 => ReadOnly;
            /// The scratch directory's quota.
            5 => NoSpace;
            6 => Exists;
            7 => NotEmpty;
            /// A path that is not absolute, is too long, or holds NUL.
            8 => BadPath;
            /// More than 40 symbolic links.
            9 => Loop;
            /// No open file of this connection has that number, or it was not opened
            /// for what was asked.
            10 => BadFile;
            /// An offset not on a page, or a range past the file's end.
            11 => BadRange;
            12 => TooLarge;
            /// A layer's bytes are not what their hashes say.
            13 => Corrupt;
            /// A layer's device failed.
            14 => Io;
            15 => TooManyFiles;
            /// A request the file does not support (a mapping of a scratch file), an
            /// unknown flag, or a quota set after the scratch directory was used.
            16 => Unsupported;
        }
    }
}

fn wire<E: core::fmt::Debug>(e: E) -> Fail {
    format!("{e:?}")
}

fn path(e: &mut Encoder, p: &[u8]) -> Result<(), Fail> {
    Bytes(p.to_vec()).encode(e).map_err(wire)
}

fn decode_reply<T: Wire>(p: &mut Probe, ordinal: u16, body: Encoder) -> Result<T, Fail> {
    let mut reply = p.request(ordinal, body)?.reply()?;
    let mut d = reply.decoder();
    let value = T::decode(&mut d).map_err(wire)?;
    d.finish().map_err(wire)?;
    Ok(value)
}

fn open_req(p: &[u8], flags: u32, mode: u32) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    path(&mut e, p)?;
    flags.encode(&mut e).map_err(wire)?;
    mode.encode(&mut e).map_err(wire)?;
    Ok(e)
}

fn open_file(p: &mut Probe, at: &[u8], flags: u32) -> Result<(u32, Stat), Fail> {
    decode_reply(p, 1, open_req(at, flags, 0o644)?)
}

fn file_req(file: u32) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    file.encode(&mut e).map_err(wire)?;
    Ok(e)
}

fn readdir_all(p: &mut Probe, file: u32) -> Result<Vec<Dirent>, Fail> {
    let mut all = Vec::new();
    let mut cookie = 0u32;
    loop {
        let mut e = file_req(file)?;
        cookie.encode(&mut e).map_err(wire)?;
        let (entries, next): (Vec<Dirent>, u32) = decode_reply(p, 4, e)?;
        if entries.is_empty() {
            return Ok(all);
        }
        if next <= cookie {
            return Err(format!("readdir's cookie went from {cookie} to {next}"));
        }
        all.extend(entries);
        cookie = next;
    }
}

fn read_req(file: u32, offset: u64, len: u32) -> Result<Encoder, Fail> {
    let mut e = file_req(file)?;
    offset.encode(&mut e).map_err(wire)?;
    len.encode(&mut e).map_err(wire)?;
    Ok(e)
}

fn code(e: imagefs::Error) -> u32 {
    e.code()
}

/// The root is a directory listing `dev`, `proc` and `tmp` in order.
fn root(p: &mut Probe) -> Result<(), Fail> {
    let (file, stat) = open_file(p, b"/", open::DIRECTORY)?;
    if stat.kind != kind::DIRECTORY {
        return Err(format!("/ is of kind {}", stat.kind));
    }
    let entries = readdir_all(p, file)?;
    let names: Vec<&[u8]> = entries.iter().map(|(n, _, _)| n.0.as_slice()).collect();
    for want in [&b"dev"[..], b"proc", b"tmp"] {
        if !names.contains(&want) {
            return Err(format!(
                "/ does not list {}",
                core::str::from_utf8(want).unwrap_or("?")
            ));
        }
    }
    if names.windows(2).any(|w| w[0] >= w[1]) {
        return Err("/'s entries are not in name order".to_string());
    }
    let mut e = file_req(file)?;
    (entries.len() as u32 + 1000).encode(&mut e).map_err(wire)?;
    let (past, _): (Vec<Dirent>, u32) = decode_reply(p, 4, e)?;
    if !past.is_empty() {
        return Err("readdir past the end listed entries".to_string());
    }
    decode_reply::<()>(p, 10, file_req(file)?)
}

/// Paths that are not absolute, too long or hold NUL; names nothing names.
fn paths(p: &mut Probe) -> Result<(), Fail> {
    let bad = code(imagefs::Error::BadPath);
    for at in [&b"tmp"[..], b"", b"/a\0b"] {
        let mut e = Encoder::new();
        path(&mut e, at)?;
        false.encode(&mut e).map_err(wire)?;
        p.request(2, e)?.failure(bad)?;
    }
    let mut e = Encoder::new();
    path(&mut e, &vec![b'/'; 5000])?;
    true.encode(&mut e).map_err(wire)?;
    p.request(2, e)?.failure(bad)?;
    let mut e = Encoder::new();
    path(&mut e, b"/no such name here/at all")?;
    true.encode(&mut e).map_err(wire)?;
    p.request(2, e)?.failure(code(imagefs::Error::NotFound))?;
    let mut e = Encoder::new();
    path(&mut e, b"/")?;
    p.request(5, e)?.failure(bad)
}

/// File numbers this connection does not hold, and a file closed twice.
fn bad_files(p: &mut Probe) -> Result<(), Fail> {
    let bad = code(imagefs::Error::BadFile);
    p.request(3, file_req(77_777)?)?.failure(bad)?;
    p.request(6, read_req(77_777, 0, 1)?)?.failure(bad)?;
    let (file, _) = open_file(p, b"/", 0)?;
    decode_reply::<()>(p, 10, file_req(file)?)?;
    p.request(10, file_req(file)?)?.failure(bad)?;
    p.request(3, file_req(file)?)?.failure(bad)
}

/// Nothing outside the scratch directory is written, made or removed.
fn read_only(p: &mut Probe) -> Result<(), Fail> {
    let ro = code(imagefs::Error::ReadOnly);
    p.request(
        1,
        open_req(b"/conformance-new", open::CREATE | open::WRITE, 0o644)?,
    )?
    .failure(ro)?;
    p.request(1, open_req(b"/", open::WRITE, 0)?)?
        .failure(code(imagefs::Error::IsDirectory))?;
    let mut e = Encoder::new();
    path(&mut e, b"/conformance-dir")?;
    0o755u32.encode(&mut e).map_err(wire)?;
    p.request(11, e)?.failure(ro)?;
    let mut e = Encoder::new();
    path(&mut e, b"/tmp")?;
    p.request(12, e)?.failure(ro)?;
    let (file, _) = open_file(p, b"/", 0)?;
    let mut e = file_req(file)?;
    0u64.encode(&mut e).map_err(wire)?;
    Bytes(vec![1]).encode(&mut e).map_err(wire)?;
    p.request(8, e)?.failure(code(imagefs::Error::BadFile))?;
    p.request(6, read_req(file, 0, 1)?)?
        .failure(code(imagefs::Error::IsDirectory))
}

/// A scratch file is made, written, read back, listed, cut and removed; a second
/// exclusive creation is refused; a directory is made and removed.
fn scratch(p: &mut Probe) -> Result<(), Fail> {
    let at = b"/tmp/conformance";
    let (file, stat) = open_file(p, at, open::CREATE | open::EXCLUSIVE | open::WRITE)?;
    if stat.kind != kind::REGULAR || stat.size != 0 {
        return Err(format!("a new scratch file is {stat:?}"));
    }
    p.request(1, open_req(at, open::CREATE | open::EXCLUSIVE, 0)?)?
        .failure(code(imagefs::Error::Exists))?;
    let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let mut e = file_req(file)?;
    5u64.encode(&mut e).map_err(wire)?;
    Bytes(data.clone()).encode(&mut e).map_err(wire)?;
    let wrote: u32 = decode_reply(p, 8, e)?;
    if wrote as usize != data.len() {
        return Err(format!("wrote {wrote} of {}", data.len()));
    }
    let Bytes(back) = decode_reply(p, 6, read_req(file, 5, READ_MAX)?)?;
    if back != data {
        return Err("a scratch file read back is not what was written".to_string());
    }
    let stat: Stat = decode_reply(p, 3, file_req(file)?)?;
    if stat.size != 10_005 {
        return Err(format!("a scratch file's size is {}", stat.size));
    }
    let mut e = file_req(file)?;
    3u64.encode(&mut e).map_err(wire)?;
    decode_reply::<()>(p, 9, e)?;
    let Bytes(cut) = decode_reply(p, 6, read_req(file, 0, 100)?)?;
    if cut != [0, 0, 0] {
        return Err(format!("a file cut to 3 bytes reads {cut:?}"));
    }
    let mut e = file_req(file)?;
    0u64.encode(&mut e).map_err(wire)?;
    7u64.encode(&mut e).map_err(wire)?;
    p.request(7, e)?
        .failure(code(imagefs::Error::Unsupported))?;
    let (tmp, _) = open_file(p, b"/tmp", open::DIRECTORY)?;
    let listed = readdir_all(p, tmp)?;
    if !listed
        .iter()
        .any(|(n, k, _)| n.0 == b"conformance" && *k == kind::REGULAR)
    {
        return Err("/tmp does not list the file made in it".to_string());
    }
    let mut e = Encoder::new();
    path(&mut e, b"/tmp/conformance-d")?;
    0o755u32.encode(&mut e).map_err(wire)?;
    decode_reply::<()>(p, 11, e)?;
    p.request(1, open_req(b"/tmp/conformance-d", open::WRITE, 0)?)?
        .failure(code(imagefs::Error::IsDirectory))?;
    p.request(1, open_req(at, open::DIRECTORY, 0)?)?
        .failure(code(imagefs::Error::NotDirectory))?;
    for name in [&b"/tmp/conformance-d"[..], at] {
        let mut e = Encoder::new();
        path(&mut e, name)?;
        decode_reply::<()>(p, 12, e)?;
    }
    let mut e = Encoder::new();
    path(&mut e, at)?;
    true.encode(&mut e).map_err(wire)?;
    p.request(2, e)?.failure(code(imagefs::Error::NotFound))?;
    decode_reply::<()>(p, 10, file_req(file)?)
}

/// The quota is set once, and held.
fn quota(p: &mut Probe) -> Result<(), Fail> {
    let mut e = Encoder::new();
    8192u64.encode(&mut e).map_err(wire)?;
    decode_reply::<()>(p, 13, e)?;
    let (file, _) = open_file(p, b"/tmp/quota", open::CREATE | open::WRITE)?;
    let mut e = file_req(file)?;
    0u64.encode(&mut e).map_err(wire)?;
    Bytes(vec![1; 9000]).encode(&mut e).map_err(wire)?;
    p.request(8, e)?.failure(code(imagefs::Error::NoSpace))?;
    let mut e = Encoder::new();
    (1u64 << 20).encode(&mut e).map_err(wire)?;
    p.request(13, e)?.failure(code(imagefs::Error::Unsupported))
}

/// The first regular file of the image, found walking from the root.
fn some_file(p: &mut Probe) -> Result<(Vec<u8>, Stat), Fail> {
    let mut todo = vec![b"/".to_vec()];
    let mut visited = 0;
    while let Some(dir) = todo.pop() {
        visited += 1;
        if visited > 4096 {
            break;
        }
        let (file, _) = open_file(p, &dir, open::DIRECTORY)?;
        let entries = readdir_all(p, file)?;
        decode_reply::<()>(p, 10, file_req(file)?)?;
        for (name, k, _) in entries {
            let mut child = dir.clone();
            if child.len() > 1 {
                child.push(b'/');
            }
            child.extend_from_slice(&name.0);
            if k == kind::DIRECTORY && child != b"/tmp" {
                todo.push(child);
            } else if k == kind::REGULAR {
                let mut e = Encoder::new();
                path(&mut e, &child)?;
                false.encode(&mut e).map_err(wire)?;
                let stat: Stat = decode_reply(p, 2, e)?;
                if stat.size > 0 {
                    return Ok((child, stat));
                }
            }
        }
    }
    Err("the image holds no regular file with bytes to map".to_string())
}

/// A file of the image maps to its bytes, read only; a range off a page or past the
/// end is refused; a directory is not mapped.
fn map(p: &mut Probe) -> Result<(), Fail> {
    let (at, stat) = some_file(p)?;
    let (file, _) = open_file(p, &at, 0)?;
    let len = stat.size.min(READ_MAX as u64) as u32;
    let Bytes(read) = decode_reply(p, 6, read_req(file, 0, len)?)?;
    let mut e = file_req(file)?;
    0u64.encode(&mut e).map_err(wire)?;
    stat.size.encode(&mut e).map_err(wire)?;
    let memory: Memory = decode_reply(p, 7, e)?;
    let (_, rights) = memory.0.info().map_err(|e| e.name().to_string())?;
    if rights.contains(Rights::WRITE) {
        return Err("a mapping's memory object carries WRITE".to_string());
    }
    let pages = stat.size.div_ceil(4096) * 4096;
    let m = memory
        .map(0, pages, 0)
        .map_err(|e| format!("mapping it: {}", e.name()))?;
    let mut mapped = vec![0u8; pages as usize];
    m.read(0, &mut mapped).map_err(|e| e.name().to_string())?;
    if mapped[..read.len()] != read[..] {
        return Err(format!(
            "{}: map and read give other bytes",
            core::str::from_utf8(&at).unwrap_or("?")
        ));
    }
    if mapped[stat.size as usize..].iter().any(|b| *b != 0) {
        return Err("a mapping is not zero past the file's end".to_string());
    }
    let range = code(imagefs::Error::BadRange);
    for (offset, len) in [(1u64, 4096u64), (0, 0), (pages, 4096), (0, pages + 4096)] {
        let mut e = file_req(file)?;
        offset.encode(&mut e).map_err(wire)?;
        len.encode(&mut e).map_err(wire)?;
        p.request(7, e)?.failure(range)?;
    }
    let (root, _) = open_file(p, b"/", 0)?;
    let mut e = file_req(root)?;
    0u64.encode(&mut e).map_err(wire)?;
    4096u64.encode(&mut e).map_err(wire)?;
    p.request(7, e)?
        .failure(code(imagefs::Error::IsDirectory))?;
    p.request(6, read_req(file, 0, READ_MAX + 1)?)?
        .failure(code(imagefs::Error::TooLarge))?;
    let Bytes(past) = decode_reply(p, 6, read_req(file, stat.size + 10, 10)?)?;
    if !past.is_empty() {
        return Err("a read past the end returned bytes".to_string());
    }
    Ok(())
}

/// Unknown open flags are refused; stats answer.
fn flags_and_stats(p: &mut Probe) -> Result<(), Fail> {
    p.request(1, open_req(b"/", open::ALL + 1, 0)?)?
        .failure(code(imagefs::Error::Unsupported))?;
    let stats: Stats = decode_reply(p, 14, Encoder::new())?;
    if stats.capacity == 0 {
        return Err("a cache of no pages".to_string());
    }
    Ok(())
}

const CASES: &[Case] = &[
    Case {
        name: "open, readdir, close: the root lists dev, proc and tmp, in order",
        check: root,
    },
    Case {
        name: "stat, readlink: bad paths and missing names",
        check: paths,
    },
    Case {
        name: "fstat, read, close: numbers the connection does not hold",
        check: bad_files,
    },
    Case {
        name: "open, mkdir, remove, write: the image is read only",
        check: read_only,
    },
    Case {
        name: "the scratch directory: made, written, read, listed, cut, removed",
        check: scratch,
    },
    Case {
        name: "quota: set once, and held",
        check: quota,
    },
    Case {
        name: "map: an image file's bytes, read only; bad ranges refused",
        check: map,
    },
    Case {
        name: "open's unknown flags, and stats",
        check: flags_and_stats,
    },
];

/// The protocol's conformance suite. It needs an image with at least one regular file
/// that has bytes, and it uses `/tmp` on each fresh connection.
pub fn suite() -> Suite {
    Suite {
        id: imagefs::ID,
        requests: imagefs::REQUESTS,
        cases: CASES,
        timeout: 60_000_000_000,
    }
}
