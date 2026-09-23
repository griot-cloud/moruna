//! RC-T15 safety_from_evidence. The safety margin is a function of evidence, not a constant: a
//! kernel seen forty thousand times with a steady ratio starts near the floor, one seen sixteen
//! times starts where an unseen one does, one seen often but erratically stays well above the
//! floor, and a probe that disagrees with the store puts the margin back to the beginning. The
//! margin is read back out of the morsel target the controller solves for, because the target
//! is the only place a margin can show. Proves f.2.

mod common;

use common::{GIB, MIB, Scratch, config, kernel, morsel_targets, probe, steady};
use moruna_kernel::{Fingerprint, KernelHints};
use moruna_testkit::{FakeKnobs, FakeSampler};

const CEILING: u64 = 8 * GIB;
const BASELINE: u64 = 400 * MIB;
const WORKERS: u16 = 8;
/// What the probe measures, and what a profile that has not drifted stores.
const AMPLIFICATION: f64 = 4.0;

fn profile_path(dir: &std::path::Path) -> std::path::PathBuf {
    let fingerprint = Fingerprint::compute("test kernel 1", b"");
    let schema: String = (0..32).map(|_| "01".to_string()).collect();
    dir.join(format!("{}-{schema}.json", fingerprint.to_hex()))
}

fn write_profile(dir: &std::path::Path, p95: f64, samples: u64, variance: f64) {
    let stored = serde_json::json!({
        "version": 1,
        "fingerprint": "",
        "schema_hash": "",
        "updated": "2026-09-22T00:00:00Z",
        "a_k_p50": p95,
        "a_k_p95": p95,
        "a_k_dev_p95": 0.0,
        "a_k_samples": samples,
        "a_k_var": variance,
        "state_bytes_max": 0,
        "final_target": 64 * MIB,
        "final_workers": 4,
        "final_safety": 1.2,
        "runs": 3,
        "prediction_error_p95": 0.1
    });
    std::fs::write(dir, serde_json::to_string(&stored).expect("json")).expect("write");
}

/// The safety the controller must have used, read back out of the target it solved for.
///
/// f.3 solves two inequalities and the target is the smaller of the two answers. The binding one
/// here is the anon inequality, `target = anon_headroom / (W x a_anon x safety)`: the arena is
/// `ceiling - baseline - reserve`, so what the process has above it is the reserve, and at this
/// ceiling that is 819 MiB against a worker half of 3.5 GiB. The margin is what is left over
/// either way; only the numerator changes.
fn implied_safety((target, workers): (u64, u16)) -> f64 {
    let reserve = (CEILING as f64 * 0.10) as u64;
    let arena = CEILING - BASELINE - reserve;
    // Every morsel in flight carries the fit's fixed term with it (11 f.3), and the probe seeds
    // that term with the whole of its own growth until a real record can separate the two. So
    // the headroom this inversion may attribute to the slope is what is left once the fixed term
    // has been funded for each of the workers, and reading it without subtracting them recovers
    // a safety that was never used (2026-09-23). The probe size is `ControllerConfig::default`'s,
    // which is what `target_with` probes at.
    let fixed = (MIB as f64 * AMPLIFICATION) as u64;
    // And only `KERNEL_ANON_SHARE` of that headroom is the kernels' to be planned into (11 f.3);
    // the rest is what the runtime allocates outside the arena, which is not a term the model
    // may spend on morsels.
    let for_kernels = ((CEILING - BASELINE - arena) as f64 * 0.6) as u64;
    let anon_headroom = for_kernels.saturating_sub(fixed) as f64;
    anon_headroom / (f64::from(workers) * AMPLIFICATION * target as f64)
}

fn target_with(profile: Option<(f64, u64, f64)>, probed: f64) -> (u64, u16) {
    let scratch = Scratch::new("safety");
    // The arena the facade would size for this ceiling and this baseline, which is the
    // controller's whole allowance (11 f.1).
    let mut cfg = common::config_with_baseline(CEILING, WORKERS, BASELINE);
    // A probe of a megabyte rather than the default sixteen. The probe's whole growth seeds the
    // fit's fixed term and that term is charged per morsel in flight (11 f.3), so a sixteen
    // megabyte probe at this amplification funds 512 MiB of fixed cost across eight workers and
    // leaves the slope so little of the headroom that every target lands on `morsel_min`. A
    // target at the floor is a target the margin cannot be read out of, and the margin is what
    // this test is about.
    cfg.probe_bytes = MIB;
    if let Some((p95, samples, variance)) = profile {
        write_profile(&profile_path(scratch.path()), p95, samples, variance);
        cfg.profiles_dir = Some(scratch.path().to_path_buf());
    }
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, probed)),
        FakeSampler::new().scripted(steady(BASELINE, 8)),
    );
    rig.run_up();
    let writes = rig.writes();
    let target = morsel_targets(&writes)
        .last()
        .map(|(_, bytes)| *bytes)
        .expect("a target after start");
    let workers = common::active_workers(&writes)
        .last()
        .copied()
        .expect("a worker count after start");
    rig.controller.stop();
    (target, workers)
}

fn close(got: f64, want: f64, what: &str) {
    assert!(
        (got - want).abs() < 0.02,
        "{what}: expected a safety of about {want}, the target implies {got}"
    );
}

#[test]
fn rc_t15_safety_from_evidence() {
    let initial = f64::from(config(CEILING, WORKERS).safety_initial);
    let floor = f64::from(config(CEILING, WORKERS).safety_floor);

    // No profile: an unseen kernel gets the initial margin.
    let none = implied_safety(target_with(None, AMPLIFICATION));
    close(none, initial, "f.2: no profile");

    // Sixteen samples: four over the square root of sixteen is a whole point of margin, which
    // the clamp holds at the initial value. A kernel seen sixteen times is an unseen kernel.
    let few = implied_safety(target_with(Some((AMPLIFICATION, 16, 0.0)), AMPLIFICATION));
    close(few, initial, "f.2: sixteen samples");

    // Forty thousand samples with a steady ratio: four over two hundred is two hundredths, so
    // the margin sits just above the floor.
    let many = implied_safety(target_with(
        Some((AMPLIFICATION, 40_000, 0.0)),
        AMPLIFICATION,
    ));
    close(many, floor + 0.02, "f.2: forty thousand samples, steady");

    // The same evidence, a ratio that moves: the variance term keeps the margin well above the
    // floor however much evidence there is, because what is being paid for is the spread.
    let noisy = implied_safety(target_with(
        Some((AMPLIFICATION, 40_000, 0.09)),
        AMPLIFICATION,
    ));
    close(
        noisy,
        floor + 0.02 + 2.0 * 0.3 / AMPLIFICATION,
        "f.2: forty thousand, noisy",
    );
    assert!(
        noisy > many && noisy < initial,
        "f.2: noise costs margin without costing all of it: {noisy} between {many} and {initial}"
    );

    // Drift: a probe four times the stored figure is a kernel the store no longer describes,
    // and the margin goes back to where an unseen kernel starts.
    let drifted = implied_safety(target_with(Some((1.0, 40_000, 0.0)), AMPLIFICATION));
    close(drifted, initial, "f.2: drift resets the margin");
}
