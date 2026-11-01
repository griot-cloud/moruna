//! `moruna.driver.net` 1.0 (SDD 29, d.1): a network card. A client sends Ethernet
//! frames and receives the frames the card receives; it reads the card's address,
//! MTU and link state, waits for the link to change, and reads the counters.
//!
//! Frames travel in messages, a batch of them each way: a card's buffers are its
//! device's (virtio-net's descriptors, gVNIC's registered page lists), the network
//! server copies a frame into its own buffers anyway, and one message carries up to
//! [`SEND_FRAMES_MAX`] frames out or 64 KiB of frames in. A frame is a whole Ethernet
//! frame without its frame check sequence: destination, source, EtherType, payload.
//!
//! Offloads are declared, never assumed: [`Info::offloads`] says what the card does to
//! frames on the client's behalf, and in 1.0 that is nothing, so a client computes
//! every checksum itself and every frame the card hands over is as it arrived.
//!
//! [`Server`] is the server side every driver runs: the requests answered over a
//! [`Hub`] (who has subscribed, each connection's bounded queue, waiting receives,
//! the link and its waiters, the counters), sends queued until their frames are on
//! the card's [`Ring`]. [`suite`] checks one card; [`pair_suite`] checks two
//! cards on one link, each frame out of one arriving at the other.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod hub;
pub mod server;
mod suites;

use alloc::vec::Vec;

use moruna_svc::{Decoder, Encoder, Wire, WireError, limits};

pub use hub::{Answer, Hub};
pub use server::{Ring, Server};
pub use suites::{ETHERTYPE, frame, pair_suite, suite};

/// Bytes of an Ethernet header: destination, source, EtherType.
pub const HEADER: usize = 14;
/// The shortest frame a client may send: a header and no payload. A card pads what it
/// must; a receiver may see a short frame padded to 60 bytes with zeros.
pub const FRAME_MIN: usize = HEADER;
/// The longest frame the protocol carries: a jumbo frame's 9000-byte payload and
/// more. A card's `max_frame` is at most this.
pub const FRAME_MAX: usize = 16 * 1024;
/// The most frames one `send` carries.
pub const SEND_FRAMES_MAX: usize = 64;
/// The most frames a connection's receive queue holds; more are dropped and counted.
pub const QUEUE_FRAMES: usize = 1024;
/// The most bytes a connection's receive queue holds.
pub const QUEUE_BYTES: usize = 4 << 20;
/// The bytes of frames one `receive` answers with, encoding included: a message's
/// body less the reply's count.
pub const RECEIVE_BYTES: usize = limits::CHANNEL_MAX_BYTES - 64;

// A receive always carries at least one frame of the largest size.
const _: () = assert!(4 + FRAME_MAX <= RECEIVE_BYTES);
// A receive's reply, its count and its header fit one message.
const _: () = assert!(RECEIVE_BYTES + 64 <= limits::CHANNEL_MAX_BYTES);

/// What a card does to frames on the client's behalf (`Info::offloads`). No bit is
/// defined in 1.0; a card sets none and a client relies on none.
pub mod offload {
    /// Every bit this version defines.
    pub const KNOWN: u64 = 0;
}

/// A frame: an Ethernet frame without its frame check sequence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame(pub Vec<u8>);

impl Wire for Frame {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        e.put_len(self.0.len())?;
        e.put_raw(&self.0);
        Ok(())
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        let n = d.take_len()?;
        Ok(Frame(d.take_raw(n)?.to_vec()))
    }
}

/// What `info` answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// The card's address: the first octet in bits 40 to 47.
    pub mac: u64,
    /// The largest payload a frame carries.
    pub mtu: u32,
    /// The largest frame: `mtu` plus the header.
    pub max_frame: u32,
    /// What the card does to frames (`offload`); zero in 1.0.
    pub offloads: u64,
    /// Transmit and receive queue pairs the driver uses.
    pub queues: u32,
}

impl Wire for Info {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.mac.encode(e)?;
        self.mtu.encode(e)?;
        self.max_frame.encode(e)?;
        self.offloads.encode(e)?;
        self.queues.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Info {
            mac: u64::decode(d)?,
            mtu: u32::decode(d)?,
            max_frame: u32::decode(d)?,
            offloads: u64::decode(d)?,
            queues: u32::decode(d)?,
        })
    }
}

/// What `link` answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub up: bool,
    /// Megabits a second, zero when the card does not say.
    pub speed_mbps: u32,
    /// Changes each time the link does.
    pub generation: u32,
}

impl Wire for Link {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.up.encode(e)?;
        self.speed_mbps.encode(e)?;
        self.generation.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Link {
            up: bool::decode(d)?,
            speed_mbps: u32::decode(d)?,
            generation: u32::decode(d)?,
        })
    }
}

/// What `stats` answers: the card's counters, and the calling connection's drops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub tx_frames: u64,
    pub tx_bytes: u64,
    pub rx_frames: u64,
    pub rx_bytes: u64,
    /// Frames this connection's full queue dropped.
    pub rx_dropped: u64,
    /// Frames the device handed over malformed, dropped.
    pub rx_errors: u64,
    /// Frames the device reported it could not send.
    pub tx_errors: u64,
}

