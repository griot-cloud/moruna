//! What can go wrong reading and unpacking an image, each with its cause named.

use std::io;
use std::path::PathBuf;

/// An image that cannot be read or unpacked.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file or directory could not be read or written.
    #[error("{what}: {source}")]
    Io {
        /// What was being done, and to which path.
        what: String,
        /// The system's answer.
        #[source]
        source: io::Error,
    },
    /// The layout, the manifest or the config is not what an OCI image is.
    #[error("{0}")]
    Format(String),
    /// A blob's bytes do not hash to the digest it is named by.
    #[error("{digest}: the bytes read hash to {actual} (a blob is named by its sha256)")]
    Digest {
        /// The digest the blob was named by.
        digest: String,
        /// What the bytes hash to.
        actual: String,
    },
    /// A blob is not the size its descriptor says.
    #[error("{digest}: the descriptor says {declared} bytes and {actual} were read")]
    Size {
        /// The blob's digest.
        digest: String,
        /// The size the descriptor declares.
        declared: u64,
        /// The bytes read.
        actual: u64,
    },
    /// A layer's media type is not one this crate reads.
    #[error(
        "{digest}: the layer's media type `{media_type}` is not one this build reads (a tar, \
         gzip or zstd layer, OCI or Docker)"
    )]
    MediaType {
        /// The layer's digest.
        digest: String,
        /// The media type found.
        media_type: String,
    },
    /// A layer's entry would write outside the root or is not one an image may hold.
    #[error("{layer}: the entry `{path}` {reason}")]
    Entry {
        /// The layer's digest.
        layer: String,
        /// The entry's path in the archive.
        path: String,
        /// Why it is refused.
        reason: String,
    },
    /// A cache directory is in a state this crate did not leave it in.
    #[error("{}: {reason}", path.display())]
    Cache {
        /// The directory.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
}

/// This crate's result.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// An I/O failure while doing `what`.
    pub(crate) fn io(what: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            what: what.into(),
            source,
        }
    }

    /// An entry of `layer` refused.
    pub(crate) fn entry(layer: &str, path: &str, reason: impl Into<String>) -> Error {
        Error::Entry {
            layer: layer.to_string(),
            path: path.to_string(),
            reason: reason.into(),
        }
    }
}
