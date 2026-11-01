//! The data-plane port on Linux (SDD 29 d.4, KA2): the engine's adapter, [`LinuxPlane`], and
//! the task's library, [`task`].
//!
//! A task on a Linux node is a sandboxed process. It exchanges Arrow batches with the engine
//! through the three calls of the kernel's plane, `batch_get`, `batch_put` and `port_call`,
//! made over one `SOCK_SEQPACKET` socket ([`wire`]), its descriptor 3. A batch is a sealed
//! memfd the task maps read only, the same pages for every consumer; a window is a memfd it maps
//! writable, unmaps at its put, and the engine seals before it checks the record, so the
//! checked bytes are the bytes every consumer reads (DP1). The engine above is written against
//! `moruna_plane::DataPlane` alone, and this adapter gives it `KernelPlane`'s events, refusals
//! and budget, which the port's conformance suite runs both through.
//!
//! [`stream`] turns records into a plain Arrow IPC stream and back, for a program that speaks
//! streams on its standard input and output rather than the three calls.
//!
//! The crate is Linux: memfd seals, `SCM_RIGHTS` and pidfds have no other home. Elsewhere it
//! builds its wire format and its streams only.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod stream;
pub mod wire;

#[cfg(target_os = "linux")]
pub mod engine;
#[cfg(target_os = "linux")]
pub mod sys;
#[cfg(target_os = "linux")]
pub mod task;

#[cfg(target_os = "linux")]
pub use engine::LinuxPlane;
