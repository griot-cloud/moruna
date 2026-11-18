//! The suite's cases for minor 1's placed transfers (`read_to`, `write_from`): sectors land at
//! any byte of a registration and nowhere else, or the driver says it cannot place them there
//! (`BadBuffer`), which a client then moves another way.

use alloc::{format, string::ToString, vec, vec::Vec};

use moruna_svc::{Encoder, Memory, ProtocolError, Rights, Wire, conformance::Answer, map};

use super::{
    Fail, Probe, block, bytes, claim, dma, empty_reply, info_of, register, release, token_body,
    transfer, wire, write_transfer,
};

/// Where a placed transfer starts in its registration: inside its second page, on no sector.
pub const OFFSET: u64 = 4096 + 100;
/// What a registration holds where no transfer has written.
const FILL: u8 = 0x5a;

pub(crate) fn to(id: u32, offset: u64, sector: u64, count: u32) -> Result<Encoder, Fail> {
    let mut e = Encoder::new();
    id.encode(&mut e).map_err(wire)?;
    offset.encode(&mut e).map_err(wire)?;
    sector.encode(&mut e).map_err(wire)?;
    count.encode(&mut e).map_err(wire)?;
    Ok(e)
}

fn from(token: u64, id: u32, offset: u64, sector: u64, count: u32) -> Result<Encoder, Fail> {
    let mut e = token_body(token)?;
    id.encode(&mut e).map_err(wire)?;
    offset.encode(&mut e).map_err(wire)?;
    sector.encode(&mut e).map_err(wire)?;
    count.encode(&mut e).map_err(wire)?;
    Ok(e)
}

/// Whether a placed transfer was made (`true`), or refused as one the driver cannot place
/// (`false`); any other answer fails the case.
fn placed(answer: Answer) -> Result<bool, Fail> {
    match answer {
        Answer::Reply(mut r) => r.decoder().finish().map(|()| true).map_err(wire),
        Answer::Failure(code) if code == block::Error::BadBuffer.code() => Ok(false),
        other => Err(format!("expected a reply or BadBuffer, got {other:?}")),
    }
}

/// A DMA buffer of `pages` pages filled with [`FILL`], and a view of it to read back.
fn filled(pages: u64) -> Result<(Memory, Memory), Fail> {
    let buffer = dma(pages * 4096)?;
    buffer
        .map(0, pages * 4096, map::WRITE)
        .and_then(|m| m.write(0, &vec![FILL; (pages * 4096) as usize]))
        .map_err(|e| e.name().to_string())?;
    let view = Memory(
        buffer
            .0
            .duplicate(Rights::READ | Rights::MAP)
            .map_err(|e| e.name().to_string())?,
    );
    Ok((buffer, view))
}

fn contents(view: &Memory, pages: u64) -> Result<Vec<u8>, Fail> {
    let m = view
        .map(0, pages * 4096, 0)
        .map_err(|e| e.name().to_string())?;
    let mut out = vec![0u8; (pages * 4096) as usize];
    m.read(0, &mut out).map_err(|e| e.name().to_string())?;
    Ok(out)
}

/// `read_to` puts at [`OFFSET`] what `read` gives, across a page's end, and leaves every other
/// byte of the registration as it was; or the driver answers `BadBuffer`.
pub(crate) fn read_to(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    let count = (2 * 4096 / i.sector_size).min(i.max_sectors);
    let n = bytes(&i, count) as usize;
    let (buffer, view) = filled(4)?;
    let (id, _) = register(p, buffer)?;
    if !placed(p.request(11, to(id, OFFSET, 0, count)?)?)? {
        return empty_reply(p, 8, super::at_id(id)?);
    }
    let alone = super::buffer_back(p, 2, transfer(0, count, dma(bytes(&i, count))?)?)?;
    let want = super::read_bytes(&alone, n)?;
    let got = contents(&view, 4)?;
    let at = OFFSET as usize;
    if got[at..at + n] != want[..] {
        return Err("read_to put other bytes at its offset than read gives".to_string());
    }
    if got[..at].iter().chain(&got[at + n..]).any(|b| *b != FILL) {
        return Err("read_to wrote outside the bytes it was asked for".to_string());
    }
    empty_reply(p, 8, super::at_id(id)?)
}

/// A placed transfer that reaches past its registration is `BadBuffer`, one past the disk
/// `OutOfRange`, a count of zero `BadCount`, an unknown registration `NoRegistration`.
pub(crate) fn placed_refusals(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    let one = (4096 / i.sector_size).min(i.max_sectors);
    let (id, _) = register(p, dma(2 * 4096)?)?;
    p.request(11, to(id + 1000, 0, 0, one)?)?
        .failure(block::Error::NoRegistration.code())?;
    p.request(11, to(id, 4096 + 1, 0, one)?)?
        .failure(block::Error::BadBuffer.code())?;
    p.request(11, to(id, u64::MAX, 0, one)?)?
        .failure(block::Error::BadBuffer.code())?;
    p.request(11, to(id, 0, 0, 0)?)?
        .failure(block::Error::BadCount.code())?;
    p.request(11, to(id, 0, i.sectors, one)?)?
        .failure(block::Error::OutOfRange.code())?;
    p.request(12, from(0, id, 0, 0, one)?)?
        .failure(block::Error::NotWriter.code())?;
    empty_reply(p, 8, super::at_id(id)?)
}

/// A sector written from [`OFFSET`] reads back, and the disk is restored; or the driver
/// answers `BadBuffer`.
pub(crate) fn write_from(p: &mut Probe) -> Result<(), Fail> {
    let i = info_of(p)?;
    if i.read_only {
        return Ok(());
    }
    let token = claim(p)?;
    let n = i.sector_size as usize;
    let last = i.sectors - 1;
    let original = super::buffer_back(p, 2, transfer(last, 1, dma(bytes(&i, 1))?)?)?;
    let pattern: Vec<u8> = (0..n).map(|b| (b as u8) ^ 0x96).collect();
    let out = dma(3 * 4096)?;
    out.map(0, 3 * 4096, map::WRITE)
        .and_then(|m| m.write(OFFSET as usize, &pattern))
        .map_err(|e| e.name().to_string())?;
    let (id, _) = register(p, out)?;
    let made = placed(p.request(12, from(token, id, OFFSET, last, 1)?)?)?;
    let read = if made {
        let back = super::buffer_back(p, 2, transfer(last, 1, dma(bytes(&i, 1))?)?)?;
        Some(super::read_bytes(&back, n)?)
    } else {
        None
    };
    drop(super::buffer_back(
        p,
        3,
        write_transfer(token, last, 1, original)?,
    )?);
    p.request(4, token_body(token)?)?.reply()?;
    release(p, token)?;
    empty_reply(p, 8, super::at_id(id)?)?;
    match read {
        Some(read) if read != pattern => {
            Err("the sector written from an offset is not the one read back".to_string())
        }
        _ => Ok(()),
    }
}
