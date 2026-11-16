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
//! - `map::HUGE` and `map::POPULATE`: an object mapped with both is in 2 MiB entries
//!   before a byte is touched, at an aligned address, and keeps them through every touch
//!   (no page faults: every page has its entry); with `map::POPULATE` alone every page has
//!   a 4 KiB entry before it is touched; an object too small for a large page maps in
//!   4 KiB entries; a revoke of one page splits its large page and a revoke of a whole one
//!   keeps it; a partial unmap leaves the rest mapped. The page table is read with
//!   `plane_probe` (the CI boot's kernel).
//!
//! - `map::POPULATE` zero pages through `process_map`: every page charged at the call, and
//!   none charged again when it is written (no page is touched for the first time);
//! - `windows_protect`'s slot form: an engine revokes a task's window through the output
//!   slot alone. A revoked page keeps its bytes, a put of a window part of which is revoked
//!   gives the record back, refusals change nothing, and `dp-task`'s write to a page
//!   revoked this way faults.
//!
//! - C5's stretched tick: a thread queued on a CPU whose tick is stretched (it ran one
//!   thread) runs there within 20 ms, and then two threads share the CPU; and grace
//!   periods complete while a CPU runs one thread in user mode, switching no task: CPU 0
//!   makes and frees more memory than the machine has, each freed frame waiting for a
//!   grace period.
//!
//! - AVX state (`avx_fixture.rs`): one more thread than there are CPUs keeps its own
//!   values in all 16 `ymm` registers through preemption, and one moves between CPUs with
//!   `thread_set_cpu`, every lane checked at the end;
//! - C5's placement: unpinned threads that first ran on a core keep off it once a worker
//!   is pinned there, while another core has no pinned thread, so they no longer take the
//!   worker's turns.
//!
//! It also measures what C4, C5 and C8 are judged by: a touch a page through large pages
//! and through 4 KiB ones, the time a pinned CPU loses to the kernel, and a read of the
//! page against a `clock_get`. `dp-revoke` shows a write to a revoked window faults.

#![no_std]
#![no_main]
// The AVX check's assembly region (`avx_fixture.rs`) is the one place unsafe code is
// allowed.
#![deny(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{
    Account, Channel, Error, Handle, Mapping, Memory, Process, Rights, Signals, Thread, charge,
    datapath::{
        ClockPage, clock_page, current_cpu, mapping_size, pin_current_thread, thread_set_cpu,
        windows_protect_slot,
    },
    futex_wait, futex_wake, map, now,
    plane::{Slot, SlotEvent, attach},
    println, raw, thread_exit, wait_one,
};

