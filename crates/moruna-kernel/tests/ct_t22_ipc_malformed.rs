//! CT-T22 ipc_malformed_never_panics: a page-aligned record (e.7) whose schema names a type id no
//! Arrow type has is `Io { op: "ipc" }` naming the field; every corruption and truncation of
//! records and of streams (e.8) is an error or a batch, never a panic, through `decode`,
//! `read_stream`, `ReadStream` and `check`. Proves CT-I13.

// The contract error is large by design (contracts d.14); `decode` returns it.
#![allow(clippy::result_large_err)]

mod common;

use arrow::buffer::Buffer;
use arrow::record_batch::RecordBatch;
use common::FakeAllocator;
use moruna_kernel::MorunaError;
use moruna_kernel::ipc::{Inflate, ReadStream, decode, encode_framing, read_stream};
use moruna_testkit::ipc::{
    batches, corruptions, panics, small, stream, streams, tally, unknown_type,
};

const PAGE: usize = 4096;

/// `batch` as one page-aligned record over a page-rounded buffer, as staging reads it back.
fn record(batch: &RecordBatch) -> Vec<u8> {
    let alloc = FakeAllocator::new();
    let (framing, bodies) = encode_framing(batch, PAGE, 0, &alloc).expect("the framing");
    let end = bodies
        .iter()
        .map(|(offset, body)| offset + body.len())
        .max()
        .unwrap_or(framing.len());
    let mut out = vec![0u8; end.div_ceil(PAGE).max(1) * PAGE];
    out[..framing.len()].copy_from_slice(&framing);
    for (offset, body) in &bodies {
        out[*offset..*offset + body.len()].copy_from_slice(body.as_slice());
    }
    out
}

#[test]
fn ct_t22_unknown_type_refused() {
    let bytes = unknown_type(&record(&small()));
    let err = decode(Buffer::from_vec(bytes), PAGE).expect_err("an unknown type is refused");
    match &err {
        MorunaError::Io { op, msg, .. } => {
            assert_eq!(*op, "ipc");
            assert!(msg.contains("field \"i\""), "{msg}");
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

#[test]
fn ct_t22_record_never_panics() {
    let mut failed = Vec::new();
    for (label, batches, _) in batches() {
        if label == "dictionaries" {
            // A record holds one batch and no dictionary messages (e.7).
            continue;
        }
        let valid = record(&batches[0]);
        let cases = corruptions(&valid, 0xC722_0000 ^ valid.len() as u64, 300);
        for case in panics(cases, |bytes| decode(Buffer::from_vec(bytes), PAGE)) {
            failed.push(format!("{label}, {case}"));
        }
    }
    assert!(failed.is_empty(), "{}", tally(&failed));
}

#[test]
fn ct_t22_stream_never_panics() {
    let mut failed = Vec::new();
    for (label, valid) in streams() {
        let seed = 0xC722_5000 ^ valid.len() as u64;
        let cases = corruptions(&valid, seed, 300);
        for case in panics(cases, |bytes| {
            read_stream(&Buffer::from_vec(bytes), &Inflate::none())
        }) {
            failed.push(format!("{label}, read_stream, {case}"));
        }
        let lazily = |bytes: Vec<u8>| {
            let limit = bytes.len() as u64;
            ReadStream::new(std::io::Cursor::new(bytes), limit).collect::<Result<Vec<_>, _>>()
        };
        for case in panics(corruptions(&valid, seed, 300), lazily) {
            failed.push(format!("{label}, ReadStream, {case}"));
        }
    }
    assert!(failed.is_empty(), "{}", tally(&failed));
}

#[test]
fn ct_t22_valid_streams_read_back() {
    for (label, batches, version) in batches() {
        let bytes = stream(&batches, version);
        let read = read_stream(&Buffer::from_vec(bytes.clone()), &Inflate::none()).expect(label);
        assert_eq!(read, batches, "{label}");
        let limit = bytes.len() as u64;
        let lazily = ReadStream::new(std::io::Cursor::new(bytes), limit)
            .collect::<Result<Vec<_>, _>>()
            .expect(label);
        assert_eq!(lazily, batches, "{label}");
    }
}
