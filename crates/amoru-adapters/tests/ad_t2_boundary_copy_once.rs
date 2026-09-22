//! AD-T2 boundary_copy_once (AD-I2): a kernel that returns a new pyarrow batch is copied into
//! the arena exactly once, the bytes are equal, and the copy lands in the run's one host tier.

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

mod common;

use std::sync::Arc;

use amoru_kernel::{Allocator, Kernel, NoState, Payload, Tier};
use amoru_testkit::FakeAllocator;

use common::CountingAllocator;

const DOUBLED: &str = r#"
import pyarrow

def doubled(batch):
    values = [v.as_py() * 2 for v in batch.column(0)]
    return pyarrow.record_batch([pyarrow.array(values, type=pyarrow.int64())], names=["n"])
"#;

fn boundary_copy_once(pinned: bool, expected_tier: Tier) {
    let counting = CountingAllocator::new(FakeAllocator::new().pinned(pinned));
    let alloc: Arc<dyn Allocator> = counting.clone();
    let kernel = common::stateless_kernel(DOUBLED, "doubled", "ad_t2_doubled", alloc);

    let values: Vec<i64> = (0..1024).collect();
    let batch = common::arena_i64_batch(counting.fake(), &values);
    let input = Payload::table(batch).expect("the batch is arena owned");

    let mut state = NoState;
    let output = kernel
        .apply(&mut state, input)
        .expect("the kernel returns a new batch");

    // AD-I2: exactly one boundary copy, counted where the copy happened.
    assert_eq!(
        counting.boundary_copies(),
        1,
        "not exactly one boundary copy"
    );
    assert_eq!(
        counting.stats().boundary_copies_total,
        1,
        "AllocStats::boundary_copies_total did not see the copy"
    );
    assert_eq!(
        kernel.stats().boundary_copies,
        1,
        "the adapter did not count the morsel it copied"
    );
    assert_eq!(
        counting.boundary_bytes(),
        values.len() as u64 * 8,
        "the copy reported the wrong byte count"
    );

    assert_eq!(output.tier(), expected_tier);
    let back = common::i64_column(&output);
    let doubled: Vec<i64> = values.iter().map(|v| v * 2).collect();
    assert_eq!(
        back, doubled,
        "the copied bytes are not the kernel's output"
    );
}

#[test]
fn ad_t2_boundary_copy_once_host() {
    boundary_copy_once(false, Tier::Host);
}

#[test]
fn ad_t2_boundary_copy_once_pinned() {
    boundary_copy_once(true, Tier::PinnedHost);
}
