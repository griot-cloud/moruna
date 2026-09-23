//! CT-T1 payload_variants: a `match` on `Payload` with two arms compiles with no wildcard
//! (a compile-time assertion via a helper function). Proves CT-I1.

mod common;

use common::{FakeAllocator, mixed_batch};
use moruna_kernel::{Payload, PayloadKind, Tier};

/// The exhaustiveness assertion: this function names both variants and no wildcard, so adding
/// a third variant to `Payload` stops the crate compiling (CT-I1, G-I6).
fn label(payload: &Payload) -> &'static str {
    match payload {
        Payload::Table(_, _) => "table",
        Payload::Tensor(_, _) => "tensor",
    }
}

#[test]
fn ct_t1_payload_variants() {
    let alloc = FakeAllocator::new();
    let table = Payload::table(mixed_batch(4)).expect("table payload");
    assert_eq!(label(&table), "table");
    assert_eq!(table.kind(), PayloadKind::Table);

    let buffer = alloc.buffer(16, Tier::Host);
    let tensor =
        moruna_kernel::ManagedTensor::from_buffer(buffer, 0, moruna_kernel::DType::F32, vec![4])
            .expect("tensor over a buffer");
    let tensor = Payload::tensor(tensor).expect("tensor payload");
    assert_eq!(label(&tensor), "tensor");
    assert_eq!(tensor.kind(), PayloadKind::Tensor);
}
