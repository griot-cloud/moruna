//! Stream D's fixture for `moruna run --vm` with data disks: an input Parquet file of KD2's rows,
//! and the check of what a run left in its output directory.
//!
//!   data-disks parquet OUT_FILE          writes ROWS rows, `x` 0..ROWS and `y = (x * 7919) % 10007`
//!   data-disks check DIR same|kd2        reads every Parquet or Arrow IPC file under DIR and
//!                                        checks each `x` once and `y` as the input's (`same`) or
//!                                        as KD2's transform makes it (`kd2`, `3 * x + y`); prints
//!                                        the rows and a SHA-256 over them in `x` order
//!
//! The rows are KD2's (`moruna-testkit`'s `kd2-fixture`, `examples/kd3/common.rs`), so a run's
//! output is checked against the same formula the node fixtures check.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, Int64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use sha2::{Digest, Sha256};

const ROWS: i64 = 100_000;
const BATCH: i64 = 10_000;

fn y_of(x: i64) -> i64 {
    (x * 7919) % 10007
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn write_parquet(path: &Path) -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("y", DataType::Int64, false),
    ]));
    let file = std::fs::File::create_new(path)?;
    let props = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(BATCH as usize))
        .build();
    let mut writer = parquet::arrow::ArrowWriter::try_new(file, schema.clone(), Some(props))?;
    for start in (0..ROWS).step_by(BATCH as usize) {
        let x: Vec<i64> = (start..(start + BATCH).min(ROWS)).collect();
        let y: Vec<i64> = x.iter().map(|v| y_of(*v)).collect();
        let columns: Vec<ArrayRef> =
            vec![Arc::new(Int64Array::from(x)), Arc::new(Int64Array::from(y))];
        writer.write(&RecordBatch::try_new(schema.clone(), columns)?)?;
    }
    writer.close()?;
    Ok(())
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            files(&path, out)?;
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("parquet" | "arrow" | "arrows" | "ipc")
        ) {
            out.push(path);
        }
    }
    Ok(())
}

fn batches(path: &Path) -> Result<Vec<RecordBatch>> {
    let file = std::fs::File::open(path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("parquet") {
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?
            .build()?;
        return Ok(reader.collect::<std::result::Result<_, _>>()?);
    }
    match arrow::ipc::reader::FileReader::try_new(std::fs::File::open(path)?, None) {
        Ok(reader) => Ok(reader.collect::<std::result::Result<_, _>>()?),
        Err(_) => Ok(arrow::ipc::reader::StreamReader::try_new(file, None)?
            .collect::<std::result::Result<_, _>>()?),
    }
}

fn check(dir: &Path, kd2: bool) -> Result<String> {
    let mut found = Vec::new();
    files(dir, &mut found)?;
    if found.is_empty() {
        return Err(format!("no Parquet or Arrow file under {}", dir.display()).into());
    }
    let mut rows = BTreeMap::new();
    for path in &found {
        for batch in batches(path)? {
            let column = |name: &str| -> Result<Int64Array> {
                let array = batch
                    .column_by_name(name)
                    .ok_or_else(|| format!("{}: no column {name}", path.display()))?;
                Ok(array
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .ok_or_else(|| format!("{}: {name} is not Int64", path.display()))?
                    .clone())
            };
            let (x, y) = (column("x")?, column("y")?);
            for i in 0..batch.num_rows() {
                if x.is_null(i) || y.is_null(i) {
                    return Err(format!("{}: a null at row {i}", path.display()).into());
                }
                let (a, b) = (x.value(i), y.value(i));
                let want = if kd2 { 3 * a + y_of(a) } else { y_of(a) };
                if !(0..ROWS).contains(&a) || b != want || rows.insert(a, b).is_some() {
                    return Err(format!("row x={a} y={b} is wrong or repeated").into());
                }
            }
        }
    }
    if rows.len() as i64 != ROWS {
        return Err(format!("{} rows of {ROWS}", rows.len()).into());
    }
    let mut hash = Sha256::new();
    for (x, y) in &rows {
        hash.update(x.to_le_bytes());
        hash.update(y.to_le_bytes());
    }
    let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "rows {} in {} files sha256 {hex}",
        rows.len(),
        found.len()
    ))
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["parquet", out] => write_parquet(Path::new(out)).map(|()| format!("{ROWS} rows in {out}")),
        ["check", dir, "same"] => check(Path::new(dir), false),
        ["check", dir, "kd2"] => check(Path::new(dir), true),
        _ => Err("usage: data-disks parquet OUT_FILE | check DIR same|kd2".into()),
    };
    match result {
        Ok(line) => {
            println!("{line}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("data-disks: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
