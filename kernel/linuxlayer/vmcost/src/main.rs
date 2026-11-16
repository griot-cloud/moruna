// SPDX-License-Identifier: MPL-2.0

//! Stream B2-vm: what a page costs the personality, measured under KVM (`cost.sh`). Not a
//! test: one boot, its figures printed as `B2VM cost: ...`.
//!
//! - **The calls alone:** `process_map` of a page a call, of 16 a call and of 4096 in one;
//!   `process_protect` and `process_unmap` a page a call and all in one.
//! - **The fault path:** a program touches 4096 pages; the personality maps a page on each
//!   entry, or the 16-page window around it (fault-around), or nothing (all mapped before,
//!   so only the core's own faults remain).
//! - **The pager:** a thread touches a pager object's pages; the pager supplies a page or 16
//!   a request.
//! - **Fork:** `process_clone` of a process holding 4096 pages, against making a process and
//!   mapping them again.
//!
//! The tester holds the process resource (slot `process`) and this program's image (slot
//! `self-image`); its child, this program as `b2vm-child`, runs with a personality region and
//! makes the calls on itself, as the personality does.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering};

use moruna_sys::{
    Channel, Console, Error, Handle, Memory, PAGE_SIZE, Process, Rights, Signals, Thread, map, now,
    personality::{self, Frame, Register, pager_request, personality_return, personality_set},
    println, start, thread_exit, wait_one,
};

moruna_sys::main!(run);

const PAGE: u64 = PAGE_SIZE;
const MAP_BASE: u64 = 0x1000_0000_0000;
const REGION_LEN: u64 = 32 << 40;
const CODE: u64 = 0x0800_0000_0000;
const STACK: u64 = CODE + 0x10_0000;
const STACK_LEN: u64 = 4 * PAGE;
const PROGRAM_FS: u64 = 0x1234_5000;

const fn ms(n: u64) -> u64 {
    n * 1_000_000
}

fn slot(name: &str) -> Handle {
    match start::take(name) {
        Some(h) => h,
        None => moruna_sys::exit(90),
    }
}

