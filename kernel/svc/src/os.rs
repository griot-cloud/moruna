//! The kernel calls the SDK is built on: `moruna-sys` on the Moruna microkernel, and
//! on a host the model of the same channel ABI in [`crate::sim`] (SDD 23, f.7). The
//! rest of the SDK names only what this module exports, so it is the same code on both.
//! A Linux program on the kernel (feature `native`, SDD 30) is on the kernel's calls too.

#[cfg(any(target_os = "none", feature = "native"))]
pub use moruna_sys::{Channel, Handle, WaitItem, now, wait, wait_one};

#[cfg(not(any(target_os = "none", feature = "native")))]
pub use crate::sim::{Channel, Handle, WaitItem, now, wait, wait_one};
