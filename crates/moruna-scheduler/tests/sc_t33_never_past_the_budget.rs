//! SC-T33 never_past_the_budget (10 f.20, placement PL-I22, adaptive-runtime 12.5): under random
//! DAGs (one or two sources, one to three stages, fan-out and fan-in), random batch sizes, stages
//! whose output is a random factor of their input that no prediction can know (0.25 to 2 times,
//! sometimes skewed to the ends), a working set of several budgets, one to three workers, and
//! every registered policy, a run through the scheduler and the real placement engine completes,
//! every sink gets every batch its paths give it exactly once, the catalog's resident bytes never
//! pass the budget (its peak sees every change), and no push is made past its claim. Where an
//! output outgrows what was claimed and nothing can leave memory, the batch goes straight to disk.
//! A sixth of the cases sit just above the run's floor (every queue's window and one unit) and
//! must finish; a sixth sit just under it and must be refused before anything runs, naming it.
//! Every policy runs each case twice: on measured ratios, and with a cost model whose every
//! prediction is wrong by a factor drawn between 1/1024 and 1024, which changes to another such
//! factor part way through the run (a model wrong by any factor, drifting mid-run); with the
//! model, every unit the run finished is learned and kept in the store.
//!
//! `MORUNA_SC_T33_CASES` (default 12) and `MORUNA_SC_T33_SEED` set the run; a value that does not
//! parse fails the test.

mod common;

use std::time::Duration;

use moruna_testkit::dag::{Draw, Rng, random_topology};

use common::{Case, Model, floor_budget, max_batch_bytes, run, scheduler_floor, smallest_batch};

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
fn sc_t33_never_past_the_budget() {
    let seed = setting("MORUNA_SC_T33_SEED", 0x5C_7280);
    let cases = setting("MORUNA_SC_T33_CASES", 12);
    println!("MORUNA_SC_T33_SEED={seed} MORUNA_SC_T33_CASES={cases}");
    let policies = moruna_scheduler::policies::standard();
    let names: Vec<String> = policies.names().iter().map(|n| n.to_string()).collect();
    assert_eq!(
        names,
        ["admission", "fifo", "lookahead", "residency_greedy"],
        "every policy of the build"
    );
    let mut rng = Rng::new(seed);
    let mut pressed = 0u64;
    let mut overflowed = 0u64;
    let mut runs = 0u64;
    let mut refused = 0u64;
    let mut drifted = 0u64;
    let mut modelled = 0u64;
    for case_index in 0..cases {
        let topology = random_topology(&mut rng, 2, 3);
        let rows = 16 + rng.below(48);
        let workers = 1 + rng.below(3) as u16;
        let mut case = Case {
            splits: Vec::new(),
            factors: (250, 1000 + rng.below(1001)),
            stage_factors: Vec::new(),
            skewed: rng.below(3) == 0,
            kernel_latency: Duration::ZERO,
            io_latency: if rng.below(4) == 0 {
                Duration::from_micros(50)
            } else {
                Duration::ZERO
            },
            workers,
            budget: 0,
            window: 1,
            policy: String::new(),
            model: Model::None,
            topology,
        };
        // The budget is the floor a run always makes progress in, plus a little; each source
        // reads enough splits that its data is several budgets.
        case.splits = vec![(1, rows); usize::from(case.topology.sources())];
        // Three kinds of case. Most give a budget at the floor a run always makes progress in
        // at its largest batch. A sixth sit just above the scheduler's own floor, the windows
        // and one unit at the smallest batch, with stages that never grow their input, so the
        // largest batch is the smallest: memory is then barely enough, outputs often go
        // straight to disk, and the run must still finish (heads on disk come back by taking
        // the room of batches used later). A sixth go just under it and must be refused before
        // anything runs, naming the floor.
        let kind = rng.below(6);
        if kind >= 4 {
            case.factors = (250, 1000);
        }
        let budget = match kind {
            4 => scheduler_floor(&case) + rng.below(smallest_batch(&case)),
            5 => scheduler_floor(&case) - 1 - rng.below(smallest_batch(&case)),
            _ => floor_budget(&case) + rng.below(max_batch_bytes(&case)),
        };
        let splits = ((3 * budget) / (rows * 8)).clamp(8, 600) as u32;
        case.splits = vec![(splits, rows); usize::from(case.topology.sources())];
        case.budget = budget;
        // A factor between 1/1024 and 1024, and another it changes to after some units.
        let factor = |rng: &mut Rng| 2f64.powi(rng.below(21) as i32 - 10);
        let wrong = Model::Wrong {
            first: factor(&mut rng),
            then: factor(&mut rng),
            after: rng.below(u64::from(splits)),
        };
        for (name, model) in names.iter().flat_map(|n| [(n, Model::None), (n, wrong)]) {
            case.policy = name.clone();
            case.model = model;
            let label = format!("sc_t33-{case_index}-{name}-{}", model.name());
            let outcome = run(&case, &label);
            if kind == 5 {
                match &outcome.refused {
                    Some(refusal) => assert!(
                        refusal.contains("floor") && refusal.contains("promotion windows"),
                        "{label}: the refusal names the floor and its terms: {refusal}"
                    ),
                    None => panic!(
                        "{label}: a budget under the floor ran: {:?}",
                        outcome.correct
                    ),
                }
                refused += 1;
                continue;
            }
            if let Err(e) = &outcome.correct {
                panic!(
                    "{e}\n  case {case:?}\n  catalog {:?}\n  policy {:?}",
                    outcome.catalog, outcome.policy
                );
            }
            assert_eq!(
                outcome.policy.name, *name,
                "{label}: the setting chose the policy"
            );
            runs += 1;
            if let Some(learning) = &outcome.policy.learning {
                modelled += 1;
                if !learning.drifts.is_empty() {
                    drifted += 1;
                }
            }
            if outcome.catalog.evictions > 0 || outcome.catalog.overflows > 0 {
                pressed += 1;
            }
            if outcome.catalog.overflows > 0 {
                overflowed += 1;
            }
        }
    }
    println!(
        "runs {runs}: under pressure {pressed}, overflowed to disk {overflowed}; refused at \
         start under the floor {refused}; with the wrong model {modelled}, of which drifting \
         {drifted}"
    );
    assert!(
        modelled * 2 == runs,
        "half the runs had the wrong model: {modelled} of {runs}"
    );
    assert!(
        drifted * 2 >= modelled,
        "the wrong model was found drifting in only {drifted} of {modelled} runs"
    );
    assert!(
        pressed * 2 >= runs,
        "only {pressed} of {runs} runs had to free memory: the cases did not press"
    );
}
