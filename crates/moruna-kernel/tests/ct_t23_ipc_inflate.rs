//! CT-T23 to CT-T26, the compressed Arrow IPC batches of contracts e.8 (CT-I13, CT-I14):
//!
//! * CT-T23 ipc_compressed_round_trip: every type the IPC tests cover, written with ZSTD and
//!   LZ4_FRAME by arrow-ipc, reads back equal through `read_stream`, inflated into arena
//!   buffers that go back when the batch drops.
//! * CT-T24 ipc_decompression_bombs: a buffer that lies about its length, offsets that cannot be
//!   believed, frames that inflate to more or less than declared, and a batch that asks for
//!   more than the ceiling are each refused with the reason, before the arena has been asked for
//!   a megabyte.
//! * CT-T25 ipc_compressed_never_panics: seeded corruption of compressed streams.
//! * CT-T26 ipc_inflate_refusals: the codecs and methods Moruna does not read, and the readers
//!   whose ceiling is zero.

// The contract error is large by design (contracts d.14); `decode` returns it.
#![allow(clippy::result_large_err)]

mod bombs;
mod common;

use std::sync::Arc;

use arrow::array::{Array, AsArray, FixedSizeBinaryArray, Int64Array, RecordBatch};
use arrow::buffer::Buffer;
use arrow::datatypes::DataType;
use arrow::ipc::{BodyCompressionMethod, CompressionType, MetadataVersion};
use bombs::{CODECS, bombs, how, int64s, offsets32, one, strings};
use common::FakeAllocator;
use moruna_kernel::Tier;
use moruna_kernel::ipc::{
    Checked, Inflate, ReadStream, check, decode, encode_compressed, read_stream,
};
use moruna_testkit::ipc::crafted::{How, packed, stream_as};
use moruna_testkit::ipc::{
    batches, body_corruptions, compressed_stream, corruptions, panics, tally,
};

/// A ceiling that holds every fixture here, whatever the codec.
const ROOMY: u64 = 64 << 20;

fn inflate_with(
    bytes: Vec<u8>,
    alloc: &FakeAllocator,
    ceiling: u64,
) -> Result<Vec<RecordBatch>, String> {
    read_stream(&Buffer::from_vec(bytes), &Inflate::new(alloc, ceiling))
}

#[test]
fn ct_t23_round_trip_every_type_both_codecs() {
    for (label, batches, version) in batches() {
        if version != MetadataVersion::V5 {
            // Compression is metadata V5's.
            continue;
        }
        for (name, codec) in CODECS {
            for level in [None, Some(1), Some(19)] {
                if level.is_some() && codec != CompressionType::ZSTD {
                    continue;
                }
                let bytes = compressed_stream(&batches, codec, level);
                let alloc = FakeAllocator::new();
                let read = inflate_with(bytes, &alloc, ROOMY)
                    .unwrap_or_else(|e| panic!("{label} {name} {level:?}: {e}"));
                assert_eq!(read, batches, "{label} {name} {level:?}");
                assert!(
                    alloc.allocations_total() > 0,
                    "{label} {name}: the body was inflated into the arena"
                );
                assert!(
                    alloc.peak(Tier::Host) <= ROOMY,
                    "{label} {name}: the arena held {} bytes",
                    alloc.peak(Tier::Host)
                );
                drop(read);
                assert_eq!(
                    alloc.in_use(Tier::Host),
                    0,
                    "{label} {name}: the arena has its bytes back"
                );
            }
        }
    }
}

