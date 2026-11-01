//! CT-T29 plane_record: the data plane's record check (SDD 29) over records arrow itself wrote.
//! Every record `encode_framing` lays out passes `check_record`, which names the buffers where
//! `encode_framing` placed them; a stream arrow-ipc writes, re-framed by `reframe`, is the same
//! record byte for byte; and a compressed record, a dictionary column, a buffer outside the
//! region, a buffer off its page and bytes that are no record are refused with the reason.

mod common;

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, DictionaryArray, Int32Array, Int64Array, ListArray, StringArray, StructArray,
};
use arrow::datatypes::{DataType, Field, Int32Type, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use common::FakeAllocator;
use moruna_ipc_check::frame;
use moruna_ipc_check::record::{Need, PAGE, check_record, framing_need, reframe, stream_parts};
use moruna_kernel::ipc::{Compression, encode_compressed, encode_framing};

fn batches() -> Vec<RecordBatch> {
    let rows = 1000;
    let ints: ArrayRef = Arc::new(Int64Array::from_iter_values(0..rows));
    let maybe: ArrayRef = Arc::new(Int32Array::from_iter(
        (0..rows as i32).map(|i| (i % 3 != 0).then_some(i)),
    ));
    let text: ArrayRef = Arc::new(StringArray::from_iter_values(
        (0..rows).map(|i| format!("row {i}")),
    ));
    let lists: ArrayRef = Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(
        (0..rows as i32).map(|i| Some(vec![Some(i), Some(i + 1)])),
    ));
    let nested: ArrayRef = Arc::new(StructArray::from(vec![
        (
            Arc::new(Field::new("a", DataType::Int64, false)),
            ints.clone(),
        ),
        (
            Arc::new(Field::new("b", DataType::Utf8, false)),
            text.clone(),
        ),
    ]));
    let empty: ArrayRef = Arc::new(Int64Array::from_iter_values(0..0));
    vec![
        RecordBatch::try_from_iter([("x", ints.clone())]).expect("one column"),
        RecordBatch::try_from_iter([("x", ints), ("m", maybe), ("s", text), ("l", lists)])
            .expect("four columns"),
        RecordBatch::try_from_iter([("n", nested)]).expect("a struct"),
        RecordBatch::try_from_iter([("e", empty)]).expect("no rows"),
    ]
}

/// The record `encode_framing` writes for `batch`, whole, and its framing's length.
fn record(batch: &RecordBatch) -> (Vec<u8>, usize, Vec<(u64, u64)>) {
    let alloc = FakeAllocator::new();
    let (framing, bodies) = encode_framing(batch, PAGE as usize, 0, &alloc).expect("encoded");
    let framing: &[u8] = &framing;
    let Ok(Need::Framing(n)) = framing_need(framing) else {
        panic!("the framing's length is known from its bytes");
    };
    let mut bytes = framing.to_vec();
    let mut placed = Vec::new();
    for (at, body) in &bodies {
        if bytes.len() < at + body.len() {
            bytes.resize(at + body.len(), 0);
        }
        bytes[*at..at + body.len()].copy_from_slice(body.as_slice());
        placed.push((*at as u64, body.len() as u64));
    }
    let body_length = frame::body_length(&framing[stream_parts(&framing[..n]).expect("parts").1])
        .expect("a body length") as usize;
    bytes.resize(bytes.len().max(n + body_length), 0);
    (bytes, n, placed)
}

/// A stream arrow-ipc writes for `batch`, as its schema message, its batch message and the
/// batch's body.
fn stream(batch: &RecordBatch) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut writer = StreamWriter::try_new(Vec::new(), &batch.schema()).expect("a writer");
    writer.write(batch).expect("written");
    writer.finish().expect("finished");
    let bytes = writer.into_inner().expect("the bytes");
    let schema_len = frame::message_len(&bytes).expect("a schema message") + 8;
    let rest = &bytes[schema_len..];
    let batch_len = frame::message_len(rest).expect("a batch message") + 8;
    let body = frame::body_length(&rest[..batch_len]).expect("a body length") as usize;
    (
        bytes[..schema_len].to_vec(),
        rest[..batch_len].to_vec(),
        rest[batch_len..batch_len + body].to_vec(),
    )
}

