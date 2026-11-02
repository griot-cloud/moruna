//! `moruna.driver.block` 2.0 (SDD 25, d.3): a block device. Data never travels in a
//! message: a client sends a handle to its buffer, a DMA memory object, the driver
//! pins it and its device reads or writes it directly, and the reply hands the buffer
//! back. Mutating commands require an actual connection-bound exclusive writer lease.
//! A client may register a buffer once (`register`), which the driver
//! pins once and keeps pinned until `unregister` or the connection ends, and then
//! move sectors to and from any of its pages (`read_at`, `write_at`) with no pin per
//! request: the image filesystem's page cache works so (SDD 27, f.5).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
pub mod lease;

use alloc::{format, string::ToString, vec, vec::Vec};

use moruna_svc::{
    Decoder, Encoder, Memory, ProtocolError, Rights, Wire, WireError,
    conformance::{Case, Fail, Probe, Suite},
    map,
};

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
        minor = 0;
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
        }
        events {}
        errors {
            /// The sectors reach past the device's end.
            1 => OutOfRange;
            /// A count of zero, or above `max_sectors`.
            2 => BadCount;
            /// The buffer is not a DMA memory object, is smaller than the transfer, or
            /// lacks a right the transfer needs.
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

fn wire<E: core::fmt::Debug>(e: E) -> Fail {
    format!("{e:?}")
}

fn info_of(p: &mut Probe) -> Result<Info, Fail> {
    let mut reply = p.request(1, Encoder::new())?.reply()?;
    let mut d = reply.decoder();
    let info = Info::decode(&mut d).map_err(wire)?;
    d.finish().map_err(wire)?;
    Ok(info)
}

fn claim(p: &mut Probe) -> Result<u64, Fail> {
    let mut answer = p.request(9, Encoder::new())?.reply()?;
    let mut decoder = answer.decoder();
    let token = u64::decode(&mut decoder).map_err(wire)?;
    decoder.finish().map_err(wire)?;
    Ok(token)
}
fn token_body(token: u64) -> Result<Encoder, Fail> {
    let mut body = Encoder::new();
    token.encode(&mut body).map_err(wire)?;
    Ok(body)
}
fn release(p: &mut Probe, token: u64) -> Result<(), Fail> {
    empty_reply(p, 10, token_body(token)?)
}
fn write_transfer(token: u64, sector: u64, count: u32, buffer: Memory) -> Result<Encoder, Fail> {
    let mut body = token_body(token)?;
    sector.encode(&mut body).map_err(wire)?;
    count.encode(&mut body).map_err(wire)?;
    buffer.encode(&mut body).map_err(wire)?;
    Ok(body)
}
fn write_at_body(token: u64, id: u32, page: u32, sector: u64, count: u32) -> Result<Encoder, Fail> {
    let mut body = token_body(token)?;
    id.encode(&mut body).map_err(wire)?;
    page.encode(&mut body).map_err(wire)?;
    sector.encode(&mut body).map_err(wire)?;
    count.encode(&mut body).map_err(wire)?;
    Ok(body)
}
fn transfer(sector: u64, count: u32, buffer: Memory) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    sector.encode(&mut e).map_err(wire)?;
    count.encode(&mut e).map_err(wire)?;
    buffer.encode(&mut e).map_err(wire)?;
    Ok(e)
}

/// A request's reply: exactly one memory object.
fn buffer_back(p: &mut Probe, ordinal: u16, body: Encoder) -> Result<Memory, Fail> {
    let mut reply = p.request(ordinal, body)?.reply()?;
    let mut d = reply.decoder();
    let memory = Memory::decode(&mut d).map_err(|e| format!("the reply's buffer: {e:?}"))?;
    d.finish().map_err(wire)?;
    Ok(memory)
}

fn dma(bytes: u64) -> Result<Memory, Fail> {
    Memory::create_dma(bytes.max(4096)).map_err(|e| e.name().to_string())
}

fn read_bytes(memory: &Memory, n: usize) -> Result<Vec<u8>, Fail> {
    let pages = (n as u64).div_ceil(4096).max(1) * 4096;
    let m = memory.map(0, pages, 0).map_err(|e| e.name().to_string())?;
    let mut out = vec![0u8; n];
    m.read(0, &mut out).map_err(|e| e.name().to_string())?;
    Ok(out)
}

fn info(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    if i.sector_size < 512 || !i.sector_size.is_power_of_two() {
        return Err(format!("sector size {}", i.sector_size));
    }
    if i.sectors == 0 || i.max_sectors == 0 {
        return Err(format!("{i:?}"));
    }
    Ok(())
}