#[test]
fn ct_t23_sliced_empty_and_incompressible_batches() {
    let every = batches()
        .into_iter()
        .find(|(label, _, _)| *label == "every_type")
        .expect("the every_type stream")
        .1;
    let whole = &every[0];
    // A slice has offsets that do not start at 0 in memory: the writer rebases them, and the
    // data after them is trimmed, which the exact size rule depends on.
    let sliced = whole.slice(1, 2);
    let empty = RecordBatch::new_empty(whole.schema());
    // 16-byte values no codec shrinks, so every buffer is stored as it is (-1).
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let rows: Vec<[u8; 16]> = (0..2_000)
        .map(|_| {
            let mut value = [0u8; 16];
            for byte in &mut value {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *byte = seed as u8;
            }
            value
        })
        .collect();
    let random = RecordBatch::try_from_iter([(
        "r",
        Arc::new(
            FixedSizeBinaryArray::try_from_iter(rows.iter().map(|r| r.as_slice()))
                .expect("fixed size binary"),
        ) as arrow::array::ArrayRef,
    )])
    .expect("a batch");
    for (name, codec) in CODECS {
        for batch in [&sliced, &empty, &random] {
            let bytes = compressed_stream(std::slice::from_ref(batch), codec, None);
            let alloc = FakeAllocator::new();
            let read = inflate_with(bytes.clone(), &alloc, ROOMY)
                .unwrap_or_else(|e| panic!("{name} {} rows: {e}", batch.num_rows()));
            assert_eq!(
                read,
                std::slice::from_ref(batch),
                "{name} {} rows",
                batch.num_rows()
            );
            if batch.num_columns() == 1 && batch.num_rows() == 2_000 {
                let flags = moruna_testkit::ipc::messages(&bytes);
                let ((start, len), (body, _)) = flags[1];
                let message = arrow::ipc::root_as_message(&bytes[start..start + len])
                    .expect("the record batch message");
                let entry = message
                    .header_as_record_batch()
                    .and_then(|b| b.buffers())
                    .map(|b| b.get(1))
                    .expect("the values buffer");
                let prefix = &bytes[body + entry.offset() as usize..][..8];
                assert_eq!(
                    i64::from_le_bytes(prefix.try_into().expect("8 bytes")),
                    -1,
                    "{name}: an incompressible buffer is stored as it is"
                );
            }
        }
    }
}

#[test]
fn ct_t24_honest_crafted_streams_read() {
    for (name, codec) in CODECS {
        let alloc = FakeAllocator::new();
        let read = inflate_with(int64s(codec, &[1, 2, 3, 4]), &alloc, ROOMY).expect(name);
        let column = read[0].column(0).as_any().downcast_ref::<Int64Array>();
        assert_eq!(column.map(|c| c.values().to_vec()), Some(vec![1, 2, 3, 4]));

        let offsets = packed(codec, &offsets32(&[0, 1, 1, 4]));
        let data = packed(codec, b"abcd");
        let alloc = FakeAllocator::new();
        let read = inflate_with(strings(codec, 3, offsets, data), &alloc, ROOMY).expect(name);
        assert_eq!(read[0].num_rows(), 3);
    }
}

#[test]
fn ct_t24_bombs_are_refused_with_the_reason_under_a_megabyte() {
    for bomb in bombs() {
        let alloc = FakeAllocator::new();
        let refused = inflate_with(bomb.bytes.clone(), &alloc, bomb.ceiling)
            .expect_err(&format!("{} is refused", bomb.label));
        assert!(
            refused.contains(bomb.says),
            "{}: wanted {:?} in {refused:?}",
            bomb.label,
            bomb.says
        );
        assert!(
            alloc.peak(Tier::Host) < 1 << 20,
            "{}: the arena held {} bytes at its peak",
            bomb.label,
            alloc.peak(Tier::Host)
        );
        assert_eq!(
            alloc.in_use(Tier::Host),
            0,
            "{}: the refusal left bytes in the arena",
            bomb.label
        );
    }
}

#[test]
fn ct_t24_bombs_name_the_column_and_the_buffer() {
    let alloc = FakeAllocator::new();
    let bad = bombs()
        .into_iter()
        .find(|b| b.label == "zstd: declared 31")
        .expect("the case");
    let refused = inflate_with(bad.bytes, &alloc, 1 << 20).expect_err("refused");
    assert!(
        refused.contains("column \"v\" has a values buffer"),
        "{refused}"
    );
}

