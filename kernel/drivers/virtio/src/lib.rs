//! Modern virtio (1.x) over PCI (SDD 25, f.7), for drivers that are ordinary
//! processes: the vendor capabilities that place the common, notify, ISR and device
//! configuration windows in the BARs; the transport that resets a device, negotiates
//! its features, sets up its queues and tells it of new buffers; and split virtqueues
//! with descriptor chains.
//!
//! Everything here reads and writes through [`moruna_mmio::Region`], so it is the same
//! code on the kernel (a mapping of a BAR or of a DMA memory object) and on a host (an
//! in-memory window), where it is tested against a model device. The [`device`] module
//! (kernel only) opens a device from the handles its supervisor gave the driver.
//!
//! Written for Moruna rather than on `virtio-drivers`: that crate's hardware
//! abstraction hands out raw pointers to device and DMA memory and is an `unsafe`
//! trait, which a driver that forbids unsafe code cannot implement, and its buffers
//! are the driver's own slices, where a Moruna driver moves a client's memory object
//! to its device by pinning it.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod queue;
pub mod transport;

#[cfg(target_os = "none")]
pub mod device;

pub use queue::{Buffer, Queue, QueueError, Used};
pub use transport::{Layout, Transport, VirtioError, Window};

/// The PCI vendor of every virtio device, and the base of modern device ids
/// (`0x1040 + virtio device type`).
pub const VENDOR: u16 = 0x1af4;
pub const MODERN_BASE: u16 = 0x1040;

/// Virtio device types (virtio 1.2, section 5).
pub mod kind {
    pub const NET: u16 = 1;
    pub const BLOCK: u16 = 2;
    pub const CONSOLE: u16 = 3;
    pub const VSOCK: u16 = 19;
}

/// Feature bits every device type shares (virtio 1.2, section 6).
pub mod feature {
    pub const INDIRECT_DESC: u64 = 1 << 28;
    pub const EVENT_IDX: u64 = 1 << 29;
    /// The device speaks virtio 1.x: required.
    pub const VERSION_1: u64 = 1 << 32;
    /// The device's DMA goes through the platform's IOMMU: taken whenever offered,
    /// since every address a Moruna driver gives a device comes from `memory_pin`.
    pub const ACCESS_PLATFORM: u64 = 1 << 33;
}

/// The device status bits (virtio 1.2, 2.1).
pub mod status {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FEATURES_OK: u8 = 8;
    pub const NEEDS_RESET: u8 = 64;
    pub const FAILED: u8 = 128;
}

/// The vector value that means none (virtio 1.2, 4.1.4.3).
pub const NO_VECTOR: u16 = 0xffff;

#[cfg(test)]
mod tests;
