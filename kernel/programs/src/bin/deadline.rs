//! KK-T9: a timer wakes a waiting thread within tolerance of its deadline, and a wait
//! nothing satisfies ends at its deadline with timed out.
//!
//! The tolerance is the scheduler's, not the clock's: the timer fires on the 1 ms tick, and
//! the woken thread then waits its turn behind the runnable threads on its CPU, each for up
//! to a 10 ms slice. Under KVM the bound is 50 ms. Under QEMU's TCG every instruction is
//! emulated, so the work ahead of the waiter takes longer, and the bound is the KVM one times
//! the emulation factor SDD 20 k measured. Either bound is far below the waiter's own 5 s
//! deadline, so a lost wake still fails: that wait ends `TimedOut`, not `SIGNALED`.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU64, Ordering};

use moruna_kb1_programs::{Machine, console, ms};
use moruna_sys::{
    Error, Memory, Signals, Thread, Timer, WaitItem, map, now, println, raw, thread_exit, wait_one,
};

moruna_sys::main!(run);

const DELAY: u64 = ms(50);
/// The bound on a machine whose CPU is not emulated: KVM, or a signature this program does
/// not know, which gets the strict bound rather than a loose one it has not earned.
const TOLERANCE: u64 = ms(50);
/// How many times slower QEMU's TCG runs the boot's work than KVM does: KK-T6's round trips,
/// the worst ratio of the CPU counts measured (SDD 20 k, KK-T9).
const EMULATION_FACTOR: u64 = 18;
/// The bound under TCG.
const TOLERANCE_TCG: u64 = TOLERANCE * EMULATION_FACTOR;
const TIMEOUT: u64 = ms(20);

static DEADLINE: AtomicU64 = AtomicU64::new(0);
static WOKE: AtomicU64 = AtomicU64::new(0);
static ANSWER: AtomicU64 = AtomicU64::new(0);

/// The waiter arms the timer itself, just before it waits, so what is measured is the
/// timer's wake, not how long the thread took to start.
extern "C" fn waiter(timer: u64) -> ! {
    let deadline = now() + DELAY;
    DEADLINE.store(deadline, Ordering::Release);
    if Error::from_raw(raw::timer_set(timer as _, deadline)).is_err() {
        ANSWER.store(u64::MAX, Ordering::Release);
        thread_exit()
    }
    let mut items = [WaitItem::raw(timer as u32, Signals::SIGNALED)];
    let answer = raw::wait(&mut items, now() + ms(5_000));
    WOKE.store(now(), Ordering::Release);
    ANSWER.store(answer, Ordering::Release);
    thread_exit()
}

fn run() -> i64 {
    let out = console();
    let machine = Machine::detect();
    let (tolerance, under) = match machine {
        Machine::Tcg => (TOLERANCE_TCG, "TCG"),
        Machine::Kvm => (TOLERANCE, "KVM"),
        Machine::Other(_) => (TOLERANCE, "an unrecognised machine"),
    };
    let (Ok(timer), Ok(stack)) = (Timer::create(), Memory::create(64 * 1024)) else {
        println!(
            out,
            "KB1 wait-deadline: FAIL: could not create the timer or the stack"
        );
        return 1;
    };
    let Ok(stack_map) = stack.map(0, 64 * 1024, map::WRITE) else {
        println!(out, "KB1 wait-deadline: FAIL: could not map the stack");
        return 1;
    };
    let thread = match Thread::spawn(waiter, stack_map.end(), timer.0.raw() as u64) {
        Ok(t) => t,
        Err(e) => {
            println!(out, "KB1 wait-deadline: FAIL: thread_create: {}", e.name());
            return 1;
        }
    };
    if !matches!(wait_one(&thread.0, Signals::TERMINATED, now() + ms(10_000)), Ok(s) if s.contains(Signals::TERMINATED))
    {
        println!(
            out,
            "KB1 wait-deadline: FAIL: the waiting thread did not end"
        );
        return 1;
    }
    if ANSWER.load(Ordering::Acquire) == u64::MAX {
        println!(out, "KB1 wait-deadline: FAIL: timer_set");
        return 1;
    }
    let deadline = DEADLINE.load(Ordering::Acquire);
    let woke = WOKE.load(Ordering::Acquire);
    if ANSWER.load(Ordering::Acquire) != 1 || woke < deadline || woke - deadline > tolerance {
        println!(
            out,
            "KB1 wait-deadline: FAIL: answer {:#x}, woke {} ns after the deadline (tolerance {} us under {})",
            ANSWER.load(Ordering::Acquire),
            woke as i64 - deadline as i64,
            tolerance / 1000,
            under
        );
        return 1;
    }

    let Ok(quiet) = Timer::create() else {
        return 1;
    };
    let started = now();
    let timed_out = wait_one(&quiet.0, Signals::SIGNALED, started + TIMEOUT);
    let waited = now() - started;
    if timed_out != Err(Error::TimedOut) || waited < TIMEOUT {
        println!(
            out,
            "KB1 wait-deadline: FAIL: an unsatisfied wait returned after {} ns", waited
        );
        return 1;
    }
    drop(stack_map);
    println!(
        out,
        "KB1 wait-deadline: ok: the timer woke the waiting thread {} us after its deadline (tolerance {} us under {}); a wait nothing satisfied timed out after {} us",
        (woke - deadline) / 1000,
        tolerance / 1000,
        under,
        waited / 1000
    );
    0
}