#[test]
fn ct_t29_every_record_encode_framing_writes_passes_with_its_buffers_where_they_were_placed() {
    for batch in batches() {
        let (bytes, n, placed) = record(&batch);
        let checked = check_record(&bytes[..n], bytes.len() as u64).expect("a valid record");
        assert_eq!(checked.body_start, n as u64);
        assert_eq!(checked.rows, batch.num_rows() as u64);
        assert_eq!(checked.fields.len(), batch.num_columns());
        assert_eq!(checked.columns.len(), batch.num_columns());
        let non_empty: Vec<(u64, u64)> = placed.into_iter().filter(|(_, l)| *l > 0).collect();
        let seen: Vec<(u64, u64)> = checked
            .buffers
            .iter()
            .copied()
            .filter(|(_, l)| *l > 0)
            .collect();
        assert_eq!(seen, non_empty);
        for (at, _) in &seen {
            assert_eq!(at % PAGE, 0);
        }
    }
}

#[test]
fn ct_t29_a_stream_reframed_is_the_record_encode_framing_writes() {
    for batch in batches() {
        let (bytes, n, placed) = record(&batch);
        let (schema_message, batch_message, body) = stream(&batch);
        let reframed = reframe(&schema_message, &batch_message).expect("reframed");
        assert_eq!(reframed.framing, bytes[..n].to_vec());
        let mut rebuilt = reframed.framing.clone();
        rebuilt.resize(reframed.record_len as usize, 0);
        for (from, to, len) in &reframed.moves {
            let (from, to, len) = (*from as usize, *to as usize, *len as usize);
            rebuilt[to..to + len].copy_from_slice(&body[from..from + len]);
        }
        let moved: Vec<(u64, u64)> = reframed.moves.iter().map(|(_, to, l)| (*to, *l)).collect();
        let placed: Vec<(u64, u64)> = placed.into_iter().filter(|(_, l)| *l > 0).collect();
        assert_eq!(moved, placed);
        assert_eq!(rebuilt, bytes[..reframed.record_len as usize].to_vec());
        check_record(&reframed.framing, reframed.record_len).expect("a valid record");
    }
}

#[test]
fn ct_t29_the_record_check_refuses_with_the_reason() {
    let refused = |framing: &[u8], len: u64, says: &str| {
        let e = check_record(framing, len).expect_err(says);
        assert!(e.contains(says), "{e:?} should say {says:?}");
    };

    let batch = &batches()[1];
    let (bytes, n, _) = record(batch);
    // A region cut short: the last buffer is outside what the task handed back.
    refused(&bytes[..n], bytes.len() as u64 - PAGE, "the record holds");
    refused(&bytes[..n], n as u64 - 1, "its framing alone takes");

    // A buffer off its page.
    let mut shifted = bytes[..n].to_vec();
    let rb = 8 + frame::message_len(&shifted).expect("a schema");
    let entries = frame::buffer_entries(&shifted[rb..]).expect("entries");
    let (i, (offset, length)) = entries
        .iter()
        .enumerate()
        .find(|(_, (_, l))| *l > 0)
        .map(|(i, e)| (i, *e))
        .expect("a buffer");
    frame::write_entry(&mut shifted[rb..], i, offset as i64 + 8, length as i64).expect("moved");
    refused(&shifted, bytes.len() as u64, "not on a page boundary");

    // A compressed record (contracts e.7: the data plane inflates nothing).
    let alloc = FakeAllocator::new();
    for codec in [Compression::Lz4, Compression::Zstd(3)] {
        let (framing, _) = encode_compressed(batch, codec, 0, &alloc).expect("compressed");
        let Ok(Need::Framing(n)) = framing_need(&framing) else {
            panic!("a framing");
        };
        refused(&framing[..n], 1 << 20, "may inflate 0 bytes");
    }

    // A dictionary column.
    let keys = DictionaryArray::<Int32Type>::from_iter(["a", "b", "a"]);
    let dictionary = RecordBatch::try_new(
        Arc::new(Schema::new(vec![Field::new(
            "d",
            keys.data_type().clone(),
            true,
        )])),
        vec![Arc::new(keys)],
    )
    .expect("a dictionary batch");
    let (schema_message, batch_message, _) = stream(&dictionary);
    let mut framing = schema_message;
    framing.extend_from_slice(&batch_message);
    refused(&framing, 1 << 20, "carries no dictionaries");

    // Bytes that are no record.
    let garbage = b"this is not an Arrow record, and it says so".to_vec();
    let e = framing_need(&garbage).expect_err("no continuation marker");
    assert!(e.contains("continuation marker"), "{e}");
    refused(&garbage, garbage.len() as u64, "continuation marker");
}
