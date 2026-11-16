// SPDX-License-Identifier: MPL-2.0

//! The image's root filesystem: a client of Moruna's image filesystem (SDD 27).
//!
//! Moruna-written. Chosen over Asterinas's overlayfs on the layers' block devices: the
//! layers are Moruna's own format, checked page by page against a SHA-256 tree, which
//! no filesystem Asterinas carries reads; the server's page cache and kept mappings are
//! shared by every task, where a filesystem in each personality would cache the same
//! pages once a task; and the server already merges the layers as SDD 19 unpacks them.

mod fs;
mod port;
#[cfg(ktest)]
pub(in crate::fs) mod test;

pub(crate) use fs::ImageFs;
pub(crate) use port::ImageSource;
// The port's types, for the target's adapter (`start/image.rs`).
#[cfg(target_os = "none")]
pub(crate) use port::{ImageDirent, ImageError, ImageResult, ImageStat};