#[test]
fn ct_t24_nothing_is_allocated_before_the_sizes_are_checked() {
    // These are refused by the sizes alone: the arena is never asked for a byte.
    for bomb in bombs() {
        let cheap = [
            "where the message implies",
            "uncompressed length of",
            "cannot hold",
            "bytes this reader may inflate",
            "to inflate",
        ];
        // A string column's offsets are inflated, into a buffer of the arena, to size its data.
        if !cheap.iter().any(|c| bomb.says.contains(c)) || bomb.says.contains("data buffer") {
            continue;
        }
        let alloc = FakeAllocator::new();
        inflate_with(bomb.bytes, &alloc, bomb.ceiling).expect_err("refused");
        assert_eq!(
            alloc.allocations_total(),
            0,
            "{}: the arena was asked for memory before the sizes were checked",
            bomb.label
        );
    }
}

#[test]
fn ct_t25_compressed_streams_never_panic() {
    let mut failed = Vec::new();
    for (label, valid) in moruna_testkit::ipc::compressed_streams() {
        let seed = 0xC725_0000 ^ valid.len() as u64;
        let alloc = FakeAllocator::new();
        let read = |bytes: Vec<u8>| inflate_with(bytes, &alloc, 16 << 20);
        for case in panics(corruptions(&valid, seed, 600), read) {
            failed.push(format!("{label}, metadata, {case}"));
        }
        for case in panics(body_corruptions(&valid, seed ^ 1, 600), read) {
            failed.push(format!("{label}, body, {case}"));
        }
        let lazily = |bytes: Vec<u8>| {
            let limit = bytes.len() as u64;
            ReadStream::new(std::io::Cursor::new(bytes), limit).collect::<Result<Vec<_>, _>>()
        };
        for case in panics(body_corruptions(&valid, seed ^ 2, 100), lazily) {
            failed.push(format!("{label}, ReadStream, {case}"));
        }
        assert_eq!(
            alloc.in_use(Tier::Host),
            0,
            "{label}: every refusal and every batch gave its bytes back"
        );
    }
    assert!(failed.is_empty(), "{}", tally(&failed));
}

#[test]
fn ct_t25_every_truncation_of_a_compressed_stream_is_an_error_or_a_batch() {
    let batch = batches()
        .into_iter()
        .find(|(label, _, _)| *label == "every_type")
        .expect("the every_type stream")
        .1;
    for (name, codec) in CODECS {
        let valid = compressed_stream(&batch, codec, None);
        let alloc = FakeAllocator::new();
        let cuts = (0..valid.len())
            .step_by(7)
            .map(|cut| (format!("cut {cut}"), valid[..cut].to_vec()));
        let failed = panics(cuts, |bytes| inflate_with(bytes, &alloc, ROOMY));
        assert!(failed.is_empty(), "{name}: {}", tally(&failed));
    }
}

#[test]
fn ct_t26_codecs_and_methods_moruna_does_not_read() {
    let schema = one("v", DataType::Int64);
    let raw = [0u8; 32];
    let buffers = [Vec::new(), packed(CompressionType::ZSTD, &raw)];
    let nodes = [(4, 0)];
    for (label, how, says) in [
        (
            "a codec of the future",
            How {
                codec: CompressionType(7),
                ..how(CompressionType::ZSTD)
            },
            "<UNKNOWN 7>, and Moruna reads LZ4_FRAME and ZSTD only",
        ),
        (
            "a method of the future",
            How {
                method: BodyCompressionMethod(9),
                ..how(CompressionType::ZSTD)
            },
            "by method <UNKNOWN 9>, and Moruna reads BUFFER only",
        ),
        (
            "metadata V4",
            How {
                version: MetadataVersion::V4,
                ..how(CompressionType::ZSTD)
            },
            "compression needs V5",
        ),
    ] {
        let alloc = FakeAllocator::new();
        let bytes = stream_as(&schema, how, 4, &nodes, &buffers);
        let refused = inflate_with(bytes, &alloc, ROOMY).expect_err(label);
        assert!(refused.contains(says), "{label}: {refused}");
        assert_eq!(alloc.allocations_total(), 0, "{label}");
    }
}