impl Wire for Stats {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        for v in [
            self.tx_frames,
            self.tx_bytes,
            self.rx_frames,
            self.rx_bytes,
            self.rx_dropped,
            self.rx_errors,
            self.tx_errors,
        ] {
            v.encode(e)?;
        }
        Ok(())
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Stats {
            tx_frames: u64::decode(d)?,
            tx_bytes: u64::decode(d)?,
            rx_frames: u64::decode(d)?,
            rx_bytes: u64::decode(d)?,
            rx_dropped: u64::decode(d)?,
            rx_errors: u64::decode(d)?,
            tx_errors: u64::decode(d)?,
        })
    }
}

moruna_svc::protocol! {
    /// A network card: Ethernet frames out and in, its address, MTU and link.
    pub protocol net {
        name = "moruna.driver.net";
        major = 1;
        minor = 0;
        requests {
            /// The card's address, MTU, largest frame, offloads and queues.
            1 => fn info() -> Info;
            /// The link as it is now.
            2 => fn link() -> Link;
            /// The link, once its generation is not `generation`: at once if it
            /// already is not.
            3 => fn link_changed(generation: u32) -> Link;
            /// Sends `frames`, answered with their count once every one is on the
            /// device's ring; a batch with a frame it refuses is refused whole.
            4 => fn send(frames: Vec<Frame>) -> u32;
            /// From now on the frames the card receives are queued for this
            /// connection, as for every other subscribed one.
            5 => fn subscribe() -> ();
            /// At most `max` of the frames queued for this connection, answered once
            /// there is one.
            6 => fn receive(max: u32) -> Vec<Frame>;
            /// The card's counters and this connection's drops.
            7 => fn stats() -> Stats;
        }
        events {}
        errors {
            /// A frame shorter than an Ethernet header.
            1 => TooShort;
            /// A frame longer than `max_frame`.
            2 => TooLong;
            /// A frame whose source is a group (multicast or broadcast) address.
            3 => BadSource;
            /// A send of no frames.
            4 => Empty;
            /// A send of more than `SEND_FRAMES_MAX` frames.
            5 => TooMany;
            /// A receive on a connection that has not subscribed.
            6 => NotSubscribed;
            /// A receive while another of this connection's waits.
            7 => InUse;
            /// The device failed.
            8 => Io;
        }
    }
}

/// The six octets of `mac`.
pub fn octets(mac: u64) -> [u8; 6] {
    let b = mac.to_be_bytes();
    [b[2], b[3], b[4], b[5], b[6], b[7]]
}

/// The address whose octets are `b`.
pub fn mac(b: [u8; 6]) -> u64 {
    u64::from_be_bytes([0, 0, b[0], b[1], b[2], b[3], b[4], b[5]])
}

/// Whether `mac` is a group address: the first octet's lowest bit.
pub fn is_group(mac: u64) -> bool {
    mac >> 40 & 1 == 1
}

/// Refuses a batch a card must not send: none, too many, a frame shorter than a
/// header or longer than `max_frame`, a group source address.
pub fn check(frames: &[Frame], max_frame: u32) -> Result<(), net::Error> {
    if frames.is_empty() {
        return Err(net::Error::Empty);
    }
    if frames.len() > SEND_FRAMES_MAX {
        return Err(net::Error::TooMany);
    }
    for f in frames {
        if f.0.len() < FRAME_MIN {
            return Err(net::Error::TooShort);
        }
        if f.0.len() > max_frame as usize {
            return Err(net::Error::TooLong);
        }
        if f.0[6] & 1 == 1 {
            return Err(net::Error::BadSource);
        }
    }
    Ok(())
}

/// A hash of a frame's flow, for choosing its transmit queue so a flow's frames keep
/// their order: the addresses and ports of an IPv4 or IPv6 TCP or UDP frame, the
/// addresses of another IP frame, and zero for any other frame.
pub fn flow_hash(frame: &[u8]) -> u32 {
    fn fnv(parts: &[&[u8]]) -> u32 {
        let mut h: u32 = 0x811c_9dc5;
        for part in parts {
            for b in *part {
                h = (h ^ *b as u32).wrapping_mul(0x0100_0193);
            }
        }
        h
    }
    let ether = frame.get(12..14).map(|t| u16::from_be_bytes([t[0], t[1]]));
    let ip = &frame[HEADER.min(frame.len())..];
    let (addresses, protocol, l4) = match ether {
        Some(0x0800) if ip.len() >= 20 && ip[0] >> 4 == 4 => {
            let ihl = (ip[0] & 0xf) as usize * 4;
            let fragment = u16::from_be_bytes([ip[6], ip[7]]) & 0x1fff != 0;
            let l4 = if fragment { None } else { ip.get(ihl..ihl + 4) };
            (&ip[12..20], ip[9], l4)
        }
        Some(0x86dd) if ip.len() >= 40 && ip[0] >> 4 == 6 => (&ip[8..40], ip[6], ip.get(40..44)),
        _ => return 0,
    };
    match (protocol, l4) {
        (6 | 17, Some(ports)) => fnv(&[addresses, &[protocol], ports]),
        _ => fnv(&[addresses]),
    }
}

#[cfg(test)]
mod tests;
