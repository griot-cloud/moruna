//! KK-T13 (SDD 20, k): the calls ABI 2.0 added for a process's own memory and
//! threads.
//!
//! - `memory_protect`: a mapping made read only keeps its bytes, and made writable again
//!   takes writes; a mapping of a handle without `WRITE` cannot be made writable;
//! - a partial `memory_unmap`: the first page of a three-page mapping stays, the object
//!   keeps the others, and a range a mapping does not hold is refused;
//! - `futex_wait` and `futex_wake`: a thread sleeping on a word is woken by a wake on
//!   it, a wait on a word that holds another value is `ShouldWait`, and one nothing
//!   wakes ends at its deadline;
//! - `thread_set_fs`: a lower-half address is taken, a non-canonical one refused;
//! - `random_read`: bytes that are not all zero, and a request past the bound refused.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use moruna_kb1_programs::{console, ms};
use moruna_sys::{
    Error, Memory, Rights, Signals, Thread, futex_wait, futex_wake, map, now, println, random,
    set_fs, thread_exit, wait_one,
};

moruna_sys::main!(run);

static WORD: AtomicU32 = AtomicU32::new(0);
static WOKEN: AtomicU64 = AtomicU64::new(0);

extern "C" fn sleeper(_: u64) -> ! {
    let r = futex_wait(&WORD, 0, now() + ms(10_000));
    WOKEN.store(if r.is_ok() { 1 } else { 2 }, Ordering::Release);
    thread_exit()
}

fn fail(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(out, "KC1 kernel calls: FAIL: {}: {}", what, e.name());
    1
}

fn run() -> i64 {
    let out = console();
    // memory_protect and a partial memory_unmap.
    let Ok(object) = Memory::create(3 * 4096) else {
        return fail(&out, "memory_create", Error::NoMemory);
    };
    let mut m = match object.map(0, 3 * 4096, map::WRITE) {
        Ok(m) => m,
        Err(e) => return fail(&out, "memory_map", e),
    };
    for page in 0..3 {
        if let Err(e) = m.write(page * 4096, &[0xa0 + page as u8; 8]) {
            return fail(&out, "writing the mapping", e);
        }
    }
    if let Err(e) = m.protect(false) {
        return fail(&out, "memory_protect to read only", e);
    }
    let mut b = [0u8; 8];
    if m.read(4096, &mut b).is_err() || b != [0xa1; 8] {
        println!(
            out,
            "KC1 kernel calls: FAIL: a read-only mapping lost its bytes"
        );
        return 1;
    }
    if let Err(e) = m.protect(true) {
        return fail(&out, "memory_protect back to writable", e);
    }
    if let Err(e) = m.write(8, &[1; 4]) {
        return fail(&out, "writing after memory_protect", e);
    }
    if let Err(e) = m.shrink(1) {
        return fail(&out, "a partial memory_unmap", e);
    }
    if m.read(0, &mut b).is_err() || b != [0xa0; 8] {
        println!(out, "KC1 kernel calls: FAIL: the page kept lost its bytes");
        return 1;
    }
    let again = match object.map(4096, 2 * 4096, 0) {
        Ok(a) => a,
        Err(e) => return fail(&out, "mapping the rest again", e),
    };
    if again.read(4096, &mut b).is_err() || b != [0xa2; 8] {
        println!(
            out,
            "KC1 kernel calls: FAIL: the object lost the pages unmapped from one mapping"
        );
        return 1;
    }
    let read_only = match object.0.duplicate(Rights::READ | Rights::MAP) {
        Ok(h) => Memory(h),
        Err(e) => return fail(&out, "handle_duplicate", e),
    };
    let mut ro = match read_only.map(0, 4096, 0) {
        Ok(m) => m,
        Err(e) => return fail(&out, "mapping a read-only handle", e),
    };
    match ro.protect(true) {
        Err(Error::AccessDenied) => {}
        other => {
            println!(
                out,
                "KC1 kernel calls: FAIL: a read-only handle's mapping made writable: {:?}", other
            );
            return 1;
        }
    }

    // futex_wait and futex_wake.
    let Ok(stack) = Memory::create(64 * 1024) else {
        return fail(&out, "the thread's stack", Error::NoMemory);
    };
    let stack_map = match stack.map(0, 64 * 1024, map::WRITE) {
        Ok(s) => s,
        Err(e) => return fail(&out, "mapping the stack", e),
    };
    let thread = match Thread::spawn(sleeper, stack_map.end(), 0) {
        Ok(t) => t,
        Err(e) => return fail(&out, "thread_create", e),
    };
    let deadline = now() + ms(10_000);
    let mut woke = 0;
    while woke == 0 && now() < deadline {
        woke = match futex_wake(&WORD, 1) {
            Ok(n) => n,
            Err(e) => return fail(&out, "futex_wake", e),
        };
    }
    if woke != 1
        || !matches!(wait_one(&thread.0, Signals::TERMINATED, now() + ms(10_000)), Ok(s) if s.contains(Signals::TERMINATED))
        || WOKEN.load(Ordering::Acquire) != 1
    {
        println!(
            out,
            "KC1 kernel calls: FAIL: the futex wake did not wake the sleeper"
        );
        return 1;
    }
    WORD.store(5, Ordering::Release);
    match futex_wait(&WORD, 4, now() + ms(1000)) {
        Err(Error::ShouldWait) => {}
        other => {
            println!(
                out,
                "KC1 kernel calls: FAIL: a wait on a changed word gave {:?}", other
            );
            return 1;
        }
    }
    let started = now();
    match futex_wait(&WORD, 5, started + ms(20)) {
        Err(Error::TimedOut) if now() >= started + ms(20) => {}
        other => {
            println!(
                out,
                "KC1 kernel calls: FAIL: a wait nothing wakes gave {:?}", other
            );
            return 1;
        }
    }

    // thread_set_fs.
    if let Err(e) = set_fs(0x7000_0000_1000) {
        return fail(&out, "thread_set_fs", e);
    }
    match set_fs(0xffff_8000_0000_0000) {
        Err(Error::InvalidArgs) => {}
        other => {
            println!(
                out,
                "KC1 kernel calls: FAIL: a non-canonical FS base gave {:?}", other
            );
            return 1;
        }
    }
    if let Err(e) = set_fs(0) {
        return fail(&out, "thread_set_fs back to zero", e);
    }

    // random_read.
    let mut bytes = [0u8; 64];
    if let Err(e) = random(&mut bytes) {
        return fail(&out, "random_read", e);
    }
    if bytes.iter().all(|b| *b == 0) {
        println!(out, "KC1 kernel calls: FAIL: 64 random bytes were all zero");
        return 1;
    }
    let mut big = [0u8; moruna_sys::limits::RANDOM_MAX + 1];
    if Error::from_raw(moruna_sys::raw::random_read(&mut big)) != Err(Error::Limit) {
        println!(
            out,
            "KC1 kernel calls: FAIL: random_read past its bound was not refused"
        );
        return 1;
    }
    println!(
        out,
        "KC1 kernel calls: ok: memory_protect, a partial memory_unmap, futex_wait and futex_wake, thread_set_fs and random_read"
    );
    0
}
