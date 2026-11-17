//! The guest's side of Hyper-V's VMBus, for the node on Azure (system design 1F).
//!
//! VMBus is how a Hyper-V guest reaches its devices. The host offers a channel for each
//! one; the guest opens a channel by sharing two rings of its own memory with the host,
//! and packets then travel through the rings with a signal in each direction. None of
//! it has a public specification: the messages and the ring are Microsoft's, vendored
//! from OpenVMM (`kernel/vendor/openvmm`), and the order of operations follows OpenVMM's
//! client.
//!
//! Three things are here:
//!
//! - [`Client`], the connection to the host: contact and the protocol version, the
//!   host's offers, GPADLs (guest memory shared with the host), opening and closing
//!   channels, rescinded offers, and unload. It is a state machine the caller drives,
//!   with no thread and no clock of its own.
//! - [`Hv`], everything the client needs from Hyper-V: post a message, signal an event,
//!   take the next message, test and clear an event flag, and the page numbers of the
//!   caller's memory. On a node the `vmbus` process implements it over the core's
//!   mechanism; on a host a test implements it as the host's side of the protocol.
//! - [`Channel`], what a driver sees of an open channel: its rings, a signal each way,
//!   and a way to share more memory. With the `testing` feature, `testing::TestChannel`
//!   is a channel in memory with a fake device on its other end.
//!
//! The host is not trusted to be correct. A message that is malformed, of the wrong
//! size, about a channel or a GPADL that does not exist, or out of order is refused
//! with a [`HostError`] that names it, and the client stays as it was: nothing the host
//! sends makes it panic.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod channel;
mod client;
mod gpadl;
mod hv;
#[cfg(feature = "testing")]
pub mod testing;

pub use channel::Channel;
pub use channel::ChannelError;
pub use channel::Wait;
pub use client::Client;
pub use client::ClientState;
pub use client::ConnectError;
pub use client::Error;
pub use client::Event;
pub use client::HostError;
pub use client::MAX_CHANNEL_ID;
pub use client::Offer;
pub use client::RingPair;
pub use client::VERSIONS;
pub use gpadl::MAX_PAGES;
pub use gpadl::PAGE_SIZE;
pub use hv::Hv;
pub use hv::HvError;
pub use hv::HvMessage;
pub use hv::MAX_PAYLOAD;
pub use hv::MESSAGE_TYPE;
pub use hv::MemoryId;
pub use hv::PostError;

pub use guid::Guid;
pub use vmbus_core::VersionInfo;
pub use vmbus_core::protocol::ChannelId;
pub use vmbus_core::protocol::ConnectionState;
pub use vmbus_core::protocol::FeatureFlags;
pub use vmbus_core::protocol::GpadlId;
pub use vmbus_core::protocol::MessageType;
pub use vmbus_core::protocol::OfferChannel;
pub use vmbus_core::protocol::Version;
