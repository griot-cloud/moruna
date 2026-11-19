//! Item 1.11e: what `wake` (the test) and `wake-measure` (the measurement) share.
//!
//! Threads that compute on a CPU and never wait, as a job's workers do; one thread that waits
//! and is woken, by another thread through a futex or by its timer, whose deadline the boot
//! processor's tick serves (an interrupt); and, for each wake, the time from the wake to the
//! woken thread's run. The caller is the waker: it is placed on a CPU of its own and computes
//! there between wakes, so it is that CPU's computing thread.
//!
//! One run at a time: the run's state is this module's statics.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use moruna_sys::{
    Error, Mapping, Memory, Signals, Thread, Timer,
    datapath::{ClockPage, current_cpu, pin_current_thread},
    futex_wait, futex_wake, map, thread_exit, wait_one,
};

use crate::ms;

/// The most wakes one run records.
pub const ROUNDS_MAX: usize = 512;

/// How the waiting thread is woken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// Another thread's `futex_wake`; the time is taken from just before that call.
    Thread,
    /// Its timer's deadline, which the boot processor's tick serves; the time is taken from
    /// the deadline, so it holds the tick's own wait (up to a timer period, 1 ms) too.
    Timer,
}

impl By {
    pub fn name(self) -> &'static str {
        match self {
            By::Thread => "a thread",
            By::Timer => "its timer",
        }
    }
}

/// One run.
pub struct Run<'a> {
    /// Given the woken thread's CPU, the CPUs that get a computing thread each, placed there;
    /// a CPU named twice gets two.
    pub spinners: &'a dyn Fn(u32) -> Vec<u32>,
    /// The woken thread's CPU: placed there, or, with `None`, left where the scheduler first
    /// put it (see [`Outcome::home`]).
    pub sleeper: Option<u32>,
    /// The caller's CPU: it is placed there and computes there until the run ends.
    pub waker: u32,
    pub by: By,
    /// Wakes, at most [`ROUNDS_MAX`].
    pub rounds: usize,
    /// Each wake comes this long after the woken thread last ran (by a thread) or after it
    /// armed its timer (by its timer): uniform in `[min, max)` microseconds, from a fixed seed,
    /// so the wakes fall at every point of the computing threads' turns.
    pub gap_us: (u64, u64),
}

/// What a run measured.
pub struct Outcome {
    /// Each wake's time to run, in nanoseconds, sorted.
    pub latencies: Vec<u64>,
    /// The same in the order of the wakes, `u64::MAX` for a wake not timed.
    pub in_order: Vec<u64>,
    /// The CPU the woken thread was on when the wakes began.
    pub home: u32,
    /// Wakes after which the woken thread ran on another CPU than `home`.
    pub moved: u32,
    /// Wakes by a thread that found the woken thread not yet asleep, and so were not counted.
    pub early: u32,
}

impl Outcome {
    /// The `p`th percentile, in nanoseconds (the smallest value at or above `p`% of them).
    pub fn percentile(&self, p: usize) -> u64 {
        percentile(&self.latencies, p)
    }

    pub fn max(&self) -> u64 {
        self.latencies.last().copied().unwrap_or(0)
    }
}

