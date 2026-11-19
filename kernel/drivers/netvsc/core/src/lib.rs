//! Azure's synthetic network card, netvsc (system design 1F, work item 1.11a), for a
//! driver that is an ordinary process: Ethernet frames out and in over a VMBus channel,
//! speaking NVSP to the host's network service (netvsp) and RNDIS inside it.
//!
//! [`Netvsc::start`] initialises the card over a channel (`moruna_vmbus::Channel`) and two
//! buffers of the caller's memory, and reads the card's address and MTU. After that the
//! caller has exactly what `moruna.driver.net` needs: [`Netvsc::mac`], [`Netvsc::mtu`],
//! [`Netvsc::link_up`], [`Netvsc::send`] for a frame out, and [`Netvsc::poll`] for the
//! frames and link changes the host has sent. Nothing waits after `start`: the caller
//! polls until `None`, which may find what came with the start's last answer, then
//! [`Netvsc::arm`]s (or flushes) and waits for the host's signal on its channel, and
//! polls again.
//!
//! # Where each step comes from
//!
//! There is no guest netvsc on OpenVMM's main branch. The wire types are
//! `netvsp_protocol`'s (vendored, `kernel/vendor/openvmm`). What the host expects is read
//! from OpenVMM's host side, `vm/devices/net/netvsp/src/lib.rs` at commit `e7e7df77`
//! ("netvsp" below), and from the guest its own tests drive it with,
//! `vm/devices/net/netvsp/src/test.rs` ("netvsp's test guest"). RNDIS messages follow
//! Microsoft's Remote NDIS pages under
//! <https://learn.microsoft.com/en-us/windows-hardware/drivers/network/> ("Microsoft",
//! with the page's name). Microsoft publishes no NVSP document, so NVSP follows
//! `netvsp_protocol`'s own comments and netvsp. Each step's comment names its source.
//!
//! # What is refused or ignored, by design
//!
//! - The virtual function: a `SendVfAssociation` is acknowledged and ignored, and
//!   nothing here sends `SwitchDataPath`.
//! - The send indirection table (`SendIndirectionTable`, NVSP 5), which says which channel
//!   each flow's frames go on: taken and ignored wherever it comes, acknowledged if asked,
//!   since the card sends on its one channel ([`Netvsc::take_table`] says the first).
//! - Sub-channels, checksum and segmentation offloads, VLAN, receive-segment coalescing:
//!   not offered in the NDIS configuration and not used.
//! - Frames are never sent by guest page numbers, only from send-buffer sections. A
//!   host whose sections cannot hold the largest frame fails [`Netvsc::start`] with
//!   [`NetvscError::Unfit`], so a started card sends every frame of its MTU.
//!
//! # The host is untrusted input
//!
//! Every length, offset, index and transaction the host sends is bounded before it is
//! used. What no well-behaved host sends is an error naming it
//! ([`NetvscError::Malformed`]), never an index out of bounds or a panic, and the card
//! goes on working after it.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

mod device;
#[cfg(feature = "trace-first-frames")]
pub mod latency;
mod nvsp;
mod rndis;
#[cfg(feature = "trace-first-frames")]
pub mod trace;
mod wire;

use alloc::string::String;
use core::fmt;

use moruna_vmbus::{ChannelError, MemoryId};

pub use device::{Event, Netvsc};

/// The NDIS version stated to the host, 6.30: what Linux's hv_netvsc states from NVSP 5
/// (`ndis_version = 0x0006001e`) and netvsp's test guest states
/// (`send_ndis_version_message`). netvsp reads it only to list the OIDs it supports.
pub const NDIS_VERSION: (u32, u32) = (6, 30);

/// An NVSP version, as the console says it: `0x60001` is 6.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nvsp(pub u32);

impl fmt::Display for Nvsp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0 >> 16, self.0 & 0xffff)
    }
}

/// What has moved through the card since it started (work item 6.3ab): counted always, an
/// addition each, for the netvsc process's meter line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Packets read off the ring from the host, of every kind.
    pub packets_in: u64,
    /// Receives among them: transfer-page packets, each naming one or more frames.
    pub receives: u64,
    /// Frames handed to the caller, and their bytes.
    pub frames_in: u64,
    /// Bytes of the frames handed to the caller.
    pub bytes_in: u64,
    /// Frames sent, and their bytes.
    pub frames_out: u64,
    /// Bytes of the frames sent.
    pub bytes_out: u64,
    /// Frames not sent at once because no send section was free.
    pub sections_full: u64,
    /// Writes the ring to the host had no room for, after which the host was asked to signal.
    pub ring_full: u64,
    /// Commits of the ring to the host.
    pub commits: u64,
    /// Signals to the host, by VMBus's rule: on a commit to an empty ring, or a read that made
    /// room the host waits for.
    pub signals: u64,
    /// Arms: the driver asked the host to signal before it waits.
    pub arms: u64,
    /// Arms that found nothing to do: the driver then waits for the host's signal.
    pub idle: u64,
}

