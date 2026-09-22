//! CT-T18 ipc_page_aligned: `encode_framing` then `decode` over a page-rounded copy round-trips
//! 20 generated batches (all e.3 types plus strings and lists); every body offset is a page
//! multiple; the decoded arrays' data pointers lie inside the input buffer (no copy); a
//! corrupted body offset is `Io { op: "ipc" }`. Proves e.7.

mod common;

use std::sync::Arc;

use amoru_kernel::ipc::{decode, encode_framing};
use amoru_kernel::{AmoruError, DType, Tier};
use arrow::array::{
    Array, ArrayData, ArrayRef, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array,
    Int64Array, StringArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array, make_array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::FakeAllocator;

const PAGE: usize = 4096;

/// Twenty batches: one per e.3 primitive, plus strings, large strings, a fixed size list, a
/// nullable column, an empty batch, and a few mixtures.
fn batches() -> Vec<RecordBatch> {
    let rows = 12usize;
    let mut out: Vec<RecordBatch> = Vec::new();
    let primitives: Vec<(&str, ArrayRef)> = vec![
        (
            "i8",
            Arc::new(Int8Array::from_iter_values((0..rows).map(|i| i as i8))),
        ),
        (
            "i16",
            Arc::new(Int16Array::from_iter_values((0..rows).map(|i| i as i16))),
        ),
        (
            "i32",
            Arc::new(Int32Array::from_iter_values((0..rows).map(|i| i as i32))),
        ),
        (
            "i64",
            Arc::new(Int64Array::from_iter_values((0..rows).map(|i| i as i64))),
        ),
        (
            "u8",
            Arc::new(UInt8Array::from_iter_values((0..rows).map(|i| i as u8))),
        ),
        (
            "u16",
            Arc::new(UInt16Array::from_iter_values((0..rows).map(|i| i as u16))),
        ),
        (
            "u32",
            Arc::new(UInt32Array::from_iter_values((0..rows).map(|i| i as u32))),
        ),
        (
            "u64",
            Arc::new(UInt64Array::from_iter_values((0..rows).map(|i| i as u64))),
        ),
        ("f16", float16_column(rows)),
        (
            "f32",
            Arc::new(Float32Array::from_iter_values((0..rows).map(|i| i as f32))),
        ),
        (
            "f64",
            Arc::new(Float64Array::from_iter_values((0..rows).map(|i| i as f64))),
        ),
    ];
    for (name, column) in &primitives {
        out.push(one(name, Arc::clone(column), false));
    }
    let strings: ArrayRef = Arc::new(StringArray::from_iter_values(
        (0..rows).map(|i| format!("value-{i}")),
    ));
    out.push(one("s", Arc::clone(&strings), false));
    let long: ArrayRef = Arc::new(arrow::array::LargeStringArray::from_iter_values(
        (0..rows).map(|i| "x".repeat(i)),
    ));
    out.push(one("ls", long, false));
    out.push(one("list", fixed_size_list(rows), false));
    let nullable: ArrayRef = Arc::new(Int32Array::from_iter(
        (0..rows).map(|i| if i % 3 == 0 { None } else { Some(i as i32) }),
    ));
    out.push(one("n", nullable, true));
    out.push(common::mixed_batch(rows));
    out.push(common::mixed_batch(0));
    out.push(common::mixed_batch(1));
    // A wide batch and a batch with one very large column.
    let schema = Arc::new(Schema::new(
        primitives
            .iter()
            .map(|(n, c)| Field::new(*n, c.data_type().clone(), false))
            .collect::<Vec<_>>(),
    ));
    let columns: Vec<ArrayRef> = primitives.iter().map(|(_, c)| Arc::clone(c)).collect();
    out.push(RecordBatch::try_new(schema, columns).expect("a wide batch"));
    let wide: ArrayRef = Arc::new(Int64Array::from_iter_values(0..40_000));
    out.push(one("big", wide, false));
    out
}

fn one(name: &str, column: ArrayRef, nullable: bool) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![Field::new(
        name,
        column.data_type().clone(),
        nullable,
    )]));
    RecordBatch::try_new(schema, vec![column]).expect("a batch")
}

