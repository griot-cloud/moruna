//! AD-T1 crossing_zero_copy (AD-I1): an identity Python kernel on a 256 MiB batch built over
//! `FakeAllocator` buffers allocates nothing of payload size, copies no payload bytes and makes
//! no boundary copy, because what it returned is the arena memory it was given.

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

mod common;

use std::sync::Arc;

use moruna_kernel::{Allocator, Kernel, NoState, Payload};
use moruna_testkit::FakeAllocator;

const IDENTITY: &str = r#"
def identity(batch):
    return batch
"#;

/// 256 MiB of `i64`s, the size AD-T1 names.
const VALUES: usize = 256 * 1024 * 1024 / 8;

#[test]
fn ad_t1_crossing_zero_copy() {
    let fake = FakeAllocator::new();
    let alloc: Arc<dyn Allocator> = Arc::new(fake.clone());
    let kernel = common::stateless_kernel(IDENTITY, "identity", "ad_t1_identity", alloc);

    let values: Vec<i64> = (0..VALUES as i64).collect();
    let batch = common::arena_i64_batch(&fake, &values);
    let input = Payload::table(batch).expect("the batch is arena owned");
    let input_bytes = input.bytes();
    let input_rows = input.rows();

    let allocations_before = fake.allocations_total();
    let stats_before = fake.stats();

    let mut state = NoState;
    let output = kernel
        .apply(&mut state, input)
        .expect("the identity kernel returns what it was given");

    // AD-I1: the crossing allocated nothing of payload size. The arena is the only place a
    // payload sized allocation could come from, and it saw no call at all.
    assert_eq!(
        fake.allocations_total(),
        allocations_before,
        "a crossing allocated from the arena"
    );
    let stats_after = fake.stats();
    assert_eq!(
        stats_after.payload_copies_total, stats_before.payload_copies_total,
        "a crossing copied payload bytes with the CPU"
    );
    assert_eq!(
        stats_after.boundary_copies_total, stats_before.boundary_copies_total,
        "an arena owned return was copied at the boundary"
    );
    assert_eq!(
        kernel.stats().boundary_copies,
        0,
        "the adapter counted a boundary copy it did not make"
    );

    assert_eq!(output.rows(), input_rows);
    assert_eq!(output.bytes(), input_bytes);
    assert_eq!(output.tier(), fake.host_tier());
    let back = common::i64_column(&output);
    assert_eq!(back.len(), VALUES);
    assert_eq!(back[0], 0);
    assert_eq!(back[VALUES - 1], (VALUES - 1) as i64);
}
