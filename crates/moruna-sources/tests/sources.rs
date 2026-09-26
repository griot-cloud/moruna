//! The SO tests of `architecture/sdd/07-sources.md` section k.
//!
//! One integration test binary rather than one per file: each binary links the whole of arrow
//! and parquet, and under `cargo llvm-cov` each is instrumented as well.

mod support;

use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, Float64Array, Int64Array, StringArray};
use moruna_kernel::{
    Allocator, MorunaError, ObjectMetadata, Payload, Reactor, RowRange, Source, SourceSchema,
    Split, Tier,
};
use moruna_sources::{
    ParquetSource, ParquetSourceConfig, RowFilter, ScalarValue, TensorSource, TensorSourceConfig,
};
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};

use support::{
    CountingAllocator, Ledger, NoObjectStore, block_on, err, scratch, seed, seed_truncated,
    write_amb1, write_dictionary_parquet, write_npy, write_one_huge_row, write_parquet,
    write_safetensors,
};

/// A Parquet source over one generated file, reading its bytes through the fake.
fn parquet_over(ledger: &Ledger, reactor: FakeReactor) -> ParquetSource {
    ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![ledger.path.display().to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(reactor) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the parquet source")
}

/// A tensor source over one generated file, reading its bytes through the fake.
fn tensor_over(path: &std::path::Path, reactor: FakeReactor) -> TensorSource {
    TensorSource::new(
        TensorSourceConfig {
            paths: vec![path.to_path_buf()],
            ..TensorSourceConfig::default()
        },
        Arc::new(reactor) as Arc<dyn Reactor>,
    )
    .expect("the tensor source")
}

/// The batch inside a table payload.
fn table(payload: &Payload) -> &moruna_kernel::arrow::array::RecordBatch {
    match payload {
        Payload::Table(batch, _) => batch,
        Payload::Tensor(_, _) => panic!("expected a table payload"),
    }
}

/// The `f32` values a tensor payload holds.
fn tensor_values(payload: &Payload) -> Vec<f32> {
    let Payload::Tensor(tensor, _) = payload else {
        panic!("expected a tensor payload");
    };
    let (ptr, offset) = tensor.data_ptr();
    let count = tensor.element_count() as usize;
    let mut out = Vec::with_capacity(count);
    // The payload's bytes are host bytes this test owns for the tensor's lifetime; reading
    // them back is what every assertion about values needs.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.add(offset as usize), count * 4) };
    for chunk in bytes.as_chunks::<4>().0 {
        out.push(f32::from_le_bytes(*chunk));
    }
    out
}

/// A deterministic pseudo-random stream, so a test with fifty ranges is reproducible.
fn rng(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// SO-T1 plan_before_read. SO-I1.
#[test]
fn so_t1_plan_before_read() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t1", 300, 100, 0);
    let reactor = seed(FakeReactor::new(), &ledger.path);
    let source = parquet_over(&ledger, reactor);
    let alloc = FakeAllocator::new();
    let splits = source.plan().expect("the plan");
    assert_eq!(splits.len(), 3);
    for split in &splits {
        let payload = block_on(source.read(split, None, &alloc, Tier::Host)).expect("a read");
        assert_eq!(payload.rows(), split.rows);
    }
    let unplanned = Split {
        id: 99,
        ..splits[0].clone()
    };
    let error = block_on(source.read(&unplanned, None, &alloc, Tier::Host)).unwrap_err();
    assert!(
        matches!(error, MorunaError::Source { split: 99, .. }),
        "{error}"
    );

    let (path, _) = write_amb1(&dir, "t1", vec![8, 4], 0.0);
    let tensor = tensor_over(&path, seed(FakeReactor::new(), &path));
    let splits = tensor.plan().expect("the plan");
    assert_eq!(splits.len(), 1);
    let error = block_on(tensor.read(&unplanned, None, &alloc, Tier::Host)).unwrap_err();
    assert!(
        matches!(error, MorunaError::Source { split: 99, .. }),
        "{error}"
    );
}

// SO-T2 metadata_exact. SO-I2.
#[test]
fn so_t2_metadata_exact() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t2", 250, 100, 7);
    let source = ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![ledger.path.display().to_string()],
            columns: Some(vec!["i64_0".into(), "str_0".into()]),
            ..ParquetSourceConfig::default()
        },
        Arc::new(seed(FakeReactor::new(), &ledger.path)) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the parquet source");
    let splits = source.plan().expect("the plan");
    assert_eq!(splits.len(), ledger.groups.len());
    for (index, split) in splits.iter().enumerate() {
        assert_eq!(split.rows, ledger.groups[index], "group {index}");
        assert!(!split.estimated);
        assert!(split.sub_splittable);
        // The projection is two of the three columns, so the metadata is about those two.
        assert_eq!(split.column_bytes.len(), 2);
        assert_eq!(split.null_counts.len(), 2);
        assert_eq!(
            split.uncompressed_bytes,
            split.column_bytes.iter().sum::<u64>()
        );
        assert_eq!(
            split.null_counts[0],
            Some(ledger.nulls_in_group(index)),
            "nulls of group {index}"
        );
        assert_eq!(split.null_counts[1], Some(0));
    }
    let stats = source.stats();
    assert_eq!(stats.splits, splits.len() as u64);
    assert_eq!(stats.footer_reads, 2);
    assert_eq!(
        stats.bytes_planned,
        splits.iter().map(|s| s.uncompressed_bytes).sum::<u64>()
    );

    let (path, values) = write_amb1(&dir, "t2", vec![16, 4], 0.0);
    let tensor = tensor_over(&path, seed(FakeReactor::new(), &path));
    let splits = tensor.plan().expect("the plan");
    assert_eq!(splits[0].rows, 16);
    assert_eq!(splits[0].uncompressed_bytes, (values.len() * 4) as u64);
    assert!(!splits[0].estimated);
    assert!(splits[0].column_bytes.is_empty());
}

// SO-T3 resident_tier. SO-I3.
#[test]
fn so_t3_resident_tier() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t3", 64, 64, 0);
    let (mrb1, _) = write_amb1(&dir, "t3", vec![8, 4], 0.0);

    for pinned in [false, true] {
        let host = if pinned { Tier::PinnedHost } else { Tier::Host };
        let other = if pinned { Tier::Host } else { Tier::PinnedHost };
        // The run has one host tier (contracts e.1). `FakeAllocator` does not refuse the tier
        // it is not pinned for, so the other tier is given a budget of zero, which is the
        // `Alloc` the arena raises for it.
        let alloc = FakeAllocator::new().pinned(pinned).with_limit(other, 0);

        let source = parquet_over(&ledger, seed(FakeReactor::new(), &ledger.path));
        let splits = source.plan().expect("the plan");
        let before = alloc.in_use(host);
        let payload =
            block_on(source.read(&splits[0], None, &alloc, host)).expect("a read in the host tier");
        assert_eq!(payload.tier(), host);
        for column in table(&payload).columns() {
            for buffer in column.to_data().buffers() {
                assert!(alloc.contains(buffer.as_ptr()));
                assert_eq!(alloc.tier_of(buffer.as_ptr()), Some(host));
            }
        }
        assert!(alloc.in_use(host) > before);
        let error = block_on(source.read(&splits[0], None, &alloc, other)).unwrap_err();
        assert!(
            matches!(error, MorunaError::Alloc { tier, .. } if tier == other),
            "{error}"
        );

        let tensor = tensor_over(&mrb1, seed(FakeReactor::new(), &mrb1));
        let splits = tensor.plan().expect("the plan");
        let payload = block_on(tensor.read(&splits[0], None, &alloc, host)).expect("a tensor read");
        assert_eq!(payload.tier(), host);
        let Payload::Tensor(t, _) = &payload else {
            panic!("a tensor");
        };
        let (ptr, offset) = t.data_ptr();
        let inside = unsafe { ptr.add(offset as usize) };
        assert!(alloc.contains(inside));
        assert_eq!(alloc.tier_of(inside), Some(host));
        let error = block_on(tensor.read(&splits[0], None, &alloc, other)).unwrap_err();
        assert!(
            matches!(error, MorunaError::Alloc { tier, .. } if tier == other),
            "{error}"
        );
    }
}

