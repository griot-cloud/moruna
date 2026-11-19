//! Item 1.11e: a thread woken while another thread computes on its CPU runs within
//! [`BOUND_US`], with every CPU computing, as a job's workers keep them.
//!
//! A thread waits and is woken `ROUNDS` times, at random points of the computing threads'
//! turns: by its timer, whose deadline the boot processor's tick serves (an interrupt, as a
//! driver's device wakes it), and by another thread's `futex_wake` (as a server's client
//! wakes it). It is placed on the boot processor, then on the last CPU, and a computing
//! thread is placed on every CPU; the waker computes on its own CPU between wakes. Each wake
//! is timed from the deadline or from the `futex_wake` to the woken thread's run. At least
//! [`WITHIN`]% of the wakes must run within the bound: the rest allow for the host's stalls
//! of a virtual CPU, which a wake on a quiet CPU shows too (1.11e's report).
//!
//! On one CPU only the timer's wake is judged: the waking thread can only be the thread
//! computing on the woken one's CPU, and a wake by the running thread does not preempt it
//! (SDD 20 f.11: a producer that wakes its own consumer keeps its CPU, so the two batch).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use moruna_kb1_programs::{
    console,
    wake::{By, Run, measure},
};
use moruna_sys::{println, start};

moruna_sys::main!(run);

const ROUNDS: usize = 100;
/// Random gaps between wakes, in microseconds: all through a computing thread's turn.
const SPREAD: (u64, u64) = (1_000, 12_000);
/// The bound, in microseconds: two timer periods. A timer's wake waits up to one period
/// (1 ms) for the boot processor's tick by design; the second is for the wake itself. Under
/// TCG, a wake on a quiet CPU ran within 0.3 ms at the 99th percentile, and within 1.2 ms
/// counting the tick's own wait (1.11e's report).
const BOUND_US: u64 = 2_000;
/// The share of wakes, in percent, that must run within the bound.
const WITHIN: usize = 95;

fn run() -> i64 {
    let out = console();
    let cpus = start::cpus() as u32;
    if cpus == 0 {
        println!(out, "wake: FAIL: the start page gives no CPUs");
        return 1;
    }
    let last = cpus - 1;
    // The woken thread on the boot processor, then on the last CPU; the waker on another CPU
    // when there is one. Every other CPU gets a computing thread.
    let mut places = Vec::new();
    places.push((0, last));
    if cpus > 1 {
        places.push((last, 0));
    }
    let mut failed = false;
    let mut worst = 0;
    for &(sleeper, waker) in &places {
        let busy = move |_home: u32| -> Vec<u32> { (0..cpus).filter(|&c| c != waker).collect() };
        // On one CPU the waker is the woken thread's CPU's running thread: the timer alone.
        let kinds: &[By] = if cpus > 1 {
            &[By::Timer, By::Thread]
        } else {
            &[By::Timer]
        };
        for &by in kinds {
            let run = Run {
                spinners: &busy,
                sleeper: Some(sleeper),
                waker,
                by,
                rounds: ROUNDS,
                gap_us: SPREAD,
            };
            let o = match measure(&run) {
                Ok(o) => o,
                Err(f) => {
                    let (what, why) = f.what();
                    println!(
                        out,
                        "wake: FAIL: woken by {} on CPU {}: {}: {}",
                        by.name(),
                        sleeper,
                        what,
                        why
                    );
                    return 1;
                }
            };
            let share = o.percentile(WITHIN);
            worst = worst.max(share);
            println!(
                out,
                "wake: woken by {} on CPU {}, every CPU computing: the median {} us, the {}th percentile {} us, the longest {} us",
                by.name(),
                sleeper,
                o.percentile(50) / 1_000,
                WITHIN,
                share / 1_000,
                o.max() / 1_000
            );
            if share > BOUND_US * 1_000 {
                failed = true;
                let over = o
                    .latencies
                    .iter()
                    .filter(|&&l| l > BOUND_US * 1_000)
                    .count();
                println!(
                    out,
                    "wake: FAIL: a thread woken by {} on CPU {}, every CPU computing, ran later than {} us after {} of {} wakes: the {}th percentile {} us, the median {} us, the longest {} us",
                    by.name(),
                    sleeper,
                    BOUND_US,
                    over,
                    o.latencies.len(),
                    WITHIN,
                    share / 1_000,
                    o.percentile(50) / 1_000,
                    o.max() / 1_000
                );
            }
        }
    }
    if failed {
        return 1;
    }
    println!(
        out,
        "wake: ok: a thread woken by {}, {} times each on CPU 0{}, every CPU computing, ran within {} us at the {}th percentile (at most {} us)",
        if cpus > 1 {
            "its timer and by a thread"
        } else {
            "its timer (one CPU: no thread to wake it from elsewhere)"
        },
        ROUNDS,
        if cpus > 1 { " and on the last CPU" } else { "" },
        BOUND_US,
        WITHIN,
        worst / 1_000
    );
    0
}
