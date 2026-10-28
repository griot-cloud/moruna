//! Moruna component 21, the record and its ring: a fixed binary record layout, and a
//! multi-producer, single-consumer ring of those records in a byte region the caller provides.
//!
//! Design: `architecture/sdd/21-record.md`. The crate is `no_std` with `alloc`; the `std`
//! feature (on by default) only adds `std::error::Error` impls. It has no dependency: the crc
//! is [`crc::crc32c`], a table generated at compile time.
//!
//! * [`record`] encodes and decodes one record, byte for byte the layout the node's logs,
//!   events and metrics are written in.
//! * [`ring`] queues encoded records in a region that may be shared memory mapped into several
//!   processes: it holds offsets and counters, never a pointer. It is the only module with
//!   `unsafe`, and only where the region's bytes are read as atomics or written as a slot.

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_code)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub mod crc;
pub mod error;
pub mod record;
pub mod ring;

pub use crc::crc32c;
pub use error::{Corruption, Error};
pub use record::{
    Decoded, FIXED, MAX_BODY, MAX_RECORD, Record, check_fits, decode, encode, encode_into,
    encoded_len,
};
pub use ring::{Consumer, Full, Producer, PushError, Ring};
