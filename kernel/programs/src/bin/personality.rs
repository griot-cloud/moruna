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
//!   object stays shared. A snapshot mapped in two places of one process is one object in
//!   its clone too, and no later write through either place reaches the clone; a second
//!   process cannot map it. A clone needs no room for the zero pages it shares.
//! - **The pager (§4.1 c).** A thread touching a pager's page waits while the pager is asked;
//!   the pager supplies it and the thread reads it. A second process touches another page,
//!   which the pager refuses, and faults. `process_read` and `process_write` wait for the
//!   pager too, and answer `Fault` on a refusal.
//! - **The entry (§4.1 a).** A process with a personality region runs a program of a few
//!   instructions: its system call enters the personality, which answers it and returns
//!   through the entry; a native call number enters too; the program's FS base is its own;
//!   a read of the region faults into the personality, as does an invalid opcode; a thread
//!   spinning in the program is interrupted into its personality. The personality's
//!   fault-tolerant copy resumes at its recovery address on a fault, with the address.
//! - **Shared futexes.** A thread of another process wakes a waiter on a word of a shared
//!   object, mapped at another address.
//! - **A shared handle table.** A personality process's clone with `SHARE_HANDLES` uses its
//!   table: a handle made in either works in the other, and a handle closed in one is closed
//!   for both; a clone without it starts with no handles; a caller with no personality, or
//!   not holding the table, is refused. Killing a task of four processes holding each
//!   other's handles frees them all, and its account returns to its starting charge.
//! - **Page tables go.** When exactly one 2 MiB region empties, its 4 KiB table goes; when a
//!   whole 512 GiB region empties, its tables go to the root.
//!
//! Slots: `console`, `self-image` (this program, for its children), and `supervisor`, on
//! which the supervisor sends a process resource. The children are this program too, each
//! in the role its name gives.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{
    Account, Channel, Console, Error, Handle, Memory, PAGE_SIZE, Process, Rights, Signals, Thread,
    futex_wait, futex_wake, map, now,
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

// The personality's fault-tolerant copy, as B2 builds it: one byte from `rdi` into `rax`,
// `rdx` zero; a fault at the load resumes at `b1_copy_fault` with the address in `rdx`.
core::arch::global_asm!(
    ".global b1_copy_byte",
    ".global b1_copy_end",
    ".global b1_copy_fault",
    "b1_copy_byte:",
    "    xor edx, edx",
    "    movzx eax, byte ptr [rdi]",
    "b1_copy_end:",
    "    ret",
    "b1_copy_fault:",
    "    mov rax, -1",
    "    ret",
);

/// What the copy returns, in `rax` and `rdx`.
#[repr(C)]
struct Copied {
    value: u64,
    fault: u64,
}

