//! Stream B1 (`architecture/data-os-streams.md`; `kernel-is-the-machine.md` §4.1): the
//! personality's four mechanisms in the core, each shown with no personality present.
//!
//! - **Interruption (§4.1 d).** A thread asleep on a futex is interrupted: its wait returns
//!   `Interrupted`; its registers read while it sleeps, and written back, let it resume.
//!   An interrupt raised while a thread runs waits for its next wait, which returns at once.
//! - **Snapshots and clones (§4.1 b).** A snapshot reads its source until it writes a page,
//!   then keeps its own. A process cloned from another shares its zero pages and its image
//!   until either writes them: writes on each side, by the kernel's copies and by the source
//!   in user mode, stay on that side, each copy charged to the writer's account; a shared
//!   object stays shared.
//! - **The pager (§4.1 c).** A thread touching a pager's page waits while the pager is asked;
//!   the pager supplies it and the thread reads it. A second process touches another page,
//!   which the pager refuses, and faults.
//! - **The entry (§4.1 a).** A process with a personality region runs a program of a few
//!   instructions: its system call enters the personality, which answers it and returns
//!   through the entry; a native call number enters too; the program's FS base is its own;
//!   a read of the region faults into the personality, as does an invalid opcode; a thread
//!   spinning in the program is interrupted into its personality.
//!
//! Slots: `console`, `self-image` (this program, for its children), and `supervisor`, on
//! which the supervisor sends a process resource. The children are this program too, each
//! in the role its name gives.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{
    Account, Channel, Console, Error, Handle, Memory, PAGE_SIZE, Process, Rights, Signals, Thread,
    futex_wait, map, now,
    personality::{
        self, Frame, Register, pager_request, personality_return, personality_set, thread_interrupt,
    },
    println, start, thread_exit, wait_one,
};

moruna_sys::main!(run);

const PAGE: u64 = PAGE_SIZE;
/// Where the kernel places what it maps: the personality's region starts here.
const MAP_BASE: u64 = 0x1000_0000_0000;
const REGION_LEN: u64 = 32 << 40;
/// The program's code and stack, below the region.
const CODE: u64 = 0x0800_0000_0000;
const STACK: u64 = CODE + 0x10_0000;
const STACK_LEN: u64 = 4 * PAGE;
/// A zero area of the clone test's source.
const ZERO: u64 = 0x0900_0000_0000;
/// A shared object's mapping in the clone test's source.
const SHARED: u64 = 0x0900_0010_0000;

/// The program: a call (`rax` 1), a native number (`abi_version`), its FS base read, a read
/// of `rbx` (in the region), an invalid opcode, and a loop.
const PROGRAM: [u8; 24] = [
    0x0f, 0x05, // 0: syscall
    0x48, 0x89, 0xc7, // 2: mov rdi, rax
    0xb8, 0x00, 0x00, 0x01, 0x00, // 5: mov eax, 0x10000 (abi_version)
    0x0f, 0x05, // 10: syscall
    0xf3, 0x48, 0x0f, 0xae, 0xc6, // 12: rdfsbase rsi
    0x48, 0x8b, 0x03, // 17: mov rax, [rbx]
    0x0f, 0x0b, // 20: ud2
    0xeb, 0xfe, // 22: jmp 22
];
const AFTER_FIRST: u64 = CODE + 2;
const AFTER_SECOND: u64 = CODE + 12;
const READ_REGION: u64 = CODE + 17;
const UD2: u64 = CODE + 20;
const LOOP: u64 = CODE + 22;
const PROGRAM_FS: u64 = 0x1234_5000;

/// The image page the clone test writes on each side.
static MARK: AtomicU64 = AtomicU64::new(0x1111);

