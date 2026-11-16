//! DS-A (`architecture/data-os-streams.md`, stream A): the data path's mechanisms, booted.
//!
//! - `thread_set_cpu`: the caller placed on each CPU in turn returns on it and stays
//!   there, which `rdtscp` shows with no call; a sleeping thread placed on a CPU wakes on
//!   it; a CPU past the last is refused;
//! - the clock page: mapped once, read only, and in step with `clock_get` (each call's
//!   time lies between two readings of the page around it);
//! - `windows_protect`: a group of windows made read only and writable again in one call,
//!   with their bytes kept, and every refusal leaving every window as it was.
//!
//! It also measures what C5 and C8 are judged by: the time a pinned CPU loses to the
//! kernel, and a read of the page against a `clock_get`. `dp-revoke` shows a write to a
//! revoked window faults.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use moruna_kb1_programs::{console, ms};
use moruna_sys::{
    Error, Mapping, Memory, Rights, Signals, Thread,
    datapath::{ClockPage, clock_page, current_cpu, pin_current_thread, thread_set_cpu},
    futex_wait, futex_wake, map, now, println, raw, thread_exit, wait_one,
};

moruna_sys::main!(run);

/// The sleeper's word: it waits while this is 0.
static WORD: AtomicU32 = AtomicU32::new(0);
/// Set once the sleeper is about to sleep.
static READY: AtomicU32 = AtomicU32::new(0);
/// The CPU the sleeper woke on, plus one; `u32::MAX` if it moved after waking.
static WOKE_ON: AtomicU32 = AtomicU32::new(0);

extern "C" fn sleeper(_: u64) -> ! {
    READY.store(1, Ordering::Release);
    while WORD.load(Ordering::Acquire) == 0 {
        let _ = futex_wait(&WORD, 0, now() + ms(10_000));
    }
    let cpu = current_cpu();
    let end = now() + ms(10);
    let mut stayed = true;
    while now() < end {
        stayed &= current_cpu() == cpu;
    }
    WOKE_ON.store(if stayed { cpu + 1 } else { u32::MAX }, Ordering::Release);
    thread_exit()
}

type Out = moruna_sys::Console;

fn fail(out: &Out, test: &str, what: &str, e: Error) -> i64 {
    println!(out, "DS-A {}: FAIL: {}: {}", test, what, e.name());
    1
}

fn run() -> i64 {
    let out = console();
    let cpus = match pin(&out) {
        Ok(n) => n,
        Err(code) => return code,
    };
    if let Err(code) = ticks(&out, cpus) {
        return code;
    }
    if let Err(code) = clock(&out) {
        return code;
    }
    if let Err(code) = revoke(&out) {
        return code;
    }
    0
}