unsafe extern "C" {
    fn b1_copy_byte(address: u64) -> Copied;
    fn b1_copy_end();
    fn b1_copy_fault();
}

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
        "b1-copy" => {
            let r = copy_child();
            report(
                &out,
                "copy",
                r,
                "the copy routine read a byte, and resumed at its recovery address with the address after a page fault and a general protection fault",
            )
        }
        "b1-futex" => {
            let r = futex_child();
            report(&out, "futex waker", r, "woke a word of the shared object")
        }
        "b1-tables" => {
            let r = tables_child();
            report(
                &out,
                "tables",
                r,
                "the table of an emptied 2 MiB region went, and those of an emptied 512 GiB region",
            )
        }
        "b1-task" => task_child(&out),
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
        "a thread waited for a page the pager supplied and read it; a page the pager refused faulted another process; process_read and process_write waited for the pager, and a refused page answered fault",
    );
    code |= report(
        out,
        "entry run",
        entry(out, &resource, &image),
        "the entry child ended",
    );
    code |= report(
        out,
        "copy run",
        copy_run(out, &resource, &image),
        "the copy child ended",
    );
    code |= report(
        out,
        "places",
        places(&resource),
        "a snapshot mapped twice in one process was one object in its clone too, no later write through either place reached the clone, and a second process could not map it",
    );
    code |= report(
        out,
        "room",
        room(&resource),
        "a clone of a process whose account had no room for its zero pages again was made, and its account's peak rose by less than a quarter of them",
    );
    code |= report(
        out,
        "tables run",
        tables(out, &resource, &image),
        "the tables child ended",
    );
    code |= report(
        out,
        "futex",
        futex(out, &resource, &image),
        "a waiter on a shared object's word was woken by another process",
    );
    // Last: the task takes the process resource with it.
    code |= report(
        out,
        "task",
        task(out, resource, &image),
        "a clone sharing a table was refused to a caller with no personality, and a task of four processes holding each other's handles, killed, freed them all and gave its account back its starting charge",
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
        personality::process_clone(resource, Some(&source), Some(&account.0), 0),
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
    let object = match personality::pager_create(kernel_end, 8 * PAGE, 0x51, None) {
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
    pager_copies(resource, &object, &port)
}

/// Where a third process maps the pager's object, for the kernel's copies.
const PAGER_AT: u64 = 0x0c00_0000_0000;

static COPY_OP: AtomicU64 = AtomicU64::new(0);
static COPY_DONE: AtomicU64 = AtomicU64::new(0);
static COPY_READ: AtomicU64 = AtomicU64::new(0);

/// `process_read` (op 1) or `process_write` (op 2) of the page `arg >> 32` of the process
/// whose handle is `arg`'s low half: 1 when it succeeded, 2 on `Fault`, 3 otherwise.
extern "C" fn copier(arg: u64) -> ! {
    let process = Process(Handle::from_raw(arg as u32));
    let at = PAGER_AT + (arg >> 32) * PAGE;
    let r = match COPY_OP.load(Ordering::Acquire) {
        1 => {
            let mut b = [0u8; 8];
            let r = process.read(at, &mut b);
            COPY_READ.store(u64::from_le_bytes(b), Ordering::Release);
            r
        }
        _ => process.write(at, b"WRITTEN!"),
    };
    core::mem::forget(process);
    let done = match r {
        Ok(()) => 1,
        Err(Error::Fault) => 2,
        Err(_) => 3,
    };
    COPY_DONE.store(done, Ordering::Release);
    thread_exit()
}

/// One kernel copy of page `page` through `process`, on a thread of its own: the pager is
/// asked for the page, which `answer` supplies or refuses. What the copy answered.
fn copy_through_pager(
    process: &Process,
    page: u64,
    op: u64,
    port: &Channel,
    answer: impl FnOnce(u64) -> Outcome,
) -> Result<u64, (&'static str, Option<Error>)> {
    COPY_OP.store(op, Ordering::Release);
    COPY_DONE.store(0, Ordering::Release);
    let raw = process.0.raw() as u64 | page << 32;
    let t = spawn(copier, raw)?;
    let (key, offset) = request(port)?;
    want(
        key == 0x51 && offset == page * PAGE,
        "the copy's request named another page",
    )?;
    want(
        COPY_DONE.load(Ordering::Acquire) == 0,
        "the copy did not wait for the pager",
    )?;
    answer(offset)?;
    call(
        wait_one(&t.0, Signals::TERMINATED, now() + ms(10_000)),
        "the copy did not end",
    )?;
    Ok(COPY_DONE.load(Ordering::Acquire))
}

/// A page the pager supplies, holding `text`.
fn supply(object: &Memory, offset: u64, text: &[u8; 8]) -> Outcome {
    let page = call(Memory::create(PAGE), "a supplied page")?;
    let m = call(page.map(0, PAGE, map::WRITE), "mapping a supplied page")?;
    call(m.write(0, text), "writing a supplied page")?;
    drop(m);
    call(
        personality::pager_supply(object, offset, PAGE, &page, 0),
        "pager_supply",
    )
}

fn pager_copies(resource: &Handle, object: &Memory, port: &Channel) -> Outcome {
    let z = call(Process::create(resource, "b1-copies"), "process_create")?;
    let theirs = call(
        object
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP),
        "duplicate",
    )?;
    call(
        z.map(Some(&theirs), 0, 8 * PAGE, PAGER_AT, map::READ | map::WRITE),
        "mapping the pager's object",
    )?;
    let read = copy_through_pager(&z, 2, 1, port, |o| supply(object, o, b"SUPPLIED"))?;
    want(
        read == 1 && COPY_READ.load(Ordering::Acquire) == u64::from_le_bytes(*b"SUPPLIED"),
        "process_read did not read the page the pager supplied",
    )?;
    let wrote = copy_through_pager(&z, 3, 2, port, |o| supply(object, o, &[0; 8]))?;
    want(
        wrote == 1,
        "process_write did not write the page the pager supplied",
    )?;
    let mine = call(object.map(3 * PAGE, PAGE, 0), "mapping the written page")?;
    let mut b = [0u8; 8];
    call(mine.read(0, &mut b), "reading the written page")?;
    want(
        &b == b"WRITTEN!",
        "process_write's bytes did not land in the pager's page",
    )?;
    let refused = copy_through_pager(&z, 4, 1, port, |o| {
        call(personality::pager_refuse(object, o, PAGE), "pager_refuse")
    })?;
    want(
        refused == 2,
        "a refused page did not answer process_read with fault",
    )
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

// ---- The fault-tolerant copy.

fn copy_run(out: &Console, resource: &Handle, image: &Handle) -> Outcome {
    let child = call(Process::create(resource, "b1-copy"), "process_create")?;
    let entry = call(child.map_image(image), "process_map_image")?;
    // The region covers the image too, so the copy routine, in the image, lies inside it.
    let routine = b1_copy_byte as *const () as u64..b1_copy_end as *const () as u64;
    let recovery = b1_copy_fault as *const () as u64;
    call(
        personality::personality_set_with_copy(
            &child,
            PAGE,
            MAP_BASE + REGION_LEN - PAGE,
            routine,
            recovery,
        ),
        "personality_set with a copy routine",
    )?;
    let console = call(console_for(out), "duplicating the console")?;
    call(
        child.start_with(entry, &mut [(Some(console), "console")]),
        "process_start",
    )?;
    call(
        wait_one(&child.0, Signals::TERMINATED, now() + ms(10_000)),
        "the copy child did not end",
    )?;
    Ok(())
}

fn copy(address: u64) -> Copied {
    // SAFETY: the routine reads one byte; a fault there resumes at its recovery address,
    // which this process's personality names, and returns the address in `fault`.
    unsafe { b1_copy_byte(address) }
}

fn copy_child() -> Outcome {
    let c = copy(&MARK as *const AtomicU64 as u64);
    want(
        c.value == 0x11 && c.fault == 0,
        "the copy did not read a byte",
    )?;
    let c = copy(0x10);
    want(
        c.value == u64::MAX && c.fault == 0x10,
        "a page fault in the copy did not resume at its recovery address",
    )?;
    let c = copy(0x8000_0000_0000_0000);
    want(
        c.value == u64::MAX && c.fault == 0,
        "a general protection fault in the copy did not resume at its recovery address",
    )
}

// ---- Shared futexes.

/// The word at the start of a mapping, for a futex.
fn word<'a>(m: &'a moruna_sys::Mapping<'_>) -> &'a AtomicU32 {
    // SAFETY: the mapping is at least a page, aligned, mapped for as long as it is
    // borrowed, and only ever accessed as atomics.
    unsafe { &*(m.addr() as *const AtomicU32) }
}

fn futex_child() -> Outcome {
    let shared = Memory(slot("shared"));
    let m = call(shared.map(0, PAGE, map::WRITE), "mapping the shared object")?;
    let woken = call(futex_wake(word(&m), u32::MAX), "futex_wake")?;
    call(m.write(8, &woken.to_le_bytes()), "writing the count")
}

static FUTEX_ASLEEP: AtomicU32 = AtomicU32::new(0);
static FUTEX_WOKEN: AtomicU64 = AtomicU64::new(0);

extern "C" fn futex_waiter(address: u64) -> ! {
    // SAFETY: as in `word`: the tester keeps the page mapped until this thread ends.
    let w = unsafe { &*(address as *const AtomicU32) };
    FUTEX_ASLEEP.store(1, Ordering::Release);
    let r = futex_wait(w, 0, now() + ms(10_000));
    FUTEX_WOKEN.store(if r.is_ok() { 1 } else { 2 }, Ordering::Release);
    thread_exit()
}

fn futex(out: &Console, resource: &Handle, image: &Handle) -> Outcome {
    let shared = call(Memory::create(PAGE), "the shared object")?;
    let m = call(shared.map(0, PAGE, map::WRITE), "mapping the shared object")?;
    let child = call(Process::create(resource, "b1-futex"), "process_create")?;
    let entry = call(child.map_image(image), "process_map_image")?;
    let console = call(console_for(out), "duplicating the console")?;
    let theirs = call(
        shared
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER),
        "duplicate",
    )?;
    // The child starts once this process's thread sleeps on word 0, which holds zero.
    let waiter = spawn(futex_waiter, m.addr())?;
    while FUTEX_ASLEEP.load(Ordering::Acquire) == 0 {
        sleep(ms(1));
    }
    sleep(ms(20));
    call(
        child.start_with(
            entry,
            &mut [(Some(console), "console"), (Some(theirs), "shared")],
        ),
        "process_start",
    )?;
    call(
        wait_one(&waiter.0, Signals::TERMINATED, now() + ms(10_000)),
        "the shared word's waiter was not woken",
    )?;
    want(
        FUTEX_WOKEN.load(Ordering::Acquire) == 1,
        "the shared word's waiter ended without a wake",
    )?;
    let mut b = [0u8; 4];
    call(m.read(8, &mut b), "reading the waker's count")?;
    want(u32::from_le_bytes(b) == 1, "the waker woke no one")
}

