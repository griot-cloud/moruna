//! The image filesystem's reader (SDD 27): `no_std`, safe, and the same code on the
//! kernel, where the `imagefs` server runs it over block devices, and on a host,
//! where `moruna-image` reads back the layer files it writes with it.
//!
//! - [`format`]: the layer file, byte by byte, and the layout its counts give;
//! - [`volume`]: one layer's nodes, names and extents, read through verified pages;
//! - [`overlay`]: the layers merged, whiteouts and opaque directories applied;
//! - [`fs`]: paths resolved over the merged image and a task's scratch directory;
//! - [`cache`]: the bounded page cache that reads a page only when it is touched and
//!   believes it only once it matches its hash;
//! - [`map`]: how a mapping of a file's pages is filled;
//! - [`scratch`]: the per-task writable directory and its quota.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cache;
pub mod error;
pub mod format;
pub mod fs;
pub mod map;
pub mod overlay;
pub mod scratch;
pub mod sha256;
pub mod volume;

#[cfg(test)]
mod tests;

pub use error::{Error, Result};
