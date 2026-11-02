//! Google's virtual NIC, gVNIC (SDD 29, f.4), for drivers that are ordinary processes:
//! the registers of BAR 0, the admin queue, the device description and its options,
//! and the GQI queue format's transmit and receive rings, in queue page lists (QPL)
//! or, where the device offers it, raw DMA addressing (RDA).
//!
//! Written from the interface Google publishes with its Linux driver (the `gve`
//! driver's `gve_register.h`, `gve_adminq.h` and `gve_desc.h`, dual licensed GPL-2.0
//! or MIT, and `Documentation/networking/device_drivers/ethernet/google/gve.rst`):
//! the layouts and the order of the steps are theirs, the code is Moruna's. There is
//! no emulator of the device, so this crate is tested against a model of it
//! (`tests`) and is **not yet proven on hardware**: KE3 proves it on Google Cloud.
//!
//! Admin commands, registers and GQI descriptors are big endian. A `Region` reads and
//! writes little endian, so every field goes through [`be32`] and its kin.
//!
//! The device is untrusted input: the doorbell and counter indices it assigns, the
//! completion counts it reports, the sequence numbers, lengths and flags of what it
//! receives, and every field of its description are bounded before they are used;
//! a value out of bounds is an error naming it.
//!
//! The DQO queue format, which the newest machine families prefer, is not built: a
//! device that offers only DQO is refused at start, by name.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod admin;
pub mod describe;
pub mod device;
pub mod regs;

use alloc::{string::String, vec::Vec};
use core::fmt;

use moruna_abi::Error;
use moruna_mmio::Region;

pub use describe::{Description, Format};
pub use device::{Gve, Settings};

/// Google's PCI vendor and gVNIC's device id.
pub const VENDOR: u16 = 0x1ae0;
pub const DEVICE: u16 = 0x0042;
/// The BARs: registers, the MSI-X table, doorbells.
pub const BAR_REGISTERS: usize = 0;
pub const BAR_MSIX: usize = 1;
pub const BAR_DOORBELLS: usize = 2;
pub const PAGE: usize = 4096;

/// Memory the device reads and writes: zeroed, mapped, and each page's device address.
pub trait Dma {
    type Region: Region;
    /// `bytes` (whole pages) of zeroed memory; `contiguous` when the device reads it as
    /// one block (a ring longer than a page), whose pages' addresses are then
    /// consecutive.
    fn alloc(
        &mut self,
        bytes: usize,
        contiguous: bool,
    ) -> Result<(Self::Region, Vec<u64>), GveError>;
}

/// Why the device cannot be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GveError {
    /// A kernel call or a window access failed.
    Kernel(Error),
    /// The device did not do `what` in time.
    Timeout(&'static str),
    /// The device lacks something this driver needs.
    Unsupported(String),
    /// The device returned a value no well-behaved device does.
    Malformed(String),
    /// An admin command failed with this status.
    Command { opcode: u32, status: u32 },
    /// The transmit ring has no room.
    Full,
}

impl From<Error> for GveError {
    fn from(e: Error) -> GveError {
        GveError::Kernel(e)
    }
}

impl fmt::Display for GveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GveError::Kernel(e) => write!(f, "kernel: {}", e.name()),
            GveError::Timeout(what) => write!(f, "the device did not {what} in time"),
            GveError::Unsupported(what) => write!(f, "unsupported device: {what}"),
            GveError::Malformed(what) => write!(f, "the device returned {what}"),
            GveError::Command { opcode, status } => write!(
                f,
                "admin command {opcode:#x} failed with status {status:#010x} ({})",
                admin::status_name(*status)
            ),
            GveError::Full => f.write_str("the transmit ring is full"),
        }
    }
}

/// A big-endian 16-, 32- or 64-bit field at `at`.
pub fn be16<R: Region + ?Sized>(r: &R, at: usize) -> Result<u16, Error> {
    Ok(r.read16(at)?.swap_bytes())
}

pub fn be32<R: Region + ?Sized>(r: &R, at: usize) -> Result<u32, Error> {
    Ok(r.read32(at)?.swap_bytes())
}

pub fn be64<R: Region + ?Sized>(r: &R, at: usize) -> Result<u64, Error> {
    Ok(r.read64(at)?.swap_bytes())
}

pub fn put_be16<R: Region + ?Sized>(r: &R, at: usize, v: u16) -> Result<(), Error> {
    r.write16(at, v.swap_bytes())
}

pub fn put_be32<R: Region + ?Sized>(r: &R, at: usize, v: u32) -> Result<(), Error> {
    r.write32(at, v.swap_bytes())
}

pub fn put_be64<R: Region + ?Sized>(r: &R, at: usize, v: u64) -> Result<(), Error> {
    r.write64(at, v.swap_bytes())
}

#[cfg(test)]
mod tests;