// ---- Page tables.

/// Two 2 MiB regions alone in their 512 GiB region.
const TABLES: u64 = 0x0a00_0000_0000;
const REGION: u64 = 2 << 20;

fn tables(out: &Console, resource: &Handle, image: &Handle) -> Outcome {
    let child = call(Process::create(resource, "b1-tables"), "process_create")?;
    let entry = call(child.map_image(image), "process_map_image")?;
    call(
        child.map(None, 0, 2 * REGION, TABLES, map::READ | map::WRITE),
        "mapping two regions",
    )?;
    // A page in each region, so each has a 4 KiB table.
    call(child.write(TABLES, b"x"), "touching the first region")?;
    call(
        child.write(TABLES + REGION, b"y"),
        "touching the second region",
    )?;
    call(
        child.unmap(TABLES + REGION, REGION),
        "emptying the second region",
    )?;
    let (mine, theirs) = call(Channel::create(), "channel_create")?;
    let console = call(console_for(out), "duplicating the console")?;
    call(
        child.start_with(
            entry,
            &mut [(Some(console), "console"), (Some(theirs.0), "peer")],
        ),
        "process_start",
    )?;
    // Once it has looked, the first region empties too, and with it the 512 GiB region.
    call(
        wait_one(&mine.0, Signals::READABLE, now() + ms(10_000)),
        "the tables child did not look",
    )?;
    let mut b = [0u8; 1];
    let mut none: [Option<Handle>; 0] = [];
    call(mine.read(&mut b, &mut none), "reading the tables child")?;
    call(child.unmap(TABLES, REGION), "emptying the first region")?;
    if let Err((e, _)) = mine.write(b"2", []) {
        return Err(("telling the tables child", Some(e)));
    }
    call(
        wait_one(&child.0, Signals::TERMINATED, now() + ms(10_000)),
        "the tables child did not end",
    )?;
    Ok(())
}

