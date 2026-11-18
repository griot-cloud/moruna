//! The check program on the node. Its start handles: `console`, and `directory`, its end of
//! the device manager's `directory-check` listener. It says each step's verdict, then the
//! count, and then waits: a service that ends stops the node, and the PM reads the console
//! of a node that keeps running.

use alloc::{format, string::String, vec, vec::Vec};

use block_protocol::block;
use devices_protocol::directory;
use moruna_svc::{
    CallError, Channel, Console, DEADLINE_INFINITE, Encoder, Memory, Rights, Signals, Timer, Wire,
    conformance, deadline_in, map, now, println, start, wait_one,
};
use storvsc_check::{FIRST, Gpt, OFFSET, SECTORS, pattern};

const SECONDS: u64 = 1_000_000_000;

/// How long storvsc may take to register the disks: its scan's steps, at 10 s each.
const FOUND_NS: u64 = 120 * SECONDS;

/// How long a disk may take to answer.
const ANSWER_NS: u64 = 60 * SECONDS;

/// Block protocol minor 2's `read_to` (item 3.3), by its ordinal.
const READ_TO: u16 = 11;

const PAGE: u64 = 4096;

/// Sleeps for `ns`.
fn pause(ns: u64) {
    if let Ok(timer) = Timer::create()
        && timer.set(now().saturating_add(ns)).is_ok()
    {
        let _ = wait_one(&timer.0, Signals::SIGNALED, DEADLINE_INFINITE);
    }
}

fn wire<E: core::fmt::Debug>(e: E) -> String {
    format!("{e:?}")
}

/// A client of the disk registered as `serial`, once storvsc has registered it.
fn find(directory: &mut directory::Client, serial: &str) -> Result<block::Client, String> {
    let deadline = now().saturating_add(FOUND_NS);
    loop {
        match directory.lookup(String::from(serial)) {
            Ok(channel) => {
                let mut disk = block::Client::connect(channel, deadline_in(ANSWER_NS))
                    .map_err(|e| format!("hello to {serial}: {e:?}"))?;
                disk.core().set_timeout(ANSWER_NS);
                return Ok(disk);
            }
            Err(CallError::Remote(directory::Error::NotFound)) if now() < deadline => {
                pause(SECONDS);
            }
            Err(e) => return Err(format!("looking up {serial}: {e:?}")),
        }
    }
}

/// DMA memory of whole pages, at least `bytes` of them.
fn dma(bytes: u64) -> Result<Memory, String> {
    Memory::create_dma(bytes.div_ceil(PAGE).max(1) * PAGE)
        .map_err(|e| format!("DMA memory: {}", e.name()))
}

/// `len` bytes of `memory` from byte `at`.
fn bytes(memory: &Memory, at: usize, len: usize) -> Result<Vec<u8>, String> {
    let pages = (at + len).div_ceil(PAGE as usize) as u64;
    let mapping = memory
        .map(0, pages * PAGE, 0)
        .map_err(|e| format!("mapping: {}", e.name()))?;
    let mut out = vec![0; len];
    mapping
        .read(at, &mut out)
        .map_err(|e| format!("reading: {}", e.name()))?;
    Ok(out)
}

fn read(disk: &mut block::Client, sector: u64, count: u32, size: u32) -> Result<Vec<u8>, String> {
    let len = count as usize * size as usize;
    let back = disk
        .read(sector, count, dma(len as u64)?)
        .map_err(|e| format!("read at {sector}: {e:?}"))?;
    bytes(&back, 0, len)
}

/// The OS disk: read-only, and its GPT header at LBA 1.
fn boot(directory: &mut directory::Client) -> Result<String, String> {
    let mut disk = find(directory, "boot")?;
    let info = disk.info().map_err(wire)?;
    if !info.read_only {
        return Err(String::from("the OS disk is served writable"));
    }
    let sector = read(&mut disk, 1, 1, info.sector_size)?;
    let gpt = Gpt::parse(&sector)?;
    Ok(format!(
        "the GPT header at LBA 1 of {} sectors of {} bytes, read through moruna.driver.block: revision {}.{}, disk {}, {} partition entries, its backup at LBA {}",
        info.sectors,
        info.sector_size,
        gpt.revision.0,
        gpt.revision.1,
        gpt.guid(),
        gpt.entries,
        gpt.alternate
    ))
}

/// The pattern's bytes for the sectors the check writes.
fn written(size: u32) -> Vec<u8> {
    let size = size as usize;
    (0..SECTORS as usize * size)
        .map(|at| pattern(FIRST + (at / size) as u64, at % size))
        .collect()
}