#[path = "../avx_fixture.rs"]
mod avx;

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
    if let Err(code) = share(&out, cpus) {
        return code;
    }
    if let Err(code) = rcu(&out, cpus) {
        return code;
    }
    if let Err(code) = placement(&out, cpus) {
        return code;
    }
    if let Err(code) = avx::check(&out, cpus) {
        return code;
    }
    if let Err(code) = clock(&out) {
        return code;
    }
    if let Err(code) = revoke(&out) {
        return code;
    }
    if let Err(code) = huge(&out) {
        return code;
    }
    let resource = match process_resource(&out) {
        Ok(r) => r,
        Err(code) => return code,
    };
    if let Err(code) = populate(&out, &resource) {
        return code;
    }
    if let Err(code) = slot_form(&out, &resource, &slot("task-image")) {
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

/// C5's figure: on the last CPU, pinned, what 300 ms of spinning loses between two
/// clock reads more than 500 ns apart. A gap under 100 us is a tick or a device
/// interrupt, what tickless worker cores would take away; a longer one is another thread
/// the CPU ran meanwhile (the boot runs every test at once).
fn ticks(out: &Out, cpus: u32) -> Result<(), i64> {
    let page = ClockPage::map().map_err(|e| fail(out, "ticks", "clock_page", e))?;
    pin_current_thread(Some(cpus - 1)).map_err(|e| fail(out, "ticks", "pinning", e))?;
    let start = page.now();
    let mut last = start;
    let (mut short, mut short_ns, mut longest_short) = (0u64, 0u64, 0u64);
    let (mut long, mut long_ns) = (0u64, 0u64);
    while last - start < ms(300) {
        let t = page.now();
        let gap = t - last;
        if gap >= 100_000 {
            long += 1;
            long_ns += gap;
        } else if gap > 500 {
            short += 1;
            short_ns += gap;
            longest_short = longest_short.max(gap);
        }
        last = t;
    }
    pin_current_thread(None).map_err(|e| fail(out, "ticks", "unpinning", e))?;
    let ran = last - start - long_ns;
    println!(
        out,
        "DS-A ticks: ok: pinned to CPU {} for {} ns: {} interruptions under 100 us took {} ns, {} per 100,000 of the {} ns it ran (the longest {} ns); {} longer gaps were other threads ({} ns)",
        cpus - 1,
        last - start,
        short,
        short_ns,
        short_ns * 100_000 / ran.max(1),
        ran,
        longest_short,
        long,
        long_ns
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

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const LARGE: u64 = 2 * MIB;
const OBJECT: u64 = 32 * MIB;

fn huge_fail(out: &Out, what: &str, e: Error) -> i64 {
    println!(out, "DS-A huge: FAIL: {}: {}", what, e.name());
    1
}

/// Every page of `m`'s `range` has an entry of `size` bytes.
fn entries(
    out: &Out,
    m: &Mapping<'_>,
    from: u64,
    to: u64,
    size: u64,
    when: &str,
) -> Result<(), i64> {
    let mut at = from;
    while at < to {
        let got = mapping_size(m.addr() + at).map_err(|e| huge_fail(out, "plane_probe", e))?;
        if got != size {
            println!(
                out,
                "DS-A huge: FAIL: {}: the page at offset {:#x} has an entry of {} bytes, not {}",
                when,
                at,
                got,
                size
            );
            return Err(1);
        }
        at += 4 * KIB;
    }
    Ok(())
}

/// Writes then reads one byte a page, `passes` times; the nanoseconds a touch.
fn touch(out: &Out, m: &Mapping<'_>, passes: u64, mark: u8) -> Result<u64, i64> {
    let pages = m.len() / (4 * KIB);
    let start = now();
    for pass in 0..passes {
        for page in 0..pages {
            let at = (page * 4 * KIB + (pass % 64) * 8) as usize;
            m.write(at, &[mark])
                .map_err(|e| huge_fail(out, "a touch", e))?;
        }
    }
    let ns = (now() - start) / (passes * pages);
    let mut b = [0u8; 1];
    for page in 0..pages {
        m.read((page * 4 * KIB) as usize, &mut b)
            .map_err(|e| huge_fail(out, "reading back", e))?;
        if b[0] != mark {
            println!(out, "DS-A huge: FAIL: page {} lost its byte", page);
            return Err(1);
        }
    }
    Ok(ns)
}

fn huge(out: &Out) -> Result<(), i64> {
    let object = Memory::create(OBJECT).map_err(|e| huge_fail(out, "memory_create", e))?;
    let large = object
        .map(0, OBJECT, map::WRITE | map::HUGE | map::POPULATE)
        .map_err(|e| huge_fail(out, "memory_map HUGE | POPULATE", e))?;
    if large.addr() % LARGE != 0 {
        println!(
            out,
            "DS-A huge: FAIL: a large mapping at {:#x}, not on a 2 MiB boundary",
            large.addr()
        );
        return Err(1);
    }
    entries(out, &large, 0, OBJECT, LARGE, "before any touch")?;
    let large_ns = touch(out, &large, 16, 0xa5)?;
    entries(
        out,
        &large,
        0,
        OBJECT,
        LARGE,
        "after every page was touched",
    )?;

    // POPULATE alone: every page present, in 4 KiB entries, before a touch.
    let plain = object
        .map(0, OBJECT, map::WRITE | map::POPULATE)
        .map_err(|e| huge_fail(out, "memory_map POPULATE", e))?;
    entries(
        out,
        &plain,
        0,
        OBJECT,
        4 * KIB,
        "POPULATE alone, before any touch",
    )?;
    let plain_ns = touch(out, &plain, 16, 0x5a)?;

    // Too small for a large page: 4 KiB entries.
    let small = Memory::create(64 * KIB).map_err(|e| huge_fail(out, "a small object", e))?;
    let small_map = small
        .map(0, 64 * KIB, map::WRITE | map::HUGE)
        .map_err(|e| huge_fail(out, "memory_map HUGE of a small object", e))?;
    entries(out, &small_map, 0, 64 * KIB, 4 * KIB, "a small object")?;

    // A revoke of one page splits its large page; the others stay large.
    let one = [[large.addr() + 4 * KIB, 4 * KIB]];
    Error::from_raw(raw::windows_protect(0, &one, map::READ))
        .map_err(|e| huge_fail(out, "revoking one page", e))?;
    entries(
        out,
        &large,
        0,
        LARGE,
        4 * KIB,
        "the large page one revoke split",
    )?;
    entries(
        out,
        &large,
        LARGE,
        OBJECT,
        LARGE,
        "the large pages beside the split",
    )?;
    Error::from_raw(raw::windows_protect(0, &one, map::READ | map::WRITE))
        .map_err(|e| huge_fail(out, "granting the page back", e))?;
    // A revoke of a whole large page changes it in place.
    let whole = [[large.addr() + 2 * LARGE, LARGE]];
    Error::from_raw(raw::windows_protect(0, &whole, map::READ))
        .map_err(|e| huge_fail(out, "revoking a large page", e))?;
    entries(
        out,
        &large,
        2 * LARGE,
        3 * LARGE,
        LARGE,
        "a revoked large page",
    )?;
    Error::from_raw(raw::windows_protect(0, &whole, map::READ | map::WRITE))
        .map_err(|e| huge_fail(out, "granting a large page back", e))?;
    touch(out, &large, 1, 0x3c)?;

    // A partial unmap keeps the rest.
    let mut large = large;
    large
        .shrink(LARGE / (4 * KIB) + 1)
        .map_err(|e| huge_fail(out, "a partial unmap", e))?;
    entries(
        out,
        &large,
        4 * KIB,
        2 * KIB * 4,
        4 * KIB,
        "after a partial unmap",
    )?;
    touch(out, &large, 1, 0xc3)?;

    println!(
        out,
        "DS-A huge: ok: {} MiB in 2 MiB entries at an aligned address before any touch, kept through every touch, split by a one-page revoke and kept by a whole one; POPULATE alone gave every page its entry; a touch a page took {} ns through large pages against {} ns through 4 KiB ones",
        OBJECT / MIB,
        large_ns,
        plain_ns
    );
    Ok(())
}

/// The process resource the supervisor sends, as it sends the accounts test its own.
fn process_resource(out: &Out) -> Result<Handle, i64> {
    let supervisor = Channel(slot("supervisor"));
    wait_one(&supervisor.0, Signals::READABLE, now() + ms(60_000))
        .map_err(|e| fail(out, "populate", "the supervisor's process resource", e))?;
    let mut buf = [0u8; 16];
    let mut handles = [None];
    supervisor
        .read(&mut buf, &mut handles)
        .map_err(|e| fail(out, "populate", "reading the process resource", e))?;
    handles[0]
        .take()
        .ok_or_else(|| fail(out, "populate", "no process resource", Error::BadState))
}

/// `map::POPULATE` zero pages through `process_map`, into a process made for it.
fn populate(out: &Out, resource: &Handle) -> Result<(), i64> {
    const SIZE: u64 = 4 << 20;
    const PAGE: u64 = 4096;
    let f = |what: &str, e: Error| fail(out, "populate", what, e);
    let account = Account::carve(16 << 20, "dp-populate").map_err(|e| f("account_create", e))?;
    let p = Process::create_in(resource, "dp-populate", &account.0)
        .map_err(|e| f("process_create", e))?;
    let used = || {
        account
            .usage()
            .map(|u| u.used)
            .map_err(|e| f("account_info", e))
    };
    let before = used()?;
    let va = p
        .map(
            None,
            0,
            SIZE,
            0,
            map::READ | map::WRITE | map::POPULATE | map::HUGE,
        )
        .map_err(|e| f("process_map POPULATE", e))?;
    let mapped = used()?;
    let pages = SIZE / PAGE;
    if mapped - before < pages * charge::ZERO_PAGE {
        println!(
            out,
            "DS-A populate: FAIL: {} zero pages charged {} bytes at the call, less than {}",
            pages,
            mapped - before,
            pages * charge::ZERO_PAGE
        );
        return Err(1);
    }
    for page in 0..pages {
        p.write(va + page * PAGE + 7, &[page as u8])
            .map_err(|e| f("process_write", e))?;
    }
    let written = used()?;
    let mut back = [0xffu8; 2];
    p.read(va + 5 * PAGE + 7, &mut back)
        .map_err(|e| f("process_read", e))?;
    if written != mapped || back != [5, 0] {
        println!(
            out,
            "DS-A populate: FAIL: writing every page charged {} bytes more, and read {:?}",
            written - mapped,
            back
        );
        return Err(1);
    }
    // Without POPULATE a page is charged at its first touch.
    let lazy = p
        .map(None, 0, 64 * PAGE, 0, map::READ | map::WRITE)
        .map_err(|e| f("process_map", e))?;
    let quiet = used()?;
    p.write(lazy, &[1]).map_err(|e| f("process_write", e))?;
    let first = used()? - quiet;
    if first != charge::ZERO_PAGE {
        println!(
            out,
            "DS-A populate: FAIL: a lazy page's first write charged {} bytes", first
        );
        return Err(1);
    }
    println!(
        out,
        "DS-A populate: ok: {} zero pages charged {} bytes at process_map with POPULATE, and nothing more when every page was written; without it a first write charged {}",
        pages,
        mapped - before,
        first
    );
    Ok(())
}

const TASK_PAGE: usize = 4096;
const TASK_WINDOW: u64 = 4 * TASK_PAGE as u64;

/// Waits for `word` from the task.
fn heard(out: &Out, peer: &Channel, word: &[u8]) -> Result<(), i64> {
    wait_one(&peer.0, Signals::READABLE, now() + ms(30_000))
        .map_err(|e| fail(out, "slot", "waiting for dp-task", e))?;
    let mut buf = [0u8; 16];
    match peer.read(&mut buf, &mut []) {
        Ok((n, 0)) if &buf[..n] == word => Ok(()),
        other => {
            println!(
                out,
                "DS-A slot: FAIL: dp-task said {:?} ({:?}), not {:?}",
                &buf[..16],
                other,
                word
            );
            Err(1)
        }
    }
}

/// The next event of `output`.
fn event(out: &Out, output: &Slot) -> Result<SlotEvent, i64> {
    wait_one(&output.0, Signals::READABLE, now() + ms(30_000))
        .map_err(|e| fail(out, "slot", "waiting for the slot", e))?;
    output.take().map_err(|e| fail(out, "slot", "slot_take", e))
}

/// The first byte of each page of a window that came back.
fn pages_of(out: &Out, memory: &Memory) -> Result<[u8; 4], i64> {
    let m = memory
        .map(0, TASK_WINDOW, 0)
        .map_err(|e| fail(out, "slot", "mapping a window back", e))?;
    let mut firsts = [0u8; 4];
    for (page, b) in firsts.iter_mut().enumerate() {
        let mut one = [0u8; 1];
        m.read(page * TASK_PAGE, &mut one)
            .map_err(|e| fail(out, "slot", "reading a window back", e))?;
        *b = one[0];
    }
    Ok(firsts)
}

fn slot_refused(
    out: &Out,
    what: &str,
    want: Error,
    result: moruna_sys::Result<()>,
) -> Result<(), i64> {
    match result {
        Err(e) if e == want => Ok(()),
        other => {
            println!(
                out,
                "DS-A slot: FAIL: {} gave {:?}, not {}",
                what,
                other,
                want.name()
            );
            Err(1)
        }
    }
}

/// `windows_protect`'s slot form, against `dp-task`.
fn slot_form(out: &Out, resource: &Handle, image: &Handle) -> Result<(), i64> {
    let f = |what: &str, e: Error| fail(out, "slot", what, e);
    let account = Account::carve(16 << 20, "dp-task").map_err(|e| f("account_create", e))?;
    let task =
        Process::create_in(resource, "dp-task", &account.0).map_err(|e| f("process_create", e))?;
    let entry = task
        .map_image(image)
        .map_err(|e| f("process_map_image", e))?;
    let output = Slot::create(true).map_err(|e| f("slot_create", e))?;
    let (ours, theirs) = Channel::create().map_err(|e| f("channel_create", e))?;
    attach(&task, &[&output], None).map_err(|(e, _)| f("process_plane", e))?;
    slot_refused(
        out,
        "a slot with nothing mapped",
        Error::BadState,
        windows_protect_slot(&output, &[[0, 4096]], map::READ),
    )?;
    let first = Memory::create(TASK_WINDOW).map_err(|e| f("memory_create", e))?;
    output
        .push(first, 0, TASK_WINDOW, 1)
        .map_err(|(e, _)| f("slot_push", e))?;
    task.start_with(entry, &mut [(Some(theirs.0), "peer")])
        .map_err(|e| f("process_start", e))?;
    heard(out, &ours, b"mapped")?;
    // Refusals first, each changing nothing.
    slot_refused(
        out,
        "a window past the slot's area",
        Error::OutOfRange,
        windows_protect_slot(&output, &[[3 * 4096, 2 * 4096]], map::READ),
    )?;
    slot_refused(
        out,
        "execute",
        Error::InvalidArgs,
        windows_protect_slot(&output, &[[0, 4096]], map::READ | map::EXECUTE),
    )?;
    let looking = Slot(
        output
            .0
            .duplicate(Rights::READ)
            .map_err(|e| f("handle_duplicate", e))?,
    );
    slot_refused(
        out,
        "a slot handle without WRITE",
        Error::AccessDenied,
        windows_protect_slot(&looking, &[[0, 4096]], map::READ),
    )?;
    // Page 1 revoked, granted back, then pages 1 and 2 revoked.
    windows_protect_slot(&output, &[[4096, 4096]], map::READ)
        .map_err(|e| f("revoking a page", e))?;
    windows_protect_slot(&output, &[[4096, 4096]], map::READ | map::WRITE)
        .map_err(|e| f("granting it back", e))?;
    windows_protect_slot(&output, &[[4096, 2 * 4096]], map::READ)
        .map_err(|e| f("revoking two pages", e))?;
    ours.write(b"go", [])
        .map_err(|(e, _)| f("channel_write", e))?;
    heard(out, &ours, b"put")?;
    // The record put while two pages were revoked comes back whole, with their bytes kept.
    let SlotEvent::Put { memory, .. } = event(out, &output)? else {
        println!(out, "DS-A slot: FAIL: the put gave another event");
        return Err(1);
    };
    let firsts = pages_of(out, &memory)?;
    if firsts != [0xb0, 0xa1, 0xa2, 0xb3] {
        println!(
            out,
            "DS-A slot: FAIL: the first window came back as {:x?}", firsts
        );
        return Err(1);
    }
    // The second window: page 1 revoked through the slot, then written by the task.
    let second = Memory::create(TASK_WINDOW).map_err(|e| f("memory_create", e))?;
    output
        .push(second, 0, TASK_WINDOW, 2)
        .map_err(|(e, _)| f("slot_push", e))?;
    heard(out, &ours, b"mapped")?;
    windows_protect_slot(&output, &[[4096, 4096]], map::READ)
        .map_err(|e| f("revoking the second window's page", e))?;
    println!(
        out,
        "DS-A slot: dp-task writes a page revoked through its slot"
    );
    ours.write(b"go", [])
        .map_err(|(e, _)| f("channel_write", e))?;
    let ended = wait_one(&task.0, Signals::TERMINATED, now() + ms(30_000));
    if !matches!(ended, Ok(s) if s.contains(Signals::TERMINATED)) {
        println!(out, "DS-A slot: FAIL: dp-task did not end: {:?}", ended);
        return Err(1);
    }
    let mut word = [0u8; 16];
    if let Ok((n, _)) = ours.read(&mut word, &mut []) {
        println!(
            out,
            "DS-A slot: FAIL: dp-task wrote a revoked page and said {:?}",
            &word[..n]
        );
        return Err(1);
    }
    let SlotEvent::Returned { memory, .. } = event(out, &output)? else {
        println!(out, "DS-A slot: FAIL: the task's end gave another event");
        return Err(1);
    };
    let firsts = pages_of(out, &memory)?;
    if firsts != [0xc0, 0xc1, 0xc2, 0xc3] {
        println!(
            out,
            "DS-A slot: FAIL: the second window came back as {:x?}", firsts
        );
        return Err(1);
    }
    println!(
        out,
        "DS-A slot: ok: windows revoked and granted back through an output slot alone, their bytes kept; a window put while two of its pages were revoked came back whole; every refusal changed nothing; and the task's write to a page revoked so faulted"
    );
    Ok(())
}

/// Set to stop the spinning threads of `share` and `rcu`.
static STOP: AtomicU32 = AtomicU32::new(0);
/// Set once the first spinner runs on its CPU.
static SPINNING: AtomicU32 = AtomicU32::new(0);
/// Set once the second thread runs on the spinner's CPU.
static JOINED: AtomicU32 = AtomicU32::new(0);
static COUNT_A: AtomicU64 = AtomicU64::new(0);
static COUNT_B: AtomicU64 = AtomicU64::new(0);
/// How long the second thread's placement on the spinner's CPU took, in nanoseconds.
static JOIN_NS: AtomicU64 = AtomicU64::new(0);

/// Spins on CPU `cpu`, counting, until `STOP`.
extern "C" fn spinner(cpu: u64) -> ! {
    if pin_current_thread(Some(cpu as u32)).is_ok() {
        SPINNING.store(1, Ordering::Release);
        while STOP.load(Ordering::Relaxed) == 0 {
            COUNT_A.fetch_add(1, Ordering::Relaxed);
        }
    }
    thread_exit()
}

/// Places itself on CPU `cpu`, beside the spinner, timing it; then spins there too.
extern "C" fn joiner(cpu: u64) -> ! {
    let start = now();
    if pin_current_thread(Some(cpu as u32)).is_ok() {
        JOIN_NS.store(now() - start, Ordering::Relaxed);
        JOINED.store(1, Ordering::Release);
        while STOP.load(Ordering::Relaxed) == 0 {
            COUNT_B.fetch_add(1, Ordering::Relaxed);
        }
    }
    thread_exit()
}

/// A thread started at `entry(arg)` on a stack of its own, which it keeps.
fn start(out: &Out, test: &str, entry: extern "C" fn(u64) -> !, arg: u64) -> Result<Thread, i64> {
    let stack = Memory::create(64 * 1024).map_err(|e| fail(out, test, "a stack", e))?;
    let mapping = stack
        .map_detached(0, 64 * 1024, map::WRITE)
        .map_err(|e| fail(out, test, "mapping a stack", e))?;
    let top = mapping.end();
    // The stack stays mapped for good: the thread exits on it.
    core::mem::forget(mapping);
    core::mem::forget(stack);
    Thread::spawn(entry, top, arg).map_err(|e| fail(out, test, "thread_create", e))
}

/// Waits until `flag` is set, for at most `ms` milliseconds.
fn until(flag: &AtomicU32, limit: u64) -> bool {
    let end = now() + ms(limit);
    while flag.load(Ordering::Acquire) == 0 {
        if now() > end {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

fn join(out: &Out, test: &str, thread: &Thread) -> Result<(), i64> {
    match wait_one(&thread.0, Signals::TERMINATED, now() + ms(10_000)) {
        Ok(s) if s.contains(Signals::TERMINATED) => Ok(()),
        other => {
            println!(
                out,
                "DS-A {}: FAIL: a thread did not end: {:?}", test, other
            );
            Err(1)
        }
    }
}

/// A thread queued on the spinner's CPU, whose tick is stretched, runs there within
/// 20 ms, and then the two share the CPU.
fn share(out: &Out, cpus: u32) -> Result<(), i64> {
    let last = u64::from(cpus - 1);
    STOP.store(0, Ordering::Relaxed);
    pin_current_thread(Some(0)).map_err(|e| fail(out, "share", "pinning", e))?;
    let a = start(out, "share", spinner, last)?;
    if !until(&SPINNING, 10_000) {
        println!(out, "DS-A share: FAIL: the spinner never ran");
        return Err(1);
    }
    // Long enough for its CPU to stretch its tick.
    let _ = futex_wait(&READY, 1, now() + ms(20));
    let b = start(out, "share", joiner, last)?;
    if !until(&JOINED, 10_000) {
        println!(
            out,
            "DS-A share: FAIL: the second thread never ran beside the spinner"
        );
        return Err(1);
    }
    let joined = JOIN_NS.load(Ordering::Relaxed);
    let (a0, b0) = (
        COUNT_A.load(Ordering::Relaxed),
        COUNT_B.load(Ordering::Relaxed),
    );
    let _ = futex_wait(&READY, 1, now() + ms(300));
    let (a1, b1) = (
        COUNT_A.load(Ordering::Relaxed),
        COUNT_B.load(Ordering::Relaxed),
    );
    STOP.store(1, Ordering::Relaxed);
    join(out, "share", &a)?;
    join(out, "share", &b)?;
    pin_current_thread(None).map_err(|e| fail(out, "share", "unpinning", e))?;
    let (da, db) = (a1 - a0, b1 - b0);
    let total = (da + db).max(1);
    if joined > ms(20) || da * 4 < total || db * 4 < total {
        println!(
            out,
            "DS-A share: FAIL: the second thread took {} ns to run beside the spinner, and they counted {} and {}",
            joined,
            da,
            db
        );
        return Err(1);
    }
    println!(
        out,
        "DS-A share: ok: a thread queued on CPU {}, its tick stretched, ran there after {} ns, and the two then shared it {}% and {}%",
        last,
        joined,
        da * 100 / total,
        db * 100 / total
    );
    Ok(())
}

/// Grace periods complete while a CPU runs one thread in user mode: CPU 0 makes, maps and
/// frees more memory than the machine has (3 GiB in 128 MiB objects) while the spinner
/// runs on the last CPU. OSTD frees an unmapped frame only after a grace period, so had
/// grace periods stalled, memory would run out.
fn rcu(out: &Out, cpus: u32) -> Result<(), i64> {
    const CHUNK: u64 = 128 << 20;
    const ROUNDS: u64 = 24;
    let last = u64::from(cpus - 1);
    STOP.store(0, Ordering::Relaxed);
    SPINNING.store(0, Ordering::Relaxed);
    pin_current_thread(Some(0)).map_err(|e| fail(out, "rcu", "pinning", e))?;
    let spin = start(out, "rcu", spinner, last)?;
    if !until(&SPINNING, 10_000) {
        println!(out, "DS-A rcu: FAIL: the spinner never ran");
        return Err(1);
    }
    let started = now();
    for round in 0..ROUNDS {
        let made = Memory::create(CHUNK).and_then(|object| {
            let m = object.map(0, CHUNK, map::WRITE | map::HUGE)?;
            for at in (0..CHUNK as usize).step_by(2 << 20) {
                m.write(at, &[round as u8])?;
            }
            Ok(())
        });
        if let Err(e) = made {
            STOP.store(1, Ordering::Relaxed);
            println!(
                out,
                "DS-A rcu: FAIL: round {} of {} ({} MiB made and freed): {}",
                round,
                ROUNDS,
                round * (CHUNK >> 20),
                e.name()
            );
            return Err(1);
        }
    }
    let took = now() - started;
    STOP.store(1, Ordering::Relaxed);
    join(out, "rcu", &spin)?;
    pin_current_thread(None).map_err(|e| fail(out, "rcu", "unpinning", e))?;
    println!(
        out,
        "DS-A rcu: ok: {} MiB made, mapped and freed on CPU 0 in {} ms while CPU {} ran one thread in user mode",
        ROUNDS * (CHUNK >> 20),
        took / 1_000_000,
        last
    );
    Ok(())
}

/// Set to stop the wakers of `placement`.
static WAKERS_STOP: AtomicU32 = AtomicU32::new(0);
/// Never changes: the wakers sleep on it with a deadline.
static WAKER_WORD: AtomicU32 = AtomicU32::new(0);
/// Each waker's CPU at its last wake, plus one.
static WAKER_CPU: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
/// The worker's CPU, plus one, once it is pinned there (zero before).
static WORKER_ON: AtomicU32 = AtomicU32::new(0);
/// Wakes of a waker on the worker's CPU once the worker is pinned there.
static WAKES_ON_WORKER: AtomicU64 = AtomicU64::new(0);
/// The worker's turns lost to other threads (gaps of 150 us or more), and their time.
static LOST_TURNS: AtomicU64 = AtomicU64::new(0);
static LOST_NS: AtomicU64 = AtomicU64::new(0);
/// Set once the worker has measured.
static MEASURED: AtomicU32 = AtomicU32::new(0);

/// Wakes every 2 ms and works 200 us, as a reactor or a ticker does, unpinned.
extern "C" fn waker(i: u64) -> ! {
    while WAKERS_STOP.load(Ordering::Relaxed) == 0 {
        let cpu = current_cpu();
        WAKER_CPU[i as usize].store(cpu + 1, Ordering::Relaxed);
        if WORKER_ON.load(Ordering::Acquire) == cpu + 1 {
            WAKES_ON_WORKER.fetch_add(1, Ordering::Relaxed);
        }
        let end = now() + 200_000;
        while now() < end {
            core::hint::spin_loop();
        }
        let _ = futex_wait(&WAKER_WORD, 0, now() + ms(2));
    }
    thread_exit()
}

/// Pins itself to CPU `cpu` and spins there 300 ms, counting the turns it loses.
extern "C" fn worker(cpu: u64) -> ! {
    let Ok(page) = ClockPage::map() else {
        thread_exit()
    };
    if pin_current_thread(Some(cpu as u32)).is_ok() {
        WORKER_ON.store(cpu as u32 + 1, Ordering::Release);
        let start = page.now();
        let mut last = start;
        let (mut turns, mut lost) = (0, 0);
        while last - start < ms(300) {
            let t = page.now();
            if t - last >= 150_000 {
                turns += 1;
                lost += t - last;
            }
            last = t;
        }
        LOST_TURNS.store(turns, Ordering::Relaxed);
        LOST_NS.store(lost, Ordering::Relaxed);
        WORKER_ON.store(0, Ordering::Release);
        let _ = pin_current_thread(None);
    }
    MEASURED.store(1, Ordering::Release);
    thread_exit()
}

/// A worker pinned to a core where an unpinned waker first ran: with a core free of
/// pinned threads, the wakers move there and the worker keeps its core.
fn placement(out: &Out, cpus: u32) -> Result<(), i64> {
    if cpus == 1 {
        println!(
            out,
            "DS-A placement: ok: one CPU, which a worker shares with every thread"
        );
        return Ok(());
    }
    // This thread stays unpinned, so a core is free of pinned threads.
    pin_current_thread(None).map_err(|e| fail(out, "placement", "unpinning", e))?;
    let mut wakers = Vec::new();
    for i in 0..3 {
        wakers.push(start(out, "placement", waker, i)?);
    }
    let _ = futex_wait(&READY, 1, now() + ms(20));
    // The worker's core: one a waker first ran on, other than the boot processor's.
    let core = WAKER_CPU
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .find(|c| *c > 1)
        .map_or(cpus - 1, |c| c - 1);
    let first = WAKER_CPU
        .iter()
        .filter(|c| c.load(Ordering::Relaxed) == core + 1)
        .count();
    let w = start(out, "placement", worker, u64::from(core))?;
    let measured = until(&MEASURED, 10_000);
    WAKERS_STOP.store(1, Ordering::Relaxed);
    join(out, "placement", &w)?;
    for t in &wakers {
        join(out, "placement", t)?;
    }
    let (turns, lost) = (
        LOST_TURNS.load(Ordering::Relaxed),
        LOST_NS.load(Ordering::Relaxed),
    );
    let on = WAKES_ON_WORKER.load(Ordering::Relaxed);
    if !measured || turns > 10 || on > 3 {
        println!(
            out,
            "DS-A placement: FAIL: a worker pinned to CPU {} lost {} turns ({} ns) in 300 ms, and the wakers woke on its core {} times ({} had first run there)",
            core,
            turns,
            lost,
            on,
            first
        );
        return Err(1);
    }
    println!(
        out,
        "DS-A placement: ok: a worker pinned to CPU {}, where {} of 3 unpinned wakers first ran, lost {} turns ({} ns) in 300 ms to other threads, and the wakers woke on its core {} times",
        core,
        first,
        turns,
        lost,
        on
    );
    Ok(())
}