/// Pins the caller to each CPU in turn, then a sleeping thread to the last; returns the
/// number of CPUs.
fn pin(out: &Out) -> Result<u32, i64> {
    let mut cpus = 0;
    loop {
        match pin_current_thread(Some(cpus)) {
            Ok(()) => {}
            Err(Error::InvalidArgs) if cpus > 0 => break,
            Err(e) => return Err(fail(out, "pin", "thread_set_cpu on the caller", e)),
        }
        // On it at once, and there for 20 ms of spinning.
        let end = now() + ms(20);
        while now() < end {
            let here = current_cpu();
            if here != cpus {
                println!(
                    out,
                    "DS-A pin: FAIL: placed on CPU {}, the thread ran on CPU {}", cpus, here
                );
                return Err(1);
            }
        }
        cpus += 1;
    }
    let last = cpus - 1;
    // A thread asleep is placed, and wakes on its CPU.
    let stack = Memory::create(64 * 1024).map_err(|e| fail(out, "pin", "a stack", e))?;
    let stack = stack
        .map(0, 64 * 1024, map::WRITE)
        .map_err(|e| fail(out, "pin", "mapping the stack", e))?;
    // From another CPU than the one it will be placed on, when there is one.
    pin_current_thread(Some(0)).map_err(|e| fail(out, "pin", "back to CPU 0", e))?;
    let thread =
        Thread::spawn(sleeper, stack.end(), 0).map_err(|e| fail(out, "pin", "thread_create", e))?;
    let deadline = now() + ms(10_000);
    while READY.load(Ordering::Acquire) == 0 && now() < deadline {
        core::hint::spin_loop();
    }
    // Time to be asleep in its wait.
    let _ = futex_wait(&READY, 1, now() + ms(20));
    thread_set_cpu(&thread, Some(last))
        .map_err(|e| fail(out, "pin", "thread_set_cpu on a sleeping thread", e))?;
    WORD.store(1, Ordering::Release);
    futex_wake(&WORD, 1).map_err(|e| fail(out, "pin", "futex_wake", e))?;
    let ended = wait_one(&thread.0, Signals::TERMINATED, now() + ms(10_000));
    let woke = WOKE_ON.load(Ordering::Acquire);
    if !matches!(ended, Ok(s) if s.contains(Signals::TERMINATED)) || woke != last + 1 {
        println!(
            out,
            "DS-A pin: FAIL: a thread placed on CPU {} asleep woke on {} (CPU + 1; {} if it moved)",
            last,
            woke,
            u32::MAX
        );
        return Err(1);
    }
    // A CPU past the last is refused; any CPU unpins.
    match pin_current_thread(Some(cpus)) {
        Err(Error::InvalidArgs) => {}
        other => {
            println!(
                out,
                "DS-A pin: FAIL: CPU {} of {} gave {:?}", cpus, cpus, other
            );
            return Err(1);
        }
    }
    let not_a_thread = Memory::create(4096).map_err(|e| fail(out, "pin", "memory_create", e))?;
    match thread_set_cpu(&Thread(not_a_thread.0), Some(0)) {
        Err(Error::WrongType) => {}
        other => {
            println!(
                out,
                "DS-A pin: FAIL: a memory handle as a thread gave {:?}", other
            );
            return Err(1);
        }
    }
    pin_current_thread(None).map_err(|e| fail(out, "pin", "unpinning", e))?;
    println!(
        out,
        "DS-A pin: ok: the caller placed on each of {} CPUs ran only there (rdtscp), a sleeping thread placed on CPU {} woke there, and a CPU past the last was refused",
        cpus,
        last
    );
    Ok(cpus)
}

/// C5's figure: on the last CPU, pinned, the time 200 ms of spinning loses in gaps of
/// more than 2 microseconds between two clock reads (ticks, interrupts, other threads).
fn ticks(out: &Out, cpus: u32) -> Result<(), i64> {
    let page = ClockPage::map().map_err(|e| fail(out, "ticks", "clock_page", e))?;
    pin_current_thread(Some(cpus - 1)).map_err(|e| fail(out, "ticks", "pinning", e))?;
    let start = page.now();
    let mut last = start;
    let (mut gaps, mut lost, mut longest) = (0u64, 0u64, 0u64);
    while last - start < ms(200) {
        let t = page.now();
        let gap = t - last;
        if gap > 2_000 {
            gaps += 1;
            lost += gap;
            longest = longest.max(gap);
        }
        last = t;
    }
    pin_current_thread(None).map_err(|e| fail(out, "ticks", "unpinning", e))?;
    println!(
        out,
        "DS-A ticks: ok: pinned to CPU {} for {} ns: {} gaps over 2 us, {} ns lost ({} per 100,000), the longest {} ns",
        cpus - 1,
        last - start,
        gaps,
        lost,
        lost * 100_000 / (last - start),
        longest
    );
    Ok(())
}

