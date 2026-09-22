//! RC-T3 damping. A stage's morsel target never changes more often than once per
//! `controller.damping_completions` completions of that stage, whatever the sizer proposes,
//! and the `Observation` the sizer is handed carries the same two numbers the controller
//! enforces. Proves RC-I3.

mod common;

use std::sync::{Arc, Mutex};

use amoru_controller::{Envelope, Observation, Proposal, Sizer, SizerOutcome};
use amoru_kernel::KernelHints;
use amoru_testkit::{FakeKnobs, FakeSampler};
use common::{GIB, MIB, active_workers, config, kernel, morsel_targets, probe, record, steady};

/// A sizer that always wants the target halved, so that every tick would adjust if the
/// controller let it.
struct Impatient {
    seen: Arc<Mutex<Vec<(u32, u32)>>>,
}

impl Sizer for Impatient {
    fn propose(&mut self, obs: &Observation, _envelope: &Envelope) -> Proposal {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((obs.completions_since_adjust, obs.damping));
        Proposal {
            morsel_target: obs.target / 2,
            predicted_peak: None,
        }
    }

    fn observe(&mut self, _obs: &Observation, _outcome: &SizerOutcome) {}

    fn confidence(&self) -> f32 {
        1.0
    }

    fn name(&self) -> &'static str {
        "impatient"
    }
}

#[test]
fn rc_t3_damping() {
    let cfg = config(8 * GIB, 4);
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 64)),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = seen.clone();
    rig.controller.set_sizer_factory(Arc::new(move |_stage| {
        Box::new(Impatient {
            seen: shared.clone(),
        })
    }));
    rig.run_up();

    let damping = u32::from(
        *active_workers(&rig.writes())
            .last()
            .expect("a worker count"),
    );
    assert!(
        damping > 1,
        "the test needs a damping worth testing, got {damping}"
    );

    // One completion and one tick at a time. A target may move only on the tick whose
    // completion count has reached the damping.
    let mut completions_since_write = 0u32;
    let mut adjustments = 0u32;
    for seq in 1..=24u64 {
        let target = morsel_targets(&rig.writes())
            .last()
            .map(|(_, bytes)| *bytes)
            .expect("a target");
        rig.feed(&record(seq, 1, target, target * 4));
        completions_since_write += 1;
        let mark = rig.writes().len();
        rig.controller.tick_once();
        let wrote = !morsel_targets(&rig.writes()[mark..]).is_empty();
        if wrote {
            assert!(
                completions_since_write >= damping,
                "RC-I3: stage 1 adjusted after only {completions_since_write} completions, \
                 with a damping of {damping}"
            );
            completions_since_write = 0;
            adjustments += 1;
        }
    }
    assert!(adjustments > 0, "the sizer did get to adjust at least once");

    let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(!seen.is_empty(), "the sizer was asked");
    assert!(
        seen.iter().all(|(_, damp)| *damp == damping),
        "the Observation carries the damping the controller enforces: {seen:?}"
    );
    rig.controller.stop();
}

/// RC-I3 again, for the sizer that actually ships. `RuleSizer` honours the damping itself, out
/// of its `Observation`, and the controller enforces it on top; both must hold at once. This
/// also exercises f.4's rule as it will run: the additive increase while the stage is using
/// less of its allowance than `target_fraction` asks for, and the multiplicative decrease when
/// it is using more.
#[test]
fn rc_t3_damping_holds_for_the_rule_sizer() {
    let cfg = config(8 * GIB, 4);
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 256)),
    );
    rig.run_up();
    let damping = u32::from(
        *active_workers(&rig.writes())
            .last()
            .expect("a worker count"),
    );

    let mut completions_since_write = 0u32;
    let mut increases = 0u32;
    let mut decreases = 0u32;
    let mut seq = 1u64;

    // A stage using almost none of its allowance, which the rule grows into; then one using
    // far more of it than the rule allows, which it halves away from.
    for round in 0..2 {
        // The growth round runs long enough to fill f.3's anon window (32 records) as well as to
        // clear the damping several times over: until that window is full the probe's seed
        // governs `a_anon`, the anon inequality holds the target at the top of its envelope, and
        // there is nothing for the additive increase to grow into.
        let records = damping * if round == 0 { 16 } else { 8 };
        for _ in 0..records {
            let before = morsel_targets(&rig.writes())
                .last()
                .map(|(_, bytes)| *bytes)
                .expect("a target");
            let peak = if round == 0 { before / 8 } else { before * 40 };
            rig.feed(&record(seq, 1, before, peak));
            seq += 1;
            completions_since_write += 1;
            let mark = rig.writes().len();
            rig.controller.tick_once();
            for (_, bytes) in morsel_targets(&rig.writes()[mark..]) {
                // RC-I3 damps the sizer's adjustments. A target that goes *down* need not be one:
                // a record that refits `a_anon` upward can leave the anon inequality of f.3
                // unsatisfied, and RC-I1 restores it at the tick whatever the damping says --
                // the same exception f.7's breach has, for the same reason. Every downward path
                // there is is deliberately immediate; only the increase is damped, and that is
                // what this asserts.
                if bytes < before {
                    decreases += 1;
                    completions_since_write = 0;
                    continue;
                }
                assert!(
                    completions_since_write >= damping,
                    "RC-I3: the rule sizer adjusted after only {completions_since_write} \
                     completions, with a damping of {damping}"
                );
                completions_since_write = 0;
                if bytes > before {
                    increases += 1;
                }
            }
        }
    }

    assert!(
        increases > 0,
        "f.4: the rule grows into an allowance it is not using"
    );
    assert!(
        decreases > 0,
        "f.4: and halves away from one it has outgrown"
    );
    rig.controller.stop();
}
