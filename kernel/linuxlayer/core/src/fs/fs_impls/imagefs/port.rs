// SPDX-License-Identifier: MPL-2.0

//! The port the image root is built on: the read-only part of `moruna.imagefs` 3.0
//! (SDD 27 d.1).
//!
//! On the target, the protocol's client answers it over the task's `filesystem`
//! channel (an adapter outside this crate, which has no `imagefs-protocol`). In host
//! tests, an image held in memory answers it. The methods, flags, kinds and error
//! numbers are the protocol's own, so the adapter only forwards.

use crate::prelude::*;

/// The most bytes one `read` answers with (`imagefs_protocol::READ_MAX`).
pub(crate) const READ_MAX: u32 = 32 * 1024;

/// `open`'s flags that a read-only client sends.
pub(crate) mod open_flags {
    /// Refuse anything but a directory.
    pub(crate) const DIRECTORY: u32 = 16;
    /// Do not follow a symbolic link at the path's end.
    pub(crate) const NOFOLLOW: u32 = 32;
}

/// What a name is, in a stat and in a directory entry.
pub(crate) mod kind {
    pub(crate) const DIRECTORY: u8 = 1;
    pub(crate) const REGULAR: u8 = 2;
    pub(crate) const SYMLINK: u8 = 3;
}

/// What the image says of a file: the protocol's `Stat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageStat {
    pub(crate) kind: u8,
    /// Permission bits.
    pub(crate) mode: u32,
    /// A file's bytes, or a link's target length.
    pub(crate) size: u64,
    /// Unique over the image.
    pub(crate) ino: u64,
}

/// One directory entry: its name, its kind and its inode number.
pub(crate) type ImageDirent = (Vec<u8>, u8, u64);

/// A refusal, by the protocol's error number. Zero is a failed channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageError(pub(crate) u32);

impl ImageError {
    pub(crate) const TRANSPORT: Self = Self(0);
    pub(crate) const NOT_FOUND: Self = Self(1);
    pub(crate) const NOT_DIRECTORY: Self = Self(2);
    pub(crate) const IS_DIRECTORY: Self = Self(3);
    pub(crate) const READ_ONLY: Self = Self(4);
    pub(crate) const NO_SPACE: Self = Self(5);
    pub(crate) const EXISTS: Self = Self(6);
    pub(crate) const NOT_EMPTY: Self = Self(7);
    pub(crate) const BAD_PATH: Self = Self(8);
    pub(crate) const LOOP: Self = Self(9);
    pub(crate) const BAD_FILE: Self = Self(10);
    pub(crate) const BAD_RANGE: Self = Self(11);
    pub(crate) const TOO_LARGE: Self = Self(12);
    pub(crate) const CORRUPT: Self = Self(13);
    pub(crate) const IO: Self = Self(14);
    pub(crate) const TOO_MANY_FILES: Self = Self(15);
    pub(crate) const UNSUPPORTED: Self = Self(16);

    /// The Linux error a program sees.
    pub(crate) fn errno(self) -> Errno {
        match self {
            Self::NOT_FOUND => Errno::ENOENT,
            Self::NOT_DIRECTORY => Errno::ENOTDIR,
            Self::IS_DIRECTORY => Errno::EISDIR,
            Self::READ_ONLY => Errno::EROFS,
            Self::NO_SPACE => Errno::ENOSPC,
            Self::EXISTS => Errno::EEXIST,
            Self::NOT_EMPTY => Errno::ENOTEMPTY,
            Self::BAD_PATH | Self::BAD_RANGE => Errno::EINVAL,
            Self::LOOP => Errno::ELOOP,
            Self::BAD_FILE => Errno::EBADF,
            Self::TOO_LARGE => Errno::EFBIG,
            Self::TOO_MANY_FILES => Errno::ENFILE,
            Self::UNSUPPORTED => Errno::EOPNOTSUPP,
            Self::CORRUPT | Self::IO | Self::TRANSPORT => Errno::EIO,
            // A number this client does not know.
            _ => Errno::EIO,
        }
    }
}

impl From<ImageError> for Error {
    fn from(error: ImageError) -> Self {
        Error::with_message(error.errno(), "the image filesystem refused")
    }
}

/// What a request of the port answers.
pub(crate) type ImageResult<T> = core::result::Result<T, ImageError>;

/// The read-only requests of `moruna.imagefs` 3.0, on one task's connection.
pub(crate) trait ImageSource: Send + Sync {
    /// What `path` (absolute) is; a link at its end is followed when `follow`.
    fn stat(&self, path: &[u8], follow: bool) -> ImageResult<ImageStat>;
    /// Opens `path` with `flags`; the file's number on the connection and its stat.
    fn open(&self, path: &[u8], flags: u32) -> ImageResult<(u32, ImageStat)>;
    /// Directory `file`'s entries from `cookie` (0 first) and the next cookie; no
    /// entries at the end.
    fn readdir(&self, file: u32, cookie: u32) -> ImageResult<(Vec<ImageDirent>, u32)>;
    /// A link's target.
    fn readlink(&self, path: &[u8]) -> ImageResult<Vec<u8>>;
    /// Up to `len` (at most [`READ_MAX`]) bytes of `file` from `offset`; fewer at its
    /// end, none past it.
    fn read(&self, file: u32, offset: u64, len: u32) -> ImageResult<Vec<u8>>;
    /// Closes `file`.
    fn close(&self, file: u32) -> ImageResult<()>;
}
