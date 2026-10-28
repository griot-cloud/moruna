//! The kernel calls the SDK is built on: `moruna-sys` on the Moruna microkernel, and
//! on a host the model of the same channel ABI in [`crate::sim`] (SDD 23, f.7). The
//! rest of the SDK names only what this module exports, so it is the same code on both.

#[cfg(target_os = "none")]
pub use moruna_sys::{Channel, Handle, WaitItem, now, wait, wait_one};

#[cfg(not(target_os = "none"))]
pub use crate::sim::{Channel, Handle, WaitItem, now, wait, wait_one};