fn read_first(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    let back = buffer_back(p, 2, transfer(0, 1, dma(bytes(&i, 1))?)?)?;
    read_bytes(&back, i.sector_size as usize).map(drop)
}

fn read_last_at_max(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    let count = i
        .max_sectors
        .min(u32::try_from(i.sectors).unwrap_or(u32::MAX));
    buffer_back(
        p,
        2,
        transfer(i.sectors - count as u64, count, dma(bytes(&i, count))?)?,
    )
    .map(drop)
}

fn read_past_end(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    p.request(2, transfer(i.sectors, 1, dma(bytes(&i, 1))?)?)?
        .failure(block::Error::OutOfRange.code())?;
    p.request(2, transfer(i.sectors - 1, 2, dma(bytes(&i, 2))?)?)?
        .failure(block::Error::OutOfRange.code())?;
    p.request(2, transfer(u64::MAX, 1, dma(bytes(&i, 1))?)?)?
        .failure(block::Error::OutOfRange.code())
}

fn bad_counts(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    p.request(2, transfer(0, 0, dma(4096)?)?)?
        .failure(block::Error::BadCount.code())?;
    let over = i.max_sectors.saturating_add(1);
    p.request(2, transfer(0, over, dma(bytes(&i, over))?)?)?
        .failure(block::Error::BadCount.code())?;
    if i.read_only {
        return p
            .request(9, Encoder::new())?
            .failure(block::Error::ReadOnly.code());
    }
    let token = claim(p)?;
    p.request(3, write_transfer(token, 0, 0, dma(4096)?)?)?
        .failure(block::Error::BadCount.code())?;
    release(p, token)
}

fn bad_buffers(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    let bad = block::Error::BadBuffer.code();
    // Not made for DMA.
    let plain = Memory::create(4096).map_err(|e| e.name().to_string())?;
    p.request(2, transfer(0, 1, plain)?)?.failure(bad)?;
    // Smaller than the transfer.
    let count = (8192 / i.sector_size + 1).min(i.max_sectors);
    if bytes(&i, count) > 4096 {
        p.request(2, transfer(0, count, dma(4096)?)?)?
            .failure(bad)?;
    }
    // A read needs WRITE on the buffer; a write needs READ.
    let ro = dma(4096)?;
    let ro = Memory(
        ro.0.duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
            .map_err(|e| e.name().to_string())?,
    );
    p.request(2, transfer(0, 1, ro)?)?.failure(bad)?;
    let wo = dma(4096)?;
    let wo = Memory(
        wo.0.duplicate(Rights::WRITE | Rights::TRANSFER)
            .map_err(|e| e.name().to_string())?,
    );
    if i.read_only {
        return Ok(());
    }
    let token = claim(p)?;
    p.request(3, write_transfer(token, 0, 1, wo)?)?
        .failure(bad)?;
    release(p, token)
}

fn write_round_trip(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    if i.read_only {
        return p
            .request(9, Encoder::new())?
            .failure(block::Error::ReadOnly.code());
    }
    let token = claim(p)?;
    let n = i.sector_size as usize;
    let last = i.sectors - 1;
    let original = buffer_back(p, 2, transfer(last, 1, dma(bytes(&i, 1))?)?)?;
    let pattern: Vec<u8> = (0..n).map(|b| (b as u8) ^ 0xa5).collect();
    let data = dma(bytes(&i, 1))?;
    data.map(0, 4096, map::WRITE)
        .and_then(|m| m.write(0, &pattern))
        .map_err(|e| e.name().to_string())?;
    drop(buffer_back(p, 3, write_transfer(token, last, 1, data)?)?);
    let back = buffer_back(p, 2, transfer(last, 1, dma(bytes(&i, 1))?)?)?;
    let read = read_bytes(&back, n)?;
    // Restore the sector before judging, so the disk ends as it began.
    drop(buffer_back(
        p,
        3,
        write_transfer(token, last, 1, original)?,
    )?);
    p.request(4, token_body(token)?)?.reply()?;
    release(p, token)?;
    if read != pattern {
        return Err("the sector read back is not the one written".to_string());
    }
    Ok(())
}

fn flush(p: &mut Probe) -> Result<(), Fail> {
    let info = info_of(p)?;
    if info.read_only {
        return p
            .request(4, token_body(0)?)?
            .failure(block::Error::NotWriter.code());
    }
    let token = claim(p)?;
    empty_reply(p, 4, token_body(token)?)?;
    release(p, token)
}

