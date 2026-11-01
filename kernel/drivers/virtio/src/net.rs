//! What a virtio network driver reads and writes besides its rings (virtio 1.2, 5.1):
//! the device configuration, the header before every frame, and the control queue's
//! command that sets how many queue pairs the device uses. Every value the device
//! gives is checked here before a driver believes it.

use core::ops::Range;

use moruna_abi::Error;
use moruna_mmio::Region;

/// virtio-net feature bits (virtio 1.2, 5.1.3).
pub mod feature {
    /// The device reports its MTU.
    pub const MTU: u64 = 1 << 3;
    /// The device has an address.
    pub const MAC: u64 = 1 << 5;
    /// The device reports its link.
    pub const STATUS: u64 = 1 << 16;
    /// The device has a control queue.
    pub const CTRL_VQ: u64 = 1 << 17;
    /// The device has several queue pairs, set through the control queue.
    pub const MQ: u64 = 1 << 22;
}

/// Offsets in the device configuration.
pub mod config {
    pub const MAC: usize = 0;
    pub const STATUS: usize = 6;
    pub const MAX_PAIRS: usize = 8;
    pub const MTU: usize = 10;
    /// The status bit that says the link is up.
    pub const LINK_UP: u16 = 1;
}

/// The header before every frame with `VERSION_1`: flags, GSO type, header length,
/// GSO size, checksum start and offset, and the buffer count.
pub const HEADER: usize = 12;
/// The most queue pairs the specification lets a device have.
pub const PAIRS_MAX: u16 = 0x8000;
/// The MTU a device without `MTU` is taken to have.
pub const DEFAULT_MTU: u16 = 1500;

/// The device configuration a driver uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetConfig {
    /// The address, the first octet in bits 40 to 47.
    pub mac: u64,
    /// Up, or `true` for a device that does not report its link.
    pub link_up: bool,
    /// The queue pairs the device has: 1 without `MQ`.
    pub max_pairs: u16,
    pub mtu: u16,
}

/// Why a device's configuration or a received buffer is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    Kernel(Error),
    /// The device's address is a group address or zero.
    BadAddress(u64),
    /// The device reports a queue pair count outside 1 to 0x8000.
    BadPairs(u16),
    /// The device reports an MTU below 68.
    BadMtu(u16),
    /// The device returned a buffer shorter than a header and a frame header, or
    /// longer than the buffer or the largest frame.
    BadLength(u32),
    /// The device's header asks for an offload this driver did not negotiate.
    Offload {
        flags: u8,
        gso: u8,
    },
}

impl From<Error> for NetError {
    fn from(e: Error) -> NetError {
        NetError::Kernel(e)
    }
}

/// The configuration's fields as the device holds them, read under one generation
/// (`Transport::device_config`) and checked by [`check_config`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawConfig {
    pub mac: [u8; 6],
    pub status: Option<u16>,
    pub max_pairs: Option<u16>,
    pub mtu: Option<u16>,
}

/// Reads the fields of a device that negotiated `features`.
pub fn read_raw<R: Region + ?Sized>(r: &R, features: u64) -> Result<RawConfig, Error> {
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = r.read8(config::MAC + i)?;
    }
    let field = |bit: u64, at: usize| -> Result<Option<u16>, Error> {
        if features & bit != 0 {
            r.read16(at).map(Some)
        } else {
            Ok(None)
        }
    };
    Ok(RawConfig {
        mac,
        status: field(feature::STATUS, config::STATUS)?,
        max_pairs: field(feature::MQ, config::MAX_PAIRS)?,
        mtu: field(feature::MTU, config::MTU)?,
    })
}

/// Checks the fields: a station's address, 1 to 0x8000 queue pairs, an MTU of at
/// least 68; a field the device does not have takes its default.
pub fn check_config(raw: RawConfig) -> Result<NetConfig, NetError> {
    let mac = raw.mac.iter().fold(0u64, |m, b| m << 8 | *b as u64);
    if mac == 0 || mac >> 40 & 1 == 1 {
        return Err(NetError::BadAddress(mac));
    }
    let max_pairs = raw.max_pairs.unwrap_or(1);
    if max_pairs == 0 || max_pairs > PAIRS_MAX {
        return Err(NetError::BadPairs(max_pairs));
    }
    let mtu = raw.mtu.unwrap_or(DEFAULT_MTU);
    if mtu < 68 {
        return Err(NetError::BadMtu(mtu));
    }
    Ok(NetConfig {
        mac,
        link_up: raw.status.is_none_or(|s| s & config::LINK_UP != 0),
        max_pairs,
        mtu,
    })
}

/// Reads and checks the configuration of a device that negotiated `features`.
pub fn read_config<R: Region + ?Sized>(r: &R, features: u64) -> Result<NetConfig, NetError> {
    check_config(read_raw(r, features)?)
}

/// Reads the link from the configuration of a device that negotiated `STATUS`.
pub fn link_up<R: Region + ?Sized>(r: &R) -> Result<bool, Error> {
    Ok(r.read16(config::STATUS)? & config::LINK_UP != 0)
}

/// Writes the header of a frame to send at `at`: no checksum, no segmentation.
pub fn write_header<R: Region + ?Sized>(r: &R, at: usize) -> Result<(), Error> {
    r.write64(at, 0)?;
    r.write32(at + 8, 0)
}

/// The frame in a receive buffer at `at` of `buffer` bytes the device returned with
/// `used` bytes written: its range, after the header. A buffer whose length is out
/// of bounds, or whose header asks for an offload, is refused.
pub fn received<R: Region + ?Sized>(
    r: &R,
    at: usize,
    buffer: usize,
    used: u32,
    max_frame: usize,
) -> Result<Range<usize>, NetError> {
    let len = used as usize;
    if len < HEADER + 14 || len > buffer || len - HEADER > max_frame {
        return Err(NetError::BadLength(used));
    }
    let (flags, gso) = (r.read8(at)?, r.read8(at + 1)?);
    if flags != 0 || gso != 0 {
        return Err(NetError::Offload { flags, gso });
    }
    Ok(at + HEADER..at + len)
}

/// The control queue's command classes and commands (virtio 1.2, 5.1.6.5).
pub mod ctrl {
    pub const CLASS_MQ: u8 = 4;
    pub const MQ_VQ_PAIRS_SET: u8 = 0;
    /// The ack a device writes when it carried the command out.
    pub const OK: u8 = 0;
}

/// Writes the command that sets `pairs` queue pairs at `at`: four bytes.
pub fn write_pairs_command<R: Region + ?Sized>(r: &R, at: usize, pairs: u16) -> Result<(), Error> {
    r.write32(
        at,
        ctrl::CLASS_MQ as u32 | (ctrl::MQ_VQ_PAIRS_SET as u32) << 8 | (pairs as u32) << 16,
    )
}

/// The index of receive queue `pair`, its transmit queue, and the control queue of a
/// device with `max_pairs` pairs and `MQ` negotiated.
pub fn rx_queue(pair: u16) -> u16 {
    2 * pair
}

pub fn tx_queue(pair: u16) -> u16 {
    2 * pair + 1
}

pub fn ctrl_queue(max_pairs: u16, mq: bool) -> u16 {
    if mq { 2 * max_pairs } else { 2 }
}