fn tables_child() -> Outcome {
    let peer = Channel(slot("peer"));
    let emptied = call(
        personality::table_gap(TABLES + REGION),
        "probing the emptied region",
    )?;
    want(
        emptied == REGION,
        "an emptied 2 MiB region's 4 KiB table stayed",
    )?;
    let kept = call(
        personality::table_gap(TABLES + PAGE),
        "probing the kept region",
    )?;
    want(kept == PAGE, "a kept region's 4 KiB table went")?;
    if let Err((e, _)) = peer.write(b"1", []) {
        return Err(("telling the tester", Some(e)));
    }
    call(
        wait_one(&peer.0, Signals::READABLE, now() + ms(10_000)),
        "the tester did not empty the first region",
    )?;
    let whole = call(
        personality::table_gap(TABLES),
        "probing the emptied 512 GiB region",
    )?;
    want(
        whole == 1 << 39,
        "an emptied 512 GiB region's tables stayed",
    )
}

// ---- A snapshot in two places.

const PLACES: u64 = 0x0d00_0000_0000;

fn places(resource: &Handle) -> Outcome {
    let word = |text: &[u8; 8]| u64::from_le_bytes(*text);
    let source = call(Memory::create(PAGE), "memory_create")?;
    let snap = call(
        personality::memory_snapshot(&source, 0, PAGE, None),
        "memory_snapshot",
    )?;
    let x = call(Process::create(resource, "b1-places"), "process_create")?;
    let (a, b) = (PLACES, PLACES + 2 * PAGE);
    for at in [a, b] {
        call(
            x.map(Some(&snap.0), 0, PAGE, at, map::READ | map::WRITE),
            "mapping the snapshot twice",
        )?;
    }
    // One process maps it: another, and this one, are refused.
    let y = call(Process::create(resource, "b1-places-y"), "process_create")?;
    match y.map(Some(&snap.0), 0, PAGE, a, map::READ | map::WRITE) {
        Err(Error::BadState) => {}
        _ => return Err(("a second process mapped the snapshot", None)),
    }
    match snap.map(0, PAGE, 0) {
        Err(Error::BadState) => {}
        _ => return Err(("a third process mapped the snapshot", None)),
    }
    call(x.write(a, b"11111111"), "writing one place")?;
    want(
        read_u64(&x, b)? == word(b"11111111"),
        "the two places are not one object",
    )?;
    let c = call(
        personality::process_clone(resource, Some(&x), None, 0),
        "process_clone",
    )?;
    call(
        x.write(b, b"22222222"),
        "writing the other place after the clone",
    )?;
    want(
        read_u64(&c, a)? == word(b"11111111") && read_u64(&c, b)? == word(b"11111111"),
        "a write through the second place reached the clone",
    )?;
    want(
        read_u64(&x, a)? == word(b"22222222"),
        "the source's two places stopped being one object",
    )?;
    call(c.write(a, b"33333333"), "writing the clone")?;
    want(
        read_u64(&c, b)? == word(b"33333333"),
        "the clone's two places are not one object",
    )?;
    want(
        read_u64(&x, a)? == word(b"22222222"),
        "the clone's write reached the source",
    )
}

