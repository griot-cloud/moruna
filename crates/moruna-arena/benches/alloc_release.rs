//! Allocate and release on the hot path (02 f.3, e.3): what one buffer costs, allocated and
//! dropped again, with one thread and with eight doing it at once, as every worker does per
//! buffer it makes. Each case prints the time per allocation and release, the median of five
//! repeats. `cargo bench -p moruna-arena --bench alloc_release`.
//!
//! - `64 KiB`: every thread the smallest class, the common size of an Arrow buffer's slot;
//! - `1 MiB`: every thread a larger class;
//! - `a class each`: each thread its own class (64 KiB to 8 MiB), so no two share a class's
//!   free list, and what is left in common is the region list and the arena's counters.

use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::time::Instant;

use moruna_arena::{Arena, ArenaConfig};
use moruna_kernel::{Allocator, Guarantee, Tier};

const THREADS: [usize; 2] = [1, 8];
const PAIRS: usize = 500_000;
const REPEATS: usize = 5;

fn arena() -> Arc<Arena> {
    Arena::new(ArenaConfig {
        host_bytes: 1 << 30,
        page_bytes: 4096,
        huge_pages: Guarantee::Probed(false),
        memlock: Guarantee::Absent,
        ..ArenaConfig::default()
    })
    .expect("an arena")
}

/// Nanoseconds per allocation and release with `threads` threads at once, thread `t` asking for
/// `size(t)` bytes: the median of `REPEATS` runs.
fn per_pair(arena: &Arc<Arena>, threads: usize, size: fn(usize) -> usize) -> f64 {
    let mut runs = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let start = Arc::new(Barrier::new(threads + 1));
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                let (arena, start) = (Arc::clone(arena), Arc::clone(&start));
                let bytes = size(t);
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..PAIRS {
                        let buffer = arena.alloc(black_box(bytes), Tier::Host).expect("room");
                        drop(black_box(buffer));
                    }
                })
            })
            .collect();
        start.wait();
        let t = Instant::now();
        for w in workers {
            w.join().expect("a worker");
        }
        runs.push(t.elapsed().as_nanos() as f64 / PAIRS as f64);
    }
    runs.sort_by(f64::total_cmp);
    runs[REPEATS / 2]
}

fn line(arena: &Arc<Arena>, size: fn(usize) -> usize) -> String {
    THREADS
        .iter()
        .map(|&t| format!("{:9.2}", per_pair(arena, t, size)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let arena = arena();
    // Warm every class the cases use, so no run claims a slab while it is timed.
    let warm: Vec<_> = (0..8)
        .flat_map(|c| (0..8).map(move |_| (64 << 10) << c))
        .map(|bytes| arena.alloc(bytes, Tier::Host).expect("room"))
        .collect();
    drop(warm);
    println!(
        "alloc + release, {PAIRS} a thread, median of {REPEATS}: ns per pair per thread, 1 thread and 8"
    );
    println!("64 KiB        {}", line(&arena, |_| 64 << 10));
    println!("1 MiB         {}", line(&arena, |_| 1 << 20));
    println!("a class each  {}", line(&arena, |t| (64 << 10) << t));
}
