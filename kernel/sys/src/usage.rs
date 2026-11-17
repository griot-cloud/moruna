//! stream U: the usage build's counters, printed by the program itself (`kernel/usage`).
//!
//! Only a program built with the `usage` feature and `-C instrument-coverage` has this
//! module; no production build does. Its coverage counters are in its own memory, which the
//! host cannot read, so the program prints them on its console, which the kernel writes to
//! the serial log the host keeps. A dump goes out:
//!
//! - when the program has handled something and then waited [`IDLE`] with nothing to do;
//! - before a wait, when it has handled something and its last dump is [`MAX_AGE`] old (a
//!   program woken by its own timer never idles);
//! - when it exits.
//!
//! So a program's last dump holds all it did, a few seconds after it goes quiet, with no
//! call from the host and no new path into or out of the guest: the console is one the
//! program already holds. Its waits answer as before; only their timing differs, by the time
//! a dump takes.

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
};

use moruna_abi::{Error, Rights, nr};
use moruna_usage::dump::{self, Header, LINE_MAX};

use crate::{WaitItem, raw, start};

/// How long a program that handled something waits idle before it dumps.
pub const IDLE: u64 = 1_000_000_000;
/// The oldest a busy program's last dump may be when it waits.
pub const MAX_AGE: u64 = 5_000_000_000;

/// No console: the program cannot dump.
const NONE: u64 = u64::MAX;

unsafe extern "C" {
    // The counter section's bounds, which the linker defines for a section named as a C
    // identifier (LLVM's `__llvm_prf_cnts`).
    static __start___llvm_prf_cnts: u8;
    static __stop___llvm_prf_cnts: u8;
}

// SAFETY: LLVM's instrumented code refers to this symbol by name, and no other crate in the
// program defines it. There is no runtime behind it: this module prints the counters.
#[unsafe(no_mangle)]
static __llvm_profile_runtime: u8 = 0;

static PROGRAM: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static PROGRAM_LEN: AtomicUsize = AtomicUsize::new(0);
static CONSOLE: AtomicU64 = AtomicU64::new(NONE);
static INSTANCE: AtomicU64 = AtomicU64::new(0);
static SEQ: AtomicU64 = AtomicU64::new(0);
static WAKES: AtomicU64 = AtomicU64::new(0);
/// Something was handled since the last dump; the start counts.
static ACTIVE: AtomicBool = AtomicBool::new(true);
static LAST: AtomicU64 = AtomicU64::new(0);
/// Held while one thread dumps; another thread's dump is skipped.
static DUMPING: AtomicBool = AtomicBool::new(false);

/// The buffer a dump's lines are built in, used only while `DUMPING` is held.
struct Buffer(UnsafeCell<[u8; LINE_MAX]>);
// SAFETY: only the thread that set `DUMPING` touches the buffer, until it clears it.
unsafe impl Sync for Buffer {}
static BUFFER: Buffer = Buffer(UnsafeCell::new([0; LINE_MAX]));

fn now() -> u64 {
    raw::call0(nr::CLOCK_GET).0
}

/// Called by `main!` before the program's `run`: its binary name, its instance and a console
/// of its own (a duplicate of the one it started with, so the program may close its own).
#[doc(hidden)]
pub fn start(program: &'static str) {
    PROGRAM_LEN.store(program.len(), Ordering::Relaxed);
    PROGRAM.store(program.as_ptr().cast_mut(), Ordering::Release);
    let mut id = [0u8; 8];
    let instance = match Error::from_raw(raw::random_read(&mut id)) {
        Ok(_) => u64::from_le_bytes(id),
        Err(_) => now() ^ (start::process_name().as_ptr() as u64).rotate_left(32),
    };
    INSTANCE.store(instance, Ordering::Relaxed);
    if let Some(console) = start::peek("console") {
        let own = raw::handle_duplicate(console, Rights::WRITE.bits());
        let own = match Error::from_raw(own) {
            Ok(v) => v,
            Err(_) => u64::from(console),
        };
        CONSOLE.store(own, Ordering::Release);
    }
    LAST.store(now(), Ordering::Relaxed);
}

fn program() -> &'static str {
    let ptr = PROGRAM.load(Ordering::Acquire);
    if ptr.is_null() {
        return "?";
    }
    // SAFETY: `start` stored the pointer and length of a `&'static str`.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, PROGRAM_LEN.load(Ordering::Relaxed)) };
    core::str::from_utf8(bytes).unwrap_or("?")
}

/// Prints the counters now; `false`, with nothing printed, when another thread is printing
/// them.
pub fn dump() -> bool {
    if DUMPING.swap(true, Ordering::AcqRel) {
        return false;
    }
    // Cleared first, so a thread that handles something while this prints marks it again.
    ACTIVE.store(false, Ordering::Release);
    let console = CONSOLE.load(Ordering::Acquire);
    if console != NONE {
        let first = (&raw const __start___llvm_prf_cnts).cast::<u64>();
        let end = &raw const __stop___llvm_prf_cnts as usize;
        let count = end.saturating_sub(first as usize) / 8;
        let header = Header {
            program: program(),
            process: start::process_name(),
            instance: INSTANCE.load(Ordering::Relaxed),
            seq: SEQ.fetch_add(1, Ordering::Relaxed),
            addr: first as u64,
            count,
            wakes: WAKES.load(Ordering::Relaxed),
        };
        // SAFETY: counters `0..count` lie between the section's bounds, are 8-byte aligned
        // and live as long as the program; other threads update them with atomic adds, so a
        // read sees a whole value.
        let counter = |i: usize| unsafe { core::ptr::read_volatile(first.add(i)) };
        // SAFETY: `DUMPING` is held, so no other thread uses the buffer.
        let buf = unsafe { &mut *BUFFER.0.get() };
        dump::encode(&header, counter, buf, &mut |line: &[u8]| {
            raw::debug_write(console as u32, line);
        });
    }
    LAST.store(now(), Ordering::Relaxed);
    DUMPING.store(false, Ordering::Release);
    true
}

/// `wait` as the program asked it, with the dumps above: the answer is the one the kernel
/// gives for the program's own deadline.
pub(crate) fn wait(items: &mut [WaitItem], deadline: u64) -> u64 {
    loop {
        let mut until = deadline;
        if ACTIVE.load(Ordering::Acquire) {
            let t = now();
            if t.saturating_sub(LAST.load(Ordering::Relaxed)) >= MAX_AGE && dump() {
                continue;
            }
            until = deadline.min(t.saturating_add(IDLE));
        }
        let answer = raw::wait_kernel(items, until);
        if until < deadline && Error::from_raw(answer) == Err(Error::TimedOut) {
            dump();
            continue;
        }
        WAKES.fetch_add(1, Ordering::Relaxed);
        ACTIVE.store(true, Ordering::Release);
        return answer;
    }
}

/// Called before the process ends.
pub(crate) fn at_exit() {
    dump();
}
