//! RC-T13 throughput. (reference host, E1)
//!
//! With defaults and no tuning, the runtime reaches at least eighty per cent of the hand-tuned
//! baseline for normalise, tokenise-explode and wide-intermediate. It is the test S3 is closed
//! by, and it is a ratio on whatever host it runs on, which is why it is recorded on the
//! reference host and labelled provisional anywhere else (preamble, reference hardware).
//!
//! It is ignored here because it needs what does not exist yet: `moruna-runtime` (the facade,
//! wave 4), the benchmark kernels and, for the figure to be comparable, the hand-tuned
//! baselines the bench agent delivers in wave 5 on the reference host named by E1.

mod common;

#[test]
#[ignore = "reference host, E1: needs moruna-runtime, the bench kernels and the wave 5 tuned baselines"]
fn rc_t13_throughput() {
    // Not a silent pass: a throughput claim nobody has measured is worse than no claim.
    panic!(
        "RC-T13 cannot run yet: moruna-runtime (wave 4), the benchmark kernels and the tuned \
         baselines (wave 5, reference host per E1) do not exist."
    );
}
