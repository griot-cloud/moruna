//! KK-T7, receiver: maps the handle it was sent, checks the pattern, and is refused
//! both a writable mapping and a duplicate with the write right.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{
    Channel, Error, Handle, Memory, ObjectType, Rights, Signals, map, now, println, wait_one,
};

moruna_sys::main!(run);

const PAGES: u64 = 2;

fn run() -> i64 {
    let out = console();
    let peer = Channel(slot("peer"));
    if let Err(e) = wait_one(&peer.0, Signals::READABLE, now() + ms(10_000)) {
        return fail(&out, "wait for the handle", e);
    }
    let mut bytes = [0u8; 16];
    let mut handles: [Option<Handle>; 1] = [None];
    let memory = match peer.read(&mut bytes, &mut handles) {
        Ok((7, 1)) if &bytes[..7] == b"pattern" => match handles[0].take() {
            Some(h) => Memory(h),
            None => return fail(&out, "the message carried no handle", Error::BadState),
        },
        Ok(_) => return fail(&out, "unexpected message", Error::BadState),
        Err(e) => return fail(&out, "channel_read", e),
    };
    match memory.0.info() {
        Ok((ObjectType::Memory, rights)) if !rights.contains(Rights::WRITE) => {}
        Ok(_) => {
            return fail(
                &out,
                "the handle is not a read-only memory object",
                Error::BadState,
            );
        }
        Err(e) => return fail(&out, "handle_info", e),
    }
    {
        let mapping = match memory.map(0, PAGES * 4096, 0) {
            Ok(m) => m,
            Err(e) => return fail(&out, "memory_map read-only", e),
        };
        let mut page = [0u8; 4096];
        for p in 0..PAGES as usize {
            if let Err(e) = mapping.read(p * 4096, &mut page) {
                return fail(&out, "read the pattern", e);
            }
            for (i, b) in page.iter().enumerate() {
                if *b != ((p * 4096 + i) * 7 + 3) as u8 {
                    return fail(&out, "the pattern differs", Error::BadState);
                }
            }
        }
    }
    let writable = memory.map(0, PAGES * 4096, map::WRITE);
    if !matches!(writable, Err(Error::AccessDenied)) {
        return fail(
            &out,
            "a writable mapping was not refused with access denied",
            Error::BadState,
        );
    }
    drop(writable);
    if !matches!(
        memory.0.duplicate(Rights::READ | Rights::WRITE),
        Err(Error::AccessDenied)
    ) {
        return fail(
            &out,
            "a duplicate with more rights was not refused",
            Error::BadState,
        );
    }
    println!(
        out,
        "KB1 handle-transfer: ok: {} pages arrived through a read-only handle with the pattern intact; mapping them writable was refused with access denied",
        PAGES
    );
    match peer.write(b"ok", []) {
        Ok(()) => 0,
        Err((e, _)) => fail(&out, "send the verdict", e),
    }
}

fn fail(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(
        out,
        "KB1 handle-transfer: FAIL: receiver: {}: {}",
        what,
        e.name()
    );
    1
}
