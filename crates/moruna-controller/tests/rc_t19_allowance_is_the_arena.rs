//! RC-T19 allowance_is_the_arena and RC-T20 resume_without_a_profile, of
//! `architecture/sdd/11-controller.md` section k.
//!
//! RC-T19 proves f.1's rule: the host allowance is the arena's capacity, which the facade
//! sized at `ceiling - baseline - reserve - kernel state`, and the baseline is not subtracted
//! again. Under the rule this branch replaces, an arena sized at the budget and a baseline
//! sampled after it left the controller nothing at any ceiling.
//!
//! RC-T20 proves f.14's fallback: a resumed run whose plan the interrupted run consumed cannot
//! probe, and that ends the run only on a fresh one.

mod common;

use common::{GIB, MIB, config_with_baseline, kernel, morsel_targets, probe, steady};
use moruna_kernel::KernelHints;
use moruna_testkit::{FakeKnobs, FakeSampler};

const CEILING: u64 = 8 * GIB;
const BASELINE: u64 = 400 * MIB;
const WORKERS: u16 = 8;

/// The message the scheduler's source drive answers a probe with when the cursor is at the end
/// of the plan (SC f.9); f.14 keys its fallback off it.
const EXHAUSTED: &str = "the source plan is exhausted; there is nothing to probe with";

#[test]
fn rc_t19_allowance_is_the_arena() {
    let cfg = config_with_baseline(CEILING, WORKERS, BASELINE);
    let arena = cfg.arena_bytes;
    let reserve = (CEILING as f64 * f64::from(cfg.reserve_fraction)) as u64;
    assert_eq!(arena, CEILING - BASELINE - reserve, "the facade's rule");
    let probe_bytes = cfg.probe_bytes;

    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        // The sampler reports the whole arena as resident, which is what it is after 02 f.1
        // touched every page. Under `ceiling - baseline - reserve` this leaves nothing.
        FakeSampler::new().scripted(steady(BASELINE + arena, 8)),
    );

    let budgets = rig.controller.prepare().expect("prepare");
    assert_eq!(
        budgets.host, arena,
        "f.1: the host allowance is the arena's capacity, not the ceiling less a sample that \
         already contains the arena"
    );
    assert_eq!(
        budgets.baseline, BASELINE,
        "f.1: the baseline is the pre-arena figure, reported and not subtracted"
    );
    assert_eq!(budgets.reserve, reserve);

    rig.controller.probe_all().expect("probe_all");
    rig.controller.start().expect("start");
    let target = morsel_targets(&rig.writes())
        .last()
        .map(|(_, bytes)| *bytes)
        .expect("a morsel target after start");
    assert!(
        target > 0,
        "f.3: an arena of {arena} bytes sizes a morsel; under the old rule it sized none"
    );
    rig.controller.stop();
}

#[test]
fn rc_t20_resume_without_a_profile() {
    let hinted = KernelHints {
        expected_amplification: Some(6.0),
        ..KernelHints::default()
    };

    // A resumed run: the probe cannot run, and the stage is seeded from its hint instead.
    let knobs = FakeKnobs::new();
    let rig = common::Rig::with_prober(
        config_with_baseline(CEILING, WORKERS, BASELINE),
        vec![kernel(1, hinted.clone())],
        knobs.clone(),
        FakeSampler::new().scripted(steady(BASELINE, 8)),
        common::RecordingProber::failing(knobs, EXHAUSTED),
    );
    rig.controller.prepare().expect("prepare");
    rig.controller
        .probe_missing()
        .expect("f.14: an exhausted plan does not end a resumed run");
    rig.controller.start().expect("start");
    let notes = rig.controller.summary().notes;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("stage 1") && n.contains("the plan is exhausted")),
        "f.14: the note names the stage and says why: {notes:?}"
    );
    assert!(
        morsel_targets(&rig.writes())
            .last()
            .is_some_and(|(_, bytes)| *bytes > 0),
        "the run is sized from the hint and continues"
    );
    rig.controller.stop();

    // A fresh run: the same answer is a plan error, because a fresh run with nothing to read
    // is a plan error.
    let knobs = FakeKnobs::new();
    let fresh = common::Rig::with_prober(
        config_with_baseline(CEILING, WORKERS, BASELINE),
        vec![kernel(1, hinted)],
        knobs.clone(),
        FakeSampler::new().scripted(steady(BASELINE, 8)),
        common::RecordingProber::failing(knobs, EXHAUSTED),
    );
    fresh.controller.prepare().expect("prepare");
    let error = fresh
        .controller
        .probe_all()
        .expect_err("a fresh run keeps the plan error");
    assert!(error.to_string().contains("nothing to probe with"));
}
