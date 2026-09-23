//! RC-T5 oscillation_freeze. A sizer that changes its mind on every adjustment is frozen at
//! the geometric mean of its last ten targets for `controller.freeze_morsels` completions, and
//! the target stops moving for that long. Proves RC-I5 and, with it, S11: the controller is
//! stable, because a controller that cannot converge is stopped from deciding rather than left
//! to oscillate.

mod common;

use std::sync::{Arc, Mutex};

use common::{GIB, MIB, config, kernel, morsel_targets, probe, record, steady};
use moruna_controller::{Envelope, Observation, Proposal, Sizer, SizerOutcome};
use moruna_kernel::KernelHints;
use moruna_testkit::{FakeKnobs, FakeSampler};

/// A sizer that alternates: twice the target, then half of it, for ever. The alternation is
/// produced here rather than hoped for out of the rule sizer's smoothing, because what RC-I5 is
/// about is the controller's response to it and not the cause.
struct Oscillator {
    up: bool,
}

impl Sizer for Oscillator {
    fn propose(&mut self, obs: &Observation, _envelope: &Envelope) -> Proposal {
        self.up = !self.up;
        Proposal {
            morsel_target: if self.up {
                obs.target.saturating_mul(2)
            } else {
                obs.target / 2
            },
            predicted_peak: None,
        }
    }

    fn observe(&mut self, _obs: &Observation, _outcome: &SizerOutcome) {}

    fn confidence(&self) -> f32 {
        0.0
    }

    fn name(&self) -> &'static str {
        "oscillator"
    }
}

#[test]
fn rc_t5_oscillation_freeze() {
    // One worker, so the damping is one and every tick may adjust: the test is about the
    // flips, not about how long they take to accumulate.
    let cfg = config(8 * GIB, 1);
    let probe_bytes = cfg.probe_bytes;
    let flips_allowed = cfg.oscillation_flips;
    let freeze_morsels = cfg.freeze_morsels;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 256)),
    );
    rig.controller
        .set_sizer_factory(Arc::new(|_stage| Box::new(Oscillator { up: false })));
    rig.run_up();

    let written = Arc::new(Mutex::new(vec![
        morsel_targets(&rig.writes())
            .last()
            .map(|(_, bytes)| *bytes)
            .expect("a target after start"),
    ]));

    // Drive until the freeze fires, keeping the sequence of targets so the geometric mean can
    // be recomputed here.
    let mut frozen_at = None;
    let mut seq = 1u64;
    for _ in 0..(flips_allowed + 8) {
        let target = *written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .expect("a target");
        rig.feed(&record(seq, 1, target, target * 4));
        seq += 1;
        let mark = rig.writes().len();
        rig.controller.tick_once();
        let fresh = morsel_targets(&rig.writes()[mark..]);
        if rig.controller.summary().freezes > 0 {
            frozen_at = fresh.last().map(|(_, bytes)| *bytes);
            break;
        }
        for (_, bytes) in fresh {
            written
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(bytes);
        }
    }

    let frozen_at = frozen_at.expect("RC-I5: the oscillation froze the target");
    let history = written.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let last_ten: Vec<u64> = history.iter().rev().take(10).rev().copied().collect();
    let logs: f64 = last_ten.iter().map(|bytes| (*bytes as f64).ln()).sum();
    let expected = (logs / last_ten.len() as f64).exp() as u64;
    assert_eq!(
        frozen_at, expected,
        "RC-I5: the freeze is at the geometric mean of the last ten targets {last_ten:?}"
    );

    // Frozen means frozen: the target does not move again until the freeze has run its course.
    for _ in 0..(freeze_morsels / 2) {
        rig.feed(&record(seq, 1, frozen_at, frozen_at * 4));
        seq += 1;
        let mark = rig.writes().len();
        rig.controller.tick_once();
        assert!(
            morsel_targets(&rig.writes()[mark..]).is_empty(),
            "RC-I5: a frozen stage takes no proposal"
        );
    }

    let summary = rig.controller.stop();
    assert_eq!(summary.freezes, 1, "one freeze, counted for the report");
    assert!(
        summary
            .notes
            .iter()
            .any(|note| note == "oscillation freeze on stage 1"),
        "the freeze is in the report: {:?}",
        summary.notes
    );
}
