//! Moruna's run events (SDD 12, MH 4.3 version 7): the facts a run tells about itself, each one
//! a record.
//!
//! - [`Event`], the fact: the task lifecycle (PY-I11, PY-I12), the plan's steps (SDD 26), the
//!   output store's reach and one record of output.
//! - [`Encoder`] writes an event as a `moruna-record` record (SDD 21), its fact FlatBuffers
//!   (`schema/event.fbs`); [`decode`] reads anyone's bytes back, verified, never panicking.
//! - [`EventMsg::json_line`] is the JSON rendering for people and for `moruna run
//!   --events-json`, with the field names the version 6 line had; [`EventMsg::rendering`] is the
//!   same rendering for a serializer of the reader's own.
//! - [`frame`] is the framed message that carries one record on the host protocol's connection.
//!
//! The crate has no dependency on Arrow, the engines or the runtime: a host, a monitor or a
//! language binding reads a run's events without linking the run. Without the `std` feature (on
//! by default) it is `no_std` with `alloc`, for the node's `api` process: everything but the
//! JSON text and the frame's `std::io` reader and writer.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]

extern crate alloc;

mod codec;
mod error;
#[cfg(feature = "std")]
pub mod frame;
#[cfg(not(feature = "std"))]
pub mod frame {
    //! The framed binary message of the host protocol that carries one event (MH 4.3, version
    //! 7): [`FRAME`], then the record, whose first four bytes are the number of bytes after
    //! them.

    /// The first byte of a frame. NUL is not the start of a JSON line and not whitespace.
    pub const FRAME: u8 = 0x00;
}
mod model;

/// The Rust flatc generates from `schema/event.fbs` (`tools/flatc/generate.sh`), committed and
/// checked in CI against what the schema generates.
#[allow(missing_docs, clippy::all, clippy::pedantic, unsafe_op_in_unsafe_fn, deprecated)]
#[rustfmt::skip]
mod generated {
    include!("generated/event_generated.rs");
}

/// The event body's FlatBuffers tables as flatc generates them from `schema/event.fbs`. Bytes
/// from anyone are read with [`decode`], which verifies them first.
pub use generated::moruna::event::fb;

pub use codec::{EVENT_IDENTIFIER, Encoder, Stamp, decode, read_records};
pub use error::{DecodeError, EncodeError};
pub use model::{
    CancelDisposition, Counters, ENGINE, EVENTS_VERSION, Element, Event, EventMsg, FailureCode,
    FailureScope, Kind, LogReach, LogStream, Phase, Rendering, SUBJECT_BASE, SkipReason,
    StepOutcome, TaskOutcome, WaitReason,
};
