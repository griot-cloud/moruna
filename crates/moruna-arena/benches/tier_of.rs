//! `tier_of` on the hot path (02 f.7): what a lookup costs with one thread, which is the lookup
//! alone, and with eight asking at once, as every worker does per buffer it writes through a
//! `BufferView` (contracts d.3). Each case runs that many threads of a fixed number of lookups
//! each and prints the time per lookup, the median of five repeats.
//! `cargo bench -p moruna-arena --bench tier_of`.
//!
//! - `arena`: a pointer in a host region of the arena, the common case;
//! - `foreign`: a pointer the arena does not know, which misses every region;
//! - `arena, growing`: the arena pointer while another thread grows the arena by a region and
//!   gives it back in a loop, so the region list changes under the lookups.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Instant;

use moruna_arena::{Arena, ArenaConfig};
use moruna_kernel::{Allocator, Guarantee, Tier};

const THREADS: [usize; 2] = [1, 8];
const LOOKUPS: usize = 2_000_000;
const REPEATS: usize = 5;

fn arena() -> Arc<Arena> {
    Arena::new(ArenaConfig {
        host_bytes: 256 << 20,
        page_bytes: 4096,
        huge_pages: Guarantee::Probed(false),
        memlock: Guarantee::Absent,
        ..ArenaConfig::default()
    })
    .expect("an arena")
}

/// Nanoseconds per call of `lookup` with `threads` threads calling at once: the median of
/// `REPEATS` runs, each timed from the barrier to the last thread's end.
fn per_lookup_with(threads: usize, lookup: &Arc<dyn Fn() -> Option<Tier> + Send + Sync>) -> f64 {
    let mut runs = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let start = Arc::new(Barrier::new(threads + 1));
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let (lookup, start) = (Arc::clone(lookup), Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..LOOKUPS {
                        black_box(lookup());
                    }
                })
            })
            .collect();
        start.wait();
        let t = Instant::now();
        for w in workers {
            w.join().expect("a worker");
        }
        runs.push(t.elapsed().as_nanos() as f64 / LOOKUPS as f64);
    }
    runs.sort_by(f64::total_cmp);
    runs[REPEATS / 2]
}

/// The time per lookup with one thread and with eight, as a line of the table.
fn per_lookup(lookup: impl Fn() -> Option<Tier> + Send + Sync + 'static) -> String {
    let lookup: Arc<dyn Fn() -> Option<Tier> + Send + Sync> = Arc::new(lookup);
    THREADS
        .iter()
        .map(|&t| format!("{:9.2}", per_lookup_with(t, &lookup)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let arena = arena();
    let buffer = arena.alloc(1 << 20, Tier::Host).expect("a buffer");
    let inside = buffer.host_ptr().expect("a host pointer") as usize + 4096;
    let outside = Box::new([0u8; 4096]);
    let foreign = outside.as_ptr() as usize;
    println!(
        "tier_of, {LOOKUPS} lookups a thread, median of {REPEATS}: ns per lookup per thread, 1 thread and 8"
    );
    let a = Arc::clone(&arena);
    let ask = move |p: usize| a.tier_of(black_box(p as *const u8));
    let at = ask.clone();
    println!("arena           {}", per_lookup(move || at(inside)));
    let at = ask.clone();
    println!("foreign         {}", per_lookup(move || at(foreign)));

    let stop = Arc::new(AtomicBool::new(false));
    let grower = {
        let (arena, stop) = (Arc::clone(&arena), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                if arena.grow(2 << 20).is_ok() {
                    arena.shrink(2 << 20);
                    n += 1;
                }
                std::thread::yield_now();
            }
            n
        })
    };
    let at = ask.clone();
    println!("arena, growing  {}", per_lookup(move || at(inside)));
    stop.store(true, Ordering::Relaxed);
    let grown = grower.join().expect("the grower");
    println!("(the grower added and gave back {grown} regions meanwhile)");
    drop(buffer);
}
