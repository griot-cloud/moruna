//! The generator's own tests: the properties preamble 6.5 asks of the data, read
//! back from the files rather than from the code that wrote them.
//!
//! The S3 half is skipped, with a printed reason, on a host with no reachable
//! S3 compatible store. It runs in CI's MinIO job, where the four environment
//! variables of `s3::REQUIRED` are set.

use std::path::{Path, PathBuf};

use moruna_bench::mrb1;
use moruna_bench::cli;
use moruna_bench::dataset::{Dataset, DatasetKind};
use moruna_bench::dtype::DType;
use moruna_bench::parquet_out::ParquetSpec;
use moruna_bench::s3;
use moruna_bench::suite::{self, Scale};

use arrow::array::Array;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::reader::{FileReader, SerializedFileReader};

fn scratch(name: &str) -> PathBuf {
    const PREFIX: &str = "moruna-bench-it";
    // Unique per process and per call: several executors run the gate at the same
    // time on one machine, and a fixed name under the system temp directory made two
    // runs delete each other's files (three generator tests failed that way on
    // 2026-09-22, green in isolation and red under a concurrent gate).
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("{}-{}-{}-{}", PREFIX, std::process::id(), n, name));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn run(line: &str) -> String {
    let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    let mut out: Vec<u8> = Vec::new();
    match moruna_bench::run(&args, &mut out) {
        Ok(()) => String::from_utf8_lossy(&out).into_owned(),
        Err(err) => panic!("{line}: {err}\n{}", String::from_utf8_lossy(&out)),
    }
}

fn read(path: &Path) -> Vec<u8> {
    match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => panic!("{}: {err}", path.display()),
    }
}

/// Every dataset is a pure function of the seed and the shape arguments: the
/// whole suite written twice, into two directories, is byte identical.
#[test]
fn the_whole_suite_is_byte_identical_between_two_runs() {
    let first = scratch("determinism-a");
    let second = scratch("determinism-b");
    run(&format!(
        "suite --scale small --seed 4242 --local-only --out {}",
        first.display()
    ));
    run(&format!(
        "suite --scale small --seed 4242 --local-only --out {}",
        second.display()
    ));
    let datasets = suite::datasets(Scale::Small).expect("suite");
    assert_eq!(datasets.len(), 13);
    for dataset in &datasets {
        let name = dataset.file_name();
        let a = read(&first.join(&name));
        let b = read(&second.join(&name));
        assert!(!a.is_empty(), "{name} is empty");
        assert_eq!(a, b, "{name} differs between two runs of the same command");
    }
    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);
}