/// The `p`th percentile of `sorted`: the value at the rank `ceil(p * n / 100)`, 1 based.
pub fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len()).div_ceil(100).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// Why a run failed.
#[derive(Debug)]
pub enum Failure {
    /// A call failed: what, and its error.
    Call(&'static str, Error),
    /// Something did not happen in time: what.
    Late(&'static str),
}

impl Failure {
    /// What failed, and the error's name if a call failed.
    pub fn what(&self) -> (&'static str, &'static str) {
        match self {
            Failure::Call(what, e) => (what, e.name()),
            Failure::Late(what) => (what, "not in time"),
        }
    }
}

/// Set to end every computing thread.
static STOP: AtomicU32 = AtomicU32::new(0);
/// Computing threads running on their CPU.
static SPINNING: AtomicU32 = AtomicU32::new(0);

/// The woken thread's CPU plus one, once it is placed; `u32::MAX` if its placement failed.
static HOME: AtomicU32 = AtomicU32::new(0);
/// Set to 1 when the wakes may begin (by its timer, the woken thread waits for it).
static GO: AtomicU32 = AtomicU32::new(0);
/// The waker's last request; the woken thread waits while it is the last one it served.
static REQUEST: AtomicU32 = AtomicU32::new(0);
/// The last request (by a thread) or wake (by its timer) the woken thread served.
static SERVED: AtomicU32 = AtomicU32::new(0);
/// When the last request was made, on the clock page.
static ASKED_AT: AtomicU64 = AtomicU64::new(0);
static ROUNDS: AtomicU32 = AtomicU32::new(0);
static GAP_MIN: AtomicU64 = AtomicU64::new(0);
static GAP_MAX: AtomicU64 = AtomicU64::new(0);
static MOVED: AtomicU32 = AtomicU32::new(0);
static EARLY: AtomicU32 = AtomicU32::new(0);
/// Set when the woken thread failed: 1 a call, 2 a wait that timed out.
static FAILED: AtomicU32 = AtomicU32::new(0);
static LATENCY: [AtomicU64; ROUNDS_MAX] = [const { AtomicU64::new(0) }; ROUNDS_MAX];

/// A 64-bit xorshift: the gaps between wakes, the same in every boot.
struct Gaps(u64);

impl Gaps {
    fn new() -> Gaps {
        Gaps(0x9e37_79b9_7f4a_7c15)
    }

    /// The next gap, in nanoseconds.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        let (min, max) = (
            GAP_MIN.load(Ordering::Relaxed),
            GAP_MAX.load(Ordering::Relaxed),
        );
        if max <= min {
            return min;
        }
        min + self.0 % (max - min)
    }
}

/// Computes on CPU `cpu`, placed there, until [`STOP`]: a worker that never waits.
extern "C" fn spin(cpu: u64) -> ! {
    if pin_current_thread(Some(cpu as u32)).is_ok() {
        SPINNING.fetch_add(1, Ordering::AcqRel);
        let mut x = cpu;
        while STOP.load(Ordering::Relaxed) == 0 {
            x = core::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
        }
    }
    thread_exit()
}

/// The woken thread. `arg`: its CPU plus one (0: not placed) in the low half; 1 in the high
/// half to be woken by its timer.
extern "C" fn sleeper(arg: u64) -> ! {
    let place = (arg & 0xffff_ffff) as u32;
    let by_timer = arg >> 32 != 0;
    if place != 0 && pin_current_thread(Some(place - 1)).is_err() {
        HOME.store(u32::MAX, Ordering::Release);
        thread_exit()
    }
    let Ok(page) = ClockPage::map() else {
        HOME.store(u32::MAX, Ordering::Release);
        thread_exit()
    };
    let home = current_cpu();
    HOME.store(home + 1, Ordering::Release);
    let rounds = ROUNDS.load(Ordering::Acquire);
    if by_timer {
        let Ok(timer) = Timer::create() else {
            FAILED.store(1, Ordering::Release);
            thread_exit()
        };
        while GO.load(Ordering::Acquire) == 0 {
            let _ = futex_wait(&GO, 0, page.now() + ms(10_000));
        }
        let mut gaps = Gaps::new();
        for round in 0..rounds {
            // The run ended early.
            if GO.load(Ordering::Acquire) == 2 {
                break;
            }
            let deadline = page.now() + gaps.next();
            if timer.set(deadline).is_err() {
                FAILED.store(1, Ordering::Release);
                break;
            }
            let woke = wait_one(&timer.0, Signals::SIGNALED, deadline + ms(5_000));
            let ran = page.now();
            if !matches!(woke, Ok(s) if s.contains(Signals::SIGNALED)) {
                FAILED.store(2, Ordering::Release);
                break;
            }
            LATENCY[round as usize].store(ran.saturating_sub(deadline), Ordering::Relaxed);
            if current_cpu() != home {
                MOVED.fetch_add(1, Ordering::Relaxed);
            }
            SERVED.store(round + 1, Ordering::Release);
        }
    } else {
        let mut served = 0;
        while served < rounds {
            let slept = futex_wait(&REQUEST, served, page.now() + ms(5_000));
            let ran = page.now();
            let asked = REQUEST.load(Ordering::Acquire);
            // Past the last: the run ended early.
            if asked > rounds {
                break;
            }
            if asked == served {
                continue;
            }
            if slept.is_ok() {
                LATENCY[(asked - 1) as usize].store(
                    ran.saturating_sub(ASKED_AT.load(Ordering::Acquire)),
                    Ordering::Relaxed,
                );
            } else {
                // The request came before this thread was asleep: no wake to time.
                LATENCY[(asked - 1) as usize].store(u64::MAX, Ordering::Relaxed);
                EARLY.fetch_add(1, Ordering::Relaxed);
            }
            if current_cpu() != home {
                MOVED.fetch_add(1, Ordering::Relaxed);
            }
            served = asked;
            SERVED.store(served, Ordering::Release);
        }
    }
    thread_exit()
}

