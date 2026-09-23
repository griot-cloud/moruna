//! CT-T3 bytes_accounting: for a generated batch, `bytes == get_array_memory_size`; for tensors
//! of each dtype and shapes `[0]`, `[]`, `[3,4]`, `bytes == count * item_size`. Proves CT-I3.

mod common;

use common::{FakeAllocator, mixed_batch};
use moruna_kernel::{DType, ManagedTensor, Morsel, NodeId, Origin, Payload, Tier};

fn origin() -> Origin {
    Origin {
        split: 0,
        row_start: 0,
        row_end: 8,
        node: NodeId::default(),
    }
}

#[test]
fn ct_t3_bytes_accounting() {
    let batch = mixed_batch(64);
    let expected = batch.get_array_memory_size() as u64;
    let payload = Payload::table(batch).expect("table payload");
    assert_eq!(payload.bytes(), expected);
    assert_eq!(payload.rows(), 64);

    let alloc = FakeAllocator::new();
    for dtype in DType::ALL {
        for shape in [vec![0i64], Vec::<i64>::new(), vec![3, 4]] {
            let count: u64 = shape.iter().map(|d| *d as u64).product();
            let bytes = count * dtype.item_size() as u64;
            let buffer = alloc.buffer((bytes as usize).max(1), Tier::Host);
            let tensor = ManagedTensor::from_buffer(buffer, 0, dtype, shape.clone())
                .expect("tensor over a buffer");
            assert_eq!(tensor.element_count(), count, "{dtype:?} {shape:?}");
            let payload = Payload::tensor(tensor).expect("tensor payload");
            assert_eq!(payload.bytes(), bytes, "{dtype:?} {shape:?}");
            // A zero-dimensional tensor has one element and one row (b, h).
            let rows = shape.first().map_or(1, |d| *d as u64);
            assert_eq!(payload.rows(), rows, "{dtype:?} {shape:?}");
        }
    }

    // The morsel caches the count, and recomputes it when the payload is replaced.
    let morsel = Morsel::new(
        7,
        0,
        Payload::table(mixed_batch(8)).expect("table"),
        origin(),
    );
    assert_eq!(morsel.bytes, morsel.payload.bytes());
    assert_eq!(morsel.features.rows, 8);
    let replaced = morsel.with_output(Payload::table(mixed_batch(2)).expect("table"));
    assert_eq!(replaced.stage, 1);
    assert_eq!(replaced.seq, 7);
    assert_eq!(replaced.bytes, replaced.payload.bytes());
    assert_eq!(replaced.features.rows, 2);

    // An empty batch is valid and its buffers still cost bytes (h).
    let empty = Payload::table(mixed_batch(0)).expect("empty batch");
    assert_eq!(empty.rows(), 0);
}