type Outcome = Result<(), (&'static str, Option<Error>)>;

fn want(ok: bool, what: &'static str) -> Outcome {
    if ok { Ok(()) } else { Err((what, None)) }
}

fn call<T>(r: Result<T, Error>, what: &'static str) -> Result<T, (&'static str, Option<Error>)> {
    r.map_err(|e| (what, Some(e)))
}

fn report(out: &Console, test: &str, outcome: Outcome, detail: &str) -> i64 {
    match outcome {
        Ok(()) => {
            println!(out, "B1 {}: ok: {}", test, detail);
            0
        }
        Err((what, e)) => {
            match e {
                Some(e) => println!(out, "B1 {}: FAIL: {}: {}", test, what, e.name()),
                None => println!(out, "B1 {}: FAIL: {}", test, what),
            }
            1
        }
    }
}

fn run() -> i64 {
    let out = console();
    match start::process_name() {
        "b1-entry" => {
            let r = entry_child();
            report(
                &out,
                "entry",
                r,
                "a call entered the personality and returned through the entry, a native number entered too, the FS base was the program's, a read of the region and an invalid opcode faulted into it, and a spinning program was interrupted into it",
            )
        }
        "b1-cow" => {
            // The kernel wrote MARK before the clone; this write faults on the page it now
            // shares with the clone, which keeps the old value.
            let before = MARK.load(Ordering::Acquire);
            MARK.store(0x5555, Ordering::Release);
            let r = want(
                before == 0x2222 && MARK.load(Ordering::Acquire) == 0x5555,
                "its image page did not take its write after the clone",
            );
            report(
                &out,
                "clone source",
                r,
                "wrote its image page after the clone (copy on write in user mode)",
            )
        }
        "b1-victim" => {
            let pager = Memory(slot("pager"));
            let m = match pager.map(0, 2 * PAGE, 0) {
                Ok(m) => m,
                Err(e) => return report(&out, "pager victim", Err(("mapping", Some(e))), ""),
            };
            let mut b = [0u8; 4];
            // The pager refuses this page: the read faults and ends this process.
            let r = m.read(PAGE as usize, &mut b);
            println!(
                out,
                "B1 pager victim: FAIL: read a refused page: {:?} {:?}", r, b
            );
            1
        }
        _ => tester(&out),
    }
}

fn tester(out: &Console) -> i64 {
    let mut code = 0;
    code |= report(
        out,
        "interrupt",
        interrupt(),
        "a thread asleep on a futex was interrupted and its wait returned interrupted; its registers read while it slept; an interrupt raised before a wait made it return at once",
    );
    code |= report(
        out,
        "snapshot",
        snapshot(),
        "a snapshot read its source, kept its own written page, and saw the source's page it had not written",
    );
    let resource = match take_resource() {
        Ok(r) => r,
        Err(e) => return report(out, "personality", Err(e), ""),
    };
    let image = slot("self-image");
    code |= report(
        out,
        "clone",
        clone(out, &resource, &image),
        "a clone shared zero and image pages until each side wrote them, the copies charged to the writer, and kept a shared object shared",
    );
    code |= report(
        out,
        "pager",
        pager(out, &resource, &image),
        "a thread waited for a page the pager supplied and read it; a page the pager refused faulted another process",
    );
    code |= report(
        out,
        "entry run",
        entry(out, &resource, &image),
        "the entry child ended",
    );
    if code == 0 {
        println!(out, "B1 personality: ok: the four mechanisms answered");
    }
    code
}

fn take_resource() -> Result<Handle, (&'static str, Option<Error>)> {
    let supervisor = Channel(slot("supervisor"));
    call(
        wait_one(&supervisor.0, Signals::READABLE, now() + ms(60_000)),
        "waiting for the process resource",
    )?;
    let mut buf = [0u8; 16];
    let mut handles = [None];
    call(
        supervisor.read(&mut buf, &mut handles),
        "reading the process resource",
    )?;
    handles[0]
        .take()
        .ok_or(("the supervisor sent no process resource", None))
}

/// A thread on a stack of its own, mapped where the kernel chooses.
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
    call(Thread::spawn(entry, top, arg), "thread_create")
}

fn sleep(ns: u64) {
    let word = AtomicU32::new(0);
    let _ = futex_wait(&word, 0, now() + ns);
}

// ---- Interruption.

