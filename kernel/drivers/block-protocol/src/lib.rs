//! `moruna.driver.block` 1.0 (SDD 25, d.3): a block device. Data never travels in a
//! message: a client sends a handle to its buffer, a DMA memory object, the driver
//! pins it and its device reads or writes it directly, and the reply hands the buffer
//! back. This is the path the image filesystem server and the engine's spill use.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

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
        major = 1;
        minor = 0;
        requests {
            /// The device's sector size, capacity and largest transfer.
            1 => fn info() -> Info;
            /// Reads `count` sectors from `sector` into the first bytes of `buffer` (a
            /// DMA memory object with `READ` and `WRITE`), and hands it back.
            2 => fn read(sector: u64, count: u32, buffer: Memory) -> Memory;
            /// Writes `count` sectors at `sector` from the first bytes of `data` (a DMA
            /// memory object with `READ`), and hands it back.
            3 => fn write(sector: u64, count: u32, data: Memory) -> Memory;
            /// Answered once every write answered before it is on stable storage.
            4 => fn flush() -> ();
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
        }
    }
}

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
    p.request(3, transfer(0, 0, dma(4096)?)?)?
        .failure(block::Error::BadCount.code())
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
    p.request(3, transfer(0, 1, wo)?)?.failure(bad)
}

fn write_round_trip(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    if i.read_only {
        return p
            .request(3, transfer(0, 1, dma(bytes(&i, 1))?)?)?
            .failure(block::Error::ReadOnly.code());
    }
    let n = i.sector_size as usize;
    let last = i.sectors - 1;
    let original = buffer_back(p, 2, transfer(last, 1, dma(bytes(&i, 1))?)?)?;
    let pattern: Vec<u8> = (0..n).map(|b| (b as u8) ^ 0xa5).collect();
    let data = dma(bytes(&i, 1))?;
    data.map(0, 4096, map::WRITE)
        .and_then(|m| m.write(0, &pattern))
        .map_err(|e| e.name().to_string())?;
    drop(buffer_back(p, 3, transfer(last, 1, data)?)?);
    let back = buffer_back(p, 2, transfer(last, 1, dma(bytes(&i, 1))?)?)?;
    let read = read_bytes(&back, n)?;
    // Restore the sector before judging, so the disk ends as it began.
    drop(buffer_back(p, 3, transfer(last, 1, original)?)?);
    p.request(4, Encoder::new())?.reply()?;
    if read != pattern {
        return Err("the sector read back is not the one written".to_string());
    }
    Ok(())
}

fn flush(p: &mut Probe) -> Result<(), Fail> {
    let mut reply = p.request(4, Encoder::new())?.reply()?;
    let d = reply.decoder();
    d.finish().map_err(wire)
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
