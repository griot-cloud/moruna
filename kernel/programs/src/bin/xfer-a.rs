//! KK-T7, sender: writes a pattern into a memory object and sends a read-only handle.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{Channel, Error, Memory, Rights, Signals, info, map, now, println, wait_one};

moruna_sys::main!(run);

pub const PAGES: u64 = 2;

fn pattern(i: usize) -> u8 {
    (i * 7 + 3) as u8
}

fn run() -> i64 {
    let out = console();
    let peer = Channel(slot("peer"));
    let memory = match Memory::create(PAGES * 4096) {
        Ok(m) => m,
        Err(e) => return fail(&out, "memory_create", e),
    };
    {
        let mapping = match memory.map(0, PAGES * 4096, map::WRITE) {
            Ok(m) => m,
            Err(e) => return fail(&out, "memory_map", e),
        };
        let mut page = [0u8; 4096];
        for p in 0..PAGES as usize {
            for (i, b) in page.iter_mut().enumerate() {
                *b = pattern(p * 4096 + i);
            }
            if let Err(e) = mapping.write(p * 4096, &page) {
                return fail(&out, "write the pattern", e);
            }
        }
    }
    let read_only = match memory
        .0
        .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
    {
        Ok(h) => h,
        Err(e) => return fail(&out, "handle_duplicate", e),
    };
    let sent = read_only.raw();
    if let Err((e, _)) = peer.write(b"pattern", [read_only]) {
        return fail(&out, "channel_write", e);
    }
    if info(sent) != Err(Error::BadHandle) {
        println!(
            out,
            "KB1 handle-transfer: FAIL: the sent handle is still in the sender's table"
        );
        return 1;
    }
    match wait_one(&peer.0, Signals::READABLE, now() + ms(10_000)) {
        Ok(_) => {}
        Err(e) => return fail(&out, "wait for the verdict", e),
    }
    let mut verdict = [0u8; 2];
    match peer.read(&mut verdict, &mut []) {
        Ok((2, 0)) if &verdict == b"ok" => 0,
        Ok(_) => 1,
        Err(e) => fail(&out, "read the verdict", e),
    }
}

fn fail(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(
        out,
        "KB1 handle-transfer: FAIL: sender: {}: {}",
        what,
        e.name()
    );
    1
}
