//! CT-T3 bytes_accounting: for a generated batch, whose buffers each have an allocation of their
//! own, `bytes == get_array_memory_size`; a batch whose buffers share one allocation counts it
//! once; a small slice of a large allocation counts the whole of it; for tensors of each dtype
//! and shapes `[0]`, `[]`, `[3,4]`, `bytes == count * item_size`. Proves CT-I3.

mod common;

use std::sync::Arc;

use arrow::array::{Array, ArrayData, ArrayRef, Int64Array, RecordBatch, make_array};
use arrow::buffer::Buffer as ArrowBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use common::{FakeAllocator, mixed_batch};
use moruna_kernel::{DType, ManagedTensor, Morsel, NodeId, Origin, Payload, Tier};

/// What Arrow keeps beside a batch's buffers: its count less every buffer's allocation.
fn bookkeeping(batch: &RecordBatch) -> u64 {
    fn of(data: &ArrayData) -> usize {
        data.buffers().iter().map(|b| b.capacity()).sum::<usize>()
            + data.nulls().map_or(0, |n| n.buffer().capacity())
            + data.child_data().iter().map(of).sum::<usize>()
    }
    let buffers: usize = batch.columns().iter().map(|c| of(&c.to_data())).sum();
    (batch.get_array_memory_size() - buffers) as u64
}

/// An `int64` column of `rows` rows over `values`, the last with every eighth row null by
/// `valid` when it is given.
fn int64_over(values: ArrowBuffer, valid: Option<ArrowBuffer>, rows: usize) -> ArrayRef {
    let data = ArrayData::builder(DataType::Int64)
        .len(rows)
        .add_buffer(values)
        .null_bit_buffer(valid)
        .build()
        .expect("an int64 column");
    make_array(data)
}

fn table(columns: Vec<ArrayRef>) -> RecordBatch {
    let fields: Vec<Field> = columns
        .iter()
        .enumerate()
        .map(|(c, column)| Field::new(format!("c{c}"), column.data_type().clone(), true))
        .collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).expect("a batch")
}

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

/// A batch whose buffers are slices of one allocation holds that allocation once, however many
/// buffers slice it: a record decoded where it lies. Arrow's count is the allocation once for
/// each buffer (6.3h, 6.3k).
#[test]
fn ct_t3_a_batch_whose_buffers_share_one_allocation_counts_it_once() {
    const ROWS: usize = 1024;
    let alloc = FakeAllocator::new();
    // Four columns' values and the last one's validity, end to end in one arena buffer.
    let values = ROWS * 8;
    let bytes: Vec<u8> = (0..4 * values + ROWS / 8).map(|i| i as u8 | 1).collect();
    let one = alloc.arrow_buffer(&bytes, Tier::Host);
    let allocation = one.capacity() as u64;
    assert_eq!(allocation, bytes.len() as u64);
    let columns: Vec<ArrayRef> = (0..4)
        .map(|c| {
            let valid = (c == 3).then(|| one.slice_with_length(4 * values, ROWS / 8));
            int64_over(one.slice_with_length(c * values, values), valid, ROWS)
        })
        .collect();
    let shared = table(columns);
    drop(one);
    // Arrow: the allocation for each of the five buffers.
    assert_eq!(
        shared.get_array_memory_size() as u64,
        5 * allocation + bookkeeping(&shared)
    );
    let payload = Payload::table(shared.clone()).expect("a table of the arena's");
    assert_eq!(payload.tier(), Tier::Host);
    assert_eq!(
        payload.bytes(),
        allocation + bookkeeping(&shared),
        "the one allocation of {allocation} bytes is counted once"
    );
    // The morsel's bytes and its features' are the payload's.
    let morsel = Morsel::new(1, 0, payload, origin());
    assert_eq!(morsel.bytes, allocation + bookkeeping(&shared));
    assert_eq!(morsel.features.bytes, morsel.bytes);

    // The same columns, each with buffers of its own, count what Arrow counts: the same
    // bytes, held as five allocations.
    let own: Vec<ArrayRef> = shared
        .columns()
        .iter()
        .map(|column| {
            let data = column.to_data();
            let valid = data.nulls().map(|n| {
                let bits = n.buffer().as_slice();
                alloc.arrow_buffer(bits, Tier::Host)
            });
            let values = alloc.arrow_buffer(data.buffers()[0].as_slice(), Tier::Host);
            int64_over(values, valid, ROWS)
        })
        .collect();
    let apart = table(own);
    assert_eq!(apart, shared);
    let payload = Payload::table(apart.clone()).expect("a table of the arena's");
    assert_eq!(payload.bytes(), apart.get_array_memory_size() as u64);
    assert_eq!(payload.bytes(), allocation + bookkeeping(&apart));

    // Two allocations, one of them shared by two columns: each once.
    let first = alloc.arrow_buffer(&bytes[..2 * values], Tier::Host);
    let second = alloc.arrow_buffer(&bytes[..values], Tier::Host);
    let mixed = table(vec![
        int64_over(first.slice_with_length(0, values), None, ROWS),
        int64_over(second.clone(), None, ROWS),
        int64_over(first.slice_with_length(values, values), None, ROWS),
    ]);
    let payload = Payload::table(mixed.clone()).expect("a table of the arena's");
    assert_eq!(
        payload.bytes(),
        (first.capacity() + second.capacity()) as u64 + bookkeeping(&mixed)
    );
}

/// A small slice of a large allocation holds the whole allocation, so it counts it: nothing
/// of the allocation is given back while the slice lives.
#[test]
fn ct_t3_a_small_slice_of_a_large_allocation_counts_the_whole_of_it() {
    const ROWS: usize = 8192;
    let large: ArrayRef = Arc::new(Int64Array::from_iter_values(0..ROWS as i64));
    let allocation = large.to_data().buffers()[0].capacity() as u64;
    assert!(allocation >= (ROWS * 8) as u64);
    let batch = table(vec![large.clone()]);
    let whole = Payload::table(batch.clone()).expect("a table");
    assert_eq!(whole.bytes(), allocation + bookkeeping(&batch));

    // Eight rows of it: 64 bytes of values, and the allocation they are part of.
    let few = batch.slice(100, 8);
    assert_eq!(few.num_rows(), 8);
    let payload = Payload::table(few.clone()).expect("a table");
    assert_eq!(payload.rows(), 8);
    assert_eq!(payload.bytes(), allocation + bookkeeping(&few));
    assert!(payload.bytes() >= allocation);

    // Two slices of the one allocation in one batch: still the allocation once.
    let twice = table(vec![large.slice(0, 8), large.slice(4000, 8)]);
    let payload = Payload::table(twice.clone()).expect("a table");
    assert_eq!(payload.bytes(), allocation + bookkeeping(&twice));
}
