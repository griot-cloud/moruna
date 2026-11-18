//! Implementation specific, non-standard path aliases

#[cfg(any(target_os = "linux", target_os = "moruna"))]
#[path = "linux.rs"]
mod sys;

#[cfg(target_os = "redox")]
#[path = "redox.rs"]
mod sys;

pub use sys::*;