// SO-T4 subsplit_exact. SO-I4.
#[test]
fn so_t4_subsplit_exact() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t4", 120, 120, 0);
    let source = parquet_over(&ledger, seed(FakeReactor::new(), &ledger.path));
    let alloc = FakeAllocator::new();
    let split = source.plan().expect("the plan").remove(0);

    let mut seen = Vec::new();
    for (start, end) in [(0u64, 10u64), (10, 25), (25, split.rows)] {
        let payload =
            block_on(source.read(&split, Some(RowRange { start, end }), &alloc, Tier::Host))
                .expect("a ranged read");
        let batch = table(&payload);
        assert_eq!(batch.num_rows() as u64, end - start);
        let ints = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("the i64 column");
        for row in 0..ints.len() {
            seen.push(ints.value(row));
        }
    }
    assert_eq!(seen, (0..split.rows as i64).collect::<Vec<_>>());

    let mut state = 20260922u64;
    for _ in 0..20 {
        let start = rng(&mut state) % split.rows;
        let end = start + 1 + rng(&mut state) % (split.rows - start);
        let payload =
            block_on(source.read(&split, Some(RowRange { start, end }), &alloc, Tier::Host))
                .expect("a random ranged read");
        let batch = table(&payload);
        let ints = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("the i64 column");
        let floats = batch
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("the f64 column");
        let strings = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("the string column");
        for (offset, row) in (start..end).enumerate() {
            assert_eq!(ints.value(offset), row as i64);
            assert_eq!(floats.value(offset), ledger.f64_at(row));
            assert_eq!(strings.value(offset), ledger.str_at(row));
        }
    }
}

// SO-T5 one_decode_copy. SO-I5.
#[test]
fn so_t5_one_decode_copy() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t5", 200, 100, 5);
    let source = parquet_over(&ledger, seed(FakeReactor::new(), &ledger.path));
    let alloc = CountingAllocator::new(FakeAllocator::new());
    let splits = source.plan().expect("the plan");

    for (index, split) in splits.iter().enumerate() {
        let payload = block_on(source.read(split, None, &alloc, Tier::Host)).expect("a read");
        // Exactly one decode copy per morsel, and it is the only one G-I2 grants.
        assert_eq!(alloc.payload_copies(), index as u64 + 1);
        let buffers: u64 = table(&payload)
            .columns()
            .iter()
            .flat_map(|c| c.to_data().buffers().to_vec())
            .map(|b| b.len() as u64)
            .sum();
        let bitmaps: u64 = table(&payload)
            .columns()
            .iter()
            .filter_map(|c| c.to_data().nulls().map(|n| n.inner().inner().len() as u64))
            .sum();
        assert_eq!(source.stats().decode_bytes, alloc.payload_copy_bytes());
        assert!(alloc.payload_copy_bytes() >= buffers + bitmaps);
    }
    assert_eq!(alloc.payload_copies(), splits.len() as u64);

    let (path, _) = write_amb1(&dir, "t5", vec![8, 4], 0.0);
    let tensor = tensor_over(&path, seed(FakeReactor::new(), &path));
    let alloc = CountingAllocator::new(FakeAllocator::new());
    let splits = tensor.plan().expect("the plan");
    for _ in 0..3 {
        let before = alloc.fake().allocations_total();
        let _payload =
            block_on(tensor.read(&splits[0], None, &alloc, Tier::Host)).expect("a tensor read");
        assert_eq!(alloc.fake().allocations_total(), before + 1);
    }
    // A tensor read copies nothing: the reactor's read lands the bytes in the arena.
    assert_eq!(tensor.stats().decode_bytes, 0);
    assert_eq!(alloc.payload_copies(), 0);
    assert_eq!(alloc.stats().payload_copies_total, 0);
}

// SO-T6 stateless. SO-I6.
#[test]
fn so_t6_stateless() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t6", 150, 50, 0);
    let source = parquet_over(&ledger, seed(FakeReactor::new(), &ledger.path));
    let alloc = FakeAllocator::new();
    let splits = source.plan().expect("the plan");

    let forward: Vec<_> = splits
        .iter()
        .map(|s| {
            let p = block_on(source.read(s, None, &alloc, Tier::Host)).expect("a read");
            table(&p).clone()
        })
        .collect();
    let backward: Vec<_> = splits
        .iter()
        .rev()
        .map(|s| {
            let p = block_on(source.read(s, None, &alloc, Tier::Host)).expect("a read");
            table(&p).clone()
        })
        .collect();
    for (a, b) in forward.iter().zip(backward.iter().rev()) {
        assert_eq!(a, b);
    }
    let again = block_on(source.read(&splits[0], None, &alloc, Tier::Host)).expect("a read");
    assert_eq!(table(&again), &forward[0]);
}

// SO-T7 zero_rows. SO-I7.
#[test]
fn so_t7_zero_rows() {
    let dir = scratch();
    // A zero-row tensor: the split plans with rows 0 and the read issues no IO.
    let (path, _) = write_amb1(&dir, "t7", vec![0, 4], 0.0);
    let reactor = seed(FakeReactor::new(), &path);
    let observer = reactor.clone();
    let tensor = tensor_over(&path, reactor);
    let alloc = FakeAllocator::new();
    let splits = tensor.plan().expect("the plan");
    assert_eq!(splits[0].rows, 0);
    assert_eq!(splits[0].uncompressed_bytes, 0);
    let payload = block_on(tensor.read(&splits[0], None, &alloc, Tier::Host)).expect("a read");
    assert_eq!(payload.rows(), 0);
    assert_eq!(payload.bytes(), 0);
    assert!(observer.ops().is_empty());

    // A zero-row range of a Parquet split reads as an empty batch of the same schema.
    let ledger = write_parquet(&dir, "t7", 40, 40, 0);
    let source = parquet_over(&ledger, seed(FakeReactor::new(), &ledger.path));
    let split = source.plan().expect("the plan").remove(0);
    let payload = block_on(source.read(
        &split,
        Some(RowRange { start: 7, end: 7 }),
        &alloc,
        Tier::Host,
    ))
    .expect("an empty read");
    assert_eq!(payload.rows(), 0);
    assert_eq!(table(&payload).num_columns(), 3);
}

