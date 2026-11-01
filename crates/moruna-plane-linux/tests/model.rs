//! KA2: the port's conformance suite against the kernel's adapter, `KernelPlane`, over the host
//! model of the kernel's calls, with the fixtures the Linux run moves (DP-T12's refusals, KA10's
//! bombs among them). The same suite runs against `LinuxPlane` in `conformance.rs`.

mod support;

use moruna_plane::conformance;
use moruna_plane::model::ModelHarness;

#[test]
fn ka2_the_suite_passes_on_the_kernel_adapter() {
    let fixtures = support::fixtures();
    assert!(fixtures.refusals.len() > 40, "{}", fixtures.refusals.len());
    let cases = conformance::check(&mut ModelHarness::default(), &fixtures)
        .unwrap_or_else(|failed| panic!("the kernel adapter:\n{failed}"));
    println!(
        "ka2 conformance, kernel adapter (host model): {cases} cases passed, {} records refused",
        fixtures.refusals.len()
    );
}
