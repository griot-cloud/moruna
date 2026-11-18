//! What a virtio network driver reads and writes besides its rings (virtio 1.2, 5.1):
//! the device configuration, the header before every frame, and the control queue's
//! command that sets how many queue pairs the device uses. Every value the device
//! gives is checked here before a driver believes it.
//!
//! A received header's flags are taken as Linux's virtio_net takes them, whatever was
//! negotiated: hosts set them without `GUEST_CSUM` (Linux's tun writes `DATA_VALID` on
//! every frame whose checksum the host trusts; item 1.6f). `DATA_VALID` is taken, and
//! the stack checks the checksum itself; `NEEDS_CSUM`'s partial checksum is completed
//! here, so the stack checks a whole one; a bit not known is ignored (virtio 1.2,
//! 5.1.6.4.2); a GSO type, which needs a feature never offered, is refused.
//! [`RxCounts`] counts the frames taken with flags and the buffers refused, by reason.

use alloc::collections::BTreeMap;
use core::{fmt, ops::Range};

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

/// The flags of a received header (virtio 1.2, 5.1.6).
pub mod flag {
    /// The checksum at `csum_offset` from `csum_start` is partial: it holds the
    /// pseudo-header's sum, and the ones' complement sum from `csum_start` to the end
    /// of the frame completes it.
    pub const NEEDS_CSUM: u8 = 1;
    /// The device validated the frame's checksum.
    pub const DATA_VALID: u8 = 2;
    /// `csum_start` and `csum_offset` count coalesced segments (`RSC_EXT`): they are
    /// not offsets.
    pub const RSC_INFO: u8 = 4;
}

/// The GSO type of a frame that asks for no segmentation.
pub const GSO_NONE: u8 = 0;
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
    /// The header asks for segmentation, which needs a `GUEST_TSO` or `GUEST_UFO`
    /// feature this driver never offers: its GSO type.
    Gso(u8),
    /// The header asks for a partial checksum to be completed (`NEEDS_CSUM`) where it
    /// cannot be: its field past the frame's end, or offsets that are counts
    /// (`RSC_INFO`).
    Checksum {
        flags: u8,
        start: u16,
        offset: u16,
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

/// A frame a device returned, checked: where it is, its header's flags, and the
/// partial checksum to complete before the frame is believed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    /// The frame, after the header.
    pub frame: Range<usize>,
    pub flags: u8,
    /// Set with `NEEDS_CSUM`.
    pub partial: Option<Partial>,
}

/// A partial checksum: summing starts at `start` and the checksum goes at
/// `start + offset`, both from the frame's first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partial {
    pub start: u16,
    pub offset: u16,
}

impl Received {
    /// Completes the partial checksum, if the frame has one, in `frame`, the frame's
    /// bytes: the ones' complement of the sum from `start` to the end (the field holds
    /// the pseudo-header's sum), stored big-endian. A zero is stored as `0xffff`, as
    /// Linux stores it, since UDP's zero says there is no checksum. Refused when the
    /// field is not inside `frame`.
    pub fn complete(&self, frame: &mut [u8]) -> Result<(), NetError> {
        let Some(Partial { start, offset }) = self.partial else {
            return Ok(());
        };
        let field = start as usize + offset as usize;
        if field + 2 > frame.len() {
            return Err(NetError::Checksum {
                flags: self.flags,
                start,
                offset,
            });
        }
        let checksum = match !ones_sum(&frame[start as usize..]) {
            0 => 0xffff,
            c => c,
        };
        frame[field..field + 2].copy_from_slice(&checksum.to_be_bytes());
        Ok(())
    }
}

