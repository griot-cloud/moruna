//! What a driver sees of an open channel (system design 1F).
//!
//! A driver is written against [`Channel`] and nothing else of VMBus: the two rings
//! packets travel in, a signal in each direction, and a way to share more memory with
//! the host. On a node the `vmbus` process gives a driver its channel; in a host test
//! `testing::TestChannel` does, with a fake device on the other end.

use core::fmt;

use vmbus_core::protocol::GpadlId;
use vmbus_ring::IncomingRing;
use vmbus_ring::OutgoingRing;
use vmbus_ring::RingMem;

use crate::hv::MemoryId;

/// How a wait ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The host signalled: there may be packets to read, or room to write.
    Signalled,
    /// The deadline passed with no signal.
    TimedOut,
}

/// Why a channel refused what its driver asked.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChannelError {
    /// The channel is closed, or the host rescinded its offer: the device is gone.
    Closed,
    /// The memory could not be shared: the channel knows no such memory, it is not 1 to
    /// 8,190 whole pages, or the host refused it.
    NotShared,
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelError::Closed => f.write_str("the channel is closed"),
            ChannelError::NotShared => f.write_str("the memory could not be shared with the host"),
        }
    }
}

impl core::error::Error for ChannelError {}

/// An open channel, as its driver uses it.
pub trait Channel {
    /// The memory the channel's rings are in.
    type Memory: RingMem;

    /// The ring the host writes and the driver reads.
    fn incoming(&self) -> &IncomingRing<Self::Memory>;

    /// The ring the driver writes and the host reads.
    fn outgoing(&self) -> &OutgoingRing<Self::Memory>;

    /// Signals the host. A driver does so when a ring says the host needs it:
    /// `OutgoingRing::commit_write` and `IncomingRing::commit_read` each return whether
    /// it does.
    fn signal_host(&mut self) -> Result<(), ChannelError>;

    /// Waits for the host's signal. `deadline` is a time on the node's monotonic clock in
    /// nanoseconds, as `moruna_svc::now` counts them; with none, the wait ends only with a
    /// signal or with the channel.
    fn wait(&mut self, deadline: Option<u64>) -> Result<Wait, ChannelError>;

    /// Shares more of the driver's memory with the host, as a GPADL the driver can then
    /// name to the device.
    fn share(&mut self, memory: MemoryId) -> Result<GpadlId, ChannelError>;
}
