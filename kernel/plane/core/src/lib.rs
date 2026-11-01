//! The engine's side of the data plane (SDD 29), `no_std` with `alloc`.
//!
//! A task exchanges Arrow batches with the engine through three calls: `batch_get`, which
//! maps the next batch of an input slot into it read only; `batch_put`, which hands the
//! record it wrote in an output slot's window back, the window leaving its address space;
//! and `port_call`, its one channel that is not Arrow. This crate is what the engine does on
//! the other side of them:
//!
//! - [`port`]: [`DataPlane`], the port the engine is written against, the same on the
//!   Moruna kernel and on Linux (`moruna-plane-linux` is its other adapter);
//! - [`plane`]: [`KernelPlane`], the port on the kernel: slots, ownership moved between
//!   tasks by moving handles, fan-out as read-only shares of one batch, residency;
//! - [`arena`]: the memory objects batches and windows live in, within a budget;
//! - [`verify`]: the record check, run before anything else of the engine sees a record;
//! - [`calls`]: the kernel calls all of it is built on, and [`kernel`], those calls through
//!   `moruna-sys`;
//! - `conformance` (feature `conformance`): the cases every adapter of the port passes, and
//!   `model` (feature `model`): the kernel's calls modelled on a host, which runs them
//!   against [`KernelPlane`].

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod arena;
pub mod calls;
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
#[cfg(any(target_os = "none", feature = "native"))]
pub mod kernel;
#[cfg(any(test, feature = "model"))]
pub mod model;
pub mod plane;
pub mod port;
pub mod verify;

#[cfg(test)]
mod tests;

pub use plane::KernelPlane;
pub use port::{BatchId, CallId, DataPlane, Event, PlaneError, Residency, Shape, TaskId};