/// A thread on a stack of its own, which goes once the thread has ended.
struct Started {
    thread: Thread,
    _mapping: Mapping<'static>,
    _stack: Memory,
}

fn start(entry: extern "C" fn(u64) -> !, arg: u64) -> Result<Started, Failure> {
    const STACK: u64 = 64 * 1024;
    let stack = Memory::create(STACK).map_err(|e| Failure::Call("a stack", e))?;
    let mapping = stack
        .map_detached(0, STACK, map::WRITE)
        .map_err(|e| Failure::Call("mapping a stack", e))?;
    let thread =
        Thread::spawn(entry, mapping.end(), arg).map_err(|e| Failure::Call("thread_create", e))?;
    Ok(Started {
        thread,
        _mapping: mapping,
        _stack: stack,
    })
}

fn join(started: &Started) -> Result<(), Failure> {
    let page = ClockPage::map().map_err(|e| Failure::Call("clock_page", e))?;
    match wait_one(
        &started.thread.0,
        Signals::TERMINATED,
        page.now() + ms(10_000),
    ) {
        Ok(s) if s.contains(Signals::TERMINATED) => Ok(()),
        Ok(_) => Err(Failure::Late("a thread to end")),
        Err(e) => Err(Failure::Call("waiting for a thread to end", e)),
    }
}