// SO-T8 pruning. f.2.
#[test]
fn so_t8_pruning() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t8", 400, 100, 0);
    let unfiltered = parquet_over(&ledger, seed(FakeReactor::new(), &ledger.path));
    assert_eq!(unfiltered.plan().expect("the plan").len(), 4);

    // Rows 0..99 are in group 0; a predicate that only they can satisfy keeps that group.
    let source = ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![ledger.path.display().to_string()],
            filters: vec![RowFilter::Lt("i64_0".into(), ScalarValue::I64(50))],
            ..ParquetSourceConfig::default()
        },
        Arc::new(seed(FakeReactor::new(), &ledger.path)) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the parquet source");
    let kept = source.plan().expect("the plan");
    assert_eq!(kept.len(), 1);
    assert_eq!(source.stats().groups_skipped, 3);

    let equality = ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![ledger.path.display().to_string()],
            filters: vec![RowFilter::Eq("i64_0".into(), ScalarValue::I64(250))],
            ..ParquetSourceConfig::default()
        },
        Arc::new(seed(FakeReactor::new(), &ledger.path)) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the parquet source");
    assert_eq!(equality.plan().expect("the plan").len(), 1);

    let above = ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![ledger.path.display().to_string()],
            filters: vec![RowFilter::Gt("i64_0".into(), ScalarValue::I64(350))],
            ..ParquetSourceConfig::default()
        },
        Arc::new(seed(FakeReactor::new(), &ledger.path)) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the parquet source");
    assert_eq!(above.plan().expect("the plan").len(), 1);

    // A file written without statistics is never skipped.
    let bare = dir.join("t8-nostats.parquet");
    {
        let schema = support::parquet_schema(false);
        let properties = parquet::file::properties::WriterProperties::builder()
            .set_statistics_enabled(parquet::file::properties::EnabledStatistics::None)
            .build();
        let file = std::fs::File::create(&bare).expect("the file");
        let mut writer =
            parquet::arrow::ArrowWriter::try_new(file, Arc::clone(&schema), Some(properties))
                .expect("the writer");
        let ints: Int64Array = (0..10i64).map(Some).collect();
        let floats: Float64Array = (0..10).map(|r| r as f64 * 0.5).collect();
        let strings: StringArray = (0..10)
            .map(|r| Some(format!("s{r:08}")))
            .collect::<Vec<_>>()
            .into();
        let batch = moruna_kernel::arrow::array::RecordBatch::try_new(
            schema,
            vec![Arc::new(ints), Arc::new(floats), Arc::new(strings)],
        )
        .expect("the batch");
        writer.write(&batch).expect("writing");
        writer.close().expect("closing");
    }
    let no_stats = ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![bare.display().to_string()],
            filters: vec![RowFilter::Gt("i64_0".into(), ScalarValue::I64(1_000_000))],
            ..ParquetSourceConfig::default()
        },
        Arc::new(seed(FakeReactor::new(), &bare)) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the parquet source");
    assert_eq!(no_stats.plan().expect("the plan").len(), 1);
    assert_eq!(no_stats.stats().groups_skipped, 0);
}

// SO-T9 tensor_view_in_arena. e.4.
#[test]
fn so_t9_tensor_view_in_arena() {
    let dir = scratch();
    let cases = [
        write_amb1(&dir, "t9", vec![64, 8], 1.0),
        write_safetensors(&dir, "t9", vec![64, 8], 1000.0),
    ];
    for (path, values) in cases {
        let reactor = seed(FakeReactor::new(), &path);
        let observer = reactor.clone();
        let source = tensor_over(&path, reactor);
        let alloc = CountingAllocator::new(FakeAllocator::new());
        let split = source.plan().expect("the plan").remove(0);
        let page = alloc.page_bytes() as u64;

        let mut state = 7u64;
        for _ in 0..50 {
            let before = observer.ops().len();
            let start = rng(&mut state) % split.rows;
            let end = start + 1 + rng(&mut state) % (split.rows - start);
            let payload =
                block_on(source.read(&split, Some(RowRange { start, end }), &alloc, Tier::Host))
                    .expect("a tensor read");

            let ops = observer.ops();
            assert_eq!(ops.len(), before + 1, "one read_file per tensor read");
            let op = ops.last().expect("the read");
            assert_eq!(op.kind, OpKind::ReadFile);
            assert!(op.offset.is_multiple_of(page), "offset {} ", op.offset);
            let file_len = std::fs::metadata(&path).expect("the file").len();
            assert!(
                op.len.is_multiple_of(page) || op.offset + op.len == file_len,
                "length {} is neither a page multiple nor the file's last page",
                op.len
            );

            let Payload::Tensor(tensor, _) = &payload else {
                panic!("a tensor");
            };
            let (ptr, offset) = tensor.data_ptr();
            // The view sits at `lo - page_floor(lo)` inside the one buffer of the read.
            assert!(alloc.contains(unsafe { ptr.add(offset as usize) }));
            assert!(alloc.contains(ptr));
            assert_eq!(offset, (op.offset + offset) % page);
            assert_eq!(tensor.shape(), &[(end - start) as i64, 8]);

            let got = tensor_values(&payload);
            let expected = &values[(start as usize) * 8..(end as usize) * 8];
            assert_eq!(got, expected, "rows {start}..{end} of {}", path.display());
        }
        assert_eq!(source.stats().decode_bytes, 0);
        assert_eq!(alloc.payload_copies(), 0);
        assert_eq!(alloc.stats().payload_copies_total, 0);
    }

    // A file whose bytes stop short of what its header claims is a `Source` error naming the
    // file and the length the tensor needed (h).
    let (path, _) = write_amb1(&dir, "t9-short", vec![64, 8], 1.0);
    let full = std::fs::metadata(&path).expect("the file").len() as usize;
    // The file keeps its header and the first page of its payload, so the read's offset is
    // still inside it. Truncating further makes `FakeReactor::read_file_opt` panic rather than
    // return a short read, which is reported as a testkit defect.
    let source = tensor_over(
        &path,
        seed_truncated(FakeReactor::new(), &path, full - 1024),
    );
    let alloc = FakeAllocator::new();
    let split = source.plan().expect("the plan").remove(0);
    let error = block_on(source.read(&split, None, &alloc, Tier::Host)).unwrap_err();
    assert!(matches!(error, MorunaError::Source { .. }), "{error}");
    assert!(error.to_string().contains("needs bytes to"), "{error}");
}

// SO-T10 iterator_source. (integration, closes in wave 3; `python` feature) f.5.
#[cfg(feature = "python")]
#[test]
#[ignore = "integration, closes in wave 3: needs CPython 3.14 free threaded with pyarrow"]
fn so_t10_iterator_source() {
    use moruna_sources::PyIteratorSource;
    use pyo3::prelude::*;
    pyo3::Python::initialize();
    let alloc = CountingAllocator::new(FakeAllocator::new());
    let (iter, schema) = pyo3::Python::attach(|py| {
        let module = pyo3::types::PyModule::from_code(
            py,
            std::ffi::CString::new(
                "import pyarrow as pa\n\
                 def gen():\n\
                 \x20   for i in range(5):\n\
                 \x20       yield pa.record_batch({'value': pa.array([i, i + 1])})\n",
            )
            .expect("the source")
            .as_c_str(),
            std::ffi::CString::new("gen.py").expect("a name").as_c_str(),
            std::ffi::CString::new("gen").expect("a module").as_c_str(),
        )
        .expect("the module");
        let iter = module
            .getattr("gen")
            .expect("gen")
            .call0()
            .expect("the generator")
            .unbind();
        let schema = SourceSchema::Table(Arc::new(moruna_kernel::arrow::datatypes::Schema::new(
            vec![moruna_kernel::arrow::datatypes::Field::new(
                "value",
                moruna_kernel::arrow::datatypes::DataType::Int64,
                true,
            )],
        )));
        (iter, schema)
    });
    let source = PyIteratorSource::new(
        iter,
        schema,
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
    )
    .expect("the iterator source");
    let splits = source.plan().expect("the plan");
    assert_eq!(splits.len(), 1);
    assert!(splits[0].estimated);
    assert!(!splits[0].sub_splittable);
    assert!(!source.repeatable());
    for _ in 0..5 {
        let payload = block_on(source.read(&splits[0], None, &alloc, Tier::Host)).expect("a pull");
        assert_eq!(payload.rows(), 2);
        for column in table(&payload).columns() {
            for buffer in column.to_data().buffers() {
                assert!(alloc.contains(buffer.as_ptr()));
            }
        }
    }
    let empty = block_on(source.read(&splits[0], None, &alloc, Tier::Host)).expect("the last pull");
    assert_eq!(empty.rows(), 0);
    let error = block_on(source.read(&splits[0], None, &alloc, Tier::Host)).unwrap_err();
    assert!(error.to_string().contains("exhausted"), "{error}");
}

