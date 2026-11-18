//! Allocate and release on the hot path (02 f.3, e.3): what one buffer costs, allocated and
//! dropped again, with one thread, four and eight doing it at once, as every worker does per
//! buffer it makes. Each case prints the time per allocation and release, the median of five
//! repeats. `cargo bench -p moruna-arena --bench alloc_release`.
//!
//! - `64 KiB`: every thread the smallest class, the common size of an Arrow buffer's slot;
//! - `1 MiB`: every thread a larger class;
//! - `a class each`: each thread its own class (64 KiB to 8 MiB), so no two share a class's
//!   free list, and what is left in common is the region list and the arena's counters;
//! - `private list`: no arena, each thread popping and pushing a list of its own under a lock of
//!   its own, which shares nothing: what the machine itself does with that many threads, the
//!   floor the arena's rows are read against.

use std::hint::black_box;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Instant;

use moruna_arena::{Arena, ArenaConfig};
use moruna_kernel::{Allocator, Guarantee, Tier};

const THREADS: [usize; 3] = [1, 4, 8];
const PAIRS: usize = 500_000;
const REPEATS: usize = 5;

/// What one thread does: `PAIRS` pairs, as thread `t`.
type Work = Arc<dyn Fn(usize) + Send + Sync>;

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

/// Nanoseconds per pair with `threads` threads doing `work` at once: the median of `REPEATS`
/// runs.
fn per_pair(threads: usize, work: &Work) -> f64 {
    let mut runs = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let start = Arc::new(Barrier::new(threads + 1));
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                let (work, start) = (Arc::clone(work), Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    work(t);
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

fn line(work: Work) -> String {
    THREADS
        .iter()
        .map(|&t| format!("{:9.2}", per_pair(t, &work)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Thread `t` allocates `size(t)` bytes and drops the buffer, `PAIRS` times.
fn alloc_release(arena: &Arc<Arena>, size: fn(usize) -> usize) -> Work {
    let arena = Arc::clone(arena);
    Arc::new(move |t| {
        let bytes = size(t);
        for _ in 0..PAIRS {
            let buffer = arena.alloc(black_box(bytes), Tier::Host).expect("room");
            drop(black_box(buffer));
        }
    })
}

/// Each thread pops and pushes a list of its own under a lock of its own.
fn private_list() -> Work {
    Arc::new(|_| {
        let list = Mutex::new(vec![0u64; 16]);
        for _ in 0..PAIRS {
            let slot = black_box(&list).lock().expect("a list").pop();
            let slot = black_box(slot).expect("a slot");
            black_box(&list).lock().expect("a list").push(slot);
        }
    })
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
        "alloc + release, {PAIRS} a thread, median of {REPEATS}: ns per pair per thread, with {THREADS:?} threads"
    );
    println!(
        "64 KiB        {}",
        line(alloc_release(&arena, |_| 64 << 10))
    );
    println!("1 MiB         {}", line(alloc_release(&arena, |_| 1 << 20)));
    println!(
        "a class each  {}",
        line(alloc_release(&arena, |t| (64 << 10) << t))
    );
    println!("private list  {}", line(private_list()));
}
