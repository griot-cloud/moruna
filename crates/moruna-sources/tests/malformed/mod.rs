//! A malformed Parquet file is untrusted input: SO-T31 and SO-T32 (07 k, SO-I10, h).
//!
//! The files are the SO fixtures (`write_parquet`), then cut short or corrupted here. A file
//! changed after planning is changed on disk, which is where a local read takes the file's
//! length from, and in the `FakeReactor`, which is where it takes the bytes from.

// The contract error is large by design (contracts d.14); `planned` returns it.
#![allow(clippy::result_large_err)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, Int64Array};
use moruna_kernel::{MorunaError, ObjectMetadata, Payload, Reactor, Source, SourceSchema, Tier};
use moruna_sources::{ParquetSource, ParquetSourceConfig};
use moruna_testkit::{FakeAllocator, FakeReactor};

use crate::support::{Ledger, NoObjectStore, block_on, scratch, write_parquet};

/// The byte span `[start, end)` of every column chunk, per row group, as the footer states it.
fn chunk_spans(path: &Path) -> Vec<Vec<(u64, u64)>> {
    let file = std::fs::File::open(path).expect("the fixture");
    let metadata = parquet::file::metadata::ParquetMetaDataReader::new()
        .parse_and_finish(&file)
        .expect("the fixture's footer");
    metadata
        .row_groups()
        .iter()
        .map(|group| {
            group
                .columns()
                .iter()
                .map(|chunk| {
                    let (start, length) = chunk.byte_range();
                    (start, start + length)
                })
                .collect()
        })
        .collect()
}

/// A source planned over the file now on disk at `path`, whose reads get `bytes`.
fn planned(path: &Path, bytes: Vec<u8>) -> moruna_kernel::Result<ParquetSource> {
    let reactor = FakeReactor::new().with_file(&path.display().to_string(), bytes);
    ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![path.display().to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(reactor) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
}

/// The `i64_0` values of a table payload.
fn ints(payload: &Payload) -> Vec<Option<i64>> {
    let Payload::Table(batch, _) = payload else {
        panic!("expected a table payload");
    };
    let column = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("i64_0 is an Int64 column");
    (0..column.len())
        .map(|i| (!column.is_null(i)).then(|| column.value(i)))
        .collect()
}

/// The ledger's `i64_0` values for row group `group`.
fn ledger_ints(ledger: &Ledger, group: usize) -> Vec<Option<i64>> {
    let start: u64 = ledger.groups[..group].iter().sum();
    (start..start + ledger.groups[group])
        .map(|row| ledger.i64_at(row))
        .collect()
}

// SO-T31 parquet_truncated_file. SO-I10, h.
#[test]
fn so_t31_parquet_truncated_file() {
    let dir = scratch();
    // Three row groups of about 30 KiB each, so the last group starts pages past the end of a
    // file cut after the first.
    let ledger = write_parquet(&dir, "t31", 3000, 1000, 7);
    let full = std::fs::read(&ledger.path).expect("the file");
    let spans = chunk_spans(&ledger.path);
    assert_eq!(spans.len(), 3);
    let alloc = FakeAllocator::new();
    let page = 4096u64;

    // Cut after planning, at the end of the first row group: the first group still reads, the
    // last one starts past the end of the file.
    let cut = spans[0].iter().map(|s| s.1).max().expect("a column");
    let last = spans[2].iter().map(|s| s.0).min().expect("a column");
    assert!(
        cut < last - last % page,
        "the last group must start pages past the cut"
    );
    let source = planned(&ledger.path, full[..cut as usize].to_vec()).expect("the plan");
    std::fs::write(&ledger.path, &full[..cut as usize]).expect("truncating the file");
    let splits = source.plan().expect("the plan");
    let first = block_on(source.read(&splits[0], None, &alloc, Tier::Host)).expect("group 0");
    assert_eq!(ints(&first), ledger_ints(&ledger, 0));
    let baseline = alloc.in_use(Tier::Host);
    let error = block_on(source.read(&splits[2], None, &alloc, Tier::Host)).unwrap_err();
    // Nothing is allocated for a read that is refused.
    assert_eq!(alloc.in_use(Tier::Host), baseline);
    let text = error.to_string();
    assert!(
        matches!(error, MorunaError::Source { split, .. } if split == splits[2].id),
        "{text}"
    );
    assert!(text.contains(&ledger.path.display().to_string()), "{text}");
    assert!(text.contains("row group 2"), "{text}");
    assert!(
        text.contains(&format!("starts at byte {}", spans[2][0].0)),
        "{text}"
    );
    assert!(text.contains("past the end of the file"), "{text}");
    assert!(text.contains(&format!("({cut} bytes)")), "{text}");

    // Cut through the middle of the second group's second column: that chunk runs past the end.
    std::fs::write(&ledger.path, &full).expect("restoring the file");
    let (start, end) = spans[1][1];
    let cut = (start + end) / 2;
    let source = planned(&ledger.path, full[..cut as usize].to_vec()).expect("the plan");
    std::fs::write(&ledger.path, &full[..cut as usize]).expect("truncating the file");
    let splits = source.plan().expect("the plan");
    let error = block_on(source.read(&splits[1], None, &alloc, Tier::Host)).unwrap_err();
    let text = error.to_string();
    assert!(
        matches!(error, MorunaError::Source { split, .. } if split == splits[1].id),
        "{text}"
    );
    assert!(text.contains("row group 1"), "{text}");
    assert!(text.contains("f64_0"), "{text}");
    assert!(text.contains(&format!("runs to byte {end}")), "{text}");
    assert!(text.contains(&format!("({cut} bytes)")), "{text}");

    // Cut before planning but with the footer kept: the plan refuses the file, naming it, the
    // row group and the chunk past its end.
    let footer_len = u64::from(u32::from_le_bytes(
        full[full.len() - 8..full.len() - 4]
            .try_into()
            .expect("four bytes"),
    )) + 8;
    let cut = spans[0].iter().map(|s| s.1).max().expect("a column") as usize;
    let mut spliced = full[..cut].to_vec();
    spliced.extend_from_slice(&full[full.len() - footer_len as usize..]);
    let path = dir.join("spliced.parquet");
    std::fs::write(&path, &spliced).expect("the spliced file");
    let error = match planned(&path, spliced.clone()) {
        Ok(_) => panic!("a footer naming bytes past the end must not plan"),
        Err(error) => error,
    };
    let text = error.to_string();
    assert!(matches!(error, MorunaError::Source { .. }), "{text}");
    assert!(text.contains(&path.display().to_string()), "{text}");
    assert!(text.contains("row group 1"), "{text}");
    assert!(text.contains("past the end of the file"), "{text}");
    assert!(
        text.contains(&format!("({} bytes)", spliced.len())),
        "{text}"
    );
}

/// What one malformed file did: planned and read (`Ok`) or refused (`Err`), or panicked.
fn exercise(path: &Path, on_disk: &[u8], planned_on: &[u8], label: &str) -> Result<(), String> {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        std::fs::write(path, planned_on).expect("the file to plan");
        let Ok(source) = planned(path, on_disk.to_vec()) else {
            return;
        };
        std::fs::write(path, on_disk).expect("the file to read");
        let SourceSchema::Table(schema) = source.schema() else {
            panic!("a Parquet source declares a table");
        };
        let alloc = FakeAllocator::new();
        let splits = source.plan().expect("a planned source plans");
        for split in &splits {
            match block_on(source.read(split, None, &alloc, Tier::Host)) {
                Ok(Payload::Table(batch, _)) => {
                    assert_eq!(batch.num_rows() as u64, split.rows, "{label}: split rows");
                    // The columns are the declared ones; key-value metadata the footer carries
                    // under a key that is no longer `ARROW:schema` rides along on the batch.
                    assert_eq!(
                        batch.schema().fields(),
                        schema.fields(),
                        "{label}: the declared columns"
                    );
                }
                Ok(Payload::Tensor(_, _)) => panic!("{label}: a Parquet read gave a tensor"),
                Err(MorunaError::Source { split: id, .. }) => {
                    assert_eq!(id, split.id, "{label}: the error names the split");
                }
                Err(other) => panic!("{label}: a read failed with {other}, not Source"),
            }
        }
    }));
    outcome.map_err(|panic| {
        let what = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_default();
        format!("{label}: {what}")
    })
}