/// Bytes of an Ethernet header: destination, source, EtherType.
pub const ETHERNET_HEADER: usize = 14;
/// Bytes of a page, the unit memory is shared with the host in.
pub const PAGE: usize = moruna_vmbus::PAGE_SIZE;

/// The bytes of memory the caller shares with the host. Once shared, both sides read
/// and write it at any time.
pub trait Shared {
    /// Its length in bytes: whole pages.
    fn len(&self) -> usize;

    /// Whether it holds no bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies `data.len()` bytes out, from `at` bytes in. The card keeps
    /// `at + data.len()` within [`Shared::len`].
    fn read(&self, at: usize, data: &mut [u8]);

    /// Copies `data` in, at `at` bytes in. The card keeps `at + data.len()` within
    /// [`Shared::len`].
    fn write(&self, at: usize, data: &[u8]);
}

/// One of the card's two buffers: memory of the caller's, zeroed, which the card shares
/// with the host through its channel.
#[derive(Clone, Copy, Debug)]
pub struct Buffer<S> {
    /// What the channel knows the memory as (`Channel::share`).
    pub id: MemoryId,
    /// The same memory's bytes.
    pub memory: S,
}

/// Why the card could not do what was asked.
#[derive(Clone, Debug)]
pub enum NetvscError {
    /// The channel failed: it is closed, or it could not share a buffer.
    Channel(ChannelError),
    /// A ring's own state is not a ring's. Nothing more can be read or written.
    Ring(vmbus_ring::Error),
    /// A buffer the caller gave cannot be used.
    Buffer(&'static str),
    /// The host did not answer this step of initialisation before the deadline.
    Timeout(&'static str),
    /// The host accepts no NVSP version from 5 up.
    Version,
    /// The host answered a step of initialisation with a failure status.
    Refused {
        /// The step.
        step: &'static str,
        /// The NVSP or RNDIS status the host gave.
        status: u32,
        /// The NVSP version in use, once negotiated: the refusal says it, and the NDIS
        /// version stated ([`NDIS_VERSION`]).
        nvsp: Option<u32>,
    },
    /// The host sent what no well-behaved host does. What it sent is dropped.
    Malformed(String),
    /// The host reports, with this NVSP status, that it could not send a frame.
    NotSent(u32),
    /// A frame of this many bytes is no frame of this card: shorter than an Ethernet
    /// header, or longer than the MTU allows.
    Length(usize),
    /// A frame that fits no section of the send buffer. `start` answers it when the
    /// largest frame of the host's MTU does not fit, and then the card is not started.
    Unfit {
        /// The frame's RNDIS message, in bytes.
        message: usize,
        /// A section, in bytes.
        section: usize,
    },
}

impl fmt::Display for NetvscError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetvscError::Channel(e) => write!(f, "{e}"),
            NetvscError::Ring(e) => write!(f, "the ring: {e}"),
            NetvscError::Buffer(what) => write!(f, "{what}"),
            NetvscError::Timeout(step) => write!(f, "the host did not answer {step} in time"),
            NetvscError::Version => f.write_str("the host accepts no NVSP version from 5 up"),
            NetvscError::Refused { step, status, nvsp } => {
                write!(f, "the host refused {step} with status {status:#010x}")?;
                match nvsp {
                    Some(version) => write!(
                        f,
                        " (NVSP {}, NDIS {}.{})",
                        Nvsp(*version),
                        NDIS_VERSION.0,
                        NDIS_VERSION.1
                    ),
                    None => Ok(()),
                }
            }
            NetvscError::Malformed(what) => write!(f, "the host sent {what}"),
            NetvscError::NotSent(status) => {
                write!(f, "the host could not send a frame (status {status})")
            }
            NetvscError::Length(bytes) => write!(f, "a frame of {bytes} bytes"),
            NetvscError::Unfit { message, section } => write!(
                f,
                "a frame whose message of {message} bytes fits no send section of {section} bytes"
            ),
        }
    }
}

impl From<ChannelError> for NetvscError {
    fn from(e: ChannelError) -> NetvscError {
        NetvscError::Channel(e)
    }
}

impl From<vmbus_ring::Error> for NetvscError {
    fn from(e: vmbus_ring::Error) -> NetvscError {
        NetvscError::Ring(e)
    }
}