// ---- A clone's room.

const ROOM: u64 = 0x0e00_0000_0000;
const ROOM_PAGES: u64 = 128;

fn room(resource: &Handle) -> Outcome {
    let budget = call(Account::carve(2 << 20, "b1-room"), "account_create")?;
    let source = call(
        Process::create_in(resource, "b1-room", &budget.0),
        "process_create",
    )?;
    call(
        source.map(None, 0, ROOM_PAGES * PAGE, ROOM, map::READ | map::WRITE),
        "mapping zero pages",
    )?;
    for i in 0..ROOM_PAGES {
        call(
            source.write(ROOM + i * PAGE, &i.to_le_bytes()),
            "touching a zero page",
        )?;
    }
    // The account fills to within 16 pages of its limit.
    let u = call(budget.usage(), "account_info")?;
    let room = u.limit - u.used - u.reserved;
    let filler = (room - 16 * PAGE - 256) / (PAGE + 16);
    let _filler = call(
        Memory::create_in(filler * PAGE, &budget.0),
        "filling the account",
    )?;
    let before = call(budget.usage(), "account_info")?;
    want(
        before.limit - before.used - before.reserved < ROOM_PAGES * PAGE / 2,
        "the account still had room for its zero pages",
    )?;
    let theirs = call(Account::carve(4 << 20, "b1-room-clone"), "account_create")?;
    let child = call(
        personality::process_clone(resource, Some(&source), Some(&theirs.0), 0),
        "process_clone with no room for the zero pages twice",
    )?;
    let after = call(budget.usage(), "account_info")?;
    want(
        after.peak - before.peak < ROOM_PAGES * PAGE / 4,
        "the clone charged the source's account its zero pages again",
    )?;
    want(
        read_u64(&child, ROOM + 5 * PAGE)? == 5,
        "the clone lost a zero page",
    )?;
    call(
        source.write(ROOM + 5 * PAGE, &9u64.to_le_bytes()),
        "the source's write",
    )?;
    want(
        read_u64(&child, ROOM + 5 * PAGE)? == 5 && read_u64(&source, ROOM + 5 * PAGE)? == 9,
        "a shared zero page did not copy on write",
    )
}

