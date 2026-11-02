//! RC-T22 the_model_sizes_the_morsel (f.16; adaptive-runtime G4; SDD 24 f.10): with a cost model,
//! the `model` sizer proposes the morsel the model's `morsel` sizes, inside the envelope, so the
//! controller has nothing to clamp; it honours damping; its predicted peak is the model's once
//! the model has measured peaks, and none before; with no row width known it keeps the target.

use std::sync::Arc;

use moruna_controller::{Envelope, ModelSizer, Observation, PlanSummary, Sizer, SizerOutcome};
use moruna_cost::{
    CostKey, Defaults, Digest, InUse, Inputs, LocalStore, LogLinear, ObservationStore, Online,
    Origin, Outcome, Target,
};
use moruna_job::Role;
use moruna_kernel::MorselFeatures;

const WIDTH: u64 = 64;

fn key() -> CostKey {
    CostKey {
        role: Role::Kernel,
        kind: "std".into(),
        fingerprint: Digest([3; 32]),
        settings: Digest([0; 32]),
        schema: Digest([0; 32]),
    }
}

/// A model that has seen `units` units of a kernel paying a millisecond a batch and 100 ns a
/// row, peaking at the batch's bytes, when `peaks`.
fn online(dir: &std::path::Path, units: u64, peaks: bool) -> Arc<Online> {
    let store: Arc<dyn ObservationStore> = Arc::new(LocalStore::open(dir).expect("opens"));
    let online = Online::new(
        InUse::new(Box::new(LogLinear::new(Defaults::standard())), None),
        store,
    );
    let k = key();
    let mut x = 0x5EED_u64;
    for n in 0..units {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let rows = 1_000 + (x >> 33) % 99_000;
        let inputs = Inputs::batch(rows, rows * WIDTH);
        let prediction = online.predict(&k, &inputs);
        let mut outcome = Outcome::new(Origin::Run);
        outcome.set(Target::Duration, 1e-3 + 1e-7 * rows as f64);
        if peaks {
            outcome.set(Target::PeakMemory, (rows * WIDTH) as f64);
        }
        online
            .settle(&k, &inputs, Some(&prediction), &outcome, n)
            .expect("settles");
    }
    Arc::new(online)
}

fn observation(target: u64, ready: bool) -> Observation {
    Observation {
        stage: 1,
        features: MorselFeatures::default(),
        active_workers: 4,
        a_k: 1.0,
        safety: 1.5,
        target,
        completions_since_adjust: if ready { 8 } else { 0 },
        damping: 8,
        recent: Vec::new(),
    }
}

fn plan(rows: u64) -> PlanSummary {
    PlanSummary {
        total_bytes: rows * WIDTH,
        total_rows: rows,
        splits: 10,
        max_split_bytes: rows * WIDTH / 10,
        sub_splittable_all: true,
    }
}

#[test]
fn rc_t22_the_model_sizes_the_morsel() {
    let dir = std::env::temp_dir().join(format!("moruna-rc-t22-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let model = online(&dir.join("learned"), 200, true);
    let mut sizer = ModelSizer::new(Arc::clone(&model), key(), &plan(10_000_000));
    assert_eq!(sizer.name(), "model");
    let wide = Envelope {
        min: 64 << 10,
        max: 256 << 20,
    };
    let p = sizer.propose(&observation(1 << 20, true), &wide);
    let expected = model.morsel(
        &key(),
        &Inputs::batch(65_536, 65_536 * WIDTH),
        10_000_000,
        wide.max as f64,
        4,
    );
    assert_eq!(
        p.morsel_target,
        expected.rows * WIDTH,
        "the model's morsel: {expected:?}"
    );
    assert_eq!(
        wide.clamp(p.morsel_target),
        (p.morsel_target, false),
        "nothing to clamp"
    );
    // A millisecond a batch against 100 ns a row: the overhead asks for about 190,000 rows.
    assert!(
        (150_000..250_000).contains(&expected.rows),
        "the overhead sets it: {expected:?}"
    );
    let peak = p.predicted_peak.expect("the model measured peaks") as f64;
    let bytes = p.morsel_target as f64;
    assert!(
        (peak / bytes - 1.0).abs() < 0.25,
        "peak {peak} for {bytes} bytes"
    );
    assert!(sizer.confidence() >= 1.0);
    sizer.observe(
        &observation(1 << 20, true),
        &SizerOutcome {
            peak_delta: 1,
            bytes_in: 1,
            wall_ns: 1,
        },
    );

    // A tight envelope: the memory one unit may take bounds it, and it stays inside.
    let tight = Envelope {
        min: 64 << 10,
        max: 1 << 20,
    };
    let p = sizer.propose(&observation(1 << 20, true), &tight);
    assert!(
        p.morsel_target <= tight.max && p.morsel_target >= tight.min,
        "{p:?}"
    );

    // Damping is honoured.
    let p = sizer.propose(&observation(777, false), &wide);
    assert_eq!((p.morsel_target, p.predicted_peak), (777, None));

    // A model that never measured a peak predicts none.
    let blind = online(&dir.join("blind"), 50, false);
    let mut sizer = ModelSizer::new(blind, key(), &plan(10_000_000));
    let p = sizer.propose(&observation(1 << 20, true), &wide);
    assert_eq!(p.predicted_peak, None);
    assert!(sizer.confidence() > 0.0 && sizer.confidence() < 1.0 + f32::EPSILON);

    // No width known: the target stands.
    let mut sizer = ModelSizer::new(Arc::clone(&model), key(), &plan(0));
    let p = sizer.propose(&observation(4096, true), &wide);
    assert_eq!(p.morsel_target, 4096);
    // ... unless the last morsel says it.
    let mut obs = observation(4096, true);
    obs.features.rows = 1_000;
    obs.features.bytes = 64_000;
    let p = sizer.propose(&obs, &wide);
    assert!(
        p.morsel_target >= wide.min && p.morsel_target != 4096,
        "{p:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
