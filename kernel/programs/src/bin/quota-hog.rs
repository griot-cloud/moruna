//! KB5-T2, a child of `quota`: one of three roles, told by the first message on its
//! `parent` slot, each run in an account of its own the parent carved:
//!
//! - `call`: makes memory objects of 1 MiB until the kernel refuses one, then reports how
//!   many it made, the error, and its account's figures, and waits to be told to end;
//! - `touch` and an address and a length: writes a byte to each page of the zero pages
//!   the parent mapped there before it started, until the kernel ends it at the first
//!   page its account has no room for;
//! - `steady`: waits for the parent's go, then makes, writes, reads back and frees
//!   memory objects within its account while the others have filled theirs, and reports.
//!
//! The one deliberate unsafe write of the touch role is the test: the kernel, not the
//! program, must stop it (as KB1's `fault`).

#![no_std]
#![no_main]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{Channel, Error, Memory, Signals, Usage, map, now, println, wait_one};

moruna_sys::main!(run);

const MIB: u64 = 1 << 20;
/// More objects than any account of the test holds.
const MANY: usize = 64;

/// The next message from the parent, up to 64 bytes.
fn next(parent: &Channel) -> Result<([u8; 64], usize), Error> {
    wait_one(&parent.0, Signals::READABLE, now() + ms(120_000))?;
    let mut bytes = [0u8; 64];
    let (n, _) = parent.read(&mut bytes, &mut [])?;
    Ok((bytes, n))
}

fn word(bytes: &[u8], at: usize) -> u64 {
    moruna_abi::read_u64(bytes, at).unwrap_or(0)
}

fn put(out: &mut [u8; 64], at: usize, v: u64) {
    out[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Makes 1 MiB objects until one is refused; reports `[made, error code, used, limit,
/// peak]` and waits to be told to end.
fn call(parent: &Channel) -> i64 {
    let mut held: [Option<Memory>; MANY] = core::array::from_fn(|_| None);
    let mut made = 0;
    let mut refused = Error::BadState;
    for place in held.iter_mut() {
        match Memory::create(MIB) {
            Ok(m) => {
                *place = Some(m);
                made += 1;
            }
            Err(e) => {
                refused = e;
                break;
            }
        }
    }
    let Ok(usage) = Usage::own() else {
        return 2;
    };
    let mut report = [0u8; 64];
    put(&mut report, 0, made);
    put(&mut report, 8, refused.code() as u64);
    put(&mut report, 16, usage.used);
    put(&mut report, 24, usage.limit);
    put(&mut report, 32, usage.peak);
    if parent.write(&report[..40], []).is_err() {
        return 3;
    }
    // Held until the parent has read the account too.
    match next(parent) {
        Ok(_) => 0,
        Err(_) => 4,
    }
}

/// Writes a byte to every page of `addr .. addr + len`, which the parent mapped as zero
/// pages; the kernel ends it at the page its account cannot pay for.
fn touch(addr: u64, len: u64) -> i64 {
    let mut at = addr;
    while at < addr + len {
        let page = core::hint::black_box(at as usize);
        // SAFETY: none in the language's sense, on purpose: the parent mapped these pages
        // writable into this process before it started, at the address it sent, and this
        // program holds no other reference to them. The test is that the kernel charges
        // each page as it is first touched and ends the process at the first it cannot.
        unsafe { core::ptr::write_volatile(page as *mut u8, 1) };
        at += 4096;
    }
    // Every page was paid for: the account was larger than the mapping, a failure.
    5
}

/// After the go: makes, writes, reads back and frees 1 MiB objects 32 times, within
/// its account; reports `[rounds, peak]`.
fn steady(parent: &Channel) -> i64 {
    if next(parent).is_err() {
        return 6;
    }
    let mut rounds = 0u64;
    for round in 0..32u64 {
        let Ok(m) = Memory::create(MIB) else {
            return 7;
        };
        let Ok(view) = m.map(0, MIB, map::READ | map::WRITE) else {
            return 8;
        };
        if view.write64(MIB as usize - 8, round).is_err()
            || view.read64(MIB as usize - 8) != Ok(round)
        {
            return 9;
        }
        drop(view);
        drop(m);
        rounds += 1;
    }
    let Ok(usage) = Usage::own() else {
        return 10;
    };
    let mut report = [0u8; 64];
    put(&mut report, 0, rounds);
    put(&mut report, 8, usage.peak);
    if parent.write(&report[..16], []).is_err() {
        return 11;
    }
    0
}

fn run() -> i64 {
    let out = console();
    let parent = Channel(slot("parent"));
    let (role, n) = match next(&parent) {
        Ok(m) => m,
        Err(e) => {
            println!(out, "KB5 quota: FAIL: a child had no role: {}", e.name());
            return 1;
        }
    };
    let said = &role[..n];
    match said {
        b"call" => call(&parent),
        b"steady" => steady(&parent),
        _ if n == 24 && said.starts_with(b"touch") => touch(word(said, 8), word(said, 16)),
        _ => {
            println!(out, "KB5 quota: FAIL: a child was given no role it knows");
            1
        }
    }
}
