//! What the library needs from Hyper-V, and nothing else (system design 1F).
//!
//! On a node the `vmbus` process implements [`Hv`] over the core's mechanism: the two
//! hypercalls, the SynIC message and event pages of CPU 0, and the physical addresses of
//! a memory object. On a host a test implements it as the host's side of the protocol.

use alloc::vec::Vec;

pub use hvdef::HvError;
pub use hvdef::HvMessage;

/// The type Hyper-V carries every VMBus message under, in both directions
/// (`openvmm: vm/devices/vmbus/vmbus_server/src/lib.rs:106`, `VMBUS_MESSAGE_TYPE`).
pub const MESSAGE_TYPE: u32 = 1;

/// The most a message carries: the payload of one SynIC message slot, 240 bytes.
pub const MAX_PAYLOAD: usize = hvdef::HV_MESSAGE_PAYLOAD_SIZE;

/// A range of the caller's memory, whole pages, named by a number only the caller and its
/// [`Hv`] understand. The library hands it back to [`Hv::page_numbers`] and never looks
/// inside it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemoryId(pub u64);

/// Why a message was not posted. Either way nothing was posted.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PostError {
    /// Hyper-V has no room for the message now (`HV_STATUS_INSUFFICIENT_BUFFERS`): the
    /// same message is to be posted again later.
    Busy,
    /// Hyper-V refused the post with any other status. The client passes it up as
    /// [`HostError::Post`](crate::HostError::Post); the `vmbus` process treats it as fatal.
    Hv(HvError),
}

/// What the library needs from Hyper-V.
pub trait Hv {
    /// Posts a message to a connection ID: a type and up to [`MAX_PAYLOAD`] bytes
    /// (`HvCallPostMessage`). `HV_STATUS_INSUFFICIENT_BUFFERS` is [`PostError::Busy`],
    /// any other failure [`PostError::Hv`] with Hyper-V's status.
    fn post_message(
        &mut self,
        connection_id: u32,
        message_type: u32,
        payload: &[u8],
    ) -> Result<(), PostError>;

    /// Signals an event on a connection ID and flag (`HvCallSignalEvent`).
    fn signal_event(&mut self, connection_id: u32, flag: u16);

    /// Takes the next message waiting on SINT 2, if any: the slot's 256 bytes as Hyper-V
    /// wrote them. Taking it frees the slot for the next one (the end-of-message write).
    fn take_message(&mut self) -> Option<HvMessage>;

    /// Tests one event flag of SINT 2 and clears it, as one step. True if it was set.
    fn test_and_clear_event(&mut self, flag: u16) -> bool;

    /// The guest-physical page number of every page of `memory`, in order. `None` when
    /// the ID names no memory.
    fn page_numbers(&mut self, memory: MemoryId) -> Option<Vec<u64>>;
}