/// A deterministic pseudo-random stream, so every case is reproducible from its label.
fn next(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// SO-T32 parquet_malformed_never_panics. SO-I10, h.
#[test]
fn so_t32_parquet_malformed_never_panics() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t32", 300, 100, 7);
    let full = std::fs::read(&ledger.path).expect("the file");
    let len = full.len() as u64;
    let spans = chunk_spans(&ledger.path);
    let metadata_len = u64::from(u32::from_le_bytes(
        full[full.len() - 8..full.len() - 4]
            .try_into()
            .expect("four bytes"),
    ));
    let footer = len - 8 - metadata_len;
    let path = dir.join("case.parquet");
    let mut seed = 0x5032_0000_0000_0031u64;
    let mut failures = Vec::new();

    // Truncations: every length around a column chunk's edges and the footer, and random ones,
    // cut before the plan and cut after it.
    let mut cuts: Vec<u64> = vec![0, 1, 4, 7, 8, 12, footer, len - 8, len - 1];
    for group in &spans {
        for (start, end) in group {
            cuts.extend([start.saturating_sub(1), *start, start + 1, end - 1, *end]);
        }
    }
    for _ in 0..200 {
        cuts.push(next(&mut seed) % len);
    }
    for cut in cuts {
        let short = &full[..cut.min(len) as usize];
        failures.extend(
            exercise(
                &path,
                short,
                short,
                &format!("cut to {cut} before the plan"),
            )
            .err(),
        );
        failures
            .extend(exercise(&path, short, &full, &format!("cut to {cut} after the plan")).err());
    }

    // Corruptions of the footer and the metadata: one to four bytes replaced, at random
    // positions in the metadata and its eight byte trailer, and every single byte of the
    // metadata set to 0x00 and to 0xFF.
    for _ in 0..600 {
        let mut bytes = full.clone();
        let count = 1 + next(&mut seed) % 4;
        let mut label = String::from("metadata bytes");
        for _ in 0..count {
            let at = footer + next(&mut seed) % (len - footer);
            let value = next(&mut seed) as u8;
            bytes[at as usize] = value;
            label.push_str(&format!(" {at}={value:#04x}"));
        }
        failures.extend(exercise(&path, &bytes, &bytes, &label).err());
    }
    for at in footer..len {
        for value in [0x00u8, 0xFF] {
            let mut bytes = full.clone();
            bytes[at as usize] = value;
            let label = format!("metadata byte {at}={value:#04x}");
            failures.extend(exercise(&path, &bytes, &bytes, &label).err());
        }
    }

    // Corruptions of the page headers at the start of every column chunk, where the page sizes
    // the decoder trusts are written.
    for group in &spans {
        for (start, end) in group {
            for _ in 0..20 {
                let mut bytes = full.clone();
                let at = start + next(&mut seed) % (end - start).min(24);
                let value = next(&mut seed) as u8;
                bytes[at as usize] = value;
                let label = format!("page header byte {at}={value:#04x}");
                failures.extend(exercise(&path, &bytes, &bytes, &label).err());
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} malformed files panicked; the first: {:?}",
        failures.len(),
        &failures[..failures.len().min(10)]
    );
}

/// Standard base64 with padding, as the Arrow writer encodes `ARROW:schema`.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let word = chunk
            .iter()
            .enumerate()
            .fold(0u32, |w, (i, b)| w | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(word >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A Parquet file of one `i64` column whose footer carries `schema` (an IPC schema message
/// behind its framing) under `ARROW:schema` in place of the one the writer would put there.
fn with_arrow_schema(schema: &[u8]) -> Vec<u8> {
    use moruna_kernel::arrow::array::Int64Array;
    use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
    use moruna_kernel::arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use parquet::arrow::arrow_writer::ArrowWriterOptions;
    use parquet::file::metadata::KeyValue;

    let fields = Schema::new(vec![Field::new("i", DataType::Int64, true)]);
    let batch = RecordBatch::try_new(
        Arc::new(fields),
        vec![Arc::new(Int64Array::from(vec![
            Some(1),
            None,
            Some(3),
            Some(4),
        ]))],
    )
    .expect("a batch");
    let mut out = Vec::new();
    let options = ArrowWriterOptions::new().with_skip_arrow_metadata(true);
    let mut writer =
        ArrowWriter::try_new_with_options(&mut out, batch.schema(), options).expect("a writer");
    writer.write(&batch).expect("the batch");
    writer.append_key_value_metadata(KeyValue::new("ARROW:schema".into(), base64(schema)));
    writer.close().expect("the footer");
    out
}

// SO-T33 parquet_arrow_schema_checked. SO-I10, contracts CT-I13.
#[test]
fn so_t33_parquet_arrow_schema_checked() {
    use moruna_kernel::arrow::ipc::MetadataVersion;
    use moruna_testkit::ipc::{Rng, VALUES, messages, small, stream, unknown_type};

    let dir = scratch();
    let path = dir.join("t33.parquet");
    let valid = stream(&[small()], MetadataVersion::V5);
    let ((start, len), _) = messages(&valid)[0];

    // The crafted footer: its schema names a type id no Arrow type has.
    let crafted = unknown_type(&valid);
    let bytes = with_arrow_schema(&crafted[start - 8..start + len]);
    std::fs::write(&path, &bytes).expect("the crafted file");
    let Err(err) = planned(&path, bytes) else {
        panic!("the crafted footer is refused");
    };
    let MorunaError::Source { msg, .. } = &err else {
        panic!("expected a Source error, got {err}");
    };
    assert!(msg.contains("the Arrow schema in the footer"), "{msg}");
    assert!(msg.contains("field \"i\""), "{msg}");

    // Every byte of the schema message set to each of the values, and random replacements:
    // each file is planned and read or refused, never a panic.
    let mut failures = Vec::new();
    let mut cases = Vec::new();
    for at in start..start + len {
        for value in VALUES {
            let mut schema = valid[start - 8..start + len].to_vec();
            schema[at - start + 8] = value;
            cases.push((format!("schema byte {at}={value:#04x}"), schema));
        }
    }
    let mut rng = Rng::new(0x5033_0000_0000_0033);
    for case in 0..300 {
        let mut schema = valid[start - 8..start + len].to_vec();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(schema.len());
            schema[at] = rng.next_u64() as u8;
        }
        cases.push((format!("random {case}"), schema));
    }
    for (label, schema) in cases {
        let bytes = with_arrow_schema(&schema);
        failures.extend(exercise(&path, &bytes, &bytes, &label).err());
    }
    assert!(
        failures.is_empty(),
        "{} footers panicked; the first: {:?}",
        failures.len(),
        &failures[..failures.len().min(10)]
    );
}
