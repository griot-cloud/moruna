//! Moruna packaged as a microVM: `moruna-vmm`, the monitor that boots the Moruna guest on
//! KVM (MH 4.8).

#![deny(missing_docs)]
#![allow(clippy::result_large_err)]

pub mod cli;
pub mod config;
pub mod control;
pub mod devices;
pub mod error;
pub mod image;
pub mod sys;

#[cfg(test)]
pub(crate) mod testing;