/// A different seed is a different corpus, and the same seed through a different
/// subcommand is the same file.
#[test]
fn the_seed_selects_the_corpus() {
    let one = scratch("seed-one");
    let two = scratch("seed-two");
    let again = scratch("seed-again");
    for (dir, seed) in [(&one, 1u64), (&two, 2), (&again, 1)] {
        run(&format!(
            "dataset text-normalise --scale small --seed {seed} --local-only --out {}",
            dir.display()
        ));
    }
    let name = "text-normalise.parquet";
    assert_ne!(read(&one.join(name)), read(&two.join(name)));
    assert_eq!(read(&one.join(name)), read(&again.join(name)));
    for dir in [one, two, again] {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The null ratio and the long text length statistics come out of the file
/// within tolerance of what was asked for.
#[test]
fn the_null_ratio_and_the_text_statistics_hold_in_the_written_file() {
    let dir = scratch("stats");
    let rows = 40_000usize;
    run(&format!(
        "parquet --name stats --rows {rows} --int-cols 1 --float-cols 1 --string-cols 1 \
         --text-cols 1 --text-mean-len 300 --text-len-stddev 80 --null-ratio 0.25 \
         --row-group-rows 10000 --local-only --seed 9 --out {}",
        dir.display()
    ));
    let path = dir.join("stats.parquet");
    let file = std::fs::File::open(&path).expect("open");
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("builder")
        .with_batch_size(8192)
        .build()
        .expect("reader");

    let mut seen = 0usize;
    let mut nulls = [0usize; 4];
    let mut lengths: Vec<f64> = Vec::new();
    for batch in reader {
        let batch = batch.expect("batch");
        seen += batch.num_rows();
        for (index, column) in batch.columns().iter().enumerate() {
            nulls[index] += column.null_count();
        }
        let text = batch
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .expect("text column is utf8");
        for i in 0..text.len() {
            if !text.is_null(i) {
                lengths.push(text.value(i).len() as f64);
            }
        }
    }
    assert_eq!(seen, rows);
    for (index, count) in nulls.iter().enumerate() {
        let ratio = *count as f64 / rows as f64;
        assert!(
            (ratio - 0.25).abs() < 0.01,
            "column {index} null ratio {ratio}"
        );
    }
    let n = lengths.len() as f64;
    let mean = lengths.iter().sum::<f64>() / n;
    let variance = lengths.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n;
    let stddev = variance.sqrt();
    assert!((mean - 300.0).abs() < 8.0, "mean length {mean}");
    assert!((stddev - 80.0).abs() < 8.0, "length stddev {stddev}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A zero null ratio writes no nulls at all, and a ratio of one writes nothing
/// but nulls: the two ends of the knob.
#[test]
fn the_ends_of_the_null_ratio_are_exact() {
    let dir = scratch("null-ends");
    for (name, ratio, expected) in [("none", "0", 0usize), ("all", "1", 2_000usize)] {
        run(&format!(
            "parquet --name {name} --rows 2000 --int-cols 1 --float-cols 0 --string-cols 0 \
             --text-cols 0 --null-ratio {ratio} --row-group-rows 1000 --local-only --out {}",
            dir.display()
        ));
        let file = std::fs::File::open(dir.join(format!("{name}.parquet"))).expect("open");
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .expect("builder")
            .build()
            .expect("reader");
        let mut nulls = 0;
        for batch in reader {
            nulls += batch.expect("batch").column(0).null_count();
        }
        assert_eq!(nulls, expected, "{name}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The row group size is honoured: every row group but the last holds exactly
/// the number of rows that was asked for.
#[test]
fn the_row_group_size_is_honoured() {
    let dir = scratch("row-groups");
    let cases = [(10_000usize, 2_500usize), (9_000, 4_096), (1_000, 4_096)];
    for (rows, group) in cases {
        let name = format!("rg-{rows}-{group}");
        run(&format!(
            "parquet --name {name} --rows {rows} --int-cols 1 --float-cols 1 --string-cols 0 \
             --text-cols 0 --row-group-rows {group} --local-only --out {}",
            dir.display()
        ));
        let file = std::fs::File::open(dir.join(format!("{name}.parquet"))).expect("open");
        let reader = SerializedFileReader::new(file).expect("reader");
        let metadata = reader.metadata();
        let expected_groups = rows.div_ceil(group);
        assert_eq!(
            metadata.num_row_groups(),
            expected_groups,
            "{name}: row group count"
        );
        let mut total = 0i64;
        for index in 0..metadata.num_row_groups() {
            let group_rows = metadata.row_group(index).num_rows();
            total += group_rows;
            if index + 1 < expected_groups {
                assert_eq!(group_rows as usize, group, "{name}: group {index}");
            } else {
                assert!(group_rows as usize <= group, "{name}: last group");
            }
        }
        assert_eq!(total as usize, rows, "{name}: total rows");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// An `MRB1` file on disk matches the table of contracts e.4 byte for byte, and
/// passes the four checks that section says a reader makes.
#[test]
fn an_amb1_file_on_disk_matches_contracts_e4() {
    let dir = scratch("mrb1");
    run(&format!(
        "mrb1 --name weights --dtype f32 --shape 128,64 --local-only --seed 5 --out {}",
        dir.display()
    ));
    let bytes = read(&dir.join("weights.mrb1"));

    let mut expected = Vec::new();
    expected.extend_from_slice(b"MRB1");
    expected.extend_from_slice(&1u16.to_le_bytes());
    expected.push(10); // F32
    expected.push(2); // ndim
    expected.extend_from_slice(&128i64.to_le_bytes());
    expected.extend_from_slice(&64i64.to_le_bytes());
    expected.extend_from_slice(&4096u64.to_le_bytes());
    assert_eq!(&bytes[..expected.len()], expected.as_slice());
    assert!(bytes[expected.len()..4096].iter().all(|b| *b == 0));

    // The reader's checks: magic, version, ndim, data_offset a multiple of 4096
    // and at or above the header end, and the file long enough for the payload.
    let data_offset = u64::from_le_bytes([
        bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
    ]);
    assert_eq!(data_offset % mrb1::PAGE_BYTES, 0);
    assert!(data_offset >= mrb1::header_end(2));
    let payload_len = 128 * 64 * 4;
    assert!(bytes.len() as u64 >= data_offset + payload_len);
    assert_eq!(bytes.len() as u64 % mrb1::TAIL_ALIGN, 0);
    assert!(
        bytes[(data_offset + payload_len) as usize..]
            .iter()
            .all(|b| *b == 0)
    );

    // The payload is the tensor's own bytes, at data_offset, row major.
    let spec = mrb1::TensorSpec::new("weights", DType::F32, vec![128, 64]).expect("spec");
    assert_eq!(
        &bytes[data_offset as usize..(data_offset + payload_len) as usize],
        spec.payload(5).as_slice()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A safetensors file round trips through the `safetensors` crate.
#[test]
fn a_safetensors_file_round_trips_through_the_crate() {
    let dir = scratch("safetensors");
    run(&format!(
        "safetensors --name model --tensor weight:f32:8,4 --tensor bias:f16:4 \
         --tensor mask:bool:4 --local-only --seed 6 --out {}",
        dir.display()
    ));
    let bytes = read(&dir.join("model.safetensors"));
    let parsed = safetensors::SafeTensors::deserialize(&bytes).expect("deserialize");
    let names: Vec<String> = {
        let mut names: Vec<String> = parsed.names().into_iter().map(str::to_string).collect();
        names.sort();
        names
    };
    assert_eq!(names, vec!["bias", "mask", "weight"]);
    let weight = parsed.tensor("weight").expect("weight");
    assert_eq!(weight.dtype(), safetensors::Dtype::F32);
    assert_eq!(weight.shape(), &[8, 4]);
    assert_eq!(weight.data().len(), 8 * 4 * 4);
    assert_eq!(
        parsed.tensor("bias").expect("bias").dtype(),
        safetensors::Dtype::F16
    );
    assert_eq!(
        parsed.tensor("mask").expect("mask").dtype(),
        safetensors::Dtype::BOOL
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Each suite dataset can be written on its own and lands the same bytes as when
/// the whole suite is written, so a benchmark can regenerate one file.
#[test]
fn one_dataset_matches_what_the_whole_suite_writes() {
    let all = scratch("suite-all");
    let one = scratch("suite-one");
    run(&format!(
        "suite --scale small --seed 77 --local-only --out {}",
        all.display()
    ));
    for name in ["numeric-embed", "embed-weights", "token-blocks-mrb1"] {
        run(&format!(
            "dataset {name} --scale small --seed 77 --local-only --out {}",
            one.display()
        ));
        let dataset = suite::dataset(name, Scale::Small).expect("dataset");
        let file = dataset.file_name();
        assert_eq!(read(&all.join(&file)), read(&one.join(&file)), "{file}");
    }
    let _ = std::fs::remove_dir_all(&all);
    let _ = std::fs::remove_dir_all(&one);
}

/// The manifest records the machine, the generator version and a digest per file.
#[test]
fn the_manifest_records_the_run() {
    let dir = scratch("manifest");
    let report = run(&format!(
        "dataset embed-weights --scale small --seed 3 --local-only --out {}",
        dir.display()
    ));
    assert!(report.starts_with("moruna-bench "), "{report}");
    let text = std::fs::read_to_string(dir.join("manifest.json")).expect("manifest");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["seed"], 3);
    assert_eq!(value["generator"], "moruna-bench");
    assert!(value["machine"].as_str().unwrap_or_default().len() > 3);
    let files = value["files"].as_array().expect("files");
    assert_eq!(files.len(), 1);
    let hash = files[0]["blake3"].as_str().expect("hash");
    let bytes = read(&dir.join("embed-weights.safetensors"));
    assert_eq!(hash, blake3_hex(&bytes));
    let _ = std::fs::remove_dir_all(&dir);
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// The ad hoc Parquet command writes exactly the column mix it was given.
#[test]
fn the_column_mix_is_what_was_asked_for() {
    let dir = scratch("mix");
    run(&format!(
        "parquet --name mix --rows 100 --int-cols 3 --float-cols 2 --string-cols 1 \
         --text-cols 2 --row-group-rows 50 --compression snappy --local-only --out {}",
        dir.display()
    ));
    let file = std::fs::File::open(dir.join("mix.parquet")).expect("open");
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).expect("builder");
    let schema = builder.schema().clone();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(
        names,
        vec![
            "i64_0", "i64_1", "i64_2", "f64_0", "f64_1", "str_0", "text_0", "text_1"
        ]
    );
    let spec = ParquetSpec {
        rows: 100,
        int_cols: 3,
        float_cols: 2,
        short_string_cols: 1,
        text_cols: 2,
        ..ParquetSpec::default()
    };
    assert_eq!(spec.column_names(), names);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The S3 compatible half. Skipped, with the reason printed, when the four
/// environment variables of preamble 6.5's MinIO job are not set.
#[test]
fn the_same_datasets_reach_the_s3_compatible_store() {
    let target = match s3::from_env() {
        s3::Discovery::Missing(missing) => {
            println!(
                "skipped: no S3 compatible store reachable ({})",
                s3::skip_note(&missing)
            );
            return;
        }
        s3::Discovery::Configured(target) => target,
    };
    let dir = scratch("s3");
    let dataset = Dataset {
        name: "s3-probe".to_string(),
        kind: DatasetKind::Amb1(mrb1::TensorSpec::new("probe", DType::I64, vec![8]).expect("spec")),
    };
    let written = dataset.write(&dir, 21).expect("write");
    let mut out: Vec<u8> = Vec::new();
    target
        .upload(
            &[(written.relative.clone(), written.path.clone())],
            &mut out,
        )
        .expect("upload");
    let report = String::from_utf8_lossy(&out).into_owned();
    assert!(report.contains(&target.url(&written.relative)), "{report}");
    let fetched = target.download(&written.relative).expect("download");
    assert_eq!(fetched, read(&written.path));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The generator refuses a command line it cannot honour instead of writing
/// half a corpus.
#[test]
fn a_refused_command_line_writes_nothing() {
    let dir = scratch("refused");
    let args: Vec<String> = format!(
        "parquet --rows 10 --int-cols 0 --float-cols 0 --string-cols 0 --text-cols 0 --out {}",
        dir.display()
    )
    .split_whitespace()
    .map(str::to_string)
    .collect();
    let mut out: Vec<u8> = Vec::new();
    assert!(moruna_bench::run(&args, &mut out).is_err());
    assert!(!dir.exists());
    assert!(cli::parse(&args).is_err());
}
