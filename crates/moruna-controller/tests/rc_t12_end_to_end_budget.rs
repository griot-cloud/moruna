//! RC-T12 end_to_end_budget. (integration, closes in wave 4; container with `--memory`)
//!
//! All five benchmark kernels complete with peak anonymous memory at or below the ceiling,
//! driven through the Rust facade, and the adversarial kernel either completes or terminates
//! cleanly with the diagnostic of G-I8. It is the test S1, S2 and S6 are closed by, and it is
//! the one that measures the controller against a real ceiling rather than a scripted sampler.
//!
//! It is ignored here because it needs what does not exist yet: `moruna-runtime` (the facade,
//! wave 4), which wires the real scheduler, placement engine, sources and sinks together, and
//! the benchmark kernels of preamble 6.5. The weekly container job runs the ignored set and is
//! what closes this test in wave 4.

mod common;

#[test]
#[ignore = "integration, closes in wave 4: needs moruna-runtime and a container with --memory"]
fn rc_t12_end_to_end_budget() {
    // Deliberately not a silent pass. Until the facade exists there is nothing to run, and a
    // test that reported success would be claiming a budget guarantee nobody has measured.
    panic!(
        "RC-T12 cannot run yet: moruna-runtime (wave 4) and the benchmark kernels of preamble \
         6.5 do not exist. It closes in wave 4, in the weekly container job, under --memory."
    );
}