/// A `Float16` column built from its raw two-byte values, so the test needs no half-precision
/// crate of its own (the preamble's dependency table has none).
fn float16_column(rows: usize) -> ArrayRef {
    let bits: Vec<u8> = (0..rows)
        .flat_map(|i| ((i as u16) << 8).to_le_bytes())
        .collect();
    let data = ArrayData::builder(DataType::Float16)
        .len(rows)
        .add_buffer(arrow::buffer::Buffer::from(bits))
        .build()
        .expect("a float16 column");
    make_array(data)
}

fn fixed_size_list(rows: usize) -> ArrayRef {
    let values: ArrayRef = Arc::new(Float32Array::from_iter_values(
        (0..rows * 3).map(|i| i as f32),
    ));
    let field = Arc::new(Field::new("item", DataType::Float32, false));
    let data = ArrayData::builder(DataType::FixedSizeList(field, 3))
        .len(rows)
        .add_child_data(values.to_data())
        .build()
        .expect("a fixed size list");
    make_array(data)
}

/// Write the framing and every body piece into one page-rounded arena buffer, exactly as a
/// staging segment record is laid out (09 e.3), and return it as an Arrow buffer.
fn assemble(alloc: &FakeAllocator, batch: &RecordBatch) -> (arrow::buffer::Buffer, Vec<usize>) {
    let (framing, bodies) =
        encode_framing(batch, PAGE, 0, alloc).expect("encode the page-aligned framing");
    let end = bodies
        .iter()
        .map(|(offset, body)| offset + body.len())
        .max()
        .unwrap_or(framing.len());
    let total = end.div_ceil(PAGE) * PAGE;
    let mut record = alloc.buffer(total.max(PAGE), Tier::Host);
    record[..framing.len()].copy_from_slice(&framing);
    let mut offsets = Vec::with_capacity(bodies.len());
    for (offset, body) in &bodies {
        record[*offset..*offset + body.len()].copy_from_slice(body.as_slice());
        offsets.push(*offset);
    }
    (
        record
            .into_arrow_buffer()
            .expect("the record as an arrow buffer"),
        offsets,
    )
}