fn register(p: &mut Probe, buffer: Memory) -> Result<(u32, u32), Fail> {
    let mut e = Encoder::new();
    buffer.encode(&mut e).map_err(wire)?;
    let mut reply = p.request(5, e)?.reply()?;
    let mut d = reply.decoder();
    let answer = <(u32, u32)>::decode(&mut d).map_err(wire)?;
    d.finish().map_err(wire)?;
    Ok(answer)
}

fn at(id: u32, page: u32, sector: u64, count: u32) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    id.encode(&mut e).map_err(wire)?;
    page.encode(&mut e).map_err(wire)?;
    sector.encode(&mut e).map_err(wire)?;
    count.encode(&mut e).map_err(wire)?;
    Ok(e)
}

fn empty_reply(p: &mut Probe, ordinal: u16, body: Encoder) -> Result<(), Fail> {
    let mut reply = p.request(ordinal, body)?.reply()?;
    reply.decoder().finish().map_err(wire)
}

/// A registered buffer of four pages reads, at its third page, what `read` reads into
/// a buffer of its own.
fn registered_read(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    let count = (4096 / i.sector_size).min(i.max_sectors);
    let buffer = dma(4 * 4096)?;
    let mine = Memory(
        buffer
            .0
            .duplicate(Rights::READ | Rights::MAP)
            .map_err(|e| e.name().to_string())?,
    );
    let (id, pages) = register(p, buffer)?;
    if pages != 4 {
        return Err(format!("a registration of four pages reports {pages}"));
    }
    empty_reply(p, 6, at(id, 2, 0, count)?)?;
    let alone = buffer_back(p, 2, transfer(0, count, dma(4096)?)?)?;
    let n = bytes(&i, count) as usize;
    let want = read_bytes(&alone, n)?;
    let m = mine.map(0, 4 * 4096, 0).map_err(|e| e.name().to_string())?;
    let mut got = vec![0u8; n];
    m.read(2 * 4096, &mut got)
        .map_err(|e| e.name().to_string())?;
    if got != want {
        return Err("read_at put other bytes in the page than read gives".to_string());
    }
    empty_reply(p, 8, at_id(id)?)
}

fn at_id(id: u32) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    id.encode(&mut e).map_err(wire)?;
    Ok(e)
}

/// Every refusal of the registered requests.
fn registered_refusals(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    let one = (4096 / i.sector_size).min(i.max_sectors);
    let bad = block::Error::BadBuffer.code();
    let plain = Memory::create(4096).map_err(|e| e.name().to_string())?;
    let mut e = Encoder::new();
    plain.encode(&mut e).map_err(wire)?;
    p.request(5, e)?.failure(bad)?;
    let ro = dma(4096)?;
    let ro = Memory(
        ro.0.duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
            .map_err(|e| e.name().to_string())?,
    );
    let mut e = Encoder::new();
    ro.encode(&mut e).map_err(wire)?;
    p.request(5, e)?.failure(bad)?;
    let (id, _) = register(p, dma(2 * 4096)?)?;
    p.request(6, at(id + 1000, 0, 0, one)?)?
        .failure(block::Error::NoRegistration.code())?;
    p.request(6, at(id, 2, 0, one)?)?.failure(bad)?;
    p.request(6, at(id, 1, 0, 2 * one)?)?.failure(bad)?;
    p.request(6, at(id, 0, 0, 0)?)?
        .failure(block::Error::BadCount.code())?;
    p.request(6, at(id, 0, i.sectors, one)?)?
        .failure(block::Error::OutOfRange.code())?;
    if i.read_only {
        p.request(7, write_at_body(0, id, 0, 0, one)?)?
            .failure(block::Error::NotWriter.code())?;
    }
    empty_reply(p, 8, at_id(id)?)?;
    p.request(6, at(id, 0, 0, one)?)?
        .failure(block::Error::NoRegistration.code())?;
    p.request(8, at_id(id)?)?
        .failure(block::Error::NoRegistration.code())
}

