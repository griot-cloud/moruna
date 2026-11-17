//! The channel a driver talks to, as system design 1F states it: the channel's two rings
//! (`vmbus_ring`), signal the host, wait for the host's signal, and share more memory with the
//! host (a GPADL).
//!
//! `moruna-vmbus` (item 1.9b) defines this trait. It was not merged when this crate was
//! written, so the trait stands here until it is.

use core::fmt::Debug;

use vmbus_ring::{IncomingRing, OutgoingRing, RingMem};

// moruna: moves to moruna-vmbus (1.9b)
/// One open VMBus channel, as its driver sees it.
///
/// 1F names four things and no types beyond the rings, so what names shared memory, what names
/// a GPADL and what an error is are left to the implementation.
pub trait Channel {
    /// The memory the two rings live in.
    type Memory: RingMem;
    /// What names memory the driver wants the host to see.
    type Shared;
    /// What names that memory once it is shared: its GPADL.
    type Gpadl;
    /// Why the channel refused: it is closed, or the host said no.
    type Error: Debug;

    /// The ring the host writes and the driver reads.
    fn incoming(&self) -> &IncomingRing<Self::Memory>;

    /// The ring the driver writes and the host reads.
    fn outgoing(&self) -> &OutgoingRing<Self::Memory>;

    /// Signals the host that a ring has changed in a way it asked to hear of.
    fn signal(&mut self) -> Result<(), Self::Error>;

    /// Waits for the host's signal.
    fn wait(&mut self) -> Result<(), Self::Error>;

    /// Shares more memory with the host, as a GPADL. storvsc never does: its data travels
    /// as guest page numbers in each request.
    fn share(&mut self, memory: Self::Shared) -> Result<Self::Gpadl, Self::Error>;
}
