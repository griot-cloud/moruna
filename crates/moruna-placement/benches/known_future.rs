//! PL-B1 known_future (09 f.18): a fan-out/fan-in DAG whose working set is two to four times
//! the memory budget, run once with the catalog's known-future eviction and once with a
//! least-recently-used order, the same seed, the same drive. It prints the bytes each spilled and
//! read back, the evictions, the misses weighted by reload cost, and the finish time.
//!
//! The DAG: source 0 fans out to stages 1, 2 and 3; stages 1 and 2 meet in stage 4 (two ports);
//! stage 3 and stage 4 each give a sink. The source runs ahead of its consumers, as one with
//! read-ahead does, so the working set (three budgets) presses memory at once; it is run with
//! only the catalog's budget holding memory back, and again with the queues' default shares. Every IO the fake reactor does resolves after
//! `MORUNA_PL_B1_IO_US` microseconds (default 100) on a thread of its own, which is the disk this
//! bench has: the finish time is the drive's wall clock over that disk, not a measured NVMe.
//! `MORUNA_PL_B1_BATCHES` (default 400) sets the source's batches and `MORUNA_PL_B1_SEED` the
//! seed. Not a test: nothing here is asserted beyond what the drive checks of every run (the
//! hard rule and every batch to every consumer once).

#[path = "../tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::{Consumer, Link, Producer, Topology};
use moruna_placement::catalog::{Candidate, EvictionOrder, KnownFuture};
use moruna_testkit::FakeAllocator;

use common::dag::{Drive, Outcome, Shape, max_bytes, run};

/// Least recently used first: the comparator, which the engine does not ship.
#[derive(Debug)]
struct Lru;

impl EvictionOrder for Lru {
    fn name(&self) -> &'static str {
        "lru"
    }

    fn order(&self, candidates: &mut [Candidate]) {
        candidates.sort_by_key(|c| (c.touched, c.id));
    }
}

fn setting(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => match value.parse() {
            Ok(parsed) => parsed,
            Err(e) => panic!("{name}={value:?} is not a number: {e}"),
        },
        Err(std::env::VarError::NotPresent) => default,
        Err(e) => panic!("{name}: {e}"),
    }
}

fn topology() -> Topology {
    let k = |stage, port| Consumer::Kernel { stage, port };
    let links = vec![
        Link {
            from: Producer::Source(0),
            to: k(1, 0),
        },
        Link {
            from: Producer::Source(0),
            to: k(2, 0),
        },
        Link {
            from: Producer::Source(0),
            to: k(3, 0),
        },
        Link {
            from: Producer::Kernel(1),
            to: k(4, 0),
        },
        Link {
            from: Producer::Kernel(2),
            to: k(4, 1),
        },
        Link {
            from: Producer::Kernel(3),
            to: Consumer::Sink(0),
        },
        Link {
            from: Producer::Kernel(4),
            to: Consumer::Sink(1),
        },
    ];
    match Topology::new(1, 4, 2, links) {
        Ok(topology) => topology,
        Err(e) => panic!("the bench's DAG: {e}"),
    }
}

fn line(label: &str, outcome: &Outcome) {
    let c = &outcome.catalog;
    println!(
        "{label:<28} spilled {:>10} B in {:>5}  reloaded {:>10} B in {:>5}  evictions {:>5}  \
         misses {:>5} ({:>10} B)  finish {:>8.1} ms",
        c.spilled_bytes,
        c.spills,
        c.reloaded_bytes,
        c.reloads,
        c.evictions,
        c.misses,
        c.miss_reload_bytes,
        outcome.wall.as_secs_f64() * 1e3
    );
}

fn main() {
    let batches = setting("MORUNA_PL_B1_BATCHES", 400);
    let seed = setting("MORUNA_PL_B1_SEED", 0xB1);
    let io = Duration::from_micros(setting("MORUNA_PL_B1_IO_US", 100));
    let rows = (192, 128);
    let claim = max_bytes(&FakeAllocator::new(), rows);
    let shape = Shape {
        topology: topology(),
        batches: vec![batches],
        rows,
    };
    let workers = 2u64;
    let queues = u64::from(shape.topology.queue_count());
    // The source runs ahead, so what is live at once is every batch it reads (each one batch,
    // shared by stages 1 to 3, about four fifths of the largest) and whatever the stages have
    // made of them so far. The budget is a third of the source's data, so the working set is
    // at least three budgets, and never below the floor the drive needs to make progress.
    let working_set = batches * claim * 4 / 5;
    let floor = (queues + 2 * workers + 3) * claim;
    let budget = (working_set / 3).max(floor);
    println!(
        "PL-B1 known_future: {batches} source batches of up to {claim} B, budget {budget} B, \
         working set about {:.1} budgets, IO {} us, seed {seed}",
        working_set as f64 / budget as f64,
        io.as_micros()
    );
    for (water, label) in [
        (Some(budget), "catalog budget only"),
        (None, "per-queue shares"),
    ] {
        println!("-- {label}");
        let mut results = Vec::new();
        for order in [
            Arc::new(KnownFuture) as Arc<dyn EvictionOrder>,
            Arc::new(Lru) as Arc<dyn EvictionOrder>,
        ] {
            let name = order.name();
            let drive = Drive {
                workers: workers as usize,
                budget,
                staging: vec![true; queues as usize],
                order,
                latency: io,
                water,
                sources_first: true,
                window: 1,
            };
            let mut rng = common::Rng::new(seed);
            let outcome = run(&shape, &drive, &mut rng, &format!("b1-{name}"));
            line(name, &outcome);
            results.push(outcome);
        }
        let (known, lru) = (&results[0].catalog, &results[1].catalog);
        println!(
            "known_future / lru: spilled {:.3}, reloaded {:.3}, finish {:.3}",
            known.spilled_bytes as f64 / lru.spilled_bytes.max(1) as f64,
            known.reloaded_bytes as f64 / lru.reloaded_bytes.max(1) as f64,
            results[0].wall.as_secs_f64() / results[1].wall.as_secs_f64()
        );
    }
}
