//! Moruna component 5, the kernel adapters: the Python kernel adapter, behind the `python`
//! feature.
//!
//! Design: `architecture/sdd/05-adapters.md`. Adapters make things that are not Rust
//! [`moruna_kernel::Kernel`]s into `Kernel`s. This crate holds the Python adapter; the two engine
//! bridges are the separate thin crates `moruna-polars` and `moruna-datafusion` (preamble 6.1).
//!
//! The Python adapter is where the zero copy claim meets the interpreter. A payload crosses into
//! Python by pointer (Arrow through the C Data Interface, tensors through DLPack) and comes back
//! the same way; the only copy the adapter ever makes is the single boundary copy of a returned
//! host payload whose buffers the arena does not own (AD-I2).

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
// Every fallible function here returns the contract's `MorunaError` (contracts d.14), whose size
// is fixed by that crate and is above clippy's 128 byte threshold. The adapter may not box it:
// the error type crosses every component boundary and is the contract's to change.
#![allow(clippy::result_large_err)]

/// `GilState` is `moruna_kernel::GilState` (contracts d.7), the type the run report carries.
pub use moruna_kernel::GilState;

pub mod guard;
pub mod land;

use moruna_kernel::{Seq, StageId};

/// The stage an `MorunaError::Kernel` from an adapter carries.
///
/// `Kernel::apply` receives a `Payload`, not a morsel, so an adapter never learns which stage it
/// is or which morsel it is holding: a kernel that knew its position could not also be a Polars
/// expression or a DataFusion function, which S7 requires. AD-I7 names both fields all the same,
/// so the adapter fills them with a sentinel the scheduler can recognise and replace rather than
/// with a plausible lie. Reported.
pub const UNKNOWN_STAGE: StageId = StageId::MAX;
/// The morsel an `MorunaError::Kernel` from an adapter carries; see [`UNKNOWN_STAGE`].
/// Also what a packaged kernel's runtime carries (17 f.4, f.8).
pub const UNKNOWN_SEQ: Seq = Seq::MAX;

#[cfg(feature = "python")]
pub mod python;

#[cfg(feature = "python")]
pub use python::{PyKernel, PyKernelSpec, PyKernelStats, python_build_info, python_gil_enabled};
