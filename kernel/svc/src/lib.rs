//! `moruna-svc`: the SDK every driver and server on the Moruna microkernel is written
//! with (SDD 23). A third party adds a driver, a filesystem or a whole new server by
//! writing a process against this crate and listing it in the boot description; the
//! kernel is not rebuilt and nothing is forked.
//!
//! - [`protocol!`] declares a protocol once: its name and version, its requests,
//!   events and errors. The declaration generates the typed client, the server trait
//!   and the handler that dispatches to it.
//! - [`Dispatcher`] is the server's loop: many channels, one `wait`, each connection
//!   serving its own protocol, listeners bringing new connections.
//! - Every connection opens with a handshake of protocol name and version, refused on
//!   a mismatch; requests carry ids their replies echo; errors are typed; handles
//!   travel inside messages; a message over the channel's bound is refused before it
//!   is sent.
//! - [`conformance`] runs a protocol's suite against any server of it.
//! - On the kernel, `moruna-sys`'s heap gives every program `alloc`; on a host the SDK
//!   runs on [`sim`], a model of the channel ABI, so all of it is tested there too.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod client;
pub mod conformance;
pub mod frame;
pub mod os;
pub mod protocol;
mod schema;
pub mod server;
#[cfg(not(target_os = "none"))]
pub mod sim;
pub mod wire;

pub use client::{CallError, ClientCore, connect, deadline_in};
pub use frame::{Kind, Transport};
pub use moruna_abi::{DEADLINE_INFINITE, Error, ObjectType, Rights, Signals, limits};
pub use os::{Channel, Handle, WaitItem, now, wait, wait_one};
pub use protocol::{Hello, NoError, ProtocolError, ProtocolId, Refusal, Refused, Status};
pub use server::{
    Completions, ConnectionId, Context, Dispatcher, Factory, Handler, Outcome, Pending, Turn, Watch,
};
pub use wire::{Decoder, Encoder, Wire, WireError};

/// What a program on the kernel needs besides the SDK: its entry, its console, its
/// start handles, the objects a driver maps, and the calls a driver and its
/// supervisor make (SDD 25).
#[cfg(target_os = "none")]
pub use moruna_sys::{
    Console, DeviceMemory, Interrupt, Mapping, Memory, Msi, Pin, Process, ResourceInfo,
    ResourceKind, Timer, dma_release, exit, main, map, println, start,
};

#[doc(hidden)]
pub mod __private {
    pub use alloc::boxed::Box;
}