#[test]
fn ct_t18_ipc_page_aligned() {
    let alloc = FakeAllocator::new();
    let batches = batches();
    assert_eq!(batches.len(), 20, "the SDD asks for 20 generated batches");

    for batch in &batches {
        let (record, offsets) = assemble(&alloc, batch);
        let base = record.as_ptr() as usize;

        // Every body offset is a page multiple (e.7).
        for offset in &offsets {
            assert_eq!(offset % PAGE, 0, "body offset {offset} is not page aligned");
        }

        let decoded = decode(record.clone(), PAGE).expect("decode the record");
        assert_eq!(decoded.num_rows(), batch.num_rows());
        assert_eq!(decoded.schema(), batch.schema());
        assert_eq!(&decoded, batch, "the round trip changed the batch");

        // The decoded arrays point into the input buffer: nothing was copied (CT-I4, G-I2).
        let mut checked = 0usize;
        for column in decoded.columns() {
            checked += resident_buffers(&column.to_data(), base, record.len());
        }
        if batch.num_rows() > 0 {
            assert!(
                checked > 0,
                "no buffer was checked for {:?}",
                batch.schema()
            );
        }
    }

    // A corrupted body offset is `Io { op: "ipc" }`.
    let batch = &batches[2];
    let (record, _) = assemble(&alloc, batch);
    let mut bytes = record.as_slice().to_vec();
    let entry = find_first_entry(&bytes);
    let corrupt = u64::from_le_bytes(bytes[entry..entry + 8].try_into().expect("8 bytes")) + 1;
    bytes[entry..entry + 8].copy_from_slice(&corrupt.to_le_bytes());
    let corrupted = arrow::buffer::Buffer::from(bytes);
    let err = decode(corrupted, PAGE).expect_err("a misaligned body offset must be rejected");
    assert!(matches!(err, AmoruError::Io { op: "ipc", .. }), "got {err}");

    // So is a record whose continuation marker is gone, and a zero page size.
    let (record, _) = assemble(&alloc, batch);
    let mut bytes = record.as_slice().to_vec();
    bytes[0] = 0;
    let err = decode(arrow::buffer::Buffer::from(bytes), PAGE).expect_err("no continuation");
    assert!(matches!(err, AmoruError::Io { op: "ipc", .. }), "got {err}");
    assert!(decode(record, 0).is_err());
    assert!(encode_framing(batch, 0, 0, &alloc).is_err());
    // and a base offset that is not page aligned.
    assert!(encode_framing(batch, PAGE, 1, &alloc).is_err());

    // A truncated record is refused rather than read past its end: no header at all,
    let short = arrow::buffer::Buffer::from(vec![0u8; 4]);
    assert!(matches!(
        decode(short, PAGE),
        Err(AmoruError::Io { op: "ipc", .. })
    ));
    // a message length of zero,
    let mut bytes = vec![0xffu8; 8];
    bytes.extend_from_slice(&[0u8; 8]);
    assert!(matches!(
        decode(arrow::buffer::Buffer::from(bytes), PAGE),
        Err(AmoruError::Io { op: "ipc", .. })
    ));
    // and a message that claims more bytes than the record holds.
    let mut bytes = vec![0xffu8; 4];
    bytes.extend_from_slice(&1000u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    assert!(matches!(
        decode(arrow::buffer::Buffer::from(bytes), PAGE),
        Err(AmoruError::Io { op: "ipc", .. })
    ));
    // A record whose second message header is gone is refused too.
    let (record, _) = assemble(&alloc, batch);
    let mut bytes = record.as_slice().to_vec();
    let schema_len = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes")) as usize;
    bytes[8 + schema_len] = 0;
    assert!(matches!(
        decode(arrow::buffer::Buffer::from(bytes), PAGE),
        Err(AmoruError::Io { op: "ipc", .. })
    ));

    // A pinned arena puts the framing buffer in the run's pinned host tier (e.1).
    let pinned = FakeAllocator::new().pinned(true);
    let (framing, _) = encode_framing(batch, PAGE, 0, &pinned).expect("encode with a pinned arena");
    assert_eq!(framing.tier(), Tier::PinnedHost);

    // A record placed further down a segment keeps every body offset page aligned.
    let (framing, bodies) =
        encode_framing(batch, PAGE, 16 * PAGE as u64, &alloc).expect("encode at an offset");
    assert!(framing.len() % PAGE == 0);
    for (offset, _) in &bodies {
        assert_eq!(offset % PAGE, 0);
        assert!(*offset >= 16 * PAGE);
    }
    // Every dtype of e.3 that has an Arrow form appeared above.
    assert_eq!(common::arrow_dtypes().len(), DType::ALL.len() - 2);
}

/// Assert that every non-empty buffer of `data` and of its children lies inside the record,
/// and return how many were checked: a decoded array that points outside it was copied.
fn resident_buffers(data: &ArrayData, base: usize, len: usize) -> usize {
    let mut checked = 0usize;
    for buffer in data.buffers() {
        if buffer.is_empty() {
            continue;
        }
        let at = buffer.as_ptr() as usize;
        assert!(
            at >= base && at < base + len,
            "a decoded buffer was copied out of the record"
        );
        checked += 1;
    }
    for child in data.child_data() {
        checked += resident_buffers(child, base, len);
    }
    checked
}

/// The file offset of the first buffer entry's offset field inside a record's record batch
/// message, found by walking the two message headers the way `decode` does.
fn find_first_entry(bytes: &[u8]) -> usize {
    let schema_len = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes")) as usize;
    let rb_start = 8 + schema_len;
    let rb_len = u32::from_le_bytes(
        bytes[rb_start + 4..rb_start + 8]
            .try_into()
            .expect("4 bytes"),
    ) as usize;
    let message = &bytes[rb_start + 8..rb_start + 8 + rb_len];
    // The first entry is the first 16-byte aligned pair whose second half is a plausible
    // length: rather than parse flatbuffers here, find the entry by scanning for the offset
    // and length of the first body piece, which `decode` has already validated.
    let parsed = arrow::ipc::root_as_message(message).expect("a record batch message");
    let rb = parsed
        .header_as_record_batch()
        .expect("a record batch header");
    let buffers = rb.buffers().expect("buffer entries");
    let raw = buffers.bytes();
    let within = raw.as_ptr() as usize - message.as_ptr() as usize;
    rb_start + 8 + within
}
