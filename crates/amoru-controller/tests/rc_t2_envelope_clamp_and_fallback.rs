//! RC-T2 envelope_clamp_and_fallback. This is the test D7 rests on: learning proposes and rules
//! bound. A sizer asking for ten times the envelope is clamped every single time and is
//! replaced by the rule sizer once it has asked twenty times; a sizer that stays inside the
//! envelope but predicts half the peak it gets is replaced when its error passes the shadow
//! rule sizer's by the configured ratio. Either way the run's morsel target never leaves the
//! envelope the budget and the probe fixed, so a confident wrong model cannot walk the process
//! into a breach. Proves RC-I2 and f.8.

mod common;

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use amoru_controller::{Envelope, Observation, Proposal, Sizer, SizerOutcome};
use amoru_kernel::{KernelHints, SizerKind};
use amoru_testkit::{FakeKnobs, FakeSampler};
use common::{GIB, MIB, config, kernel, morsel_targets, probe, record, steady};

/// Asks for ten times the top of the envelope, every time.
struct Greedy {
    calls: Arc<AtomicU64>,
}

impl Sizer for Greedy {
    fn propose(&mut self, _obs: &Observation, envelope: &Envelope) -> Proposal {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Proposal {
            morsel_target: envelope.max.saturating_mul(10),
            predicted_peak: Some(envelope.max),
        }
    }

    fn observe(&mut self, _obs: &Observation, _outcome: &SizerOutcome) {}

    fn confidence(&self) -> f32 {
        1.0
    }

    fn name(&self) -> &'static str {
        "greedy"
    }
}

/// Stays where it is, and predicts half the peak it is told about.
struct Halfwit {
    last_peak: u64,
}

impl Sizer for Halfwit {
    fn propose(&mut self, obs: &Observation, _envelope: &Envelope) -> Proposal {
        Proposal {
            morsel_target: obs.target,
            predicted_peak: Some((self.last_peak / 2).max(1)),
        }
    }

    fn observe(&mut self, _obs: &Observation, outcome: &SizerOutcome) {
        self.last_peak = outcome.peak_delta;
    }

    fn confidence(&self) -> f32 {
        1.0
    }

    fn name(&self) -> &'static str {
        "halfwit"
    }
}

fn rig_with(sizer: impl Fn() -> Box<dyn Sizer> + Send + Sync + 'static) -> common::Rig {
    // One worker, so the damping is one and a proposal is taken every tick.
    let mut cfg = config(8 * GIB, 1);
    cfg.sizer = SizerKind::Learned;
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 128)),
    );
    rig.controller
        .set_sizer_factory(Arc::new(move |_stage| sizer()));
    rig
}

#[test]
fn rc_t2_clamped_sizer_is_replaced() {
    let calls = Arc::new(AtomicU64::new(0));
    let counter = calls.clone();
    let rig = rig_with(move || {
        Box::new(Greedy {
            calls: counter.clone(),
        })
    });
    rig.run_up();
    let envelope_top = morsel_targets(&rig.writes())
        .last()
        .map(|(_, bytes)| *bytes)
        .expect("a target after start");

    for seq in 1..=30u64 {
        rig.feed(&record(seq, 1, envelope_top, envelope_top * 4));
        rig.controller.tick_once();
    }

    // RC-I2: the clamp holds on every single proposal, so no written target ever exceeds what
    // the budget and the probe allow.
    for (_, bytes) in morsel_targets(&rig.writes()) {
        assert!(
            bytes <= envelope_top,
            "RC-I2: a target of {bytes} escaped an envelope topping out at {envelope_top}"
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        20,
        "f.8: the sizer is replaced once it has made twenty proposals, and is not asked again"
    );

    let summary = rig.controller.stop();
    assert_eq!(summary.sizer, "rule", "f.8: the rule sizer takes over");
    assert!(
        summary.fallback_at.is_some(),
        "f.8: the fallback is recorded"
    );
    assert!(
        summary
            .notes
            .iter()
            .any(|note| note.contains("sizer fallback on stage 1")
                && note.contains("clamped too often")),
        "f.8: the report says which sizer went and why: {:?}",
        summary.notes
    );
}

#[test]
fn rc_t2_mispredicting_sizer_is_replaced() {
    let rig = rig_with(|| Box::new(Halfwit { last_peak: 0 }));
    rig.run_up();
    let target = morsel_targets(&rig.writes())
        .last()
        .map(|(_, bytes)| *bytes)
        .expect("a target after start");

    for seq in 1..=30u64 {
        rig.feed(&record(seq, 1, target, target * 4));
        rig.controller.tick_once();
    }

    let summary = rig.controller.stop();
    assert_eq!(
        summary.sizer, "rule",
        "f.8: a sizer that mispredicts is replaced"
    );
    assert!(
        summary.fallback_at.is_some(),
        "f.8: the fallback is recorded"
    );
    assert!(
        summary.notes.iter().any(|note| {
            note.contains("sizer fallback on stage 1")
                && note.contains("prediction error above the rule sizer's")
        }),
        "f.8: and for the other reason: {:?}",
        summary.notes
    );
}

/// The v1 `LearnedSizer` is a stub: it returns the rule sizer's proposal and predicts nothing.
/// f.8 scores a missing prediction as an error of 1.0 on every record, so the shipped learned
/// sizer falls back to the rule sizer of its own accord, which is the whole point of building
/// the fallback before there is a model to fall back from. Selected through `cfg.sizer` alone,
/// with no factory installed, so this is the path a user asking for `sizer = learned` takes.
#[test]
fn rc_t2_the_learned_stub_falls_back_on_its_own() {
    let mut cfg = config(8 * GIB, 1);
    cfg.sizer = SizerKind::Learned;
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 4.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 128)),
    );
    rig.run_up();
    let target = morsel_targets(&rig.writes())
        .last()
        .map(|(_, bytes)| *bytes)
        .expect("a target after start");

    for seq in 1..=30u64 {
        rig.feed(&record(seq, 1, target, target * 4));
        rig.controller.tick_once();
    }

    let summary = rig.controller.stop();
    assert_eq!(
        summary.sizer, "rule",
        "f.8: the stub does not keep the wheel"
    );
    assert!(
        summary.fallback_at.is_some(),
        "f.8: and the report says when it let go"
    );
}
