//! PL-T37 never_past_the_budget_every_batch_once (PL-I18 to PL-I22, adaptive-runtime 12.5): under
//! random DAGs (fan-out, fan-in, one or two sources, one to four stages), random batch sizes, a
//! working set two to four times the memory budget and random arrival orders, with every push
//! admitted under a claim, the bytes the catalog holds and claims never pass the budget (checked
//! after every step, and by the catalog's own peak, which sees every change between steps), and
//! every batch reaches every consumer exactly once, in push order, with its own bytes. Each case
//! draws its promotion window, 1 to 4, so the batches no shed may touch vary too.
//!
//! `MORUNA_PL_T37_CASES` (default 48) and `MORUNA_PL_T37_SEED` set the run; a value that does not
//! parse fails the test rather than falling back.

mod common;

use std::sync::Arc;
use std::time::Duration;

use moruna_placement::catalog::KnownFuture;
use moruna_testkit::FakeAllocator;

use common::dag::{Drive, Shape, max_bytes, run};
use moruna_testkit::dag::random_topology;

/// The widest promotion window a case draws.
const MAX_WINDOW: u64 = 4;

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

#[test]
fn pl_t37_never_past_the_budget_every_batch_once() {
    let seed = setting("MORUNA_PL_T37_SEED", 0xCA7A_1060);
    let cases = setting("MORUNA_PL_T37_CASES", 48);
    println!("MORUNA_PL_T37_SEED={seed} MORUNA_PL_T37_CASES={cases}");
    let mut rng = common::Rng::new(seed);
    let alloc = FakeAllocator::new();
    let mut evicting = 0u64;
    let mut spilling = 0u64;
    let mut dropping = 0u64;
    for case in 0..cases {
        let topology = random_topology(&mut rng, 2, 4);
        let rows = (8 + rng.below(32) as usize, 1 + rng.below(96) as usize);
        let claim = max_bytes(&alloc, rows);
        let workers = 1 + rng.below(3);
        // The promotion window, 1 to MAX_WINDOW: every queue's first `window` batches are hot.
        let window = 1 + rng.below(MAX_WINDOW);
        let queues = u64::from(topology.queue_count());
        // Room for every queue's window, every unit's input and output, and a push.
        let budget = (queues * window + 2 * workers + 3) * claim + rng.below(claim);
        // A working set of two to four budgets: each source batch becomes one batch per stage
        // it passes through, at about half the largest size on average.
        let factor = 2 + rng.below(3);
        let stages = u64::from(topology.stages());
        let per_source =
            (factor * budget / (claim / 2).max(1) / (1 + stages) / u64::from(topology.sources()))
                .max(2);
        // A source's queue may stage or drop and re-read (PL-I6); a kernel output's always stages,
        // as the default has it, because it cannot be read again.
        let staging = (0..topology.queue_count())
            .map(|q| topology.source_of(q).is_none() || rng.below(2) == 0)
            .collect();
        let shape = Shape {
            batches: vec![per_source; usize::from(topology.sources())],
            topology,
            rows,
        };
        let drive = Drive {
            workers: workers as usize,
            budget,
            staging,
            order: Arc::new(KnownFuture),
            latency: Duration::ZERO,
            // Half the cases leave the queues' shares at the default; the other half let every
            // queue hold the whole budget, so the catalog's budget is the only bound.
            water: (case % 2 == 1).then_some(budget),
            sources_first: rng.below(2) == 0,
            window: window as u16,
        };
        let outcome = run(&shape, &drive, &mut rng, &format!("t37-{case}"));
        assert!(
            outcome.takes >= outcome.batches,
            "case {case}: every batch was taken"
        );
        if outcome.catalog.evictions > 0 {
            evicting += 1;
        }
        if outcome.catalog.spills > 0 {
            spilling += 1;
        }
        if outcome.catalog.evictions > outcome.catalog.spills {
            dropping += 1;
        }
    }
    println!("cases {cases}: evicting {evicting}, spilling {spilling}, dropping {dropping}");
    // The property is only worth its cases if memory was really short in most of them.
    assert!(
        evicting * 2 >= cases,
        "only {evicting} of {cases} cases evicted anything: the working set did not press"
    );
    assert!(
        spilling > 0 && dropping > 0,
        "both spills and drops were exercised"
    );
    assert_eq!(moruna_placement::locks::violations(), 0, "lock order (4.2)");
}
