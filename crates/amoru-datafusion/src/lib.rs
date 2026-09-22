//! Amoru engine bridge: hosts a kernel inside DataFusion (05-adapters f.6, AD-I6).
//!
//! Design: `architecture/sdd/05-adapters.md` f.6. The bridge is a pure wrapper: it converts
//! DataFusion's `ColumnarValue`s into a [`Payload`], calls [`Kernel::apply`] with [`NoState`],
//! and returns the first output column. There is no kernel logic here, which is what makes S7
//! checkable: the same kernel object runs in the runtime and in DataFusion, and the two produce
//! the same bytes.
//!
//! DataFusion and Amoru share one `arrow` (preamble 6.2), so the conversion is a move of an
//! `ArrayRef`, not a copy and not an FFI hop.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
// Every fallible call into Amoru returns the contract's `AmoruError` (contracts d.14), whose
// size is fixed by that crate and is above clippy's 128 byte threshold.
#![allow(clippy::result_large_err)]

#[cfg(feature = "datafusion")]
mod udf;

#[cfg(feature = "datafusion")]
pub use udf::{KernelUdf, datafusion_udf};
