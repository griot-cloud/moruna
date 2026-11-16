//! The baseline and gate-canon on the oracle's fixtures: each NYC job's input written as Parquet
//! in several row groups, run through the pool, and the output's canonical hash equal to the
//! Python function's (canon.py's, in the manifest), whatever the split.

use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::RecordBatch;
use arrow::ipc::reader::FileReader;
use moruna_gate_baseline::canonical::{digest, read_dump, write_dump};
use moruna_gate_baseline::{Output, parquet_files, plan, run};
use moruna_gate_functions::batch::by_name;
use moruna_gate_functions::canon::compare;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use serde_json::Value;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures")
}

fn manifest() -> Value {
    serde_json::from_reader(File::open(fixtures().join("manifest.json")).unwrap()).unwrap()
}

fn read_ipc(path: &Path) -> RecordBatch {
    let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    arrow::compute::concat_batches(&schema, &batches).unwrap()
}

/// `batch` as a Parquet file of row groups of `rows` rows.
fn parquet(batch: &RecordBatch, path: &Path, rows: usize) {
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(rows))
        .build();
    let mut w =
        ArrowWriter::try_new(File::create(path).unwrap(), batch.schema(), Some(props)).unwrap();
    w.write(batch).unwrap();
    w.close().unwrap();
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gate-baseline-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn every_nyc_job_matches_python_through_the_pool() {
    let m = manifest();
    let dir = scratch("nyc");
    let input = read_ipc(&fixtures().join("inputs/nyc/main.arrow"));
    let file = dir.join("in.parquet");
    parquet(&input, &file, 1000);
    let units = plan(std::slice::from_ref(&file), 0).unwrap();
    assert_eq!(units.len(), 4);
    assert_eq!(plan(std::slice::from_ref(&file), 2500).unwrap().len(), 2);
    for job in ["enrich", "audit", "occupancy", "score"] {
        let out = dir.join(job);
        let o = run(
            std::slice::from_ref(&file),
            by_name(job).unwrap(),
            &out,
            2,
            0,
            Output::default(),
        )
        .unwrap();
        assert_eq!(o.rows_in, 4000);
        assert_eq!(o.units, 4);
        assert_eq!(o.files_out, 4);
        let (d, files, groups) = digest(job, std::slice::from_ref(&out), true).unwrap();
        assert_eq!((files, groups), (4, 4));
        let want = &m["jobs"][job]["main"];
        assert_eq!(d.rows, want["rows"].as_u64().unwrap(), "{job}");
        assert_eq!(d.hash_exact, want["hash_exact"].as_str().unwrap(), "{job}");
        assert_eq!(d.hash_all, want["hash_all"].as_str().unwrap(), "{job}");
        // The dump reads back, and a comparison with itself passes.
        let dump = dir.join(format!("{job}.dump"));
        write_dump(&dump, &d).unwrap();
        let back = read_dump(&dump).unwrap();
        assert_eq!((back.rows, &back.hash_all), (d.rows, &d.hash_all));
        let bits = |rows: &[(u128, Vec<Option<f64>>)]| -> Vec<(u128, Vec<Option<u64>>)> {
            rows.iter()
                .map(|(h, v)| (*h, v.iter().map(|x| x.map(f64::to_bits)).collect()))
                .collect()
        };
        assert_eq!(bits(&back.tolerant_rows), bits(&d.tolerant_rows));
        assert!(compare(job, &back, &d).unwrap().ok);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_refused_batch_fails_the_run_and_files_roll() {
    let dir = scratch("refused");
    let input = read_ipc(&fixtures().join("inputs/nyc/null_times.arrow"));
    let file = dir.join("in.parquet");
    parquet(&input, &file, 200);
    let err = run(
        std::slice::from_ref(&file),
        by_name("audit").unwrap(),
        &dir.join("a"),
        1,
        0,
        Output::default(),
    );
    assert!(err.is_err());
    // A file size smaller than a row group: one file a write.
    let tiny = Output {
        file_bytes: 1,
        ..Output::default()
    };
    let o = run(
        std::slice::from_ref(&file),
        by_name("score").unwrap(),
        &dir.join("s"),
        1,
        100,
        tiny,
    )
    .unwrap();
    assert_eq!(o.files_out, 6);
    assert_eq!(parquet_files(&dir.join("s")).unwrap().len(), 6);
    assert!(Output::default().json().contains("\"zstd_level\": 1"));
    assert!(plan(&[dir.join("missing.parquet")], 0).is_err());
    assert!(read_dump(&file).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