/// A sector written from a registered buffer reads back through another, and the
/// disk is restored.
fn registered_write(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    if i.read_only {
        return Ok(());
    }
    let token = claim(p)?;
    let n = i.sector_size as usize;
    let last = i.sectors - 1;
    let original = buffer_back(p, 2, transfer(last, 1, dma(bytes(&i, 1))?)?)?;
    let out = dma(4096)?;
    let pattern: Vec<u8> = (0..n).map(|b| (b as u8) ^ 0x3c).collect();
    out.map(0, 4096, map::WRITE)
        .and_then(|m| m.write(0, &pattern))
        .map_err(|e| e.name().to_string())?;
    let back = dma(4096)?;
    let view = Memory(
        back.0
            .duplicate(Rights::READ | Rights::MAP)
            .map_err(|e| e.name().to_string())?,
    );
    let (wid, _) = register(p, out)?;
    let (rid, _) = register(p, back)?;
    empty_reply(p, 7, write_at_body(token, wid, 0, last, 1)?)?;
    empty_reply(p, 6, at(rid, 0, last, 1)?)?;
    let read = read_bytes(&view, n)?;
    drop(buffer_back(
        p,
        3,
        write_transfer(token, last, 1, original)?,
    )?);
    p.request(4, token_body(token)?)?.reply()?;
    release(p, token)?;
    if read != pattern {
        return Err(
            "the sector read back through a registration is not the one written".to_string(),
        );
    }
    Ok(())
}

fn writer_permissions(p: &mut Probe) -> Result<(), Fail> {
    let info = info_of(p)?;
    p.request(4, token_body(0)?)?
        .failure(block::Error::NotWriter.code())?;
    p.request(3, write_transfer(0, 0, 1, dma(bytes(&info, 1))?)?)?
        .failure(block::Error::NotWriter.code())?;
    if info.read_only {
        return p
            .request(9, Encoder::new())?
            .failure(block::Error::ReadOnly.code());
    }
    let token = claim(p)?;
    p.request(9, Encoder::new())?
        .failure(block::Error::LeaseInUse.code())?;
    p.request(
        4,
        token_body(token.checked_add(1).ok_or("lease token overflow")?)?,
    )?
    .failure(block::Error::NotWriter.code())?;
    release(p, token)?;
    p.request(4, token_body(token)?)?
        .failure(block::Error::NotWriter.code())
}

/// A connection holds at most `REGISTRATIONS_MAX` registrations.
fn registration_bound(p: &mut Probe) -> Result<(), Fail> {
    for _ in 0..REGISTRATIONS_MAX {
        register(p, dma(4096)?)?;
    }
    let mut e = Encoder::new();
    dma(4096)?.encode(&mut e).map_err(wire)?;
    p.request(5, e)?
        .failure(block::Error::TooManyRegistrations.code())
}

fn not_a_memory_object(p: &mut Probe) -> Result<(), Fail> {
    let (a, _b) = moruna_svc::Channel::create().map_err(|e| e.name().to_string())?;
    let mut e = Encoder::new();
    0u64.encode(&mut e).map_err(wire)?;
    1u32.encode(&mut e).map_err(wire)?;
    a.encode(&mut e).map_err(wire)?;
    p.request(2, e)?.status(moruna_svc::Status::BadMessage)
}

const CASES: &[Case] = &[
    Case {
        name: "writer lease: actual connection token, wrong/stale tokens and readonly refusal",
        check: writer_permissions,
    },
    Case {
        name: "info: a power-of-two sector size, a capacity and a largest transfer",
        check: info,
    },
    Case {
        name: "read: the first sector comes back in the buffer sent",
        check: read_first,
    },
    Case {
        name: "read: the largest transfer, ending at the last sector",
        check: read_last_at_max,
    },
    Case {
        name: "read: sectors past the end are refused with OutOfRange",
        check: read_past_end,
    },
    Case {
        name: "read and write: a count of zero or above the largest is BadCount",
        check: bad_counts,
    },
    Case {
        name: "read and write: a buffer not for DMA, too small, or without the rights is BadBuffer",
        check: bad_buffers,
    },
    Case {
        name: "write: a sector written reads back, and the disk is restored",
        check: write_round_trip,
    },
    Case {
        name: "flush: answered",
        check: flush,
    },
    Case {
        name: "read: a handle that is not a memory object is a bad message",
        check: not_a_memory_object,
    },
    Case {
        name: "read_at: a registered buffer's page holds what read gives",
        check: registered_read,
    },
    Case {
        name: "register, read_at, write_at, unregister: every refusal",
        check: registered_refusals,
    },
    Case {
        name: "write_at: a sector written from one registration reads back through another",
        check: registered_write,
    },
    Case {
        name: "register: a connection's registrations are bounded",
        check: registration_bound,
    },
];

/// The protocol's conformance suite.
pub fn suite() -> Suite {
    Suite {
        id: block::ID,
        requests: block::REQUESTS,
        cases: CASES,
        timeout: 30_000_000_000,
    }
}
