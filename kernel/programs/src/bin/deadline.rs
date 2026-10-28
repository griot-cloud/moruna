//! KK-T9: a timer wakes a waiting thread within tolerance of its deadline, and a wait
//! nothing satisfies ends at its deadline with timed out.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU64, Ordering};

use moruna_kb1_programs::{console, ms};
use moruna_sys::{
    Error, Memory, Signals, Thread, Timer, WaitItem, map, now, println, raw, thread_exit, wait_one,
};

moruna_sys::main!(run);

const DELAY: u64 = ms(50);
const TOLERANCE: u64 = ms(50);
const TIMEOUT: u64 = ms(20);

static WOKE: AtomicU64 = AtomicU64::new(0);
static ANSWER: AtomicU64 = AtomicU64::new(0);

extern "C" fn waiter(timer: u64) -> ! {
    let mut items = [WaitItem::raw(timer as u32, Signals::SIGNALED)];
    let answer = raw::wait(&mut items, now() + ms(5_000));
    WOKE.store(now(), Ordering::Release);
    ANSWER.store(answer, Ordering::Release);
    thread_exit()
}

fn run() -> i64 {
    let out = console();
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
    let deadline = now() + DELAY;
    if timer.set(deadline).is_err() {
        println!(out, "KB1 wait-deadline: FAIL: timer_set");
        return 1;
    }
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
    let woke = WOKE.load(Ordering::Acquire);
    if ANSWER.load(Ordering::Acquire) != 1 || woke < deadline || woke - deadline > TOLERANCE {
        println!(
            out,
            "KB1 wait-deadline: FAIL: answer {:#x}, woke {} ns after the deadline",
            ANSWER.load(Ordering::Acquire),
            woke as i64 - deadline as i64
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
        "KB1 wait-deadline: ok: the timer woke the waiting thread {} us after its deadline (tolerance {} us); a wait nothing satisfied timed out after {} us",
        (woke - deadline) / 1000,
        TOLERANCE / 1000,
        waited / 1000
    );
    0
}