static SLEPT: AtomicU64 = AtomicU64::new(0);
static GO: AtomicU32 = AtomicU32::new(0);

extern "C" fn sleeper(_: u64) -> ! {
    let word = AtomicU32::new(0);
    let r = futex_wait(&word, 0, now() + ms(10_000));
    SLEPT.store(
        match r {
            Err(Error::Interrupted) => 1,
            _ => 2,
        },
        Ordering::Release,
    );
    thread_exit()
}

extern "C" fn late_sleeper(_: u64) -> ! {
    while GO.load(Ordering::Acquire) == 0 {
        core::hint::spin_loop();
    }
    let word = AtomicU32::new(0);
    let start = now();
    let r = futex_wait(&word, 0, now() + ms(10_000));
    let quick = now() - start < ms(5_000);
    SLEPT.store(
        match r {
            Err(Error::Interrupted) if quick => 3,
            _ => 4,
        },
        Ordering::Release,
    );
    thread_exit()
}

fn interrupt() -> Outcome {
    let t = spawn(sleeper, 0)?;
    // Asleep: its registers can be read, and written back unchanged.
    let mut frame = [0u8; personality::FRAME_LEN];
    let mut read = Err(Error::BadState);
    for _ in 0..200 {
        read = personality::thread_state_read(&t, &mut frame);
        if read.is_ok() {
            break;
        }
        sleep(ms(5));
    }
    call(read, "thread_state_read of a sleeping thread")?;
    let f = Frame(frame);
    want(
        f.rip() != 0 && f.register(Register::Rsp) != 0,
        "a sleeping thread's registers read zero",
    )?;
    call(
        personality::thread_state_write(&t, &f.0),
        "thread_state_write of a sleeping thread",
    )?;
    call(thread_interrupt(&t), "thread_interrupt")?;
    call(
        wait_one(&t.0, Signals::TERMINATED, now() + ms(5_000)),
        "the interrupted thread did not end",
    )?;
    want(
        SLEPT.load(Ordering::Acquire) == 1,
        "the interrupted wait did not return interrupted",
    )?;
    match personality::thread_state_read(&t, &mut frame) {
        Err(Error::BadState) => {}
        _ => return Err(("an ended thread's registers were read", None)),
    }
    // Raised before the wait: the wait returns at once.
    let late = spawn(late_sleeper, 0)?;
    call(
        thread_interrupt(&late),
        "thread_interrupt of a running thread",
    )?;
    GO.store(1, Ordering::Release);
    call(
        wait_one(&late.0, Signals::TERMINATED, now() + ms(8_000)),
        "the late thread did not end",
    )?;
    want(
        SLEPT.load(Ordering::Acquire) == 3,
        "a pending interrupt did not stop the next wait at once",
    )
}

// ---- Snapshots.

fn snapshot() -> Outcome {
    let source = call(Memory::create(2 * PAGE), "memory_create")?;
    let sm = call(source.map(0, 2 * PAGE, map::WRITE), "mapping the source")?;
    call(sm.write(0, b"OOOO"), "writing the source")?;
    let snap = call(
        personality::memory_snapshot(&source, 0, 2 * PAGE, None),
        "memory_snapshot",
    )?;
    let tm = call(snap.map(0, 2 * PAGE, map::WRITE), "mapping the snapshot")?;
    let mut b = [0u8; 4];
    call(tm.read(0, &mut b), "reading the snapshot")?;
    want(&b == b"OOOO", "the snapshot did not read its source")?;
    call(tm.write(0, b"TTTT"), "writing the snapshot")?;
    call(sm.read(0, &mut b), "reading the source")?;
    want(&b == b"OOOO", "the snapshot's write reached its source")?;
    call(tm.read(0, &mut b), "reading the snapshot again")?;
    want(&b == b"TTTT", "the snapshot lost its write")?;
    call(
        sm.write(PAGE as usize, b"PPPP"),
        "writing the source's second page",
    )?;
    call(
        tm.read(PAGE as usize, &mut b),
        "reading the snapshot's second page",
    )?;
    want(
        &b == b"PPPP",
        "a page the snapshot had not written did not show its source",
    )
}