// SO-T11 schema_mismatch. h.
#[test]
fn so_t11_schema_mismatch() {
    let dir = scratch();
    let first = write_parquet(&dir, "t11-a", 20, 20, 0);
    let second = dir.join("t11-b.parquet");
    {
        let schema = Arc::new(moruna_kernel::arrow::datatypes::Schema::new(vec![
            moruna_kernel::arrow::datatypes::Field::new(
                "i64_0",
                moruna_kernel::arrow::datatypes::DataType::Float64,
                false,
            ),
        ]));
        let file = std::fs::File::create(&second).expect("the file");
        let mut writer =
            parquet::arrow::ArrowWriter::try_new(file, Arc::clone(&schema), None).expect("writer");
        let values: Float64Array = (0..20).map(|r| r as f64).collect();
        let batch =
            moruna_kernel::arrow::array::RecordBatch::try_new(schema, vec![Arc::new(values)])
                .expect("the batch");
        writer.write(&batch).expect("writing");
        writer.close().expect("closing");
    }
    let error = err(ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![
                first.path.display().to_string(),
                second.display().to_string(),
            ],
            columns: Some(vec!["i64_0".into()]),
            ..ParquetSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    ));
    let text = error.to_string();
    assert!(text.contains("t11-a"), "{text}");
    assert!(text.contains("t11-b"), "{text}");

    // A projection naming a column the file does not have names the column and the file.
    let error = err(ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![first.path.display().to_string()],
            columns: Some(vec!["absent".into()]),
            ..ParquetSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    ));
    let text = error.to_string();
    assert!(text.contains("absent") && text.contains("t11-a"), "{text}");
}

// SO-T12 bandwidth. (reference host, E1; provisional elsewhere with the host named) S4, S12.
#[test]
#[ignore = "reference host, E1: a bandwidth ratio against the reactor's RE-T11 figure, which \
            needs the real reactor over local NVMe"]
fn so_t12_bandwidth() {
    // The figure is a ratio of this source's throughput after decode to the reactor's RE-T11
    // figure on the same host. It needs the real reactor (component 6) and the reference host;
    // neither is available to a component test over `FakeReactor`.
    unimplemented!("closes on the reference host with the real reactor");
}

// SO-T13 repeatable_reads. SO-I8.
#[test]
fn so_t13_repeatable_reads() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t13", 200, 50, 3);
    let source = parquet_over(&ledger, seed(FakeReactor::new(), &ledger.path));
    let alloc = FakeAllocator::new();
    assert!(source.repeatable());
    let first = source.plan().expect("the plan");
    let second = source.plan().expect("the plan");
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.rows, b.rows);
        assert_eq!(a.uncompressed_bytes, b.uncompressed_bytes);
        assert_eq!(a.null_counts, b.null_counts);
    }

    let mut state = 99u64;
    for _ in 0..50 {
        let split = &first[(rng(&mut state) % first.len() as u64) as usize];
        let start = rng(&mut state) % split.rows;
        let end = start + 1 + rng(&mut state) % (split.rows - start);
        let rows = Some(RowRange { start, end });
        let a = block_on(source.read(split, rows, &alloc, Tier::Host)).expect("a read");
        let b = block_on(source.read(split, rows, &alloc, Tier::Host)).expect("a read");
        assert_eq!(table(&a), table(&b));
    }

    let (path, _) = write_amb1(&dir, "t13", vec![32, 4], 0.0);
    let tensor = tensor_over(&path, seed(FakeReactor::new(), &path));
    assert!(tensor.repeatable());
    let split = tensor.plan().expect("the plan").remove(0);
    let a = block_on(tensor.read(&split, None, &alloc, Tier::Host)).expect("a read");
    let b = block_on(tensor.read(&split, None, &alloc, Tier::Host)).expect("a read");
    assert_eq!(tensor_values(&a), tensor_values(&b));
}

// SO-T14 oversized_row_natural_size. SO-I9.
#[test]
fn so_t14_oversized_row_natural_size() {
    let dir = scratch();
    // A single row above the smallest value `morsel.max_bytes` may take (64 MiB, preamble
    // section 5), which is the bound on the scheduler's target a source can know. The
    // document's 300 MiB is the same property at a size that makes the gate several
    // gigabytes heavier.
    let huge = 65 * 1024 * 1024;
    let path = write_one_huge_row(&dir, "t14", huge);
    let source = ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![path.display().to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(seed(FakeReactor::new(), &path)) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the parquet source");
    let alloc = FakeAllocator::new();
    let split = source.plan().expect("the plan").remove(0);
    let rows = Some(RowRange { start: 1, end: 2 });
    let payload = block_on(source.read(&split, rows, &alloc, Tier::Host)).expect("the huge row");
    assert_eq!(payload.rows(), 1);
    assert!(payload.bytes() >= huge as u64, "{}", payload.bytes());
    assert_eq!(source.stats().oversized_rows, 1);

    // With a budget under the row, the arena's `Alloc` comes back, not a truncated payload.
    let capped = FakeAllocator::new().with_limit(Tier::Host, 32 * 1024 * 1024);
    let error = block_on(source.read(&split, rows, &capped, Tier::Host)).unwrap_err();
    assert!(matches!(error, MorunaError::Alloc { .. }), "{error}");

    // The same for a tensor whose one row is above the bound.
    let rows_of = (huge / 4) as i64;
    let (tensor_path, _) = write_amb1(&dir, "t14", vec![1, rows_of], 0.0);
    let tensor = tensor_over(&tensor_path, seed(FakeReactor::new(), &tensor_path));
    let split = tensor.plan().expect("the plan").remove(0);
    let payload = block_on(tensor.read(&split, None, &alloc, Tier::Host)).expect("the huge row");
    assert_eq!(payload.rows(), 1);
    assert!(payload.bytes() >= huge as u64);
    assert_eq!(tensor.stats().oversized_rows, 1);
    let error = block_on(tensor.read(&split, None, &capped, Tier::Host)).unwrap_err();
    assert!(matches!(error, MorunaError::Alloc { .. }), "{error}");
}

// SO-T15 read_failure_paths. h, SO-I6.
#[test]
fn so_t15_read_failure_paths() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t15", 80, 80, 0);
    let reactor = seed(FakeReactor::new(), &ledger.path).fail_next(OpKind::ReadFile, 1);
    let observer = reactor.clone();
    let source = parquet_over(&ledger, reactor);
    let alloc = FakeAllocator::new();
    let split = source.plan().expect("the plan").remove(0);
    let baseline = alloc.in_use(Tier::Host);

    let error = block_on(source.read(&split, None, &alloc, Tier::Host)).unwrap_err();
    assert!(
        matches!(error, MorunaError::Source { split: id, .. } if id == split.id),
        "{error}"
    );
    assert!(error.to_string().contains("reading bytes"), "{error}");
    // The buffer the failed read was given is released.
    assert_eq!(alloc.in_use(Tier::Host), baseline);

    let good = block_on(source.read(&split, None, &alloc, Tier::Host)).expect("the retry");
    assert_eq!(good.rows(), split.rows);
    let again = block_on(source.read(&split, None, &alloc, Tier::Host)).expect("a third read");
    assert_eq!(table(&good), table(&again));

    // An allocation failure comes back before any reactor operation is issued.
    let ops_before = observer.ops().len();
    let failing = FakeAllocator::new().fail_next(1);
    let error = block_on(source.read(&split, None, &failing, Tier::Host)).unwrap_err();
    assert!(matches!(error, MorunaError::Alloc { .. }), "{error}");
    assert_eq!(observer.ops().len(), ops_before);

    // The same on the tensor path.
    let (path, _) = write_amb1(&dir, "t15", vec![16, 4], 0.0);
    let reactor = seed(FakeReactor::new(), &path).fail_next(OpKind::ReadFile, 1);
    let observer = reactor.clone();
    let tensor = tensor_over(&path, reactor);
    let split = tensor.plan().expect("the plan").remove(0);
    let baseline = alloc.in_use(Tier::Host);
    let error = block_on(tensor.read(&split, None, &alloc, Tier::Host)).unwrap_err();
    assert!(matches!(error, MorunaError::Source { .. }), "{error}");
    assert_eq!(alloc.in_use(Tier::Host), baseline);
    let ops_before = observer.ops().len();
    let failing = FakeAllocator::new().fail_next(1);
    let error = block_on(tensor.read(&split, None, &failing, Tier::Host)).unwrap_err();
    assert!(matches!(error, MorunaError::Alloc { .. }), "{error}");
    assert_eq!(observer.ops().len(), ops_before);
    let good = block_on(tensor.read(&split, None, &alloc, Tier::Host)).expect("the retry");
    assert_eq!(good.rows(), 16);
}

