//! Moruna packaged as a microVM: `moruna-vmm`, the monitor that boots the Moruna guest on
//! KVM (MH 4.8).

#![deny(missing_docs)]
#![allow(clippy::result_large_err)]

pub mod acpi;
pub mod boot;
pub mod cli;
pub mod config;
pub mod control;
pub mod devices;
pub mod error;
pub mod fdt;
pub mod image;
pub mod layout;
pub mod machine;
pub mod sys;

#[cfg(test)]
pub(crate) mod testing;
