//! Moruna component 19: an OCI image as a root filesystem and a command (SDD 19).
//!
//! An image is a manifest, a config naming the command, and content-addressed layers. There is no
//! container runtime here: [`unpack`] reads an image from an [`ImageSource`], checks every digest
//! as it reads, and lays the layers over one another into a directory the sandbox mounts read
//! only (`moruna-sandbox`, SDD 19 e.3). The crate is pure Rust: it reads and writes files and runs
//! nothing, so it builds and tests wherever Moruna does; running the command is Linux's.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod digest;
mod error;
mod layer;
mod source;
mod unpack;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg(test)]
mod tests;

pub use digest::Digest;
pub use error::{Error, Result};
pub use source::{Config, Descriptor, ImageSource, Manifest, OciLayout};
pub use unpack::{COMPLETE, Image, unpack};

/// The directories every unpacked root holds, which the sandbox mounts onto (SDD 19 f.3): made
/// by [`unpack`] when the image has none, refused when the image has one that is not a directory.
pub const MOUNT_POINTS: [&str; 3] = ["proc", "dev", "tmp"];
