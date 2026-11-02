//! DR-T18 and DR-T25 to DR-T27 (SDD 25, k), a client of `nvme0` and `nvme1` found
//! through the directory, the block driver tests of KB3 run over NVMe:
//!
//! - DR-T18: `nvme0`'s namespace holds pattern A (`kernel/tools/disk-image.py`, the
//!   boot description's `disk nvme0`); every sector is read and checked, pattern B is
//!   written over it, flushed and read back (check-boot.sh checks the image file on the
//!   host afterwards);
//! - DR-T25: the block protocol's conformance suite, the suite `virtio-blk` passes,
//!   against `nvme0`;
//! - DR-T26: 32 reads in flight on `nvme1` (reads throttled by the boot), the driver
//!   killed under them through control: each read answered or ended with its
//!   connection closed, at least one closed, none hanging, the driver started again
//!   and a read through a new connection returning the disk's data;
//! - DR-T27: `nvme0`'s throughput (the whole namespace written and read in the largest
//!   transfers, eight in flight, from one registered buffer) and its latency for 4 KiB
//!   reads at queue depth 1 (each timed: mean, median, 99th percentile) and 32 (the
//!   rate, and the mean by Little's law). These are printed, not judged: under TCG they
//!   measure the emulator.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};

use block_protocol::{Info, block};
use devices_protocol::{control, directory};
use moruna_svc::{
    CallError, Channel, Console, Encoder, Memory, Rights, Wire, conformance, deadline_in, map, now,
    println, start,
};

moruna_svc::main!(run);

/// The disks the boot describes: nvme0 16 MiB, nvme1 4 MiB.
const NVME0_SECTORS: u64 = 32768;
const NVME1_SECTORS: u64 = 8192;
const SECONDS: u64 = 1_000_000_000;
const CRASH_READS: usize = 32;

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

fn open(directory: &mut directory::Client, name: &str) -> Result<block::Client, String> {
    let channel = directory
        .lookup(String::from(name))
        .map_err(|e| format!("looking up {name}: {e:?}"))?;
    let mut client = block::Client::connect(channel, deadline_in(60 * SECONDS))
        .map_err(|e| format!("hello to {name}: {e:?}"))?;
    client.core().set_timeout(120 * SECONDS);
    Ok(client)
}

fn dma(bytes: u64) -> Result<Memory, String> {
    Memory::create_dma(bytes.max(4096)).map_err(|e| format!("DMA memory: {}", e.name()))
}

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

fn fill(
    buffer: &Memory,
    first: u64,
    count: u64,
    pattern: fn(u64, u64) -> u8,
) -> Result<(), String> {
    let bytes = count * 512;
    let m = buffer
        .map(0, bytes.div_ceil(4096) * 4096, map::WRITE)
        .map_err(|e| format!("mapping the buffer: {}", e.name()))?;
    let data: Vec<u8> = (0..bytes)
        .map(|at| pattern(first + at / 512, at % 512))
        .collect();
    m.write(0, &data).map_err(|e| String::from(e.name()))
}

