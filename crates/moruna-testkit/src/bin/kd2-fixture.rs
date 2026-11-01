//! KD2's boot fixture (SDD 30 k), written by `kernel/tools/build.sh` into the Linux image's top
//! layer: `input.arrow`, an Arrow IPC file of `ROWS` rows in batches of `BATCH`, columns
//! `x: Int64` (the row's number) and `y: Int64` (`(x * 7919) % 10007`), no nulls. The embedder
//! (`kernel/tools/embedder.py`) checks the rows the engine's tasks produce from it against the
//! same formula, row for row.
//!
//!   kd2-fixture OUT_DIR

use std::process::ExitCode;
use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;

/// The rows of the fixture.
const ROWS: i64 = 100_000;
/// The rows of each record batch, and so of each split the `arrow_ipc` source reads.
const BATCH: i64 = 10_000;

fn y_of(x: i64) -> i64 {
    (x * 7919) % 10007
}

fn write(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let schema = Arc::new(Schema::new(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("y", DataType::Int64, false),
    ]));
    let path = dir.join("input.arrow");
    let file = std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut writer =
        FileWriter::try_new(file, &schema).map_err(|e| format!("the IPC writer: {e}"))?;
    for start in (0..ROWS).step_by(BATCH as usize) {
        let x: Vec<i64> = (start..(start + BATCH).min(ROWS)).collect();
        let y: Vec<i64> = x.iter().map(|v| y_of(*v)).collect();
        let columns: Vec<ArrayRef> =
            vec![Arc::new(Int64Array::from(x)), Arc::new(Int64Array::from(y))];
        let batch = RecordBatch::try_new(Arc::clone(&schema), columns)
            .map_err(|e| format!("a batch: {e}"))?;
        writer.write(&batch).map_err(|e| format!("writing: {e}"))?;
    }
    writer.finish().map_err(|e| format!("finishing: {e}"))?;
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir] = args.as_slice() else {
        eprintln!("usage: kd2-fixture OUT_DIR");
        return ExitCode::from(2);
    };
    match write(std::path::Path::new(dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("kd2-fixture: {why}");
            ExitCode::FAILURE
        }
    }
}
