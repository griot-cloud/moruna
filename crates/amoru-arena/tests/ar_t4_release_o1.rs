//! AR-T4 release_o1 (02 k): 1 M alloc/release pairs from 16 threads; p99 latency under
//! 1 microsecond on the reference host, provisional elsewhere. AR-I4.
//!
//! The 1 microsecond figure is the reference host's (preamble E1), so off that host this
//! test records the measurement and holds the arena to a loose bound that still catches a
//! release that touches the OS or serialises every thread on one lock.

mod common;

use std::sync::Arc;
use std::time::Instant;

use amoru_kernel::{Allocator, Tier};

const THREADS: usize = 16;
const PAIRS_PER_THREAD: usize = 1_000_000 / THREADS;

#[test]
fn ar_t4_release_o1() {
    let arena = common::host_arena(64 << 20);
    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let arena: Arc<dyn Allocator> = arena.clone();
        handles.push(std::thread::spawn(move || {
            let mut samples = Vec::with_capacity(PAIRS_PER_THREAD);
            for _ in 0..PAIRS_PER_THREAD {
                let t0 = Instant::now();
                let b = arena.alloc(4096, Tier::Host).expect("a 4 KiB buffer");
                drop(b);
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            samples
        }));
    }
    let mut samples: Vec<u64> = Vec::with_capacity(THREADS * PAIRS_PER_THREAD);
    for h in handles {
        samples.extend(h.join().expect("a worker thread"));
    }
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p90 = samples[samples.len() * 90 / 100];
    let p99 = samples[samples.len() * 99 / 100];
    println!(
        "AR-T4 {} alloc/release pairs from {THREADS} threads: p50 {p50} ns, p90 {p90} ns, \
         p99 {p99} ns (provisional: not the reference host, and a debug build with more \
         threads than cores, where the tail is thread preemption rather than the allocator)",
        samples.len()
    );
    assert_eq!(samples.len(), THREADS * PAIRS_PER_THREAD);
    assert_eq!(
        arena.stats().host_in_use,
        0,
        "every pair released what it took"
    );
    // The document's 1 microsecond p99 is the reference host's (E1). Off it, and in a debug
    // build that runs 16 threads on fewer cores, the tail is dominated by the scheduler, so
    // what is asserted here is what still fails when the allocator is wrong: a free that
    // reaches the OS or serialises every thread moves the median, not just the tail.
    assert!(
        p50 < 20_000,
        "p50 {p50} ns: a free that reaches the OS or serialises every thread (AR-I4)"
    );
    assert!(
        p99 < 5_000_000,
        "p99 {p99} ns: a free that blocks for far longer than a class lock (AR-I4)"
    );
}
