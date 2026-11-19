//! Item 1.11e, the measurement: the time from a wake to the woken thread's run, on a quiet
//! machine, with every CPU computing, and with every CPU but one computing.
//!
//! The caller (the waker) is placed on the last CPU and computes there between wakes. Each
//! run makes `ROUNDS` wakes, each a random 1 to 20 ms after the woken thread last ran or armed
//! its timer (the same gaps in every boot), and prints the time to run at the 10th, 50th,
//! 90th and 99th percentiles and the most, the CPU the woken thread was on, and how many
//! wakes it ran elsewhere. Not a test: it judges nothing. Booted alone, with 4 CPUs, for the
//! report (`kernel/initramfs/wake-measure.image`).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use moruna_kb1_programs::{
    Machine, console,
    wake::{By, Run, measure},
};
use moruna_sys::{println, start};

moruna_sys::main!(run);

const ROUNDS: usize = 150;
/// Random gaps between wakes, in microseconds: all through a 10 ms turn and past it.
const SPREAD: (u64, u64) = (1_000, 20_000);
/// Gaps of a request passing from thread to thread: each wake soon after the last.
const QUICK: (u64, u64) = (200, 2_000);

type Out = moruna_sys::Console;

fn run() -> i64 {
    let out = console();
    let cpus = start::cpus() as u32;
    if cpus == 0 {
        println!(out, "wake-measure: FAIL: the start page gives no CPUs");
        return 1;
    }
    let machine = match Machine::detect() {
        Machine::Kvm => "KVM",
        Machine::Tcg => "TCG",
        Machine::Other(_) => "another machine",
    };
    println!(
        out,
        "wake-measure: {} CPUs under {}; {} wakes a run; times in us", cpus, machine, ROUNDS
    );
    let waker = cpus - 1;
    // Every CPU but the waker's gets a computing thread; the waker computes on its own.
    let all_busy = move |_home: u32| -> Vec<u32> { (0..cpus).filter(|&c| c != waker).collect() };
    let quiet = |_home: u32| -> Vec<u32> { Vec::new() };
    let mut code = 0;
    for by in [By::Thread, By::Timer] {
        code |= one(
            &out,
            "quiet: only the waker computes",
            &quiet,
            Some(0),
            waker,
            by,
            SPREAD,
        );
        code |= one(
            &out,
            "busy: every CPU computes, the woken thread on CPU 0",
            &all_busy,
            Some(0),
            waker,
            by,
            SPREAD,
        );
        if cpus >= 3 {
            code |= one(
                &out,
                "busy: every CPU computes, the woken thread on CPU 1",
                &all_busy,
                Some(1),
                waker,
                by,
                SPREAD,
            );
        }
    }
    if cpus >= 3 {
        // Two computing threads on the woken thread's CPU: one more ahead of it in the queue.
        let two_on_1 = move |_home: u32| -> Vec<u32> {
            let mut v: Vec<u32> = (0..cpus).filter(|&c| c != waker).collect();
            v.push(1);
            v
        };
        code |= one(
            &out,
            "busy: every CPU computes, two on CPU 1, the woken thread on CPU 1",
            &two_on_1,
            Some(1),
            waker,
            By::Thread,
            SPREAD,
        );
        // The woken thread is not placed: it stays where the scheduler put it, a computing
        // thread is placed there, and one other CPU is left idle.
        let one_idle = move |home: u32| -> Vec<u32> {
            let idle = (0..cpus).find(|&c| c != home && c != waker).unwrap_or(home);
            (0..cpus).filter(|&c| c != waker && c != idle).collect()
        };
        code |= one(
            &out,
            "one idle: every CPU but one computes, the woken thread not placed",
            &one_idle,
            None,
            waker,
            By::Thread,
            SPREAD,
        );
    }
    code |= one(
        &out,
        "quick: every CPU computes, the woken thread on CPU 0, wakes 0.2 to 2 ms apart",
        &all_busy,
        Some(0),
        waker,
        By::Thread,
        QUICK,
    );
    if code == 0 {
        println!(out, "wake-measure: done");
    }
    code
}

fn one(
    out: &Out,
    name: &str,
    spinners: &dyn Fn(u32) -> Vec<u32>,
    sleeper: Option<u32>,
    waker: u32,
    by: By,
    gap_us: (u64, u64),
) -> i64 {
    let run = Run {
        spinners,
        sleeper,
        waker,
        by,
        rounds: ROUNDS,
        gap_us,
    };
    match measure(&run) {
        Ok(o) => {
            println!(
                out,
                "wake-measure: {}; woken by {}: p10 {} p50 {} p90 {} p99 {} max {} ({} wakes, {} not asleep yet), on CPU {}, ran elsewhere {} times",
                name,
                by.name(),
                o.percentile(10) / 1_000,
                o.percentile(50) / 1_000,
                o.percentile(90) / 1_000,
                o.percentile(99) / 1_000,
                o.max() / 1_000,
                o.latencies.len(),
                o.early,
                o.home,
                o.moved
            );
            // How the times spread, in ms, and which wakes (from 1) took over 5 ms.
            let edges = [
                500_000u64, 1_000_000, 2_000_000, 5_000_000, 9_000_000, 11_000_000,
            ];
            let mut counts = [0usize; 7];
            for &l in &o.latencies {
                counts[edges.iter().filter(|&&e| l >= e).count()] += 1;
            }
            let slow: Vec<usize> = o
                .in_order
                .iter()
                .enumerate()
                .filter(|&(_, &l)| l != u64::MAX && l >= 5_000_000)
                .map(|(i, _)| i + 1)
                .collect();
            println!(
                out,
                "wake-measure:   under 0.5 ms {}, 0.5-1 {}, 1-2 {}, 2-5 {}, 5-9 {}, 9-11 {}, over 11 {}; the wakes over 5 ms: {:?}",
                counts[0],
                counts[1],
                counts[2],
                counts[3],
                counts[4],
                counts[5],
                counts[6],
                slow
            );
            0
        }
        Err(f) => {
            let (what, why) = f.what();
            println!(
                out,
                "wake-measure: FAIL: {}; woken by {}: {}: {}",
                name,
                by.name(),
                what,
                why
            );
            1
        }
    }
}
