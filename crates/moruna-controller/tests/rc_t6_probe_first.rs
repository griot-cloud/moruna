//! RC-T6 probe_first. No stage is given a morsel target before its probe has run, the probes
//! run once per stage in stage order, and a stage whose kernel asked for a row count is probed
//! at that many rows converted to bytes through the upstream stage's own measurement. Proves
//! RC-I6 and f.2.

mod common;

use common::{MIB, config, kernel, morsel_targets, probe, steady};
use moruna_kernel::KernelHints;
use moruna_testkit::{FakeKnobs, FakeSampler};

#[test]
fn rc_t6_probe_first() {
    let cfg = config(8 * common::GIB, 8);
    let probe_bytes = cfg.probe_bytes;
    let first = probe(probe_bytes, 3.0);
    let knobs = FakeKnobs::new()
        .probe_result(1, first.clone())
        .probe_result(2, probe(probe_bytes, 2.0));
    let preferred_rows = 100_000u64;
    let kernels = vec![
        kernel(1, KernelHints::default()),
        kernel(
            2,
            KernelHints {
                preferred_rows: Some(preferred_rows),
                ..KernelHints::default()
            },
        ),
    ];
    let rig = common::Rig::new(
        cfg,
        kernels,
        knobs,
        FakeSampler::new().scripted(steady(400 * MIB, 4)),
    );

    rig.controller.prepare().expect("prepare");
    assert!(
        morsel_targets(&rig.writes()).is_empty(),
        "RC-I6: no morsel target is written before the probe"
    );

    rig.controller.probe_all().expect("probe_all");
    assert!(
        morsel_targets(&rig.writes()).is_empty(),
        "RC-I6: no morsel target is written while probing either"
    );

    let calls = rig.prober.calls();
    assert_eq!(calls.len(), 2, "one probe per stage");
    assert_eq!(calls[0].0, 1, "stage 1 is probed first");
    assert_eq!(calls[1].0, 2, "stage 2 second");
    assert_eq!(
        calls[0].1, probe_bytes,
        "a stage with no hint is probed at morsel.probe_bytes"
    );

    // f.2: bytes per row for a later stage is the upstream probe's own ratio, because a kernel
    // changes how wide a row is.
    let per_row = first.bytes_in as f64 / first.rows_in as f64;
    let expected = (preferred_rows as f64 * per_row) as u64;
    assert_eq!(
        calls[1].1, expected,
        "stage 2 is probed at its preferred rows"
    );

    rig.controller.start().expect("start");
    assert_eq!(
        morsel_targets(&rig.writes()).len(),
        2,
        "both stages sized after the probes"
    );
    rig.controller.stop();
}

/// RC-I6 from the other side: `start` refuses a controller that has not probed. The order in
/// preamble 4.4 is fixed, but a controller that would size on a hint if the order were not kept
/// is upholding the invariant by convention rather than by construction.
#[test]
fn rc_t6_start_refuses_before_the_probe() {
    let cfg = config(8 * common::GIB, 4);
    let probe_bytes = cfg.probe_bytes;
    let rig = common::Rig::new(
        cfg,
        vec![kernel(1, KernelHints::default())],
        FakeKnobs::new().probe_result(1, probe(probe_bytes, 3.0)),
        FakeSampler::new().scripted(steady(400 * MIB, 4)),
    );
    rig.controller.prepare().expect("prepare");

    let error = rig
        .controller
        .start()
        .expect_err("RC-I6: sizing before the probe is refused");
    let message = error.to_string();
    assert!(
        message.contains("probe_all or probe_missing must run first"),
        "RC-I6: and the error says what is missing: {message}"
    );
    assert!(
        morsel_targets(&rig.writes()).is_empty(),
        "RC-I6: and nothing was written on the way out"
    );

    // The same controller, once probed, starts.
    rig.controller.probe_all().expect("probe_all");
    rig.controller.start().expect("start");
    assert_eq!(
        morsel_targets(&rig.writes()).len(),
        1,
        "sized after the probe"
    );
    rig.controller.stop();
}