// ---- A shared handle table.

/// The task's processes run below the region, each on a stack of its own.
const TASK_STACKS: u64 = 0x0f00_0000_0000;
const TASK_STACK: u64 = 16 * PAGE;
/// The object the task's processes share, mapped where the kernel chose, before any clone.
static TASK_SHARED: AtomicU64 = AtomicU64::new(0);

/// The task's shared words: a step, and values its processes hand each other.
fn task_word(i: usize) -> &'static AtomicU64 {
    // SAFETY: the shared object is a page, mapped before the clones and never unmapped
    // while the task lives; its words are only accessed as atomics.
    unsafe { &*((TASK_SHARED.load(Ordering::Acquire) + 8 * i as u64) as *const AtomicU64) }
}

fn task_step() -> &'static AtomicU32 {
    // SAFETY: as in `task_word`; word 0's low half is the step.
    unsafe { &*(TASK_SHARED.load(Ordering::Acquire) as *const AtomicU32) }
}

fn set_step(step: u32) {
    task_step().store(step, Ordering::Release);
    let _ = futex_wake(task_step(), u32::MAX);
}

fn await_step(step: u32) -> bool {
    let end = now() + ms(10_000);
    loop {
        let seen = task_step().load(Ordering::Acquire);
        if seen >= step {
            return true;
        }
        if now() >= end {
            return false;
        }
        let _ = futex_wait(task_step(), seen, now() + ms(100));
    }
}

/// A clone without `SHARE_HANDLES`: the console's value names nothing in it.
extern "C" fn task_probe(_: u64) -> ! {
    let value = task_word(2).load(Ordering::Acquire) as u32;
    let empty = matches!(moruna_sys::info(value), Err(Error::BadHandle));
    task_word(1).store(if empty { 1 } else { 2 }, Ordering::Release);
    thread_exit()
}