// ---- Clones.

fn read_u64(p: &Process, addr: u64) -> Result<u64, (&'static str, Option<Error>)> {
    let mut b = [0u8; 8];
    call(p.read(addr, &mut b), "process_read")?;
    Ok(u64::from_le_bytes(b))
}

fn clone(out: &Console, resource: &Handle, image: &Handle) -> Outcome {
    let source = call(Process::create(resource, "b1-cow"), "process_create")?;
    let entry = call(source.map_image(image), "process_map_image")?;
    call(
        source.map(None, 0, 2 * PAGE, ZERO, map::READ | map::WRITE),
        "mapping zero pages",
    )?;
    let shared = call(Memory::create(PAGE), "the shared object")?;
    call(
        source.map(Some(&shared.0), 0, PAGE, SHARED, map::READ | map::WRITE),
        "mapping the shared object",
    )?;
    let mark = &MARK as *const AtomicU64 as u64;
    call(source.write(ZERO, b"AAAAAAAA"), "writing a zero page")?;
    call(
        source.write(mark, &0x2222u64.to_le_bytes()),
        "writing the image page",
    )?;

    let account = call(Account::carve(16 << 20, "b1-clone"), "account_create")?;
    let child = call(
        personality::process_clone(resource, &source, Some(&account.0)),
        "process_clone",
    )?;
    let after_clone = call(account.usage(), "account_info")?.used;
    want(
        read_u64(&child, ZERO)? == u64::from_le_bytes(*b"AAAAAAAA"),
        "the clone did not read the source's zero page",
    )?;
    want(
        read_u64(&child, mark)? == 0x2222,
        "the clone did not read the source's image page",
    )?;
    want(
        call(account.usage(), "account_info")?.used == after_clone,
        "the clone's reads were charged: it copied pages it shares",
    )?;
    // Each side's write stays on its side.
    call(source.write(ZERO, b"BBBBBBBB"), "the source's write")?;
    want(
        read_u64(&child, ZERO)? == u64::from_le_bytes(*b"AAAAAAAA"),
        "the source's write reached the clone",
    )?;
    call(child.write(ZERO + 8, b"CCCCCCCC"), "the clone's write")?;
    want(
        read_u64(&source, ZERO + 8)? == 0,
        "the clone's write reached the source",
    )?;
    want(
        read_u64(&child, ZERO + 8)? == u64::from_le_bytes(*b"CCCCCCCC"),
        "the clone lost its write",
    )?;
    let after_write = call(account.usage(), "account_info")?.used;
    want(
        after_write >= after_clone + PAGE,
        "the clone's copy was not charged to its account",
    )?;
    // A page neither had touched.
    call(
        child.write(ZERO + PAGE, b"D"),
        "the clone's write to an untouched page",
    )?;
    want(
        read_u64(&source, ZERO + PAGE)? == 0,
        "an untouched page's write reached the source",
    )?;
    // The shared object stays shared.
    call(
        source.write(SHARED, b"SSSSSSSS"),
        "writing the shared object",
    )?;
    want(
        read_u64(&child, SHARED)? == u64::from_le_bytes(*b"SSSSSSSS"),
        "a shared object's write did not reach the clone",
    )?;
    // The source runs and writes its image page in user mode.
    let console = call(console_for(out), "duplicating the console")?;
    call(
        source.start_with(entry, &mut [(Some(console), "console")]),
        "process_start",
    )?;
    call(
        wait_one(&source.0, Signals::TERMINATED, now() + ms(10_000)),
        "the clone's source did not end",
    )?;
    want(
        read_u64(&child, mark)? == 0x2222,
        "the source's write in user mode reached the clone",
    )
}

fn console_for(out: &Console) -> Result<Handle, Error> {
    out.0.duplicate(Rights::WRITE | Rights::TRANSFER)
}

// ---- The pager.

static PAGE_READ: AtomicU64 = AtomicU64::new(0);

