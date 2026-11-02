//! Protocol identity, versions and the handshake (SDD 23, d.1 and f.1).
//!
//! A protocol is named (1 to 64 bytes of `[a-z0-9._-]`) and versioned `major.minor`.
//! The client opens every channel with `Hello` (its protocol's name and version); the
//! server answers `Welcome` (its own version) or `Refuse` (why, and what it serves)
//! and then closes. A server accepts a client of the same name and major, whatever its
//! minor. A minor version only adds requests and events, each marked with the minor it
//! arrived in, and each side then uses only what both have: the client does not send a
//! request newer than the server's minor (`Unsupported`, unsent), and the server does
//! not emit an event newer than the client's. A major version is a different protocol.

use alloc::string::String;
use core::fmt::Debug;

use crate::wire::{Decoder, Encoder, Wire, WireError};

/// The longest protocol name.
pub const NAME_MAX: usize = 64;

/// A protocol's name and version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolId {
    pub name: &'static str,
    pub major: u16,
    pub minor: u16,
}

/// Whether `name` is a valid protocol name.
pub const fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > NAME_MAX {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'_' || c == b'-') {
            return false;
        }
        i += 1;
    }
    true
}

impl ProtocolId {
    /// A protocol id; a `const` with an invalid name fails to compile.
    pub const fn new(name: &'static str, major: u16, minor: u16) -> ProtocolId {
        assert!(
            valid_name(name),
            "a protocol name is 1 to 64 bytes of [a-z0-9._-]"
        );
        ProtocolId { name, major, minor }
    }

    /// Whether a server of this protocol serves a client that says `hello`.
    pub fn accepts(&self, hello: &Hello) -> Result<(), Refusal> {
        if hello.name != self.name {
            return Err(Refusal::UnknownProtocol);
        }
        if hello.major != self.major {
            return Err(Refusal::MajorMismatch);
        }
        Ok(())
    }

    /// This protocol's `Hello`.
    pub fn hello(&self) -> Hello {
        Hello {
            name: String::from(self.name),
            major: self.major,
            minor: self.minor,
        }
    }
}

/// The body of `Hello`, and the server's identity in `Refuse`: a name (a `u8` length
/// and its bytes), a `u16` major and a `u16` minor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub name: String,
    pub major: u16,
    pub minor: u16,
}

impl Wire for Hello {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        if !valid_name(&self.name) {
            return Err(WireError::Invalid);
        }
        e.put_raw(&[self.name.len() as u8]);
        e.put_raw(self.name.as_bytes());
        self.major.encode(e)?;
        self.minor.encode(e)
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let n = d.take_raw(1)?[0];
        let name = core::str::from_utf8(d.take_raw(n as usize)?).map_err(|_| WireError::Invalid)?;
        if !valid_name(name) {
            return Err(WireError::Invalid);
        }
        Ok(Hello {
            name: String::from(name),
            major: u16::decode(d)?,
            minor: u16::decode(d)?,
        })
    }
}

/// Why a server refused a handshake (the first `u32` of `Refuse`'s body).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The server serves another protocol.
    UnknownProtocol = 1,
    /// Same protocol, another major version.
    MajorMismatch = 2,
    /// The first frame was not a well-formed `Hello`.
    NotHello = 3,
}

impl Refusal {
    pub const ALL: [Refusal; 3] = [
        Refusal::UnknownProtocol,
        Refusal::MajorMismatch,
        Refusal::NotHello,
    ];

    pub fn from_code(code: u32) -> Option<Refusal> {
        Refusal::ALL.iter().copied().find(|r| *r as u32 == code)
    }
}

/// A refused handshake: the reason and what the server serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub reason: Refusal,
    pub server: Hello,
}

impl Wire for Refused {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        (self.reason as u32).encode(e)?;
        self.server.encode(e)
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let reason = Refusal::from_code(u32::decode(d)?).ok_or(WireError::Invalid)?;
        Ok(Refused {
            reason,
            server: Hello::decode(d)?,
        })
    }
}

/// A failure the SDK reports for a request, rather than the server's code: the code
/// in a `Failure` frame at or above [`Status::FIRST`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The server's protocol has no request with this ordinal.
    UnknownMethod = 0x1_0001,
    /// The request's body did not decode as that request's arguments: truncated,
    /// invalid, a missing or wrong handle, or something left over.
    BadMessage = 0x1_0002,
    /// The server's reply did not fit the channel's bound.
    ReplyTooLarge = 0x1_0003,
}

impl Status {
    /// Codes below this are the protocol's own errors.
    pub const FIRST: u32 = 0x1_0000;
    pub const ALL: [Status; 3] = [
        Status::UnknownMethod,
        Status::BadMessage,
        Status::ReplyTooLarge,
    ];

    pub fn from_code(code: u32) -> Option<Status> {
        Status::ALL.iter().copied().find(|s| *s as u32 == code)
    }
}

/// A protocol's typed errors, as the `protocol!` schema declares them: codes 1 to
/// 65535.
pub trait ProtocolError: Sized + Copy + Debug + PartialEq {
    fn code(&self) -> u32;
    fn from_code(code: u32) -> Option<Self>;
}

/// The error type of a protocol that declares none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoError {}

impl ProtocolError for NoError {
    fn code(&self) -> u32 {
        match *self {}
    }
    fn from_code(_: u32) -> Option<Self> {
        None
    }
}

/// Whether every ordinal is nonzero and appears once; the schema asserts it at
/// compile time.
pub const fn ordinals_valid(ordinals: &[u16]) -> bool {
    let mut i = 0;
    while i < ordinals.len() {
        if ordinals[i] == 0 {
            return false;
        }
        let mut j = i + 1;
        while j < ordinals.len() {
            if ordinals[i] == ordinals[j] {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

/// Whether something that arrived in minor version `since` exists in minor `minor`.
pub const fn since_valid(since: u16, minor: u16) -> bool {
    since <= minor
}

/// Whether every error code is in 1 to 65535 and appears once.
pub const fn codes_valid(codes: &[u32]) -> bool {
    let mut i = 0;
    while i < codes.len() {
        if codes[i] == 0 || codes[i] >= Status::FIRST {
            return false;
        }
        let mut j = i + 1;
        while j < codes.len() {
            if codes[i] == codes[j] {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}
