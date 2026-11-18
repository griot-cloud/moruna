//! storvsc, Azure's disk protocol, for a driver that is an ordinary process (system design
//! 1F, item 1.10a): it turns block reads and writes into SCSI requests on a VMBus channel.
//!
//! What is here is exactly what `moruna.driver.block` needs:
//!
//! - [`Storvsc::new`] starts storvsc's initialisation (begin, version, properties, end) and
//!   [`Storvsc::poll`] carries it to [`Event::Ready`];
//! - [`Storvsc::read_capacity`] and [`Disk::from_capacity`] give a disk's block count and
//!   block size (READ CAPACITY 16);
//! - [`Storvsc::read`] and [`Storvsc::write`] move a block range (READ 16, WRITE 16), and
//!   [`Storvsc::flush`] is SYNCHRONIZE CACHE;
//! - [`Storvsc::read_serial`] and [`serial`] give the device's serial (INQUIRY, VPD page 0x80);
//! - [`Storvsc::report_luns`] and [`luns`] give a target's logical units (REPORT LUNS), so a
//!   driver finds its disks with one command a target.
//!
//! **Nothing is copied.** Data never passes through the library: a request names the guest
//! pages the caller gives, in VMBus's "GPA direct" packet form, and the host moves the bytes.
//! That holds for the capacity, the serial and the LUNs too, which the host writes into a page
//! of the caller's; the caller then hands that page's bytes to the decoder.
//!
//! **The caller drives it.** There is no thread and no async runtime. A request returns its
//! transaction ID at once; [`Storvsc::poll`] reads what the host has sent and reports each
//! completion with that ID; [`Storvsc::wait`] sleeps until the host signals, and
//! [`Storvsc::arm`] asks it to signal and returns at once, for a driver that waits on its
//! channel's wake among other handles, as a node's `storvsc` process does. Requests may be
//! in flight together, as many as the ring holds of the largest ([`Storvsc::limit`]), and
//! complete in any order. There are no sub-channels.
//!
//! The channel is `moruna-vmbus`'s [`Channel`]: a node's `vmbus` process gives one to the
//! driver, and a host test uses `moruna_vmbus::testing::TestChannel`.
//!
//! **The host is untrusted input.** Every packet is bounded and checked before it is used: its
//! type, its size, its operation, its transaction ID, and the byte count it claims. A
//! completion for an unknown ID, a packet of the wrong size and a SCSI error are errors to the
//! caller, never an index and never a panic.
//!
//! The reference for the order of operations and the packet layout is OpenVMM's `storvsc_driver`
//! (`vm/devices/storage/storvsc_driver/src/lib.rs` at `e7e7df77`), cited below as `storvsc:`;
//! its host side, `vm/devices/storage/storvsp/src/lib.rs`, is cited as `storvsp:`. The wire
//! types and the ring are Microsoft's, vendored under `kernel/vendor/openvmm`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod device;
mod ring;
pub mod scsi;

use core::fmt;

pub use device::{Completion, Event, Storvsc, VERSIONS};
pub use moruna_vmbus::{Channel, ChannelError, Wait};
pub use scsi::{Address, Disk, Failure, ScsiError, Sense, luns, serial};
pub use scsi_defs::{AdditionalSenseCode, ScsiStatus, SenseKey, srb::SrbStatus};
pub use storvsp_protocol::{NtStatus, Operation};

/// The page size of guest page numbers and of VMBus rings.
pub const PAGE: usize = vmbus_ring::PAGE_SIZE;

/// What a well-behaved host or device never sends, in a few words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Why a call could not be made, or what the host sent that cannot be used.
#[derive(Debug)]
pub enum Error {
    /// The channel refused to signal or to wait. A request that meets it is in the ring but
    /// not in flight: if the host answers it anyway, the answer is an unknown transaction.
    Channel(ChannelError),
    /// A ring's control words or a packet's descriptor are corrupt. The device is unusable
    /// from here on.
    Ring(vmbus_ring::Error),
    /// The host sent a packet no storvsp sends.
    Malformed(Malformed),
    /// The host sent a packet that asks something of the driver, which this driver has no
    /// answer for.
    Unexpected(Operation),
    /// The host completed a transaction that is not in flight.
    UnknownTransaction(u64),
    /// The host refused a step of the initialisation.
    Refused {
        /// The step.
        operation: Operation,
        /// The host's answer.
        status: NtStatus,
    },
    /// The host accepts no protocol version this driver speaks.
    NoVersion,
    /// The initialisation has not finished: poll until [`Event::Ready`].
    NotReady,
    /// The initialisation failed, or a ring is corrupt: nothing more can be done here.
    Failed,
    /// As many requests are in flight as may be, or the ring to the host has no room: wait
    /// for completions and ask again.
    Full,
    /// A request for no blocks.
    Empty,
    /// The blocks reach past the disk's end.
    OutOfRange,
    /// More bytes than one request may carry ([`Storvsc::max_transfer_bytes`]).
    TooLarge,
    /// The pages given do not cover the transfer.
    ShortBuffer,
    /// A byte offset that is not within the first page.
    Offset,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Channel(e) => write!(f, "the channel failed: {e}"),
            Error::Ring(e) => write!(f, "a ring is corrupt: {e}"),
            Error::Malformed(what) => write!(f, "the host sent {what}"),
            Error::Unexpected(operation) => {
                write!(
                    f,
                    "the host sent {operation:?}, which this driver does not serve"
                )
            }
            Error::UnknownTransaction(id) => {
                write!(
                    f,
                    "the host completed transaction {id}, which is not in flight"
                )
            }
            Error::Refused { operation, status } => {
                write!(f, "the host refused {operation:?} with {status:?}")
            }
            Error::NoVersion => f.write_str("the host accepts no protocol version of this driver"),
            Error::NotReady => f.write_str("the initialisation has not finished"),
            Error::Failed => f.write_str("the device failed earlier and is unusable"),
            Error::Full => f.write_str("no room for another request: wait for completions"),
            Error::Empty => f.write_str("a request for no blocks"),
            Error::OutOfRange => f.write_str("the blocks reach past the disk's end"),
            Error::TooLarge => f.write_str("more bytes than one request may carry"),
            Error::ShortBuffer => f.write_str("the pages given do not cover the transfer"),
            Error::Offset => f.write_str("a byte offset that is not within the first page"),
        }
    }
}
