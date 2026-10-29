//! A task's open files (SDD 28, f.5): the console's three streams, the three device
//! nodes the table allows (`/dev/null`, `/dev/zero`, `/dev/urandom`), and files and
//! directories of the image and the scratch directory, which are the image
//! filesystem's (`moruna.imagefs`, SDD 27 d.1), translated to Linux's meaning.

use alloc::vec::Vec;

use imagefs_protocol::{Dirent, Stat as FsStat, imagefs, kind};
use moruna_linux::types::{Stat, dtype, errno, mode};
use moruna_svc::CallError;

/// A device node of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    Null,
    Zero,
    Random,
}

/// An open file description, which `dup` shares.
pub enum Open {
    /// Standard input (0), output (1) or error (2): output goes to the task's capture
    /// and the console, input is at its end.
    Console(u8),
    Device(Device),
    /// A file or directory of the image filesystem: its number on the task's
    /// connection, its path, what it is, and the description's offset and flags.
    Fs(FsFile),
}

pub struct FsFile {
    pub number: u32,
    pub path: Vec<u8>,
    pub stat: FsStat,
    pub offset: u64,
    /// `O_ACCMODE`, `O_APPEND`, `O_NONBLOCK` as opened (what `F_GETFL` gives).
    pub flags: u64,
    /// A directory's entries, read at its first `getdents64` (`.` and `..` first), and
    /// how many have been given.
    pub listing: Option<Vec<Dirent>>,
}

impl FsFile {
    pub fn is_dir(&self) -> bool {
        self.stat.kind == kind::DIRECTORY
    }
}

/// The errno for an image filesystem's refusal.
pub fn errno_of(e: CallError<imagefs::Error>) -> u64 {
    use imagefs::Error as E;
    match e {
        CallError::Remote(e) => match e {
            E::NotFound | E::BadPath => errno::ENOENT,
            E::NotDirectory => errno::ENOTDIR,
            E::IsDirectory => errno::EISDIR,
            E::ReadOnly => errno::EROFS,
            E::NoSpace => errno::ENOSPC,
            E::Exists => errno::EEXIST,
            E::NotEmpty => errno::ENOTEMPTY,
            E::Loop => errno::ELOOP,
            E::BadFile => errno::EBADF,
            E::BadRange => errno::EINVAL,
            E::TooLarge => errno::EFBIG,
            E::Corrupt | E::Io => errno::EIO,
            E::TooManyFiles => errno::EMFILE,
            E::Unsupported => errno::EOPNOTSUPP,
        },
        _ => errno::EIO,
    }
}

/// Linux's `stat` of what the image filesystem says.
pub fn stat_of(s: &FsStat) -> Stat {
    let (kind_bits, nlink) = match s.kind {
        kind::DIRECTORY => (mode::S_IFDIR, 2),
        kind::SYMLINK => (mode::S_IFLNK, 1),
        _ => (mode::S_IFREG, 1),
    };
    Stat {
        ino: s.ino,
        mode: kind_bits | (s.mode & 0o7777),
        nlink,
        rdev: 0,
        size: s.size,
    }
}

/// `d_type` of an image filesystem entry's kind.
pub fn dtype_of(k: u8) -> u8 {
    match k {
        kind::DIRECTORY => dtype::DT_DIR,
        kind::SYMLINK => dtype::DT_LNK,
        _ => dtype::DT_REG,
    }
}

/// The `stat` of a console stream: a character device, as a pseudo-terminal's is.
pub fn console_stat(stream: u8) -> Stat {
    Stat {
        ino: 1 + stream as u64,
        mode: mode::S_IFCHR | 0o620,
        nlink: 1,
        rdev: 0x8800,
        size: 0,
    }
}

/// The `stat` of a device node: `null` (1, 3), `zero` (1, 5), `urandom` (1, 9).
pub fn device_stat(d: Device) -> Stat {
    let minor = match d {
        Device::Null => 3,
        Device::Zero => 5,
        Device::Random => 9,
    };
    Stat {
        ino: 16 + minor,
        mode: mode::S_IFCHR | 0o666,
        nlink: 1,
        rdev: 0x100 | minor,
        size: 0,
    }
}

/// The device node a path names, when it is one of the table's.
pub fn device(path: &[u8]) -> Option<Device> {
    match path {
        b"/dev/null" => Some(Device::Null),
        b"/dev/zero" => Some(Device::Zero),
        b"/dev/urandom" | b"/dev/random" => Some(Device::Random),
        _ => None,
    }
}