/// Writes the pattern, reads it back twice, the second time with `read_to`.
fn exchange(disk: &mut block::Client, token: u64, size: u32) -> Result<(), String> {
    let want = written(size);
    let buffer = dma(want.len() as u64)?;
    buffer
        .map(0, (want.len() as u64).div_ceil(PAGE) * PAGE, map::WRITE)
        .and_then(|m| m.write(0, &want))
        .map_err(|e| format!("filling: {}", e.name()))?;
    disk.write(token, FIRST, SECTORS, buffer)
        .map_err(|e| format!("write: {e:?}"))?;
    disk.flush(token).map_err(|e| format!("flush: {e:?}"))?;
    if read(disk, FIRST, SECTORS, size)? != want {
        return Err(String::from(
            "the sectors read back are not the ones written",
        ));
    }
    let registered = dma(OFFSET + want.len() as u64)?;
    let view = Memory(
        registered
            .0
            .duplicate(Rights::READ | Rights::MAP)
            .map_err(|e| format!("duplicating: {}", e.name()))?,
    );
    let (id, _) = disk
        .register(registered)
        .map_err(|e| format!("register: {e:?}"))?;
    let mut body = Encoder::new();
    (id, OFFSET, FIRST, SECTORS)
        .encode(&mut body)
        .map_err(wire)?;
    let answer = disk.core().call(READ_TO, body).map(drop);
    disk.unregister(id)
        .map_err(|e| format!("unregister: {e:?}"))?;
    answer.map_err(|e| format!("read_to at byte {OFFSET}: {e:?}"))?;
    if bytes(&view, OFFSET as usize, want.len())? != want {
        return Err(format!(
            "the sectors read_to put at byte {OFFSET} are not the ones written"
        ));
    }
    Ok(())
}

/// The data disk: written, read back, read with `read_to`, and restored.
fn data(directory: &mut directory::Client) -> Result<String, String> {
    let mut disk = find(directory, "lun-0")?;
    let info = disk.info().map_err(wire)?;
    if info.read_only {
        return Err(String::from("lun-0 is served read-only"));
    }
    let size = info.sector_size;
    let original = read(&mut disk, FIRST, SECTORS, size)?;
    let token = disk
        .acquire_write()
        .map_err(|e| format!("acquire_write: {e:?}"))?;
    let exchanged = exchange(&mut disk, token, size);
    // Restored whatever the exchange came to.
    let saved = dma(original.len() as u64)?;
    saved
        .map(0, (original.len() as u64).div_ceil(PAGE) * PAGE, map::WRITE)
        .and_then(|m| m.write(0, &original))
        .map_err(|e| format!("filling: {}", e.name()))?;
    let restored = disk
        .write(token, FIRST, SECTORS, saved)
        .map_err(|e| format!("restoring: {e:?}"))
        .and_then(|_| disk.flush(token).map_err(|e| format!("flush: {e:?}")))
        .and_then(|()| {
            disk.release_write(token)
                .map_err(|e| format!("release_write: {e:?}"))
        });
    exchanged?;
    restored?;
    Ok(format!(
        "{} sectors of {size} bytes: wrote {SECTORS} at {FIRST}, flushed, read them back, read them again with read_to at byte {OFFSET} of a registration, and restored them",
        info.sectors
    ))
}

struct Lookup<'a> {
    directory: &'a mut directory::Client,
}

impl conformance::Connector for Lookup<'_> {
    fn connect(&mut self) -> Result<Channel, conformance::Fail> {
        self.directory
            .lookup(String::from("lun-0"))
            .map_err(|e| format!("looking up lun-0: {e:?}"))
    }
}

fn say(out: &Console, step: &str, verdict: &Result<String, String>) -> u32 {
    match verdict {
        Ok(what) => {
            println!(out, "storvsc-check: {}: ok: {}", step, what);
            1
        }
        Err(why) => {
            println!(out, "storvsc-check: {}: FAIL: {}", step, why);
            0
        }
    }
}

pub fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(listener) = start::take("directory").map(Channel) else {
        println!(out, "storvsc-check: FAIL: no directory");
        return 1;
    };
    let mut directory = match devices_protocol::open_directory(listener, deadline_in(ANSWER_NS)) {
        Ok(directory) => directory,
        Err(e) => {
            println!(out, "storvsc-check: FAIL: the directory: {:?}", e);
            return 1;
        }
    };
    directory.core().set_timeout(ANSWER_NS);
    let mut passed = say(&out, "boot", &boot(&mut directory));
    passed += say(&out, "lun-0", &data(&mut directory));
    let suite = block_protocol::suite();
    let summary = conformance::run(
        &suite,
        &mut Lookup {
            directory: &mut directory,
        },
        &mut |check| {
            if let Err(why) = check.result {
                println!(
                    out,
                    "storvsc-check: conformance: FAIL: {}: {}", check.name, why
                );
            }
        },
    );
    let verdict = if summary.ok() {
        Ok(format!(
            "{} checks of {} passed against lun-0",
            summary.passed, suite.id.name
        ))
    } else {
        Err(String::from("a check failed, said above"))
    };
    passed += say(&out, "conformance", &verdict);
    println!(out, "storvsc-check: done: {} of 3 steps passed", passed);
    // A service that ends stops the node: the check waits, and the node runs on.
    loop {
        pause(3600 * SECONDS);
    }
}