// SO-T16 local_paths_use_read_file. d.1, e.1.
#[test]
fn so_t16_local_paths_use_read_file() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "t16", 60, 60, 0);
    let alloc = FakeAllocator::new();

    for url in [
        ledger.path.display().to_string(),
        format!("file://{}", ledger.path.display()),
    ] {
        let reactor = seed(FakeReactor::new(), &ledger.path);
        let observer = reactor.clone();
        // `NoObjectStore` panics on either of its methods, so a local url that reached the
        // object store would fail the test rather than pass it quietly.
        let source = ParquetSource::new(
            ParquetSourceConfig {
                urls: vec![url.clone()],
                ..ParquetSourceConfig::default()
            },
            Arc::new(reactor) as Arc<dyn Reactor>,
            Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
        )
        .expect("the parquet source");
        let split = source.plan().expect("the plan").remove(0);
        block_on(source.read(&split, None, &alloc, Tier::Host)).expect("a read");
        assert!(!observer.ops().is_empty(), "{url}");
        for op in observer.ops() {
            assert_eq!(op.kind, OpKind::ReadFile, "{url} issued {:?}", op.kind);
        }
    }

    // A directory is listed with `read_dir`, not through the object store.
    let listed = ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![dir.display().to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(seed(FakeReactor::new(), &ledger.path)) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the parquet source");
    assert_eq!(listed.plan().expect("the plan").len(), 1);
}

