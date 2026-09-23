//! RC-T16 state_growth. A kernel whose state grows with the morsels it has seen is the case
//! the linear model `a_k x bytes_in` cannot describe: the peak per morsel stays where the probe
//! measured it while the process walks into the ceiling anyway. `KernelState::footprint`
//! reaches the controller as `TraceRecord::state_bytes`, the state term of f.3 funds it out of
//! the morsels and the workers, and a state that has taken more than half the budget on its own
//! is a run that is diagnosed rather than killed. Proves f.3's state term and the StateGrowth
//! row of f.6.

mod common;

use moruna_kernel::{KernelHints, SchedulerStats, StageStats};
use moruna_testkit::{FakeKnobs, FakeSampler};
use common::{GIB, MIB, morsel_targets, probe, record, sample, stateful_kernel};

const CEILING: u64 = 8 * GIB;
/// What one instance holds when the first record arrives. It grows by half again on every
/// round, because the StateGrowth row of f.6 is written about a state that is growing (more
/// than a tenth per tick) and not about one that has merely grown: a kernel accumulating a
/// fixed number of bytes per morsel falls below that rate on its own and is caught later by
/// the memory row and the breach path instead.
const INITIAL_STATE: u64 = 64 * MIB;

#[test]
fn rc_t16_state_growth() {
    let baseline = 400 * MIB;
    let cfg = common::config_with_baseline(CEILING, 4, baseline);
    let probe_bytes = cfg.probe_bytes;
    let morsel_min = cfg.morsel_min;
    let reserve = (CEILING as f64 * f64::from(cfg.reserve_fraction)) as u64;
    let host_budget = CEILING - baseline - reserve;
    // Anonymous memory rises with the state, which is what the memory rows of f.6 look at.
    let over_the_line = CEILING - reserve / 2 + MIB;

    let stats = SchedulerStats {
        per_stage: vec![StageStats {
            stage: 1,
            tasks: 0,
            busy_ns: 0,
            errors: 0,
            skipped: 0,
            instances_live: 2,
        }],
        workers_active: 4,
        workers_busy: 4,
        ..SchedulerStats::default()
    };
    let mut samples = vec![sample(baseline, 1_000)];
    for at in 1..512u64 {
        samples.push(sample(over_the_line, 1_000 + at * 1_000));
    }

    let rig = common::Rig::new(
        cfg,
        vec![stateful_kernel(1, 2, KernelHints::default())],
        FakeKnobs::new()
            .probe_result(1, probe(probe_bytes, 2.0))
            .stats(stats),
        FakeSampler::new().scripted(samples),
    );
    rig.run_up();

    let first_target = morsel_targets(&rig.writes())
        .last()
        .map(|(_, bytes)| *bytes)
        .expect("a target after start");
    let first_budgets = common::last_budgets(&rig.placement).expect("prepare set budgets");
    let mut saw_state_growth = false;
    let mut seq = 1u64;
    let mut held = INITIAL_STATE;

    while rig.knobs.terminated().is_none() && seq < 256 {
        // Two instances, each reporting a larger footprint than the last time round.
        for instance in 0..2u16 {
            let mut growing = record(seq, 1, 4 * MIB, 8 * MIB);
            growing.instance = instance;
            growing.state_bytes = held;
            growing.mem_anon_before = baseline;
            growing.mem_anon_peak = baseline + 8 * MIB;
            rig.feed(&growing);
            seq += 1;
        }
        rig.controller.tick_once();
        if rig
            .controller
            .summary()
            .timeline
            .iter()
            .any(|(_, class)| *class == moruna_controller::Bottleneck::StateGrowth)
        {
            saw_state_growth = true;
        }
        held = held * 3 / 2;
    }

    assert!(saw_state_growth, "f.6: the StateGrowth row fired");

    let targets = morsel_targets(&rig.writes());
    let last_target = targets.last().map(|(_, bytes)| *bytes).expect("a target");
    assert!(
        last_target < first_target,
        "f.3: the targets shrink to fund the state ({first_target} to {last_target})"
    );
    assert!(
        targets.iter().all(|(_, bytes)| *bytes >= morsel_min),
        "no target is written below the floor: {targets:?}"
    );

    let last_budgets = common::last_budgets(&rig.placement).expect("budgets were written");
    assert!(
        common::host_pool(&last_budgets) < common::host_pool(&first_budgets),
        "f.3: the placement half ends smaller than it started"
    );

    let diagnostic = rig
        .knobs
        .terminated()
        .expect("a state that takes more than half the budget on its own terminates the run");
    assert!(
        diagnostic.starts_with("budget: morsel ") && diagnostic.contains("stage 1"),
        "G-I8: the diagnostic names the stage whose state grew: {diagnostic}"
    );
    assert!(
        diagnostic.contains(&format!("exceeds budget {}", host_budget / 2)),
        "the diagnostic carries the half the state was allowed: {diagnostic}"
    );
    rig.controller.stop();
}
