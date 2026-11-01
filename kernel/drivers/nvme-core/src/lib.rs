//! NVMe over PCI (NVM Express Base Specification 2.0, NVM Command Set 1.0) for drivers
//! that are ordinary processes (SDD 25, f.13): the controller's registers and how a
//! driver resets, enables and shuts it down; admin and I/O queue pairs in pinned
//! pages; the commands a block driver sends and the completions it reads; PRP lists;
//! and the Identify data a driver believes only once it has checked it.
//!
//! The device is untrusted input. Every value it returns is bounded before it is used:
//! a completion's command id, submission queue id and submission queue head; the queue
//! counts it grants; every Identify field a size, an index or an offset is taken from.
//! A value out of bounds is an error naming the value, never an index or a panic.
//!
//! Everything here reads and writes through [`moruna_mmio::Region`] and keeps time
//! through [`moruna_mmio::Clock`], so it is the same code on the kernel (a mapping of BAR 0 or of
//! a DMA memory object, the kernel's clock) and on a host, where it is tested against
//! a model controller (`tests`). [`engine::Engine`] is the whole of a driver's I/O
//! path that is not the kernel: queue choice, PRP lists, deadlines and recovery.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod command;
pub mod controller;
pub mod engine;
pub mod identify;
pub mod prp;
pub mod queue;

use alloc::string::String;
use core::fmt;

use moruna_abi::Error;

pub use command::{Command, Completion, Status};
pub use controller::{Cap, Controller};
pub use engine::{Engine, Io, IoOp, Settings};
pub use identify::{ControllerId, NamespaceId};
pub use queue::QueuePair;

/// The memory page size every Moruna driver uses, and the controller's (CC.MPS = 0).
pub const PAGE: usize = 4096;
/// Bytes of a submission and of a completion queue entry (CC.IOSQES = 6, IOCQES = 4).
pub const SQE: usize = 64;
pub const CQE: usize = 16;
/// Entries of every queue: one page of submission entries. A queue is one page
/// because a DMA memory object's pages need not be contiguous and a controller that
/// sets CAP.CQR requires a queue to be.
pub const QUEUE_ENTRIES: u16 = (PAGE / SQE) as u16;
/// The most pages one transfer carries; its PRP list fits one page.
pub const MAX_TRANSFER_PAGES: u32 = 256;

/// Why the controller cannot be used, or a request not made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NvmeError {
    /// A kernel call or a window access failed.
    Kernel(Error),
    /// The controller did not do `what` within its time.
    Timeout(&'static str),
    /// The controller reports Controller Fatal Status (CSTS.CFS).
    Fatal,
    /// The controller lacks something this driver needs.
    Unsupported(String),
    /// The controller returned a value no well-behaved controller does.
    Malformed(String),
    /// An admin command failed with this status.
    Command(&'static str, Status),
    /// Every submission slot is taken: wait for completions.
    Full,
}

impl From<Error> for NvmeError {
    fn from(e: Error) -> NvmeError {
        NvmeError::Kernel(e)
    }
}

impl fmt::Display for NvmeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NvmeError::Kernel(e) => write!(f, "kernel: {}", e.name()),
            NvmeError::Timeout(what) => write!(f, "the controller did not {what} in time"),
            NvmeError::Fatal => f.write_str("the controller reports a fatal status (CSTS.CFS)"),
            NvmeError::Unsupported(what) => write!(f, "unsupported controller: {what}"),
            NvmeError::Malformed(what) => write!(f, "the controller returned {what}"),
            NvmeError::Command(what, status) => write!(f, "{what} failed: {status}"),
            NvmeError::Full => f.write_str("every submission slot is taken"),
        }
    }
}

#[cfg(test)]
mod tests;