/// A clone sharing the table: a channel it makes is the parent's, and the parent's is its.
extern "C" fn task_share(_: u64) -> ! {
    if let Ok((a, b)) = Channel::create() {
        task_word(3).store(a.0.into_raw() as u64, Ordering::Release);
        task_word(4).store(b.0.into_raw() as u64, Ordering::Release);
    }
    set_step(1);
    if await_step(2) {
        let k1 = Channel(Handle::from_raw(task_word(5).load(Ordering::Acquire) as u32));
        let k2 = Channel(Handle::from_raw(task_word(6).load(Ordering::Acquire) as u32));
        let mut b = [0u8; 1];
        let mut none: [Option<Handle>; 0] = [];
        let ok = k1.write(b"k", []).is_ok() && k2.read(&mut b, &mut none).is_ok() && &b == b"k";
        task_word(7).store(if ok { 1 } else { 2 }, Ordering::Release);
        // Closed here, for the parent too.
        let _ = k1.0.close();
        core::mem::forget(k2);
        set_step(3);
    }
    task_rest()
}

/// One of the task's four, asleep until the task is killed.
extern "C" fn task_sleeper(_: u64) -> ! {
    task_rest()
}

fn task_rest() -> ! {
    let word = AtomicU32::new(0);
    loop {
        let _ = futex_wait(&word, 0, now() + ms(60_000));
    }
}

/// Clones this process, maps the clone a stack, and starts it at `entry`.
fn task_clone(
    resource: &Handle,
    index: u64,
    flags: u64,
    entry: extern "C" fn(u64) -> !,
) -> Result<Process, (&'static str, Option<Error>)> {
    let child = call(
        personality::process_clone(resource, None, None, flags),
        "process_clone of itself",
    )?;
    let stack = TASK_STACKS + index * 2 * TASK_STACK;
    call(
        child.map(None, 0, TASK_STACK, stack, map::READ | map::WRITE),
        "mapping a clone's stack",
    )?;
    call(
        child.launch(entry as *const () as u64, stack + TASK_STACK - 8, 0),
        "process_launch of a clone",
    )?;
    Ok(child)
}

/// The task's first process: its checks, its report, then a rest until the tester kills
/// the task, so a failed check never leaves a clone running.
fn task_child(out: &Console) -> ! {
    let resource = slot("resource");
    let report_to = Channel(slot("report"));
    let outcome = Memory::create(PAGE)
        .and_then(|shared| shared.map_detached(0, PAGE, map::WRITE))
        .map_err(|e| ("the task's shared object", Some(e)))
        .and_then(|m| {
            TASK_SHARED.store(m.addr(), Ordering::Release);
            core::mem::forget(m);
            task_steps(out, &resource)
        });
    let text: &[u8] = if outcome.is_ok() { b"ok" } else { b"fail" };
    report(
        out,
        "shared handles",
        outcome,
        "a handle made in a clone worked in its parent and one made in the parent in the clone, one closed in the clone was closed in the parent, a clone without the flag had no handles, and a clone of another table or with an unknown flag was refused",
    );
    let _ = report_to.write(text, []);
    task_rest()
}