/// The clock page against `clock_get`.
fn clock(out: &Out) -> Result<(), i64> {
    let page = ClockPage::map().map_err(|e| fail(out, "clock page", "clock_page", e))?;
    let again = clock_page().map_err(|e| fail(out, "clock page", "a second clock_page", e))?;
    if again != page.address() {
        println!(
            out,
            "DS-A clock page: FAIL: mapped twice, at {:#x} and {:#x}",
            page.address(),
            again
        );
        return Err(1);
    }
    // Pinned, so every read is one CPU's counter.
    pin_current_thread(Some(0)).map_err(|e| fail(out, "clock page", "pinning", e))?;
    for _ in 0..10_000 {
        let before = page.now();
        let call = now();
        let after = page.now();
        if !(before <= call && call <= after) {
            println!(
                out,
                "DS-A clock page: FAIL: clock_get gave {} between page readings {} and {}",
                call,
                before,
                after
            );
            return Err(1);
        }
    }
    // A sleep moves both on alike.
    let (page_before, call_before) = (page.now(), now());
    let _ = futex_wait(&WORD, 1, now() + ms(5));
    let (page_after, call_after) = (page.now(), now());
    if page_after - page_before < ms(5) || page_after > call_after || call_before < page_before {
        println!(
            out,
            "DS-A clock page: FAIL: across a 5 ms sleep the page went {} -> {}, clock_get {} -> {}",
            page_before,
            page_after,
            call_before,
            call_after
        );
        return Err(1);
    }
    // Read only: it cannot be made writable.
    match Error::from_raw(raw::windows_protect(
        0,
        &[[page.address(), 4096]],
        map::READ | map::WRITE,
    )) {
        Err(Error::AccessDenied) => {}
        other => {
            println!(
                out,
                "DS-A clock page: FAIL: making the page writable gave {:?}", other
            );
            return Err(1);
        }
    }
    // What a reading costs, against a call.
    const READS: u64 = 100_000;
    let start = now();
    let mut sum = 0u64;
    for _ in 0..READS {
        sum = sum.wrapping_add(page.now());
    }
    let page_ns = now() - start;
    let start = now();
    for _ in 0..READS {
        sum = sum.wrapping_add(now());
    }
    let call_ns = now() - start;
    pin_current_thread(None).map_err(|e| fail(out, "clock page", "unpinning", e))?;
    println!(
        out,
        "DS-A clock page: ok: mapped once, read only, clock_get between every two readings; a reading {} ns against {} ns for clock_get ({})",
        page_ns / READS,
        call_ns / READS,
        sum & 1
    );
    Ok(())
}

/// Windows of two pages each, as stream P's output windows are made.
const WINDOWS: usize = 64;
const WINDOW: u64 = 2 * 4096;

