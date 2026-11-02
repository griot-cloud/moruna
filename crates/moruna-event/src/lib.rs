//! Moruna's run events (SDD 12, MH 4.3 version 7): the facts a run tells about itself, each one
//! a record.
//!
//! - [`Event`], the fact: the task lifecycle (PY-I11, PY-I12), the plan's steps (SDD 26), the
//!   output store's reach and one record of output.
//! - [`Encoder`] writes an event as a `moruna-record` record (SDD 21), its fact FlatBuffers
//!   (`schema/event.fbs`); [`decode`] reads anyone's bytes back, verified, never panicking.
//! - [`EventMsg::json_line`] is the JSON rendering for people and for `moruna run
//!   --events-json`, with the field names the version 6 line had.
//! - [`frame`] is the framed message that carries one record on the host protocol's connection.
//!
//! The crate has no dependency on Arrow, the engines or the runtime: a host, a monitor or a
//! language binding reads a run's events without linking the run.

#![deny(missing_docs)]

mod codec;
mod error;
pub mod frame;
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
    FailureScope, Kind, LogReach, LogStream, Phase, SUBJECT_BASE, SkipReason, StepOutcome,
    TaskOutcome, WaitReason,
};
