//! BAR 0's registers (`gve_register.h`), all big endian, and BAR 2's doorbells.

/// Offsets in BAR 0.
pub const DEVICE_STATUS: usize = 0x00;
pub const DRIVER_STATUS: usize = 0x04;
pub const MAX_TX_QUEUES: usize = 0x08;
pub const MAX_RX_QUEUES: usize = 0x0c;
/// The admin queue's page frame number, for a device of revision 0.
pub const ADMINQ_PFN: usize = 0x10;
pub const ADMINQ_DOORBELL: usize = 0x14;
pub const ADMINQ_EVENT_COUNTER: usize = 0x18;
/// A byte register the driver writes its version string into, a byte at a time.
pub const DRIVER_VERSION: usize = 0x1f;
/// The admin queue's address and length, for a device of revision 1 or later.
pub const ADMINQ_BASE_HI: usize = 0x20;
pub const ADMINQ_BASE_LO: usize = 0x24;
pub const ADMINQ_LENGTH: usize = 0x28;
/// The bytes of BAR 0 this driver uses.
pub const SIZE: usize = 0x2c;

/// Device status bits.
pub mod device_status {
    /// The device asks the driver to reset it.
    pub const RESET: u32 = 1 << 1;
    pub const LINK: u32 = 1 << 2;
    pub const REPORT_STATS: u32 = 1 << 3;
    /// The device has finished a reset the driver asked for (revision 1).
    pub const IS_RESET: u32 = 1 << 4;
}

/// Driver status bits (revision 1).
pub mod driver_status {
    pub const RUN: u32 = 1 << 0;
    pub const RESET: u32 = 1 << 1;
}

/// A notification block's interrupt doorbell values (`gve_desc.h`).
pub mod irq {
    pub const ACK: u32 = 1 << 31;
    pub const MASK: u32 = 1 << 30;
    pub const EVENT: u32 = 1 << 29;
}

/// What the driver writes into `DRIVER_VERSION`, ending with a newline.
pub const VERSION: &[u8] = b"GVE-moruna-0.1\n";