fn task_steps(out: &Console, resource: &Handle) -> Outcome {
    use moruna_abi::process::SHARE_HANDLES;
    // Refused: another process's table, and a flag no ABI has.
    let other = call(Process::create(resource, "b1-other"), "process_create")?;
    match personality::process_clone(resource, Some(&other), None, SHARE_HANDLES) {
        Err(Error::AccessDenied) => {}
        _ => return Err(("a clone shared a table the caller does not hold", None)),
    }
    match personality::process_clone(resource, None, None, 1 << 7) {
        Err(Error::InvalidArgs) => {}
        _ => return Err(("a clone took an unknown flag", None)),
    }
    // Without the flag: no handles.
    task_word(2).store(out.0.raw() as u64, Ordering::Release);
    let probe = task_clone(resource, 0, 0, task_probe)?;
    call(
        wait_one(&probe.0, Signals::TERMINATED, now() + ms(10_000)),
        "the clone without the flag did not end",
    )?;
    want(
        task_word(1).load(Ordering::Acquire) == 1,
        "a clone without the flag held the parent's handles",
    )?;
    // With it: one table.
    let _share = task_clone(resource, 1, SHARE_HANDLES, task_share)?;
    want(await_step(1), "the sharing clone did not make its channel")?;
    let h1 = Channel(Handle::from_raw(task_word(3).load(Ordering::Acquire) as u32));
    let h2 = Channel(Handle::from_raw(task_word(4).load(Ordering::Acquire) as u32));
    let mut b = [0u8; 1];
    let mut none: [Option<Handle>; 0] = [];
    let theirs_work = h1.write(b"h", []).is_ok() && h2.read(&mut b, &mut none).is_ok();
    core::mem::forget((h1, h2));
    want(
        theirs_work && &b == b"h",
        "a handle made in the clone did not work in the parent",
    )?;
    let (k1, k2) = call(Channel::create(), "channel_create")?;
    let k1_raw = k1.0.into_raw();
    task_word(5).store(k1_raw as u64, Ordering::Release);
    task_word(6).store(k2.0.into_raw() as u64, Ordering::Release);
    set_step(2);
    want(
        await_step(3),
        "the sharing clone did not use the parent's channel",
    )?;
    want(
        task_word(7).load(Ordering::Acquire) == 1,
        "a handle made in the parent did not work in the clone",
    )?;
    match moruna_sys::info(k1_raw) {
        Err(Error::BadHandle) => {}
        _ => {
            return Err((
                "a handle closed in the clone stayed open in the parent",
                None,
            ));
        }
    }
    // Two more: a task of four, each holding the others' handles in the one table.
    task_clone(resource, 2, SHARE_HANDLES, task_sleeper)?;
    task_clone(resource, 3, SHARE_HANDLES, task_sleeper)?;
    Ok(())
}

fn task(out: &Console, resource: Handle, image: &Handle) -> Outcome {
    use moruna_abi::process::SHARE_HANDLES;
    // This process has no personality: it may not share its table.
    match personality::process_clone(&resource, None, None, SHARE_HANDLES) {
        Err(Error::AccessDenied) => {}
        _ => return Err(("a process with no personality shared its table", None)),
    }
    let account = call(Account::carve(64 << 20, "b1-task"), "account_create")?;
    let start = call(account.usage(), "account_info")?.used;
    let t = call(
        Process::create_in(&resource, "b1-task", &account.0),
        "process_create",
    )?;
    let entry = call(t.map_image(image), "process_map_image")?;
    call(personality_set(&t, MAP_BASE, REGION_LEN), "personality_set")?;
    let (mine, theirs) = call(Channel::create(), "channel_create")?;
    let console = call(console_for(out), "duplicating the console")?;
    call(
        t.start_with(
            entry,
            &mut [
                (Some(console), "console"),
                (Some(resource), "resource"),
                (Some(theirs.0), "report"),
            ],
        ),
        "process_start",
    )?;
    let reported = wait_one(&mine.0, Signals::READABLE, now() + ms(30_000));
    let mut b = [0u8; 4];
    let mut none: [Option<Handle>; 0] = [];
    let read = mine.read(&mut b, &mut none);
    let running = call(account.usage(), "account_info")?.used;
    // Killed whatever it reported, so none of its processes outlives the test.
    call(t.kill(), "process_kill of the task")?;
    call(reported, "the task did not report")?;
    want(
        matches!(read, Ok((2, _))) && &b[..2] == b"ok",
        "the task's own checks failed",
    )?;
    want(running > start, "the task charged nothing")?;
    call(
        wait_one(&t.0, Signals::TERMINATED, now() + ms(10_000)),
        "the task did not end",
    )?;
    drop(t);
    // Its processes end on their own CPUs: the charge goes back as the last of them goes.
    let end = now() + ms(10_000);
    loop {
        if call(account.usage(), "account_info")?.used == start {
            return Ok(());
        }
        if now() >= end {
            return Err(("the killed task's account kept a charge", None));
        }
        sleep(ms(10));
    }
}