extern "C" fn pager_reader(object: u64) -> ! {
    let memory = Memory(Handle::from_raw(object as u32));
    let mut b = [0u8; 4];
    let r = memory.map(0, PAGE, 0).and_then(|m| m.read(0, &mut b));
    core::mem::forget(memory);
    PAGE_READ.store(
        if r.is_ok() && &b == b"PAGE" { 1 } else { 2 },
        Ordering::Release,
    );
    thread_exit()
}

/// Reads the next request from the pager's channel: its key and offset.
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
    want(
        moruna_abi::read_u32(&buf, pager_request::KIND) == Some(pager_request::FAULT)
            && word(pager_request::LEN) == PAGE,
        "a request that is not one page's fault",
    )?;
    Ok((word(pager_request::KEY), word(pager_request::OFFSET)))
}

fn pager(out: &Console, resource: &Handle, image: &Handle) -> Outcome {
    let (kernel_end, port) = call(Channel::create(), "channel_create")?;
    let object = match personality::pager_create(kernel_end, 4 * PAGE, 0x51, None) {
        Ok(m) => m,
        Err((e, _)) => return Err(("pager_create", Some(e))),
    };
    let for_reader = call(object.0.duplicate(Rights::READ | Rights::MAP), "duplicate")?;
    let reader = spawn(pager_reader, for_reader.into_raw() as u64)?;
    let (key, offset) = request(&port)?;
    want(key == 0x51 && offset == 0, "the request named another page")?;
    let page = call(Memory::create(PAGE), "the supplied page")?;
    let pm = call(page.map(0, PAGE, map::WRITE), "mapping the supplied page")?;
    call(pm.write(0, b"PAGE"), "writing the supplied page")?;
    drop(pm);
    call(
        personality::pager_supply(&object, 0, PAGE, &page, 0),
        "pager_supply",
    )?;
    call(
        wait_one(&reader.0, Signals::TERMINATED, now() + ms(10_000)),
        "the reader did not end",
    )?;
    want(
        PAGE_READ.load(Ordering::Acquire) == 1,
        "the reader did not read the supplied page",
    )?;
    // Another process touches the second page, which the pager refuses.
    let victim = call(Process::create(resource, "b1-victim"), "process_create")?;
    let entry = call(victim.map_image(image), "process_map_image")?;
    let console = call(console_for(out), "duplicating the console")?;
    let theirs = call(
        object
            .0
            .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER),
        "duplicate",
    )?;
    call(
        victim.start_with(
            entry,
            &mut [(Some(console), "console"), (Some(theirs), "pager")],
        ),
        "process_start",
    )?;
    let (key, offset) = request(&port)?;
    want(
        key == 0x51 && offset == PAGE,
        "the victim's request named another page",
    )?;
    call(
        personality::pager_refuse(&object, PAGE, PAGE),
        "pager_refuse",
    )?;
    call(
        wait_one(&victim.0, Signals::TERMINATED, now() + ms(10_000)),
        "the victim did not end",
    )?;
    Ok(())
}

// ---- The entry.

fn entry(out: &Console, resource: &Handle, image: &Handle) -> Outcome {
    let child = call(Process::create(resource, "b1-entry"), "process_create")?;
    let entry = call(child.map_image(image), "process_map_image")?;
    let code = call(Memory::create(PAGE), "the program's code")?;
    let cm = call(code.map(0, PAGE, map::WRITE), "mapping the program's code")?;
    call(cm.write(0, &PROGRAM), "writing the program")?;
    drop(cm);
    call(
        child.map(Some(&code.0), 0, PAGE, CODE, map::READ | map::EXECUTE),
        "mapping the program",
    )?;
    call(
        child.map(None, 0, STACK_LEN, STACK, map::READ | map::WRITE),
        "mapping the program's stack",
    )?;
    call(
        personality_set(&child, MAP_BASE, REGION_LEN),
        "personality_set",
    )?;
    match personality_set(&child, MAP_BASE, REGION_LEN) {
        Err(Error::BadState) => {}
        _ => return Err(("a second personality_set was taken", None)),
    }
    let console = call(console_for(out), "duplicating the console")?;
    call(
        child.start_with(entry, &mut [(Some(console), "console")]),
        "process_start",
    )?;
    call(
        wait_one(&child.0, Signals::TERMINATED, now() + ms(20_000)),
        "the entry child did not end",
    )?;
    Ok(())
}