#[test]
fn ct_t26_readers_that_inflate_nothing_refuse_a_compressed_batch() {
    let bytes = int64s(CompressionType::ZSTD, &[1, 2, 3, 4]);
    // A reader of Moruna's own streams has a ceiling of zero.
    let limit = bytes.len() as u64;
    let refused = ReadStream::new(std::io::Cursor::new(bytes.clone()), limit)
        .collect::<Result<Vec<_>, _>>()
        .expect_err("a spill file is never compressed");
    assert!(
        refused.contains("bytes this reader may inflate"),
        "{refused}"
    );
    let refused = read_stream(&Buffer::from_vec(bytes), &Inflate::none()).expect_err("refused");
    assert!(
        refused.contains("0 of the 0 bytes this reader may inflate"),
        "{refused}"
    );

    // A staging record is never compressed: `decode` refuses one, naming the ceiling.
    let alloc = FakeAllocator::new();
    let batch = RecordBatch::try_from_iter([(
        "v",
        Arc::new(Int64Array::from(vec![1, 2, 3, 4])) as arrow::array::ArrayRef,
    )])
    .expect("a batch");
    for compression in [
        moruna_kernel::ipc::Compression::Zstd(3),
        moruna_kernel::ipc::Compression::Lz4,
    ] {
        let (framing, bodies) = encode_compressed(&batch, compression, 0, &alloc).expect("record");
        let end = bodies
            .iter()
            .map(|(offset, body)| offset + body.len())
            .max()
            .unwrap_or(framing.len());
        let mut record = vec![0u8; end];
        record[..framing.len()].copy_from_slice(&framing);
        for (offset, body) in &bodies {
            record[*offset..*offset + body.len()].copy_from_slice(body.as_slice());
        }
        let refused = decode(Buffer::from_vec(record), 4096).expect_err("a compressed record");
        assert!(
            refused.to_string().contains("may inflate"),
            "{compression:?}: {refused}"
        );
    }
}

#[test]
fn ct_t26_encode_compressed_wants_a_compression_and_a_valid_level() {
    let alloc = FakeAllocator::new();
    let batch = RecordBatch::try_from_iter([(
        "v",
        Arc::new(Int64Array::from(vec![1, 2, 3])) as arrow::array::ArrayRef,
    )])
    .expect("a batch");
    let none = encode_compressed(&batch, moruna_kernel::ipc::Compression::None, 0, &alloc);
    assert!(none.is_err());
    let level = encode_compressed(
        &batch,
        moruna_kernel::ipc::Compression::Zstd(1000),
        0,
        &alloc,
    );
    assert!(
        level.is_err_and(|e| e.to_string().contains("level")),
        "a zstd level outside zstd's range"
    );
    let unaligned = encode_compressed(&batch, moruna_kernel::ipc::Compression::Lz4, 3, &alloc);
    assert!(unaligned.is_err());
}

#[test]
fn ct_t26_compression_by_name() {
    use moruna_kernel::ipc::Compression;
    assert_eq!(Compression::parse("none", None), Ok(Compression::None));
    assert_eq!(Compression::parse("lz4", None), Ok(Compression::Lz4));
    assert_eq!(Compression::parse("zstd", None), Ok(Compression::Zstd(3)));
    assert_eq!(
        Compression::parse("zstd", Some(9)),
        Ok(Compression::Zstd(9))
    );
    for refused in [
        Compression::parse("zstd", Some(1000)),
        Compression::parse("lz4", Some(3)),
        Compression::parse("none", Some(3)),
        Compression::parse("gzip", None),
    ] {
        assert!(refused.is_err(), "{refused:?}");
    }
    for compression in [Compression::None, Compression::Zstd(3), Compression::Lz4] {
        assert_eq!(
            Compression::parse(compression.name(), None).map(Compression::name),
            Ok(compression.name())
        );
    }
}