/// Computes (on the caller's CPU) until `done()`, for at most `limit` nanoseconds.
fn until(page: &ClockPage, limit: u64, done: impl Fn() -> bool) -> bool {
    let end = page.now() + limit;
    while !done() {
        if page.now() > end {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// Runs `run` and returns what it measured. The caller is left placed on `run.waker`.
pub fn measure(run: &Run<'_>) -> Result<Outcome, Failure> {
    let rounds = run.rounds.min(ROUNDS_MAX);
    let page = ClockPage::map().map_err(|e| Failure::Call("clock_page", e))?;
    pin_current_thread(Some(run.waker)).map_err(|e| Failure::Call("placing the waker", e))?;
    STOP.store(0, Ordering::Relaxed);
    SPINNING.store(0, Ordering::Relaxed);
    HOME.store(0, Ordering::Relaxed);
    GO.store(0, Ordering::Relaxed);
    REQUEST.store(0, Ordering::Relaxed);
    SERVED.store(0, Ordering::Relaxed);
    MOVED.store(0, Ordering::Relaxed);
    EARLY.store(0, Ordering::Relaxed);
    FAILED.store(0, Ordering::Relaxed);
    ROUNDS.store(rounds as u32, Ordering::Relaxed);
    GAP_MIN.store(run.gap_us.0 * 1_000, Ordering::Relaxed);
    GAP_MAX.store(run.gap_us.1 * 1_000, Ordering::Relaxed);
    for slot in &LATENCY {
        slot.store(0, Ordering::Relaxed);
    }

    // The woken thread first, placed while the CPUs are quiet; then the computing threads.
    let place = run.sleeper.map_or(0, |cpu| u64::from(cpu) + 1);
    let timer = u64::from(run.by == By::Timer) << 32;
    let sleeper = start(sleeper, place | timer)?;
    let mut spinners = Vec::new();
    let result = drive(run, rounds, &page, &mut spinners);

    // End every thread, whatever happened.
    STOP.store(1, Ordering::Release);
    GO.store(2, Ordering::Release);
    REQUEST.store(rounds as u32 + 1, Ordering::Release);
    let _ = futex_wake(&REQUEST, 1);
    let _ = futex_wake(&GO, 1);
    for s in &spinners {
        join(s)?;
    }
    join(&sleeper)?;
    let home = result?;
    match FAILED.load(Ordering::Acquire) {
        0 => {}
        1 => return Err(Failure::Late("the woken thread's calls to succeed")),
        _ => return Err(Failure::Late("the timer to wake the woken thread")),
    }

    let in_order: Vec<u64> = LATENCY[..rounds]
        .iter()
        .map(|l| l.load(Ordering::Relaxed))
        .collect();
    let mut latencies: Vec<u64> = in_order
        .iter()
        .copied()
        .filter(|&l| l != u64::MAX)
        .collect();
    latencies.sort_unstable();
    Ok(Outcome {
        latencies,
        in_order,
        home,
        moved: MOVED.load(Ordering::Relaxed),
        early: EARLY.load(Ordering::Relaxed),
    })
}

/// Waits for the woken thread to be placed, starts the computing threads into `spinners`,
/// and makes the wakes; returns the woken thread's CPU.
fn drive(
    run: &Run<'_>,
    rounds: usize,
    page: &ClockPage,
    spinners: &mut Vec<Started>,
) -> Result<u32, Failure> {
    if !until(page, ms(10_000), || HOME.load(Ordering::Acquire) != 0) {
        return Err(Failure::Late("the woken thread to run"));
    }
    let home = HOME.load(Ordering::Acquire);
    if home == u32::MAX {
        return Err(Failure::Late("the woken thread to be placed"));
    }
    let home = home - 1;
    let cpus = (run.spinners)(home);
    for &cpu in &cpus {
        spinners.push(start(spin, u64::from(cpu))?);
    }
    let all = cpus.len() as u32;
    if !until(page, ms(10_000), || SPINNING.load(Ordering::Acquire) == all) {
        return Err(Failure::Late("the computing threads to run on their CPUs"));
    }
    // Long enough for the woken thread to be asleep, and for each CPU running one thread to
    // stretch its tick.
    until(page, ms(30), || false);

    if run.by == By::Timer {
        GO.store(1, Ordering::Release);
        let _ = futex_wake(&GO, 1);
        let limit = ms(5_000) + rounds as u64 * (GAP_MAX.load(Ordering::Relaxed) + ms(50));
        if !until(page, limit, || {
            SERVED.load(Ordering::Acquire) == rounds as u32 || FAILED.load(Ordering::Acquire) != 0
        }) {
            return Err(Failure::Late("the timer's wakes to be served"));
        }
        return Ok(home);
    }
    let mut gaps = Gaps::new();
    for round in 1..=rounds as u32 {
        let next = page.now() + gaps.next();
        until(page, u64::MAX / 2, || page.now() >= next);
        ASKED_AT.store(page.now(), Ordering::Release);
        REQUEST.store(round, Ordering::Release);
        futex_wake(&REQUEST, 1).map_err(|e| Failure::Call("futex_wake", e))?;
        if !until(page, ms(5_000), || SERVED.load(Ordering::Acquire) == round) {
            return Err(Failure::Late("a request to be served"));
        }
    }
    Ok(home)
}