/// The program's first registers.
fn program_frame(rip: u64, stack: u64) -> Frame {
    let mut f = Frame::default();
    f.set_rip(rip);
    f.set_register(Register::Rsp, stack);
    f.set_rflags(0x202);
    f.set_fs_base(PROGRAM_FS);
    f
}

static SPUN: AtomicU64 = AtomicU64::new(0);
static SPINNING: AtomicU32 = AtomicU32::new(0);

extern "C" fn spinner(_: u64) -> ! {
    let mut f = program_frame(LOOP, STACK + STACK_LEN / 2);
    SPINNING.store(1, Ordering::Release);
    let r = personality_return(&mut f);
    let interrupted = moruna_abi::personality::reason::INTERRUPTED;
    let entered = r == Ok(interrupted) && f.reason() == interrupted && f.rip() == LOOP;
    SPUN.store(if entered { 1 } else { 2 }, Ordering::Release);
    thread_exit()
}

fn entry_child() -> Outcome {
    use moruna_abi::personality::reason;
    let mut f = program_frame(CODE, STACK + STACK_LEN);
    f.set_register(Register::Rax, 1);
    f.set_register(Register::Rdi, 21);
    // The program reads the frame's own address, inside the region.
    let inside = &f as *const Frame as u64;
    want(inside >= MAP_BASE, "the frame is not inside the region")?;
    f.set_register(Register::Rbx, inside);

    let r = call(personality_return(&mut f), "the first personality_return")?;
    want(
        r == reason::SYSCALL && f.reason() == reason::SYSCALL,
        "the program's call did not enter as a call",
    )?;
    want(
        f.register(Register::Rax) == 1 && f.register(Register::Rdi) == 21 && f.rip() == AFTER_FIRST,
        "the frame did not hold the program's call",
    )?;
    f.set_register(Register::Rax, 42);

    let r = call(personality_return(&mut f), "the second personality_return")?;
    want(r == reason::SYSCALL, "the native number did not enter")?;
    want(
        f.register(Register::Rax) == 0x1_0000 && f.register(Register::Rdi) == 42,
        "the answer did not reach the program, or the native number reached the kernel",
    )?;
    want(f.rip() == AFTER_SECOND, "the second call's frame is wrong")?;
    f.set_register(Register::Rax, 0);

    let r = call(personality_return(&mut f), "the third personality_return")?;
    want(
        r == reason::PAGE_FAULT,
        "a read of the region did not fault into the personality",
    )?;
    want(
        f.fault_address() == inside && f.vector() == 14 && f.rip() == READ_REGION,
        "the region's fault frame is wrong",
    )?;
    want(
        f.register(Register::Rsi) == PROGRAM_FS && f.fs_base() == PROGRAM_FS,
        "the program's FS base was not its own",
    )?;
    f.set_rip(UD2);

    let r = call(personality_return(&mut f), "the fourth personality_return")?;
    want(
        r == reason::EXCEPTION && f.vector() == 6 && f.rip() == UD2,
        "an invalid opcode did not enter as an exception",
    )?;

    // A thread spinning in its program is interrupted into its personality.
    let t = spawn(spinner, 0)?;
    while SPINNING.load(Ordering::Acquire) == 0 {
        sleep(ms(1));
    }
    sleep(ms(20));
    call(thread_interrupt(&t), "interrupting the spinning program")?;
    call(
        wait_one(&t.0, Signals::TERMINATED, now() + ms(10_000)),
        "the spinning thread did not end",
    )?;
    want(
        SPUN.load(Ordering::Acquire) == 1,
        "the spinning program was not interrupted into its personality",
    )
}