// SO-T16, the object-store half (MH 4.6, H7): an object URL's footer is read with
// `read_object` into the arena at plan time and its column chunks with `read_object` at read
// time; nothing touches `read_file`. A prefix lists every Parquet object under it.
#[test]
fn so_t16_object_urls_use_the_object_store() {
    let dir = scratch();
    let first = write_parquet(&dir, "t16-obj-a", 60, 20, 0);
    let second = write_parquet(&dir, "t16-obj-b", 40, 40, 0);
    let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    let reactor = FakeReactor::new()
        .with_file(
            "s3://bucket/data/a.parquet",
            std::fs::read(&first.path).expect("fixture"),
        )
        .with_file(
            "s3://bucket/data/b.parquet",
            std::fs::read(&second.path).expect("fixture"),
        )
        .with_file("s3://bucket/data/notes.txt", b"not parquet".to_vec());
    let observer = reactor.clone();
    let source = ParquetSource::with_allocator(
        ParquetSourceConfig {
            urls: vec!["s3://bucket/data/".to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(reactor.clone()) as Arc<dyn Reactor>,
        Arc::new(reactor) as Arc<dyn ObjectMetadata>,
        alloc.clone(),
    )
    .expect("a source over objects");
    let plan = source.plan().expect("the plan");
    assert_eq!(plan.len(), 4, "three row groups of a, one of b");
    let mut rows = 0;
    for split in &plan {
        rows += block_on(source.read(split, None, alloc.as_ref(), Tier::Host))
            .expect("a read")
            .rows();
    }
    assert_eq!(rows, 100, "every row of both objects");
    let kinds: Vec<OpKind> = observer.ops().iter().map(|op| op.kind).collect();
    assert!(kinds.contains(&OpKind::ListPrefix));
    assert!(kinds.contains(&OpKind::ReadObject));
    assert!(
        !kinds.contains(&OpKind::ReadFile),
        "an object never goes through read_file: {kinds:?}"
    );

    // One object by name, listed as nothing under it, is sized with `head_object`.
    let single = FakeReactor::new().with_file(
        "gs://bucket/one.parquet",
        std::fs::read(&second.path).expect("fixture"),
    );
    let lister = HeadOnly(single.clone());
    let one = ParquetSource::with_allocator(
        ParquetSourceConfig {
            urls: vec!["gs://bucket/one.parquet".to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(single.clone()) as Arc<dyn Reactor>,
        Arc::new(lister) as Arc<dyn ObjectMetadata>,
        alloc.clone(),
    )
    .expect("a single object");
    assert_eq!(one.plan().expect("plan").len(), 1);
    assert!(
        single.ops().iter().any(|op| op.kind == OpKind::HeadObject),
        "sized by head_object"
    );

    // A footer larger than the first tail read comes back in a second read.
    let wide = write_parquet(&dir, "t16-obj-wide", 3000, 1, 0);
    let big = FakeReactor::new().with_file(
        "s3://bucket/wide.parquet",
        std::fs::read(&wide.path).expect("fixture"),
    );
    let many = ParquetSource::with_allocator(
        ParquetSourceConfig {
            urls: vec!["s3://bucket/wide.parquet".to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(big.clone()) as Arc<dyn Reactor>,
        Arc::new(big.clone()) as Arc<dyn ObjectMetadata>,
        alloc.clone(),
    )
    .expect("a footer over 64 KiB");
    assert_eq!(many.plan().expect("plan").len(), 3000);
    assert!(
        big.ops()
            .iter()
            .filter(|op| op.kind == OpKind::ReadObject)
            .count()
            >= 2,
        "the tail, then the rest of the metadata"
    );

    // Built without an allocator, an object URL says what it needs.
    let without = FakeReactor::new().with_file(
        "s3://bucket/data/a.parquet",
        std::fs::read(&first.path).expect("fixture"),
    );
    let error = ParquetSource::new(
        ParquetSourceConfig {
            urls: vec!["s3://bucket/data/a.parquet".to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(without.clone()) as Arc<dyn Reactor>,
        Arc::new(without) as Arc<dyn ObjectMetadata>,
    )
    .err()
    .expect("no allocator, no object footer");
    assert!(error.to_string().contains("with_allocator"), "{error}");

    // An object that is not Parquet is refused naming it.
    for (url, bytes) in [
        ("s3://bucket/short.parquet", b"PAR1".to_vec()),
        ("s3://bucket/plain.parquet", vec![0u8; 64]),
        (
            "s3://bucket/liar.parquet",
            [
                vec![0u8; 16],
                1000u32.to_le_bytes().to_vec(),
                b"PAR1".to_vec(),
            ]
            .concat(),
        ),
    ] {
        let fake = FakeReactor::new().with_file(url, bytes);
        let error = ParquetSource::with_allocator(
            ParquetSourceConfig {
                urls: vec![url.to_string()],
                ..ParquetSourceConfig::default()
            },
            Arc::new(fake.clone()) as Arc<dyn Reactor>,
            Arc::new(fake) as Arc<dyn ObjectMetadata>,
            alloc.clone(),
        )
        .err()
        .expect("not parquet");
        assert!(error.to_string().contains(url), "{url}: {error}");
    }
}

/// Lists nothing, so a single object's URL falls through to `head_object`, as a real store's
/// one-level listing of an object's own key does.
struct HeadOnly(FakeReactor);

impl ObjectMetadata for HeadOnly {
    fn head_object(&self, url: &str) -> moruna_kernel::Completion<moruna_kernel::ObjectMeta> {
        let completion = self.0.head_object(url);
        let meta = completion.wait();
        let (sender, out) = moruna_kernel::Completion::channel();
        // The real reactor names an object by its key, not its URL.
        sender.resolve(meta.map(|mut m| {
            m.url = "one.parquet".to_string();
            m
        }));
        out
    }

    fn list_prefix(&self, _url: &str) -> moruna_kernel::Completion<Vec<moruna_kernel::ObjectMeta>> {
        let (sender, out) = moruna_kernel::Completion::channel();
        sender.resolve(Ok(Vec::new()));
        out
    }
}

/// Dictionary columns are unpacked before the copy (f.4), so no payload leaves this crate as a
/// dictionary array.
#[test]
fn dictionary_columns_are_unpacked_before_the_copy() {
    let dir = scratch();
    let path = write_dictionary_parquet(&dir, "dict", 32);
    let source = ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![path.display().to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(seed(FakeReactor::new(), &path)) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the parquet source");
    let alloc = FakeAllocator::new();
    let split = source.plan().expect("the plan").remove(0);
    let payload = block_on(source.read(&split, None, &alloc, Tier::Host)).expect("a read");
    let batch = table(&payload);
    assert!(!matches!(
        batch.column(0).data_type(),
        moruna_kernel::arrow::datatypes::DataType::Dictionary(_, _)
    ));
    let strings = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("a plain string column");
    assert_eq!(strings.value(5), "v1");
}

/// The three tensor formats plan alike, and the ones that cannot be read are refused at `new`
/// (e.3, h).
#[test]
fn tensor_formats_plan_and_refuse() {
    let dir = scratch();
    let npy = write_npy(&dir, "row-major", &[8, 4], false);
    let source = tensor_over(&npy, seed(FakeReactor::new(), &npy));
    let split = source.plan().expect("the plan").remove(0);
    assert_eq!(split.rows, 8);
    let alloc = FakeAllocator::new();
    let payload = block_on(source.read(&split, None, &alloc, Tier::Host)).expect("a read");
    assert_eq!(tensor_values(&payload).len(), 32);
    let SourceSchema::Tensor { shape, .. } = source.schema() else {
        panic!("a tensor schema");
    };
    assert_eq!(shape, vec![-1, 4], "a variable batch dimension");

    let fortran = write_npy(&dir, "column-major", &[8, 4], true);
    let error = err(TensorSource::new(
        TensorSourceConfig {
            paths: vec![fortran],
            ..TensorSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
    ));
    assert!(error.to_string().contains("Fortran order"), "{error}");

    let plain = dir.join("plain.bin");
    std::fs::write(&plain, b"not a tensor at all").expect("the file");
    let error = err(TensorSource::new(
        TensorSourceConfig {
            paths: vec![plain],
            ..TensorSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
    ));
    assert!(matches!(error, MorunaError::Plan(_)), "{error}");

    let error = err(TensorSource::new(
        TensorSourceConfig {
            paths: vec![std::path::PathBuf::from("s3://bucket/weights.safetensors")],
            ..TensorSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
    ));
    assert!(error.to_string().contains("local files only"), "{error}");
}

/// A safetensors file of several tensors plans one split per tensor, and the projection picks
/// them by name (d.1, e.3).
#[test]
fn safetensors_tensors_are_selected_by_name() {
    let dir = scratch();
    let path = dir.join("model.safetensors");
    let header = "{\"bias\":{\"dtype\":\"F32\",\"shape\":[4],\"data_offsets\":[64,80]},\
                  \"weight\":{\"dtype\":\"F32\",\"shape\":[4,4],\"data_offsets\":[0,64]}}";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(header.len() as u64).to_le_bytes());
    bytes.extend_from_slice(header.as_bytes());
    for i in 0..20 {
        bytes.extend_from_slice(&(i as f32).to_le_bytes());
    }
    std::fs::write(&path, &bytes).expect("the file");

    let all = tensor_over(&path, seed(FakeReactor::new(), &path));
    let splits = all.plan().expect("the plan");
    assert_eq!(splits.len(), 2);
    // Splits are in data-offset order: weight first, then bias.
    assert_eq!(splits[0].rows, 4);
    assert_eq!(splits[1].rows, 4);

    let one = TensorSource::new(
        TensorSourceConfig {
            paths: vec![path.clone()],
            tensors: Some(vec!["bias".into()]),
            ..TensorSourceConfig::default()
        },
        Arc::new(seed(FakeReactor::new(), &path)) as Arc<dyn Reactor>,
    )
    .expect("the tensor source");
    assert_eq!(one.plan().expect("the plan").len(), 1);

    let error = err(TensorSource::new(
        TensorSourceConfig {
            paths: vec![path],
            tensors: Some(vec!["absent".into()]),
            ..TensorSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
    ));
    assert!(error.to_string().contains("absent"), "{error}");
}

/// Both sources report the schema the plan implies (contracts d.4).
#[test]
fn schemas_describe_the_plan() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "schema", 10, 10, 0);
    let source = parquet_over(&ledger, seed(FakeReactor::new(), &ledger.path));
    let SourceSchema::Table(schema) = source.schema() else {
        panic!("a table schema");
    };
    assert_eq!(schema.fields().len(), 3);

    let (path, _) = write_amb1(&dir, "schema", vec![4, 2], 0.0);
    let tensor = tensor_over(&path, seed(FakeReactor::new(), &path));
    let SourceSchema::Tensor { dtype, shape } = tensor.schema() else {
        panic!("a tensor schema");
    };
    assert_eq!(dtype, moruna_kernel::DType::F32);
    assert_eq!(shape, vec![-1, 2]);
}

/// A `ParquetSource` with no url, and a `TensorSource` with no path, are refused at `new`.
#[test]
fn an_empty_configuration_is_refused() {
    let error = err(ParquetSource::new(
        ParquetSourceConfig::default(),
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    ));
    assert!(matches!(error, MorunaError::Plan(_)), "{error}");
    let error = err(TensorSource::new(
        TensorSourceConfig::default(),
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
    ));
    assert!(matches!(error, MorunaError::Plan(_)), "{error}");
}

/// A `TensorSource` over one crafted file, for the failure paths of h.
fn tensor_err(dir: &std::path::Path, name: &str, bytes: &[u8]) -> MorunaError {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("the file");
    err(TensorSource::new(
        TensorSourceConfig {
            paths: vec![path],
            ..TensorSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
    ))
}

/// A safetensors header, length prefixed as the format wants.
fn safetensors_bytes(header: &str, payload: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(header.len() as u64).to_le_bytes());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend(std::iter::repeat_n(0u8, payload));
    bytes
}

/// Every way a safetensors header can be wrong is named, not panicked on (h).
#[test]
fn safetensors_headers_are_validated() {
    let dir = scratch();
    let cases: [(&str, Vec<u8>, &str); 8] = [
        ("short.safetensors", vec![0u8; 4], "has no header length"),
        (
            "past-end.safetensors",
            {
                let mut b = 4096u64.to_le_bytes().to_vec();
                b.extend_from_slice(b"{}");
                b
            },
            "but the file is",
        ),
        (
            "not-json.safetensors",
            safetensors_bytes("not json at all", 0),
            "not JSON",
        ),
        (
            "not-object.safetensors",
            safetensors_bytes("[1,2,3]", 0),
            "not a JSON object",
        ),
        (
            "empty.safetensors",
            safetensors_bytes("{\"__metadata__\":{\"k\":\"v\"}}", 0),
            "names no tensor",
        ),
        (
            "bad-dtype.safetensors",
            safetensors_bytes(
                "{\"w\":{\"dtype\":\"F8_E5M2\",\"shape\":[4],\"data_offsets\":[0,4]}}",
                4,
            ),
            "no DType for",
        ),
        (
            "negative.safetensors",
            safetensors_bytes(
                "{\"w\":{\"dtype\":\"F32\",\"shape\":[-4],\"data_offsets\":[0,16]}}",
                16,
            ),
            "negative dimension",
        ),
        (
            "span.safetensors",
            safetensors_bytes(
                "{\"w\":{\"dtype\":\"F32\",\"shape\":[4],\"data_offsets\":[0,8]}}",
                8,
            ),
            "but its shape needs",
        ),
    ];
    for (name, bytes, wanted) in cases {
        let error = tensor_err(&dir, name, &bytes).to_string();
        assert!(error.contains(wanted), "{name}: {error}");
    }

    // A tensor without a dtype, a shape or offsets is named too.
    for (name, header) in [
        ("no-dtype", "{\"w\":{\"shape\":[1],\"data_offsets\":[0,4]}}"),
        (
            "no-shape",
            "{\"w\":{\"dtype\":\"F32\",\"data_offsets\":[0,4]}}",
        ),
        ("no-offsets", "{\"w\":{\"dtype\":\"F32\",\"shape\":[1]}}"),
        (
            "one-offset",
            "{\"w\":{\"dtype\":\"F32\",\"shape\":[1],\"data_offsets\":[0]}}",
        ),
        (
            "float-dim",
            "{\"w\":{\"dtype\":\"F32\",\"shape\":[1.5],\"data_offsets\":[0,4]}}",
        ),
        ("not-a-tensor", "{\"w\":7}"),
    ] {
        let error = tensor_err(
            &dir,
            &format!("{name}.safetensors"),
            &safetensors_bytes(header, 8),
        );
        assert!(!error.to_string().is_empty(), "{name}");
    }

    // A header longer than the 64 KiB probe is read a second time (e.3).
    let padding = " ".repeat(70 * 1024);
    let header =
        format!("{{\"w\":{{\"dtype\":\"F32\",\"shape\":[4],\"data_offsets\":[0,16]}}{padding}}}");
    let header = header.replace("}}  ", "}} ");
    let path = dir.join("long-header.safetensors");
    let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend(std::iter::repeat_n(0u8, 16));
    std::fs::write(&path, &bytes).expect("the file");
    let source = tensor_over(&path, seed(FakeReactor::new(), &path));
    assert_eq!(source.plan().expect("the plan")[0].rows, 4);
}

/// Every way a `.npy` header can be wrong is named, not panicked on (h).
#[test]
fn npy_headers_are_validated() {
    let dir = scratch();
    let header = |text: &str| {
        let mut h = text.to_string();
        while !(10 + h.len() + 1).is_multiple_of(64) {
            h.push(' ');
        }
        h.push('\n');
        h
    };
    let v1 = |text: &str, payload: usize| {
        let h = header(text);
        let mut bytes = b"\x93NUMPY\x01\x00".to_vec();
        bytes.extend_from_slice(&(h.len() as u16).to_le_bytes());
        bytes.extend_from_slice(h.as_bytes());
        bytes.extend(std::iter::repeat_n(0u8, payload));
        bytes
    };

    // A version 2 header, whose length is four bytes wide.
    let h = header("{'descr': '<f4', 'fortran_order': False, 'shape': (2,), }");
    let mut v2 = b"\x93NUMPY\x02\x00".to_vec();
    v2.extend_from_slice(&(h.len() as u32).to_le_bytes());
    v2.extend_from_slice(h.as_bytes());
    v2.extend(std::iter::repeat_n(0u8, 8));
    let path = dir.join("v2.npy");
    std::fs::write(&path, &v2).expect("the file");
    let source = tensor_over(&path, seed(FakeReactor::new(), &path));
    assert_eq!(source.plan().expect("the plan")[0].rows, 2);

    for (name, bytes, wanted) in [
        ("tiny.npy", b"\x93NUMPY".to_vec(), "NumPy magic"),
        (
            "version.npy",
            {
                let mut b = b"\x93NUMPY\x09\x00".to_vec();
                b.extend_from_slice(&[0u8; 8]);
                b
            },
            "major version",
        ),
        (
            "past-end.npy",
            {
                let mut b = b"\x93NUMPY\x01\x00".to_vec();
                b.extend_from_slice(&40000u16.to_le_bytes());
                b.extend_from_slice(&[0u8; 8]);
                b
            },
            "but the file is",
        ),
        (
            "no-descr.npy",
            v1("{'fortran_order': False, 'shape': (1,), }", 4),
            "no descr",
        ),
        (
            "no-order.npy",
            v1("{'descr': '<f4', 'shape': (1,), }", 4),
            "no fortran_order",
        ),
        (
            "no-shape.npy",
            v1("{'descr': '<f4', 'fortran_order': False, }", 4),
            "no shape",
        ),
        (
            "bad-shape.npy",
            v1("{'descr': '<f4', 'fortran_order': False, 'shape': 5, }", 4),
            "does not parse",
        ),
        (
            "big-endian.npy",
            v1(
                "{'descr': '>f4', 'fortran_order': False, 'shape': (1,), }",
                4,
            ),
            "little endian",
        ),
    ] {
        let error = tensor_err(&dir, name, &bytes).to_string();
        assert!(error.contains(wanted), "{name}: {error}");
    }

    // A version 2 header short of its own length prefix.
    let error = tensor_err(&dir, "v2-tiny.npy", b"\x93NUMPY\x02\x00\x00\x00").to_string();
    assert!(error.contains("version 2 header"), "{error}");
}

/// Every way an `MRB1` header can be wrong is named, not panicked on (contracts e.4, h).
#[test]
fn amb1_headers_are_validated() {
    let dir = scratch();
    let good = |dtype: u8, ndim: u8, shape: &[i64], data_offset: u64, payload: usize| {
        let mut bytes = b"MRB1".to_vec();
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(dtype);
        bytes.push(ndim);
        for d in shape {
            bytes.extend_from_slice(&d.to_le_bytes());
        }
        bytes.extend_from_slice(&data_offset.to_le_bytes());
        bytes.resize(data_offset as usize, 0);
        bytes.extend(std::iter::repeat_n(0u8, payload));
        bytes
    };
    for (name, bytes, wanted) in [
        (
            "tiny.mrb1",
            b"MRB1\x01\x00".to_vec(),
            "shorter than a header",
        ),
        (
            "dtype.mrb1",
            good(200, 1, &[4], 4096, 16),
            "unknown dtype code",
        ),
        ("ndim.mrb1", good(10, 9, &[4], 4096, 16), "ndim 9 exceeds"),
        (
            "offset.mrb1",
            good(10, 1, &[4], 4000, 16),
            "not a multiple of 4096",
        ),
        (
            "short.mrb1",
            good(10, 1, &[4], 4096, 4),
            "short of the 4112 bytes",
        ),
    ] {
        let error = tensor_err(&dir, name, &bytes).to_string();
        assert!(error.contains(wanted), "{name}: {error}");
    }

    // A header whose padding is not what the MRB1 writer would produce is rejected.
    let mut tampered = good(10, 1, &[4], 4096, 16);
    tampered[100] = 0xff;
    let error = tensor_err(&dir, "tampered.mrb1", &tampered).to_string();
    assert!(error.contains("would produce"), "{error}");

    // A file whose header is cut before its padding ends.
    let mut cut = good(10, 1, &[4], 4096, 16);
    cut.truncate(64);
    let error = tensor_err(&dir, "cut.mrb1", &cut).to_string();
    assert!(error.contains("short of"), "{error}");
}

/// The rest of the plan and read failures of h.
#[test]
fn plan_and_read_failures_are_named() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "fail", 40, 20, 0);
    let alloc = FakeAllocator::new();

    // A filter naming a column the file does not have.
    let error = err(ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![ledger.path.display().to_string()],
            filters: vec![RowFilter::Gt("absent".into(), ScalarValue::I64(1))],
            ..ParquetSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    ));
    assert!(error.to_string().contains("absent"), "{error}");

    // A url that is not there at all.
    let error = err(ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![dir.join("absent.parquet").display().to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    ));
    assert!(matches!(error, MorunaError::Plan(_)), "{error}");

    // A file that is not Parquet at all.
    let junk = dir.join("junk.parquet");
    std::fs::write(&junk, b"not parquet").expect("the file");
    let error = err(ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![junk.display().to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    ));
    assert!(error.to_string().contains("footer"), "{error}");
    std::fs::remove_file(&junk).expect("removing the junk");

    // A directory with no Parquet file in it.
    let empty = scratch();
    let error = err(ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![empty.display().to_string()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(FakeReactor::new()) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    ));
    assert!(error.to_string().contains("no Parquet file"), "{error}");

    // A range outside the split, on both sources.
    let source = parquet_over(&ledger, seed(FakeReactor::new(), &ledger.path));
    let split = source.plan().expect("the plan").remove(0);
    let error = block_on(source.read(
        &split,
        Some(RowRange { start: 0, end: 999 }),
        &alloc,
        Tier::Host,
    ))
    .unwrap_err();
    assert!(error.to_string().contains("outside the split"), "{error}");

    // An object url is refused at plan time with the escalation named.
    let reactor = FakeReactor::new().with_file("s3://bucket/a.parquet", vec![0u8; 8]);
    let error = err(ParquetSource::new(
        ParquetSourceConfig {
            urls: vec!["s3://bucket/".into()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(reactor.clone()) as Arc<dyn Reactor>,
        Arc::new(reactor) as Arc<dyn ObjectMetadata>,
    ));
    assert!(error.to_string().contains("E10"), "{error}");

    // A prefix with nothing under it heads the object and still reports the escalation.
    let bare = FakeReactor::new().with_file("s3://empty/a", vec![0u8; 1]);
    let error = err(ParquetSource::new(
        ParquetSourceConfig {
            urls: vec!["s3://other/".into()],
            ..ParquetSourceConfig::default()
        },
        Arc::new(bare.clone()) as Arc<dyn Reactor>,
        Arc::new(bare) as Arc<dyn ObjectMetadata>,
    ));
    assert!(matches!(error, MorunaError::Io { .. }), "{error}");
}

/// A projected set of columns far apart in the file is fetched as more than one range (f.3).
#[test]
fn distant_columns_are_fetched_as_separate_ranges() {
    let dir = scratch();
    // Wide rows put the projected chunks more than a megabyte apart.
    let ledger = write_parquet(&dir, "wide", 40_000, 40_000, 0);
    let reactor = seed(FakeReactor::new(), &ledger.path);
    let observer = reactor.clone();
    let source = ParquetSource::new(
        ParquetSourceConfig {
            urls: vec![ledger.path.display().to_string()],
            columns: Some(vec!["i64_0".into(), "str_0".into()]),
            ..ParquetSourceConfig::default()
        },
        Arc::new(reactor) as Arc<dyn Reactor>,
        Arc::new(NoObjectStore) as Arc<dyn ObjectMetadata>,
    )
    .expect("the parquet source");
    let alloc = FakeAllocator::new();
    let split = source.plan().expect("the plan").remove(0);
    let payload = block_on(source.read(&split, None, &alloc, Tier::Host)).expect("a read");
    assert_eq!(payload.rows(), 40_000);
    assert!(!observer.ops().is_empty());
    for op in observer.ops() {
        assert_eq!(op.kind, OpKind::ReadFile);
        assert!(op.offset.is_multiple_of(4096), "offset {}", op.offset);
    }
}

/// Environment fact (section l): `RowSelection` skips the rows before the selection rather
/// than decoding them. A ten row range of a one million row group must cost far less than the
/// whole group. Run with `--ignored --nocapture`; it is a measurement, not a gate.
#[test]
#[ignore = "environment fact of section l, measured before coding; prints a ratio"]
fn fact_row_selection_skips() {
    let dir = scratch();
    let ledger = write_parquet(&dir, "fact", 1_000_000, 1_000_000, 0);
    let source = parquet_over(&ledger, seed(FakeReactor::new(), &ledger.path));
    let alloc = FakeAllocator::new();
    let split = source.plan().expect("the plan").remove(0);
    let whole = std::time::Instant::now();
    let all = block_on(source.read(&split, None, &alloc, Tier::Host)).expect("the whole group");
    let whole = whole.elapsed();
    assert_eq!(all.rows(), 1_000_000);
    drop(all);
    let narrow = std::time::Instant::now();
    let ten = block_on(source.read(
        &split,
        Some(RowRange {
            start: 900_000,
            end: 900_010,
        }),
        &alloc,
        Tier::Host,
    ))
    .expect("ten rows");
    let narrow = narrow.elapsed();
    assert_eq!(ten.rows(), 10);
    println!(
        "whole group {whole:?}, ten rows at 900000 {narrow:?}, ratio {:.3}",
        narrow.as_secs_f64() / whole.as_secs_f64()
    );
}