type Outcome = Result<(), (&'static str, Option<Error>)>;

fn want(ok: bool, what: &'static str) -> Outcome {
    if ok { Ok(()) } else { Err((what, None)) }
}

fn call<T>(r: Result<T, Error>, what: &'static str) -> Result<T, (&'static str, Option<Error>)> {
    r.map_err(|e| (what, Some(e)))
}

fn report(out: &Console, test: &str, outcome: Outcome) -> i64 {
    match outcome {
        Ok(()) => {
            println!(out, "B2VM cost: {}: done", test);
            0
        }
        Err((what, Some(e))) => {
            println!(out, "B2VM cost: {}: FAIL: {}: {}", test, what, e.name());
            1
        }
        Err((what, None)) => {
            println!(out, "B2VM cost: {}: FAIL: {}", test, what);
            1
        }
    }
}

fn run() -> i64 {
    let out = Console(slot("console"));
    if start::process_name() == "b2vm-child" {
        return report(&out, "child", cost_child(&out));
    }
    let resource = slot("process");
    let image = slot("self-image");
    report(&out, "tester", b2vm_cost(&out, &resource, &image))
}

fn console_for(out: &Console) -> Result<Handle, Error> {
    out.0.duplicate(Rights::WRITE | Rights::TRANSFER)
}

fn program_frame(rip: u64, stack: u64) -> Frame {
    let mut f = Frame::default();
    f.set_rip(rip);
    f.set_register(Register::Rsp, stack);
    f.set_rflags(0x202);
    f.set_fs_base(PROGRAM_FS);
    f
}

fn spawn(
    entry: extern "C" fn(u64) -> !,
    arg: u64,
) -> Result<Thread, (&'static str, Option<Error>)> {
    let stack = call(Memory::create(64 * 1024), "a thread's stack")?;
    let m = call(
        stack.map_detached(0, 64 * 1024, map::WRITE),
        "mapping a thread's stack",
    )?;
    let top = m.end();
    core::mem::forget(m);
    core::mem::forget(stack);
    call(Thread::spawn(entry, top, arg), "thread_create")
}

fn request(port: &Channel) -> Result<(u64, u64), (&'static str, Option<Error>)> {
    call(
        wait_one(&port.0, Signals::READABLE, now() + ms(10_000)),
        "waiting for the pager's request",
    )?;
    let mut buf = [0u8; pager_request::SIZE];
    let mut none: [Option<Handle>; 0] = [];
    let (n, _) = call(port.read(&mut buf, &mut none), "reading the request")?;
    want(n == pager_request::SIZE, "a request of the wrong size")?;
    let word = |at| moruna_abi::read_u64(&buf, at).unwrap_or(u64::MAX);
    Ok((word(pager_request::KEY), word(pager_request::OFFSET)))
}

const COST_PAGES: u64 = 4096;
const COST_LEN: u64 = COST_PAGES * PAGE;
/// The program's data, below the region.
const COST_DATA: u64 = 0x0b00_0000_0000;
const AROUND: u64 = 16;

/// Touches `rcx` pages from `rdi`, a byte each, then calls.
const TOUCH: [u8; 19] = [
    0xc6, 0x07, 0x01, // 0: mov byte ptr [rdi], 1
    0x48, 0x81, 0xc7, 0x00, 0x10, 0x00, 0x00, // 3: add rdi, 0x1000
    0x48, 0xff, 0xc9, // 10: dec rcx
    0x75, 0xf1, // 13: jnz 0
    0x0f, 0x05, // 15: syscall
    0xeb, 0xfe, // 17: jmp 17
];

fn tsc() -> u64 {
    // SAFETY: reads the time-stamp counter; touches no memory.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Times `f`: nanoseconds and cycles.
fn timed(f: impl FnOnce() -> Outcome) -> Result<(u64, u64), (&'static str, Option<Error>)> {
    let (t, c) = (now(), tsc());
    f()?;
    Ok((now() - t, tsc() - c))
}

fn print_cost(out: &Console, what: &str, (ns, cycles): (u64, u64), per: u64, unit: &str) {
    println!(
        out,
        "B2VM cost: {}: {} ns ({} cycles) per {}, {} ns in all",
        what,
        ns / per,
        cycles / per,
        unit,
        ns
    );
}

const RW: u64 = map::READ | map::WRITE;
const RWF: u64 = RW | map::FIXED;

/// The child: on itself, as the personality's calls are.
fn cost_child(out: &Console) -> Outcome {
    let me = Process(slot("self"));
    let code = call(Memory::create(PAGE), "the touch program")?;
    let cm = call(code.map(0, PAGE, map::WRITE), "mapping the touch program")?;
    call(cm.write(0, &TOUCH), "writing the touch program")?;
    drop(cm);
    call(
        me.map(Some(&code.0), 0, PAGE, CODE, map::READ | map::EXECUTE | map::FIXED),
        "mapping the touch program in",
    )?;
    let chunk = call(Memory::create(COST_LEN), "a chunk")?;

    // The calls alone, no fault: a page a call, a run of 16 a call, all in one call.
    for (what, run) in [("map 1 page a call", 1), ("map 16 pages a call", AROUND), ("map 4096 pages in one call", COST_PAGES)] {
        let t = timed(|| {
            let mut at = 0;
            while at < COST_LEN {
                call(
                    me.map(Some(&chunk.0), at, run * PAGE, COST_DATA + at, RWF),
                    "process_map",
                )?;
                at += run * PAGE;
            }
            Ok(())
        })?;
        print_cost(out, what, t, COST_PAGES, "page");
        print_cost(out, what, t, COST_PAGES / run, "call");
        if run == 1 {
            let t = timed(|| {
                for i in 0..COST_PAGES {
                    call(me.protect(COST_DATA + i * PAGE, PAGE, map::READ), "process_protect")?;
                }
                Ok(())
            })?;
            print_cost(out, "protect 1 page a call", t, COST_PAGES, "call");
            let t = timed(|| call(me.protect(COST_DATA, COST_LEN, RW), "process_protect"))?;
            print_cost(out, "protect 4096 one-page areas in one call", t, COST_PAGES, "page");
            let t = timed(|| {
                for i in 0..COST_PAGES {
                    call(me.unmap(COST_DATA + i * PAGE, PAGE), "process_unmap")?;
                }
                Ok(())
            })?;
            print_cost(out, "unmap 1 page a call", t, COST_PAGES, "call");
        } else {
            let t = timed(|| call(me.unmap(COST_DATA, COST_LEN), "process_unmap"))?;
            print_cost(out, what, t, COST_PAGES, "page unmapped in one call, after");
        }
    }

    // The fault path: the program touches each page; the personality maps on each entry.
    for (what, around) in [("fault, map 1 page an entry", 1), ("fault-around, map 16 pages an entry", AROUND), ("premapped, the core's own faults only", 0)] {
        call(me.unmap(COST_DATA, COST_LEN), "clearing the data")?;
        if around == 0 {
            call(me.map(Some(&chunk.0), 0, COST_LEN, COST_DATA, RWF), "premapping")?;
        }
        let mut f = program_frame(CODE, STACK + STACK_LEN);
        f.set_register(Register::Rdi, COST_DATA);
        f.set_register(Register::Rcx, COST_PAGES);
        let mut entries = 0u64;
        let t = timed(|| {
            loop {
                let r = call(personality_return(&mut f), "personality_return")?;
                match r {
                    moruna_abi::personality::reason::PAGE_FAULT if around != 0 => {
                        let page = f.fault_address() & !(PAGE - 1);
                        let window = around * PAGE;
                        let start = (page & !(window - 1)).max(COST_DATA);
                        let end = (start + window).min(COST_DATA + COST_LEN);
                        call(
                            me.map(Some(&chunk.0), start - COST_DATA, end - start, start, RWF),
                            "process_map at a fault",
                        )?;
                        entries += 1;
                    }
                    moruna_abi::personality::reason::SYSCALL => return Ok(()),
                    _ => return Err(("an entry the touch program should not make", None)),
                }
            }
        })?;
        print_cost(out, what, t, COST_PAGES, "page");
        println!(out, "B2VM cost: {}: {} entries", what, entries);
    }
    Ok(())
}

static TOUCHED: AtomicU64 = AtomicU64::new(0);

extern "C" fn pager_toucher(object: u64) -> ! {
    let memory = Memory(Handle::from_raw(object as u32));
    let ok = memory.map(0, COST_LEN, 0).is_ok_and(|m| {
        let mut b = [0u8; 1];
        (0..COST_PAGES).all(|i| m.read((i * PAGE) as usize, &mut b).is_ok())
    });
    core::mem::forget(memory);
    TOUCHED.store(if ok { 1 } else { 2 }, Ordering::Release);
    thread_exit()
}

/// A pager's round trip a page: a thread touches each page of a pager's object, and this
/// thread supplies it, a page or a run of 16 a request.
fn pager_cost(out: &Console, around: u64) -> Outcome {
    let (kernel_end, port) = call(Channel::create(), "channel_create")?;
    let object = match personality::pager_create(kernel_end, COST_LEN, 0x77, None) {
        Ok(m) => m,
        Err((e, _)) => return Err(("pager_create", Some(e))),
    };
    let source = call(Memory::create(COST_LEN), "the pages supplied")?;
    let theirs = call(object.0.duplicate(Rights::READ | Rights::MAP), "duplicate")?;
    TOUCHED.store(0, Ordering::Release);
    let mut supplied = 0u64;
    let t = timed(|| {
        let toucher = spawn(pager_toucher, theirs.into_raw() as u64)?;
        while supplied < COST_PAGES {
            let (_, offset) = request(&port)?;
            let window = around * PAGE;
            let start = offset & !(window - 1);
            call(
                personality::pager_supply(&object, start, window, &source, start),
                "pager_supply",
            )?;
            supplied += around;
        }
        call(
            wait_one(&toucher.0, Signals::TERMINATED, now() + ms(60_000)),
            "the toucher did not end",
        )?;
        Ok(())
    })?;
    want(TOUCHED.load(Ordering::Acquire) == 1, "the toucher did not read every page")?;
    let what = if around == 1 {
        "pager, supply 1 page a request"
    } else {
        "pager, supply 16 pages a request"
    };
    print_cost(out, what, t, COST_PAGES, "page");
    Ok(())
}

/// A fork's address space: `process_clone` of a process holding 4096 pages, against making
/// a process and mapping them again, a page or a run of 16 a call.
fn fork_cost(out: &Console, resource: &Handle) -> Outcome {
    let chunk = call(Memory::create(COST_LEN), "a chunk")?;
    for (what, run) in [("one-page areas", 1), ("16-page areas", AROUND)] {
        let source = call(Process::create(resource, "b2vm-src"), "process_create")?;
        let mut at = 0;
        while at < COST_LEN {
            call(
                source.map(Some(&chunk.0), at, run * PAGE, COST_DATA + at, RWF),
                "mapping the source",
            )?;
            at += run * PAGE;
        }
        let mut child = None;
        let t = timed(|| {
            child = Some(call(
                personality::process_clone(resource, &source, None),
                "process_clone",
            )?);
            Ok(())
        })?;
        drop(child);
        println!(
            out,
            "B2VM cost: process_clone of 4096 pages in {}: {} ns ({} cycles) in all",
            what, t.0, t.1
        );
        let t = timed(|| {
            let fresh = call(Process::create(resource, "b2vm-copy"), "process_create")?;
            let mut at = 0;
            while at < COST_LEN {
                call(
                    fresh.map(Some(&chunk.0), at, run * PAGE, COST_DATA + at, RWF),
                    "replaying",
                )?;
                at += run * PAGE;
            }
            Ok(())
        })?;
        println!(
            out,
            "B2VM cost: process_create and replay of 4096 pages in {}: {} ns ({} cycles) in all",
            what, t.0, t.1
        );
    }
    Ok(())
}

fn b2vm_cost(out: &Console, resource: &Handle, image: &Handle) -> Outcome {
    fork_cost(out, resource)?;
    pager_cost(out, 1)?;
    pager_cost(out, AROUND)?;
    let child = call(Process::create(resource, "b2vm-child"), "process_create")?;
    let entry = call(child.map_image(image), "process_map_image")?;
    call(
        child.map(None, 0, STACK_LEN, STACK, map::READ | map::WRITE),
        "mapping the program's stack",
    )?;
    call(personality_set(&child, MAP_BASE, REGION_LEN), "personality_set")?;
    let console = call(console_for(out), "duplicating the console")?;
    let me = call(
        child
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER),
        "duplicating the child's own handle",
    )?;
    call(
        child.start_with(entry, &mut [(Some(console), "console"), (Some(me), "self")]),
        "process_start",
    )?;
    call(
        wait_one(&child.0, Signals::TERMINATED, now() + ms(120_000)),
        "the cost child did not end",
    )?;
    Ok(())
}
