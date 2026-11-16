//! DS-A (`architecture/data-os-streams.md`, stream A): a write to a window
//! `windows_protect` revoked faults, while the window's bytes stay readable and the
//! page beside it stays writable. The boot expects this process to fault.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::console;
use moruna_sys::{Error, Memory, map, println, raw};

moruna_sys::main!(run);

fn run() -> i64 {
    let out = console();
    let fail = |what: &str, e: Error| {
        println!(out, "DS-A revoke: FAIL: {}: {}", what, e.name());
        1
    };
    let object = match Memory::create(4 * 4096) {
        Ok(o) => o,
        Err(e) => return fail("memory_create", e),
    };
    let m = match object.map(0, 4 * 4096, map::WRITE) {
        Ok(m) => m,
        Err(e) => return fail("memory_map", e),
    };
    if let Err(e) = m.write(4096, b"kept") {
        return fail("writing the window", e);
    }
    // The window is pages 1 and 2; pages 0 and 3 are not in it.
    let window = [[m.addr() + 4096, 2 * 4096]];
    if let Err(e) = Error::from_raw(raw::windows_protect(0, &window, map::READ)) {
        return fail("windows_protect", e);
    }
    let mut b = [0u8; 4];
    if m.read(4096, &mut b).is_err() || &b != b"kept" {
        println!(out, "DS-A revoke: FAIL: a revoked window lost its bytes");
        return 1;
    }
    if let Err(e) = m
        .write(0, b"beside")
        .and_then(|()| m.write(3 * 4096, b"after"))
    {
        return fail("writing beside the window", e);
    }
    println!(
        out,
        "DS-A revoke: about to write a revoked window at {:#x}",
        m.addr() + 2 * 4096
    );
    let _ = m.write(2 * 4096, b"x");
    println!(
        out,
        "DS-A revoke: FAIL: a write to a revoked window went through"
    );
    1
}
