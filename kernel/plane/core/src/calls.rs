//! The kernel calls the engine's side of the data plane is built on (SDD 29 d.2).
//!
//! On the kernel they are `moruna-sys`'s ([`crate::kernel::Kernel`]); on a host the tests
//! run the plane over a model of the same calls. The rest of the crate names only this
//! trait, so it is the same code on both.

use moruna_abi::Error;

/// What `slot_take` gave back.
#[derive(Debug)]
pub enum Taken<M> {
    /// An input batch the task is done with.
    Released { cookie: u64 },
    /// A record the task put in the window pushed with `cookie`.
    Put {
        cookie: u64,
        memory: M,
        offset: u64,
        len: u64,
    },
    /// A window the task never used.
    Returned { cookie: u64, memory: M },
}

/// What a wait saw ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ready {
    /// Something is ready, or the deadline is not reached and the wait was woken.
    Some,
    /// The deadline passed.
    TimedOut,
}

/// The kernel's data-plane calls, and the few others the engine's side makes.
pub trait Calls {
    /// An owned handle to a memory object.
    type Memory;
    /// An owned handle to the engine's end of a slot.
    type Slot;
    /// An owned channel endpoint.
    type Channel;

    /// `memory_create`: `bytes` (whole pages), zeroed.
    fn memory(&mut self, bytes: u64) -> Result<Self::Memory, Error>;
    /// A handle to the same memory object without `WRITE`, for a consumer.
    fn share(&mut self, memory: &Self::Memory) -> Result<Self::Memory, Error>;
    /// Copies `buf.len()` bytes at `offset` of the object out, through a mapping made
    /// and removed for the call: how the engine reads a record's framing.
    fn read(&mut self, memory: &Self::Memory, offset: u64, buf: &mut [u8]) -> Result<(), Error>;
    /// Copies `data` into the object at `offset`: how a port reply is written.
    fn write(&mut self, memory: &Self::Memory, offset: u64, data: &[u8]) -> Result<(), Error>;

    /// `slot_create`.
    fn slot(&mut self, output: bool) -> Result<Self::Slot, Error>;
    /// `slot_push`; the memory moves into the slot, or comes back with the error.
    fn push(
        &mut self,
        slot: &Self::Slot,
        memory: Self::Memory,
        offset: u64,
        len: u64,
        cookie: u64,
    ) -> Result<(), (Error, Self::Memory)>;
    /// `slot_take`.
    fn take(&mut self, slot: &Self::Slot) -> Result<Taken<Self::Memory>, Error>;
    /// `slot_close`.
    fn close(&mut self, slot: &Self::Slot) -> Result<(), Error>;
    /// `slot_refuse`.
    fn refuse(&mut self, slot: &Self::Slot, reason: &str) -> Result<(), Error>;

    /// `channel_create`.
    fn channel(&mut self) -> Result<(Self::Channel, Self::Channel), Error>;
    /// `channel_read` of one message into `buf`: its length.
    fn receive(&mut self, channel: &Self::Channel, buf: &mut [u8]) -> Result<usize, Error>;
    /// `channel_write` of `bytes` with the memory object, if any, moved.
    fn send(
        &mut self,
        channel: &Self::Channel,
        bytes: &[u8],
        memory: Option<Self::Memory>,
    ) -> Result<(), Error>;

    /// `wait` until a slot is readable or its task has ended, or a channel is readable
    /// or its peer closed, or the deadline.
    fn wait(
        &mut self,
        slots: &[&Self::Slot],
        channels: &[&Self::Channel],
        deadline: u64,
    ) -> Result<Ready, Error>;
}