/// DR-T18: the whole namespace read, written, flushed and read back.
fn data(directory: &mut directory::Client) -> Result<Info, String> {
    let mut disk = open(directory, "nvme0")?;
    let info = disk.info().map_err(|e| format!("info: {e:?}"))?;
    if info.sector_size != 512 || info.sectors != NVME0_SECTORS || info.read_only {
        return Err(format!(
            "nvme0 is {info:?}, not the 16 MiB namespace the boot attaches"
        ));
    }
    let lease = disk
        .acquire_write()
        .map_err(|e| format!("acquire writer: {e:?}"))?;
    let chunk = (info.max_sectors as u64).min(2048);
    let mut buffer = dma(chunk * 512)?;
    let pass = |disk: &mut block::Client,
                mut buffer: Memory,
                write: bool,
                pattern: fn(u64, u64) -> u8|
     -> Result<Memory, String> {
        let mut sector = 0;
        while sector < info.sectors {
            let n = chunk.min(info.sectors - sector);
            if write {
                fill(&buffer, sector, n, pattern)?;
                buffer = disk
                    .write(lease, sector, n as u32, buffer)
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
    disk.flush(lease).map_err(|e| format!("flush: {e:?}"))?;
    drop(pass(&mut disk, buffer, false, pattern_b).map_err(|e| format!("pattern B: {e}"))?);
    disk.release_write(lease)
        .map_err(|e| format!("release writer: {e:?}"))?;
    Ok(info)
}

/// DR-T26: reads in flight when the driver is killed end, and the restarted driver
/// serves.
fn crash(
    directory: &mut directory::Client,
    control: &mut control::Client,
) -> Result<String, String> {
    let mut disk = open(directory, "nvme1")?;
    let info = disk.info().map_err(|e| format!("info: {e:?}"))?;
    if info.sectors != NVME1_SECTORS {
        return Err(format!(
            "nvme1 is {info:?}, not the 4 MiB namespace the boot attaches"
        ));
    }
    let chunk = info.max_sectors.min(2048);
    let buffer = dma(chunk as u64 * 512)?;
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
        .restart(String::from("nvme1"))
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
    let mut again = open(directory, "nvme1")?;
    let one = again
        .read(0, 1, dma(4096)?)
        .map_err(|e| format!("a read after the restart: {e:?}"))?;
    check(&one, 0, 1, pattern_a)?;
    Ok(format!(
        "nvme1 was killed with {closed} of {CRASH_READS} reads in flight (each ended with its connection closed, {answered} answered before), was started again (start {starts}), reset its controller, and a read through a new connection returned the disk's data"
    ))
}

fn at(lease: Option<u64>, id: u32, page: u32, sector: u64, count: u32) -> Result<Encoder, String> {
    let mut e = Encoder::new();
    if let Some(lease) = lease {
        lease.encode(&mut e).map_err(|e| format!("{e:?}"))?;
    }
    for r in [
        id.encode(&mut e),
        page.encode(&mut e),
        sector.encode(&mut e),
        count.encode(&mut e),
    ] {
        r.map_err(|e| format!("{e:?}"))?;
    }
    Ok(e)
}

/// Runs `ops` registered transfers keeping `depth` in flight; `next(i)` gives the
/// i-th one's `(page, sector, count)`. Returns the nanoseconds it took and, at depth
/// 1, each one's latency.
fn pipeline(
    disk: &mut block::Client,
    ordinal: u16,
    lease: Option<u64>,
    id: u32,
    ops: usize,
    depth: usize,
    next: &dyn Fn(usize) -> (u32, u64, u32),
) -> Result<(u64, Vec<u64>), String> {
    let mut flight = alloc::collections::VecDeque::new();
    let mut latencies = Vec::new();
    let begin = now();
    let mut sent = 0;
    while sent < ops || !flight.is_empty() {
        while sent < ops && flight.len() < depth {
            let (page, sector, count) = next(sent);
            let txid = disk
                .core()
                .send(ordinal, at(lease, id, page, sector, count)?)
                .map_err(|e| format!("sending: {e:?}"))?;
            flight.push_back((txid, now()));
            sent += 1;
        }
        let Some((txid, started)) = flight.pop_front() else {
            break;
        };
        disk.core()
            .reply(txid, deadline_in(120 * SECONDS))
            .map_err(|e| format!("transfer {txid}: {e:?}"))?;
        if depth == 1 {
            latencies.push(now() - started);
        }
    }
    Ok((now() - begin, latencies))
}

fn rate(bytes: u64, ns: u64) -> String {
    let kib_per_s = bytes * 1_000_000_000 / 1024 / ns.max(1);
    format!(
        "{}.{:02} MiB/s",
        kib_per_s / 1024,
        kib_per_s % 1024 * 100 / 1024
    )
}

fn micros(ns: u64) -> String {
    format!("{}.{:01} us", ns / 1000, ns % 1000 / 100)
}

/// DR-T27: throughput and latency.
fn performance(directory: &mut directory::Client, info: &Info) -> Result<Vec<String>, String> {
    let mut disk = open(directory, "nvme0")?;
    let lease = disk
        .acquire_write()
        .map_err(|e| format!("acquire writer: {e:?}"))?;
    let bytes = info.sectors * 512;
    let buffer = dma(bytes)?;
    let view = Memory(
        buffer
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP)
            .map_err(|e| format!("duplicating the buffer: {}", e.name()))?,
    );
    fill(&view, 0, info.sectors, pattern_b)?;
    let (id, pages) = disk
        .register(buffer)
        .map_err(|e| format!("register: {e:?}"))?;
    if pages as u64 * 4096 < bytes {
        return Err(format!("a registration of {bytes} bytes has {pages} pages"));
    }
    let chunk = (info.max_sectors as u64).min(2048);
    let transfers = info.sectors.div_ceil(chunk) as usize;
    let sequential = |i: usize| {
        let sector = i as u64 * chunk;
        let n = chunk.min(info.sectors - sector);
        ((sector / 8) as u32, sector, n as u32)
    };
    let mut lines = Vec::new();
    let (written, _) = pipeline(&mut disk, 7, Some(lease), id, transfers, 8, &sequential)?;
    let flushed = now();
    disk.flush(lease).map_err(|e| format!("flush: {e:?}"))?;
    let written = written + (now() - flushed);
    lines.push(format!(
        "sequential write: {} MiB in {} ms ({} transfers of {} KiB, 8 in flight, and a flush): {}",
        bytes >> 20,
        written / 1_000_000,
        transfers,
        chunk / 2,
        rate(bytes, written)
    ));
    fill(&view, 0, info.sectors, |_, _| 0)?;
    let (read, _) = pipeline(&mut disk, 6, None, id, transfers, 8, &sequential)?;
    check(&view, 0, info.sectors, pattern_b)
        .map_err(|e| format!("after the sequential read: {e}"))?;
    lines.push(format!(
        "sequential read: {} MiB in {} ms: {}",
        bytes >> 20,
        read / 1_000_000,
        rate(bytes, read)
    ));
    // 4 KiB reads at scattered offsets: a linear congruential walk over the pages.
    let pages4k = info.sectors / 8;
    let scattered = move |i: usize| {
        let page = (i as u64).wrapping_mul(2_654_435_761) % pages4k;
        (page as u32, page * 8, 8)
    };
    let (_, mut each) = pipeline(&mut disk, 6, None, id, 2000, 1, &scattered)?;
    each.sort_unstable();
    let mean = each.iter().sum::<u64>() / each.len() as u64;
    lines.push(format!(
        "4 KiB reads at queue depth 1: {} reads, mean {}, median {}, 99th percentile {}",
        each.len(),
        micros(mean),
        micros(each[each.len() / 2]),
        micros(each[each.len() * 99 / 100])
    ));
    let ops = 8000;
    let (took, _) = pipeline(&mut disk, 6, None, id, ops, 32, &scattered)?;
    lines.push(format!(
        "4 KiB reads at queue depth 32: {} reads in {} ms, {} reads a second, mean {} (by Little's law)",
        ops,
        took / 1_000_000,
        ops as u64 * 1_000_000_000 / took.max(1),
        micros(took * 32 / ops as u64)
    ));
    disk.unregister(id)
        .map_err(|e| format!("unregister: {e:?}"))?;
    disk.release_write(lease)
        .map_err(|e| format!("release writer: {e:?}"))?;
    Ok(lines)
}

// Exercises actual distinct driver SDK connections, not numeric stand-ins for identity.
fn writer_connections(directory: &mut directory::Client) -> Result<(), String> {
    let mut first = open(directory, "nvme0")?;
    let mut second = open(directory, "nvme0")?;
    let token = first
        .acquire_write()
        .map_err(|e| format!("first claim: {e:?}"))?;
    match second.flush(token) {
        Err(moruna_svc::CallError::Remote(block::Error::NotWriter)) => {}
        other => return Err(format!("foreign connection flush: {other:?}")),
    }
    match second.acquire_write() {
        Err(moruna_svc::CallError::Remote(block::Error::LeaseInUse)) => {}
        other => return Err(format!("foreign connection claim: {other:?}")),
    }
    // Read permission remains available without acquiring somebody else's writer.
    drop(
        second
            .read(0, 1, dma(4096)?)
            .map_err(|e| format!("readonly peer while leased: {e:?}"))?,
    );
    first
        .release_write(token)
        .map_err(|e| format!("first release: {e:?}"))?;
    let next = second
        .acquire_write()
        .map_err(|e| format!("second claim: {e:?}"))?;
    if next == token {
        return Err(String::from("namespace lease generation was reused"));
    }
    match second.flush(token) {
        Err(moruna_svc::CallError::Remote(block::Error::NotWriter)) => {}
        other => return Err(format!("stale generation flush: {other:?}")),
    }
    second
        .release_write(next)
        .map_err(|e| format!("second release: {e:?}"))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let (Some(d), Some(c)) = (start::take("directory"), start::take("control")) else {
        println!(out, "KE1 nvme: FAIL: no directory or control");
        return 1;
    };
    let mut dir = match devices_protocol::open_directory(Channel(d), deadline_in(300 * SECONDS)) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KE1 nvme: FAIL: the directory: {:?}", e);
            return 1;
        }
    };
    dir.core().set_timeout(300 * SECONDS);
    let mut ctl = match devices_protocol::open_control(Channel(c), deadline_in(300 * SECONDS)) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KE1 nvme: FAIL: control: {:?}", e);
            return 1;
        }
    };
    ctl.core().set_timeout(60 * SECONDS);
    let info = match data(&mut dir) {
        Ok(info) => info,
        Err(e) => {
            println!(out, "KE1 nvme: FAIL: {}", e);
            return 1;
        }
    };
    println!(
        out,
        "KE1 nvme: ok: read all {} sectors of the host's pattern through nvme0 by DMA into a memory object, wrote a new pattern over them, flushed, and read it back",
        info.sectors
    );
    let mut failed = 0;
    let suite = block_protocol::suite();
    let summary = conformance::run(
        &suite,
        &mut Lookup {
            directory: &mut dir,
            name: "nvme0",
        },
        &mut |check| {
            if let Err(why) = check.result {
                println!(
                    out,
                    "KE1 conformance over nvme0: FAIL: {}: {}", check.name, why
                );
            }
        },
    );
    if summary.ok() {
        println!(
            out,
            "KE1 conformance {} over nvme0: ok: {} checks passed", suite.id.name, summary.passed
        );
    } else {
        failed += 1;
    }
    match writer_connections(&mut dir) {
        Ok(()) => println!(
            out,
            "KE1 namespace lease: ok: actual foreign/stale connections refused, readonly peer permitted, ownership handed off"
        ),
        Err(error) => {
            println!(out, "KE1 namespace lease: FAIL: {}", error);
            failed += 1;
        }
    }
    match performance(&mut dir, &info) {
        Ok(lines) => {
            for line in lines {
                println!(out, "KE1 nvme0 performance: {}", line);
            }
        }
        Err(e) => {
            println!(out, "KE1 nvme0 performance: FAIL: {}", e);
            failed += 1;
        }
    }
    match crash(&mut dir, &mut ctl) {
        Ok(what) => println!(out, "KE1 nvme driver crash: ok: {}", what),
        Err(e) => {
            println!(out, "KE1 nvme driver crash: FAIL: {}", e);
            failed += 1;
        }
    }
    failed
}