/// The 16-bit ones' complement sum of `bytes` as big-endian words, an odd last byte
/// padded with a zero (RFC 1071), folded to 16 bits.
pub fn ones_sum(bytes: &[u8]) -> u16 {
    let (words, rest) = bytes.as_chunks::<2>();
    let mut sum: u64 = words.iter().map(|w| u16::from_be_bytes(*w) as u64).sum();
    if let [last] = rest {
        sum += (*last as u64) << 8;
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

/// The frame in a receive buffer at `at` of `buffer` bytes the device returned with
/// `used` bytes written: its range after the header, its flags and its partial
/// checksum. A buffer is refused whose length is out of bounds, whose header asks for
/// segmentation, or whose partial checksum's field is past the frame's end or whose
/// offsets are `RSC_INFO`'s counts. `DATA_VALID` and bits not known are taken: the
/// stack checks every checksum itself.
pub fn received<R: Region + ?Sized>(
    r: &R,
    at: usize,
    buffer: usize,
    used: u32,
    max_frame: usize,
) -> Result<Received, NetError> {
    let len = used as usize;
    if len < HEADER + 14 || len > buffer || len - HEADER > max_frame {
        return Err(NetError::BadLength(used));
    }
    let (flags, gso) = (r.read8(at)?, r.read8(at + 1)?);
    if gso != GSO_NONE {
        return Err(NetError::Gso(gso));
    }
    let partial = if flags & flag::NEEDS_CSUM != 0 {
        let (start, offset) = (r.read16(at + 6)?, r.read16(at + 8)?);
        if flags & flag::RSC_INFO != 0 || start as usize + offset as usize + 2 > len - HEADER {
            return Err(NetError::Checksum {
                flags,
                start,
                offset,
            });
        }
        Some(Partial { start, offset })
    } else {
        None
    };
    Ok(Received {
        frame: at + HEADER..at + len,
        flags,
        partial,
    })
}

/// The header of the buffer at `at`, as the device wrote it.
pub fn header<R: Region + ?Sized>(r: &R, at: usize) -> Result<[u8; HEADER], Error> {
    let mut bytes = [0u8; HEADER];
    r.read_bytes(at, &mut bytes)?;
    Ok(bytes)
}

/// Why a buffer the device returned was refused, as a driver counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// Its length was out of bounds.
    Length,
    /// Its flags asked for a partial checksum that cannot be completed: the flags.
    Flags(u8),
    /// Its header asked for segmentation: the GSO type.
    Gso(u8),
}

impl Reason {
    /// What a buffer refused with `e` is counted under; `None` for an error that is
    /// not the buffer's (the kernel's, or the configuration's).
    pub fn of(e: &NetError) -> Option<Reason> {
        match *e {
            NetError::BadLength(_) => Some(Reason::Length),
            NetError::Checksum { flags, .. } => Some(Reason::Flags(flags)),
            NetError::Gso(gso) => Some(Reason::Gso(gso)),
            NetError::Kernel(_)
            | NetError::BadAddress(_)
            | NetError::BadPairs(_)
            | NetError::BadMtu(_) => None,
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Length => f.write_str("length"),
            Reason::Flags(flags) => write!(f, "flags {flags:#04x}"),
            Reason::Gso(gso) => write!(f, "gso {gso:#04x}"),
        }
    }
}

/// The first buffer a driver refused: why, the bytes the device wrote, and its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct First {
    pub reason: Reason,
    pub used: u32,
    pub header: [u8; HEADER],
}

impl fmt::Display for First {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the first malformed buffer from the card: {}, {} bytes written, header",
            self.reason, self.used
        )?;
        self.header.iter().try_for_each(|b| write!(f, " {b:02x}"))
    }
}

/// What a driver counts of the buffers its device returned: those refused, by
/// reason; the frames taken whose header had flags, by flags; and the first refusal,
/// held until it is printed. Shown at the end of the driver's last line, so a boot
/// that drops frames says why.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RxCounts {
    refused: BTreeMap<Reason, u64>,
    taken: BTreeMap<u8, u64>,
    first: Option<First>,
    told: bool,
}

impl RxCounts {
    /// Counts a buffer refused for `reason`: `used` bytes written, and its header.
    pub fn refuse(&mut self, reason: Reason, used: u32, header: [u8; HEADER]) {
        *self.refused.entry(reason).or_default() += 1;
        if !self.told && self.first.is_none() {
            self.first = Some(First {
                reason,
                used,
                header,
            });
        }
    }

    /// Counts a frame taken, when its header had flags.
    pub fn take(&mut self, flags: u8) {
        if flags != 0 {
            *self.taken.entry(flags).or_default() += 1;
        }
    }

    /// The buffers refused.
    pub fn refused(&self) -> u64 {
        self.refused.values().sum()
    }

    /// The first refusal, once, for its line on the console.
    pub fn first_to_print(&mut self) -> Option<First> {
        let first = self.first.take()?;
        self.told = true;
        Some(first)
    }
}

impl fmt::Display for RxCounts {
    /// `3 malformed from the card (length: 1, flags 0x05: 1, gso 0x01: 1), 46 taken
    /// with flags (0x02: 46)`, an empty list left out.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} malformed from the card", self.refused())?;
        for (i, (reason, n)) in self.refused.iter().enumerate() {
            write!(f, "{}{reason}: {n}", if i == 0 { " (" } else { ", " })?;
        }
        if !self.refused.is_empty() {
            f.write_str(")")?;
        }
        write!(f, ", {} taken with flags", self.taken.values().sum::<u64>())?;
        for (i, (flags, n)) in self.taken.iter().enumerate() {
            write!(f, "{}{flags:#04x}: {n}", if i == 0 { " (" } else { ", " })?;
        }
        if !self.taken.is_empty() {
            f.write_str(")")?;
        }
        Ok(())
    }
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
