//! `moruna.driver.block` 2.0 (SDD 25, d.3): a block device. Data never travels in a
//! message: a client sends a handle to its buffer, a DMA memory object, the driver
//! pins it and its device reads or writes it directly, and the reply hands the buffer
//! back. Mutating commands require an actual connection-bound exclusive writer lease.
//! A client may register a buffer once (`register`), which the driver
//! pins once and keeps pinned until `unregister` or the connection ends, and then
//! move sectors to and from any of its pages (`read_at`, `write_at`) with no pin per
//! request: the image filesystem's page cache works so (SDD 27, f.5).
//!
//! Minor 2 places a transfer at any byte of a registration (`read_to`, `write_from`), so a
//! client reading a byte range of the disk into memory it did not choose lands the whole
//! sectors in place, and only the sectors at the range's edges pass through a buffer of its
//! own (`architecture/moruna-os-system.md` 3C, item 3.3). A driver whose device cannot place
//! data at that byte answers `BadBuffer`, and the client moves that one transfer another way.
//! The suite checks them only on a driver that announces minor 2 at its welcome, and that the
//! rest answer them `UnknownMethod`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
pub mod lease;

// The conformance suite makes DMA memory, which only the kernel has: the SDK's host model
// of channels and memory does not model DMA. On a host the protocol, its types and the
// writer lease build alone, so a driver's logic is tested there request by request.
#[cfg(target_os = "none")]
mod suite;
#[cfg(target_os = "none")]
pub use suite::suite;

use moruna_svc::{Decoder, Encoder, Memory, Wire, WireError};

/// What `info` answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Bytes in a sector: 512 or a larger power of two.
    pub sector_size: u32,
    /// The device's capacity, in sectors.
    pub sectors: u64,
    /// The most sectors one read or write moves.
    pub max_sectors: u32,
    pub read_only: bool,
}

impl Wire for Info {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        self.sector_size.encode(e)?;
        self.sectors.encode(e)?;
        self.max_sectors.encode(e)?;
        self.read_only.encode(e)
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Info {
            sector_size: u32::decode(d)?,
            sectors: u64::decode(d)?,
            max_sectors: u32::decode(d)?,
            read_only: bool::decode(d)?,
        })
    }
}

moruna_svc::protocol! {
    /// A block device: its geometry, and sectors read into and written from memory
    /// objects its device moves data to and from directly.
    pub protocol block {
        name = "moruna.driver.block";
        major = 2;
        minor = 2;
        requests {
            /// The device's sector size, capacity and largest transfer.
            1 => fn info() -> Info;
            /// Reads `count` sectors from `sector` into the first bytes of `buffer` (a
            /// DMA memory object with `READ` and `WRITE`), and hands it back.
            2 => fn read(sector: u64, count: u32, buffer: Memory) -> Memory;
            /// Writes `count` sectors at `sector` from the first bytes of `data` (a DMA
            /// memory object with `READ`), and hands it back.
            3 => fn write(lease:u64, sector: u64, count: u32, data: Memory) -> Memory;
            /// Answered once every write answered before it is on stable storage.
            4 => fn flush(lease:u64) -> ();
            /// Pins `buffer` (a DMA memory object with `READ` and `WRITE`) for as long
            /// as the connection keeps it: its registration number and its pages.
            5 => fn register(buffer: Memory) -> (u32, u32);
            /// Reads `count` sectors from `sector` into registered buffer `id`, from its
            /// page `page`.
            6 => fn read_at(id: u32, page: u32, sector: u64, count: u32) -> ();
            /// Writes `count` sectors at `sector` from registered buffer `id`, from its
            /// page `page`.
            7 => fn write_at(lease:u64, id: u32, page: u32, sector: u64, count: u32) -> ();
            /// Ends registration `id`; its pages are unpinned once no transfer uses them.
            8 => fn unregister(id: u32) -> ();
            /// Exclusive namespace writer bound to this actual SDK connection.
            9 => fn acquire_write() -> u64;
            /// Close admission and release only after original writes and final flush.
            10 => fn release_write(lease:u64) -> ();
            /// Reads `count` sectors from `sector` into registered buffer `id`, the first
            /// byte at its byte `offset`, which need not start a page or a sector.
            11 since 2 => fn read_to(id: u32, offset: u64, sector: u64, count: u32) -> ();
            /// Writes `count` sectors at `sector` from registered buffer `id`, from its byte
            /// `offset`.
            12 since 2 => fn write_from(lease: u64, id: u32, offset: u64, sector: u64, count: u32) -> ();
        }
        events {}
        errors {
            /// The sectors reach past the device's end.
            1 => OutOfRange;
            /// A count of zero, or above `max_sectors`.
            2 => BadCount;
            /// The buffer is not a DMA memory object, is smaller than the transfer, or
            /// lacks a right the transfer needs; or (minor 2) the driver cannot place the
            /// transfer at the byte asked for.
            3 => BadBuffer;
            /// A write to a read-only device.
            4 => ReadOnly;
            /// The device failed the transfer.
            5 => Io;
            /// No registration of this connection has that number.
            6 => NoRegistration;
            /// The connection holds as many registrations as it may.
            7 => TooManyRegistrations;
            /// Another connection owns or is draining the namespace writer.
            8 => LeaseInUse;
            /// This actual connection/generation does not own the writer.
            9 => NotWriter;
            /// Release/disconnect closed writer admission; original IO still owns it.
            10 => LeaseClosing;
            /// Required final durability barrier or ownership accounting failed.
            11 => LeaseFailed;
        }
    }
}

/// The most registrations one connection holds.
pub const REGISTRATIONS_MAX: usize = 64;

/// The bytes a transfer of `count` sectors moves.
pub fn bytes(info: &Info, count: u32) -> u64 {
    info.sector_size as u64 * count as u64
}