#[test]
fn ct_t23_buffers_padded_as_arrow_cpp_writes_them_are_read() {
    // Arrow C++ writes the data of a string column padded to a multiple of 64 and a sliced
    // array's other buffers padded to 8: 7 bytes of data in a buffer of 64, 3 values of 4
    // bytes in a buffer of 16, and the validity bitmap of 3 rows in a buffer of 8.
    for (name, codec) in CODECS {
        let alloc = FakeAllocator::new();
        let mut data = b"abcdefg".to_vec();
        data.resize(64, 0);
        let bytes = strings(
            codec,
            3,
            packed(codec, &offsets32(&[0, 3, 3, 7])),
            packed(codec, &data),
        );
        let read = inflate_with(bytes, &alloc, ROOMY).unwrap_or_else(|e| panic!("{name}: {e}"));
        let text = read[0].column(0).as_string::<i32>();
        assert_eq!(
            (text.value(0), text.value(1), text.value(2)),
            ("abc", "", "defg"),
            "{name}"
        );

        let ints: Vec<u8> = [1i32, 2, 3, 0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let bytes = stream_as(
            &one("v", DataType::Int32),
            how(codec),
            3,
            &[(3, 0)],
            &[packed(codec, &[0xff; 8]), packed(codec, &ints)],
        );
        let read = inflate_with(bytes, &alloc, ROOMY).unwrap_or_else(|e| panic!("{name}: {e}"));
        let column = read[0]
            .column(0)
            .as_primitive::<arrow::datatypes::Int32Type>();
        assert_eq!(column.values().to_vec(), [1, 2, 3], "{name}");

        // One byte past the padding is a lie, and so is any size short of the data.
        for (declared, wanted) in [(65usize, "7 to 64"), (6, "7 to 64")] {
            let mut data = b"abcdefg".to_vec();
            data.resize(declared, 0);
            let bytes = strings(
                codec,
                3,
                packed(codec, &offsets32(&[0, 3, 3, 7])),
                packed(codec, &data),
            );
            let refused = inflate_with(bytes, &alloc, ROOMY).expect_err("refused");
            assert!(refused.contains(wanted), "{name}: {refused}");
        }
    }
}

#[test]
fn ct_t26_check_hands_back_the_inflated_message_and_body() {
    let bytes = int64s(CompressionType::ZSTD, &[1, 2, 3, 4]);
    let spans = moruna_testkit::ipc::messages(&bytes);
    let alloc = FakeAllocator::new();
    let inflate = Inflate::new(&alloc, ROOMY);
    let nothing = Buffer::from_vec(Vec::<u8>::new());

    let ((start, len), _) = spans[0];
    let message = arrow::ipc::root_as_message(&bytes[start..start + len]).expect("the schema");
    let Ok(Checked::Schema(schema)) = check(&message, &nothing, None, &inflate) else {
        panic!("the first message is a schema");
    };
    let schema = arrow::ipc::convert::fb_to_schema(schema);

    let ((start, len), (body, body_len)) = spans[1];
    let message = arrow::ipc::root_as_message(&bytes[start..start + len]).expect("the batch");
    let body = Buffer::from_vec(bytes[body..body + body_len].to_vec());
    let Ok(Checked::Batch(passed)) = check(&message, &body, Some(&schema), &inflate) else {
        panic!("the second message is a record batch");
    };
    assert!(passed.is_inflated());
    let (rows, compressed, inflated_len) = passed
        .with_batch(&body, |batch, inflated| {
            (
                batch.length(),
                batch.compression().is_some(),
                inflated.len(),
            )
        })
        .expect("the inflated message parses");
    assert_eq!(rows, 4);
    assert!(
        !compressed,
        "the message handed to arrow-ipc is not compressed"
    );
    assert_eq!(
        inflated_len, 64,
        "the 32 bytes of values, placed at a multiple of 64"
    );
    // A batch that was not compressed is handed back as it came.
    let plain = moruna_testkit::ipc::stream(&[moruna_testkit::ipc::small()], MetadataVersion::V5);
    let spans = moruna_testkit::ipc::messages(&plain);
    let ((start, len), (body, body_len)) = spans[1];
    let message = arrow::ipc::root_as_message(&plain[start..start + len]).expect("the batch");
    let body = Buffer::from_vec(plain[body..body + body_len].to_vec());
    let schema = moruna_testkit::ipc::small().schema();
    let Ok(Checked::Batch(passed)) = check(&message, &body, Some(&schema), &inflate) else {
        panic!("a record batch");
    };
    assert!(!passed.is_inflated());
    assert_eq!(
        passed.with_batch(&body, |_, handed| handed.len()),
        Ok(body.len())
    );
}
