//! Amoru engine bridge: hosts a kernel inside Polars (05-adapters f.5, AD-I6).
//!
//! Design: `architecture/sdd/05-adapters.md` f.5. The bridge is a pure wrapper: it converts the
//! `Series` Polars hands an expression plugin into a [`Payload`], calls
//! [`amoru_kernel::Kernel::apply`] with [`amoru_kernel::NoState`], and returns the first output
//! column as a `Series`. There is no kernel logic here, which is what makes S7 checkable: the
//! same kernel object runs in the runtime and in Polars, and the two produce the same bytes.
//!
//! The plugin contract is one output series, so a kernel with more than one output column is not
//! exposed through Polars.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
// Every fallible call into Amoru returns the contract's `AmoruError` (contracts d.14), whose
// size is fixed by that crate and is above clippy's 128 byte threshold.
#![allow(clippy::result_large_err)]

#[cfg(feature = "polars")]
mod ffi;
#[cfg(feature = "polars")]
mod plugin;

#[cfg(feature = "polars")]
pub use plugin::{polars_plugin, run_kernel};
