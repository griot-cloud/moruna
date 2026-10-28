//! DR-T9 to DR-T11 (SDD 25, k), a client of `block0` found through the directory:
//!
//! - the disk the boot attaches holds pattern A (`kernel/tools/disk-image.py`); every
//!   sector is read through the driver and checked, pattern B is written over it,
//!   flushed and read back (the CI step checks the image file on the host afterwards);
//! - the block protocol's conformance suite runs against the driver;
//! - 32 reads are sent and the driver is restarted (killed) under them: every read is
//!   answered or ends with the connection closed, at least one ends closed, none hangs,
//!   and a read through a new connection afterwards returns the disk's data.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};

use block_protocol::{Info, block};
use devices_protocol::{control, directory};
use moruna_svc::{
    CallError, Channel, Console, Encoder, Memory, Rights, Wire, conformance, deadline_in, map,
    println, start,
};

moruna_svc::main!(run);

/// The disk the CI step writes: 4 MiB.
const SECTORS: u64 = 8192;
const SECONDS: u64 = 1_000_000_000;
const CRASH_READS: usize = 32;

/// Pattern A, which the CI step writes, and B, which this test writes.
fn pattern_a(sector: u64, i: u64) -> u8 {
    ((sector * 31 + i * 7 + 0x11) & 0xff) as u8
}

fn pattern_b(sector: u64, i: u64) -> u8 {
    ((sector * 13 + i * 5 + 0x77) & 0xff) as u8
}

struct Lookup<'a> {
    directory: &'a mut directory::Client,
    name: &'static str,
}

impl conformance::Connector for Lookup<'_> {
    fn connect(&mut self) -> Result<Channel, conformance::Fail> {
        self.directory
            .lookup(String::from(self.name))
            .map_err(|e| format!("looking up {}: {e:?}", self.name))
    }
}

fn open(directory: &mut directory::Client) -> Result<block::Client, String> {
    let channel = directory
        .lookup(String::from("block0"))
        .map_err(|e| format!("looking up block0: {e:?}"))?;
    let mut client = block::Client::connect(channel, deadline_in(60 * SECONDS))
        .map_err(|e| format!("hello to block0: {e:?}"))?;
    client.core().set_timeout(60 * SECONDS);
    Ok(client)
}

/// Checks `count` sectors from `first` in `buffer` against `pattern`.
fn check(
    buffer: &Memory,
    first: u64,
    count: u64,
    pattern: fn(u64, u64) -> u8,
) -> Result<(), String> {
    let bytes = count * 512;
    let m = buffer
        .map(0, bytes.div_ceil(4096) * 4096, 0)
        .map_err(|e| format!("mapping the buffer: {}", e.name()))?;
    let mut data = vec![0u8; bytes as usize];
    m.read(0, &mut data).map_err(|e| e.name())?;
    for (at, b) in data.iter().enumerate() {
        let (sector, i) = (first + at as u64 / 512, at as u64 % 512);
        if *b != pattern(sector, i) {
            return Err(format!(
                "sector {sector} byte {i} is {b:#04x}, not {:#04x}",
                pattern(sector, i)
            ));
        }
    }
    Ok(())
}

fn fill(buffer: &Memory, first: u64, count: u64) -> Result<(), String> {
    let bytes = count * 512;
    let m = buffer
        .map(0, bytes.div_ceil(4096) * 4096, map::WRITE)
        .map_err(|e| format!("mapping the buffer: {}", e.name()))?;
    let data: Vec<u8> = (0..bytes)
        .map(|at| pattern_b(first + at / 512, at % 512))
        .collect();
    m.write(0, &data).map_err(|e| String::from(e.name()))
}

/// DR-T9: the whole disk read, written, flushed and read back.
fn data(directory: &mut directory::Client) -> Result<Info, String> {
    let mut disk = open(directory)?;
    let info = disk.info().map_err(|e| format!("info: {e:?}"))?;
    if info.sector_size != 512 || info.sectors != SECTORS || info.read_only {
        return Err(format!(
            "block0 is {info:?}, not the 4 MiB disk the boot attaches"
        ));
    }
    let chunk = (info.max_sectors as u64).min(2048);
    let mut buffer =
        Memory::create_dma(chunk * 512).map_err(|e| format!("DMA memory: {}", e.name()))?;
    let pass = |disk: &mut block::Client,
                mut buffer: Memory,
                write: bool,
                pattern: fn(u64, u64) -> u8|
     -> Result<Memory, String> {
        let mut sector = 0;
        while sector < SECTORS {
            let n = chunk.min(SECTORS - sector);
            if write {
                fill(&buffer, sector, n)?;
                buffer = disk
                    .write(sector, n as u32, buffer)
                    .map_err(|e| format!("write at {sector}: {e:?}"))?;
            } else {
                buffer = disk
                    .read(sector, n as u32, buffer)
                    .map_err(|e| format!("read at {sector}: {e:?}"))?;
                check(&buffer, sector, n, pattern)?;
            }
            sector += n;
        }
        Ok(buffer)
    };
    buffer = pass(&mut disk, buffer, false, pattern_a).map_err(|e| format!("pattern A: {e}"))?;
    buffer = pass(&mut disk, buffer, true, pattern_b)?;
    disk.flush().map_err(|e| format!("flush: {e:?}"))?;
    drop(pass(&mut disk, buffer, false, pattern_b).map_err(|e| format!("pattern B: {e}"))?);
    Ok(info)
}

