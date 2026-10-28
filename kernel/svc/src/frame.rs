//! Frames (SDD 23, e.1): every message on a protocol's channel is one frame, an 8-byte
//! header and a body.
//!
//! ```text
//! offset 0  u8   kind       (Kind)
//! offset 1  u8   reserved   zero
//! offset 2  u16  ordinal    the request's or event's number; zero otherwise
//! offset 4  u32  txid       a request's id, echoed by its reply; zero otherwise
//! offset 8  body
//! ```
//!
//! A frame that does not fit the channel's bound is refused here, before any call is
//! made, so the sender learns it and nothing reaches the peer.

use alloc::vec::Vec;

use moruna_abi::{Error, limits};

use crate::{
    os::{Channel, Handle},
    wire::{Decoder, Encoder},
};

/// The header's size in bytes.
pub const HEADER: usize = 8;

/// What a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Client to server, first: the protocol's name and the client's version.
    Hello = 1,
    /// Server to client: the handshake succeeded; the server's version.
    Welcome = 2,
    /// Server to client: the handshake failed, and why; the server then closes.
    Refuse = 3,
    /// Client to server: a call, with a nonzero txid.
    Request = 4,
    /// Server to client: the result of the request with the same txid.
    Reply = 5,
    /// Server to client: the request with the same txid failed; a `u32` code.
    Failure = 6,
    /// Server to client, at any time after the welcome: an event.
    Event = 7,
    /// To a listener: one channel endpoint, to be served as a new connection.
    Connect = 8,
}

impl Kind {
    const ALL: [Kind; 8] = [
        Kind::Hello,
        Kind::Welcome,
        Kind::Refuse,
        Kind::Request,
        Kind::Reply,
        Kind::Failure,
        Kind::Event,
        Kind::Connect,
    ];

    fn from_byte(b: u8) -> Option<Kind> {
        Kind::ALL.iter().copied().find(|k| *k as u8 == b)
    }
}

/// Why a frame could not be sent or received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// The frame is over the channel's bound; nothing was sent.
    TooLarge,
    /// The peer's queue is full.
    Full,
    /// The peer has closed its end.
    PeerClosed,
    /// The peer sent something that is not a frame.
    Malformed,
    /// The peer's minor version predates the frame (an event); nothing was sent.
    Unsupported,
    /// Any other refusal by the kernel.
    Kernel(Error),
}

impl From<Error> for Transport {
    fn from(e: Error) -> Transport {
        match e {
            Error::PeerClosed => Transport::PeerClosed,
            Error::ShouldWait => Transport::Full,
            Error::Limit => Transport::TooLarge,
            e => Transport::Kernel(e),
        }
    }
}

/// A received frame.
pub struct Frame {
    pub kind: Kind,
    pub ordinal: u16,
    pub txid: u32,
    bytes: Vec<u8>,
    handles: Vec<Option<Handle>>,
}

impl Frame {
    /// The body's length in bytes.
    pub fn body_len(&self) -> usize {
        self.bytes.len() - HEADER
    }

    /// The number of handles the frame carried.
    pub fn handle_count(&self) -> usize {
        self.handles.len()
    }

    /// A decoder over the body and the handles.
    pub fn decoder(&mut self) -> Decoder<'_> {
        Decoder::new(&self.bytes[HEADER..], &mut self.handles)
    }
}

/// Sends `body` as a frame of `kind`. Over the bound, nothing is sent and the handles
/// are closed with the encoder.
pub fn send(
    channel: &Channel,
    kind: Kind,
    ordinal: u16,
    txid: u32,
    body: Encoder,
) -> Result<(), Transport> {
    let (mut bytes, mut handles) = body.into_parts();
    if bytes.len() > limits::CHANNEL_MAX_BYTES || handles.len() > limits::CHANNEL_MAX_HANDLES {
        return Err(Transport::TooLarge);
    }
    bytes[0] = kind as u8;
    bytes[1] = 0;
    bytes[2..4].copy_from_slice(&ordinal.to_le_bytes());
    bytes[4..8].copy_from_slice(&txid.to_le_bytes());
    channel
        .write_with(&bytes, &mut handles)
        .map_err(Transport::from)
}

/// Reads the next frame; `None` if nothing is queued.
pub fn recv(channel: &Channel) -> Result<Option<Frame>, Transport> {
    let Some((nb, nh)) = channel.needed().map_err(Transport::from)? else {
        return Ok(None);
    };
    let mut bytes = alloc::vec![0u8; nb];
    let mut handles = Vec::new();
    handles.resize_with(nh, || None);
    if nb != 0 || nh != 0 {
        channel
            .read(&mut bytes, &mut handles)
            .map_err(Transport::from)?;
    }
    if bytes.len() < HEADER || bytes[1] != 0 {
        return Err(Transport::Malformed);
    }
    let kind = Kind::from_byte(bytes[0]).ok_or(Transport::Malformed)?;
    Ok(Some(Frame {
        kind,
        ordinal: u16::from_le_bytes([bytes[2], bytes[3]]),
        txid: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        bytes,
        handles,
    }))
}
