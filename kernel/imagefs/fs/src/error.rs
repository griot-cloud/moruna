//! What a filesystem operation fails with. Each is one of the protocol's errors
//! (SDD 27, d.1); `Corrupt` and `Io` carry what happened, for the server's log.

use alloc::string::String;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    NotFound,
    NotDirectory,
    IsDirectory,
    /// A write, a creation or a removal outside the scratch directory.
    ReadOnly,
    /// The scratch directory's quota would be passed.
    NoSpace,
    Exists,
    NotEmpty,
    /// A path that is empty, relative, or holds a name no directory entry has.
    BadPath,
    /// More than 40 symbolic links on one path.
    Loop,
    /// A range that is not one: an offset not on a page, or past the end.
    BadRange,
    /// A request larger than one answer carries.
    TooLarge,
    /// A layer's bytes are not what its hashes or its format say.
    Corrupt(String),
    /// A device failed to read.
    Io(String),
    /// A request the object it names does not support.
    Unsupported,
}

pub type Result<T> = core::result::Result<T, Error>;