fn check_windows(out: &Out, maps: &[Mapping<'_>], round: u8) -> Result<(), i64> {
    for (i, m) in maps.iter().enumerate() {
        let mut b = [0u8; 2];
        m.read(4096 + 7, &mut b)
            .map_err(|e| fail(out, "revoke", "reading a window", e))?;
        if b != [round, i as u8] {
            println!(
                out,
                "DS-A revoke: FAIL: window {} holds {:?} after round {}", i, b, round
            );
            return Err(1);
        }
    }
    Ok(())
}

fn fill_windows(out: &Out, maps: &[Mapping<'_>], round: u8) -> Result<(), i64> {
    for (i, m) in maps.iter().enumerate() {
        m.write(4096 + 7, &[round, i as u8])
            .map_err(|e| fail(out, "revoke", "writing a window", e))?;
    }
    Ok(())
}

fn refused(out: &Out, what: &str, want: Error, raw_result: u64) -> Result<(), i64> {
    match Error::from_raw(raw_result) {
        Err(e) if e == want => Ok(()),
        other => {
            println!(
                out,
                "DS-A revoke: FAIL: {} gave {:?}, not {}",
                what,
                other,
                want.name()
            );
            Err(1)
        }
    }
}

fn revoke(out: &Out) -> Result<(), i64> {
    let object = Memory::create(WINDOWS as u64 * WINDOW)
        .map_err(|e| fail(out, "revoke", "memory_create", e))?;
    // Each window its own mapping, so it can also be protected alone.
    let mut maps = Vec::with_capacity(WINDOWS);
    for i in 0..WINDOWS {
        let m = object
            .map(i as u64 * WINDOW, WINDOW, map::WRITE)
            .map_err(|e| fail(out, "revoke", "mapping a window", e))?;
        maps.push(m);
    }
    let mut ranges = [[0u64; 2]; WINDOWS];
    for (r, m) in ranges.iter_mut().zip(&maps) {
        *r = [m.addr(), m.len()];
    }
    fill_windows(out, &maps, 1)?;
    // Revoked in one call, the bytes kept; granted back in one call, writable again.
    let start = now();
    refused_ok(
        out,
        "the revoke",
        raw::windows_protect(0, &ranges, map::READ),
    )?;
    let grouped = now() - start;
    check_windows(out, &maps, 1)?;
    refused_ok(
        out,
        "the grant back",
        raw::windows_protect(0, &ranges, map::READ | map::WRITE),
    )?;
    fill_windows(out, &maps, 2)?;
    check_windows(out, &maps, 2)?;
    // The same revoke one window at a time, a call and a flush each.
    let start = now();
    for m in maps.iter_mut() {
        m.protect(false)
            .map_err(|e| fail(out, "revoke", "memory_protect", e))?;
    }
    let single = now() - start;
    for m in maps.iter_mut() {
        m.protect(true)
            .map_err(|e| fail(out, "revoke", "memory_protect back", e))?;
    }
    // Refusals, each leaving every window as it was.
    refused(
        out,
        "write without read",
        Error::InvalidArgs,
        raw::windows_protect(0, &ranges, map::WRITE),
    )?;
    refused(
        out,
        "no access",
        Error::InvalidArgs,
        raw::windows_protect(0, &ranges, 0),
    )?;
    refused(
        out,
        "execute",
        Error::InvalidArgs,
        raw::windows_protect(0, &ranges, map::READ | map::EXECUTE),
    )?;
    let many = [[ranges[0][0], 4096]; moruna_abi::windows::MAX + 1];
    refused(
        out,
        "one window past the most",
        Error::InvalidArgs,
        raw::windows_protect(0, &many, map::READ),
    )?;
    let last = maps[WINDOWS - 1].end();
    refused(
        out,
        "a window over a hole, after a good one",
        Error::OutOfRange,
        raw::windows_protect(0, &[ranges[0], [last, 4096]], map::READ),
    )?;
    refused(
        out,
        "an unaligned window",
        Error::InvalidArgs,
        raw::windows_protect(0, &[[ranges[0][0] + 8, 4096]], map::READ),
    )?;
    refused(
        out,
        "a memory handle for the process",
        Error::WrongType,
        raw::windows_protect(object.0.raw(), &ranges, map::READ),
    )?;
    let read_only = object
        .0
        .duplicate(Rights::READ | Rights::MAP)
        .map_err(|e| fail(out, "revoke", "handle_duplicate", e))?;
    let read_only = Memory(read_only);
    let looking = read_only
        .map(0, WINDOW, 0)
        .map_err(|e| fail(out, "revoke", "a read-only mapping", e))?;
    refused(
        out,
        "making a read-only handle's window writable",
        Error::AccessDenied,
        raw::windows_protect(0, &[[looking.addr(), WINDOW]], map::READ | map::WRITE),
    )?;
    refused_ok(out, "no windows", raw::windows_protect(0, &[], map::READ))?;
    // After every refusal, every window still takes writes.
    fill_windows(out, &maps, 3)?;
    check_windows(out, &maps, 3)?;
    println!(
        out,
        "DS-A revoke: ok: {} windows made read only and writable again in one call each, their bytes kept, every refusal changing none; the revoke took {} ns in one call against {} ns in {} memory_protect calls",
        WINDOWS,
        grouped,
        single,
        WINDOWS
    );
    Ok(())
}

fn refused_ok(out: &Out, what: &str, raw_result: u64) -> Result<(), i64> {
    Error::from_raw(raw_result)
        .map(|_| ())
        .map_err(|e| fail(out, "revoke", what, e))
}