/// DR-T11: reads in flight when the driver is killed end, and the restarted driver
/// serves.
fn crash(
    directory: &mut directory::Client,
    control: &mut control::Client,
    info: &Info,
) -> Result<String, String> {
    let mut disk = open(directory)?;
    let chunk = info.max_sectors.min(2048);
    let buffer =
        Memory::create_dma(chunk as u64 * 512).map_err(|e| format!("DMA memory: {}", e.name()))?;
    let mut sent = Vec::new();
    for _ in 0..CRASH_READS {
        let copy = buffer
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .map_err(|e| format!("duplicating the buffer: {}", e.name()))?;
        let mut body = Encoder::new();
        0u64.encode(&mut body).map_err(|e| format!("{e:?}"))?;
        chunk.encode(&mut body).map_err(|e| format!("{e:?}"))?;
        Memory(copy)
            .encode(&mut body)
            .map_err(|e| format!("{e:?}"))?;
        sent.push(
            disk.core()
                .send(2, body)
                .map_err(|e| format!("sending read: {e:?}"))?,
        );
    }
    let starts = control
        .restart(String::from("block0"))
        .map_err(|e| format!("restart: {e:?}"))?;
    let (mut answered, mut closed) = (0, 0);
    for id in sent {
        match disk.core().reply(id, deadline_in(60 * SECONDS)) {
            Ok(_) => answered += 1,
            Err(CallError::PeerClosed) => closed += 1,
            Err(e) => return Err(format!("a read in flight ended {e:?}, not closed")),
        }
    }
    if closed == 0 {
        return Err(format!(
            "all {answered} reads were answered before the driver was killed"
        ));
    }
    let mut again = open(directory)?;
    let one = Memory::create_dma(4096).map_err(|e| format!("DMA memory: {}", e.name()))?;
    let one = again
        .read(0, 1, one)
        .map_err(|e| format!("a read after the restart: {e:?}"))?;
    check(&one, 0, 1, pattern_b)?;
    Ok(format!(
        "block0 was killed with {closed} of {CRASH_READS} reads in flight (each ended with its connection closed, {answered} answered before), was started again (start {starts}), and a read through a new connection returned the disk's data"
    ))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let (Some(d), Some(c)) = (start::take("directory"), start::take("control")) else {
        println!(out, "KB3 block: FAIL: no directory or control");
        return 1;
    };
    let connect = |channel| directory::Client::connect(channel, deadline_in(300 * SECONDS));
    let mut dir = match connect(Channel(d)) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KB3 block: FAIL: the directory: {:?}", e);
            return 1;
        }
    };
    dir.core().set_timeout(300 * SECONDS);
    let mut ctl = match control::Client::connect(Channel(c), deadline_in(300 * SECONDS)) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KB3 block: FAIL: control: {:?}", e);
            return 1;
        }
    };
    ctl.core().set_timeout(60 * SECONDS);
    let info = match data(&mut dir) {
        Ok(info) => info,
        Err(e) => {
            println!(out, "KB3 block: FAIL: {}", e);
            return 1;
        }
    };
    println!(
        out,
        "KB3 block: ok: read all {} sectors of the host's pattern through block0 by DMA into a memory object, wrote a new pattern over them, flushed, and read it back",
        info.sectors
    );
    let suite = block_protocol::suite();
    let mut failed = 0;
    let summary = conformance::run(
        &suite,
        &mut Lookup {
            directory: &mut dir,
            name: "block0",
        },
        &mut |check| {
            if let Err(why) = check.result {
                println!(out, "KB3 conformance: FAIL: {}: {}", check.name, why);
            }
        },
    );
    if summary.ok() {
        println!(
            out,
            "KB3 conformance {}: ok: {} checks passed against block0",
            suite.id.name,
            summary.passed
        );
    } else {
        failed += 1;
    }
    match crash(&mut dir, &mut ctl, &info) {
        Ok(what) => println!(out, "KB3 driver crash: ok: {}", what),
        Err(e) => {
            println!(out, "KB3 driver crash: FAIL: {}", e);
            failed += 1;
        }
    }
    failed
}
