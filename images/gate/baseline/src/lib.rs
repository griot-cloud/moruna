//! The PM's control 2 (`architecture/arrow-core-0.7.md` section 9): the gate's Rust functions
//! in a plain Rust thread pool, the "other options" baseline the 0.7 runs are set beside.
//!
//! It works as `bench/vm/runner.py`'s loop does: a unit is a run of consecutive row groups of
//! one file holding at least one batch of rows; units run on a rayon pool; each unit's batches
//! go through the function into one output file. It writes what every gate engine writes
//! ([`Output`]): zstd level 1, row groups of at most 1 Mi rows and 128 MiB, files of at most
//! 1 GiB, Moruna's Parquet sink's settings.
//!
//! It also measures the PM's time split: read and decode, the function, encode and write, summed
//! over the workers, and the waits (the pool's threads times the wall, less the three).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use arrow::array::RecordBatch;
use moruna_gate_functions::Error as FnError;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use rayon::prelude::*;

/// What every gate engine writes (control 3): Moruna's Parquet sink's defaults
/// (`crates/moruna-sinks/src/parquet_sink.rs`: `ZstdLevel::default()`, which is 1, 128 MiB row
/// groups, 1 GiB files) and the parquet writer's row cap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Output {
    pub zstd_level: i32,
    pub row_group_rows: usize,
    pub row_group_bytes: usize,
    pub file_bytes: usize,
}

impl Default for Output {
    fn default() -> Output {
        Output {
            zstd_level: 1,
            row_group_rows: 1 << 20,
            row_group_bytes: 128 << 20,
            file_bytes: 1 << 30,
        }
    }
}

impl Output {
    pub fn json(&self) -> String {
        format!(
            "{{\"zstd_level\": {}, \"row_group_rows\": {}, \"row_group_bytes\": {}, \"file_bytes\": {}}}",
            self.zstd_level, self.row_group_rows, self.row_group_bytes, self.file_bytes
        )
    }

    fn props(&self) -> Result<WriterProperties, String> {
        let level = ZstdLevel::try_new(self.zstd_level).map_err(|e| e.to_string())?;
        Ok(WriterProperties::builder()
            .set_compression(Compression::ZSTD(level))
            .set_max_row_group_row_count(Some(self.row_group_rows))
            .build())
    }
}

/// A run of consecutive row groups of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    pub file: PathBuf,
    pub row_groups: Vec<usize>,
    pub rows: u64,
}

/// The units of `files`, as the loop cuts them: row groups are gathered until they hold at
/// least `batch` rows (one row group a unit when `batch` is 0).
pub fn plan(files: &[PathBuf], batch: usize) -> Result<Vec<Unit>, String> {
    let mut units = Vec::new();
    for f in files {
        let file = File::open(f).map_err(|e| format!("{}: {e}", f.display()))?;
        let meta = ParquetRecordBatchReaderBuilder::try_new(file)
            .map_err(|e| format!("{}: {e}", f.display()))?
            .metadata()
            .clone();
        let mut cur = Vec::new();
        let mut rows = 0u64;
        for g in 0..meta.num_row_groups() {
            cur.push(g);
            rows += meta.row_group(g).num_rows().max(0) as u64;
            if rows >= batch.max(1) as u64 {
                units.push(Unit {
                    file: f.clone(),
                    row_groups: std::mem::take(&mut cur),
                    rows,
                });
                rows = 0;
            }
        }
        if !cur.is_empty() {
            units.push(Unit {
                file: f.clone(),
                row_groups: cur,
                rows,
            });
        }
    }
    Ok(units)
}

/// The time split, in seconds summed over the workers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Split {
    pub read_decode_s: f64,
    pub function_s: f64,
    pub encode_write_s: f64,
}

impl Split {
    fn add(&mut self, o: &Split) {
        self.read_decode_s += o.read_decode_s;
        self.function_s += o.function_s;
        self.encode_write_s += o.encode_write_s;
    }
}

/// What a run did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    pub units: usize,
    pub rows_in: u64,
    pub rows_out: u64,
    pub files_out: usize,
    pub split: Split,
}

/// A batch function.
pub type Function = fn(&RecordBatch) -> Result<RecordBatch, FnError>;

/// An output file being written, rolled at the output's file size.
struct Sink<'o> {
    dir: &'o Path,
    unit: usize,
    output: Output,
    props: WriterProperties,
    writer: Option<ArrowWriter<File>>,
    files: usize,
}

impl Sink<'_> {
    fn write(&mut self, b: &RecordBatch) -> Result<(), String> {
        if self.writer.is_none() {
            let name = match self.files {
                0 => format!("{:05}.parquet", self.unit),
                k => format!("{:05}-{k}.parquet", self.unit),
            };
            let f = File::create(self.dir.join(name)).map_err(|e| e.to_string())?;
            let w = ArrowWriter::try_new(f, b.schema(), Some(self.props.clone()))
                .map_err(|e| e.to_string())?;
            self.writer = Some(w);
            self.files += 1;
        }
        let Some(w) = self.writer.as_mut() else {
            return Err("no writer".into());
        };
        w.write(b).map_err(|e| e.to_string())?;
        if w.in_progress_size() >= self.output.row_group_bytes {
            w.flush().map_err(|e| e.to_string())?;
        }
        if w.bytes_written() + w.in_progress_size() >= self.output.file_bytes {
            self.close()?;
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), String> {
        if let Some(w) = self.writer.take() {
            w.close().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Runs `f` over one unit into `out`: its rows in and out, files written and time split.
pub fn run_unit(
    unit: &Unit,
    index: usize,
    f: Function,
    out: &Path,
    batch: usize,
    output: Output,
) -> Result<Outcome, String> {
    let mut split = Split::default();
    let t = Instant::now();
    let file = File::open(&unit.file).map_err(|e| e.to_string())?;
    let size = if batch > 0 {
        batch
    } else {
        unit.rows.max(1) as usize
    };
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| e.to_string())?
        .with_row_groups(unit.row_groups.clone())
        .with_batch_size(size)
        .build()
        .map_err(|e| e.to_string())?;
    split.read_decode_s += t.elapsed().as_secs_f64();
    let mut sink = Sink {
        dir: out,
        unit: index,
        output,
        props: output.props()?,
        writer: None,
        files: 0,
    };
    let mut done = Outcome {
        units: 1,
        ..Outcome::default()
    };
    let mut reader = reader.into_iter();
    loop {
        let t = Instant::now();
        let Some(b) = reader.next() else { break };
        let b = b.map_err(|e| format!("{}: {e}", unit.file.display()))?;
        split.read_decode_s += t.elapsed().as_secs_f64();
        let t = Instant::now();
        let o = f(&b).map_err(|e| format!("{}: {e}", unit.file.display()))?;
        split.function_s += t.elapsed().as_secs_f64();
        let t = Instant::now();
        sink.write(&o)?;
        split.encode_write_s += t.elapsed().as_secs_f64();
        done.rows_in += b.num_rows() as u64;
        done.rows_out += o.num_rows() as u64;
    }
    let t = Instant::now();
    sink.close()?;
    split.encode_write_s += t.elapsed().as_secs_f64();
    done.files_out = sink.files;
    done.split = split;
    Ok(done)
}

/// Runs `f` over `files` on `threads` workers (the machine's when 0) into `out`.
pub fn run(
    files: &[PathBuf],
    f: Function,
    out: &Path,
    threads: usize,
    batch: usize,
    output: Output,
) -> Result<Outcome, String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let units = plan(files, batch)?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| e.to_string())?;
    let total = Mutex::new(Outcome::default());
    pool.install(|| {
        units.par_iter().enumerate().try_for_each(|(i, u)| {
            let o = run_unit(u, i, f, out, batch, output)?;
            let mut t = total.lock().unwrap_or_else(|e| e.into_inner());
            t.units += o.units;
            t.rows_in += o.rows_in;
            t.rows_out += o.rows_out;
            t.files_out += o.files_out;
            t.split.add(&o.split);
            Ok::<(), String>(())
        })
    })?;
    Ok(total.into_inner().unwrap_or_else(|e| e.into_inner()))
}

/// The files a path names: itself, or every `.parquet` file under a directory, in name order.
pub fn parquet_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut out = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))? {
            let p = e.map_err(|e| e.to_string())?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "parquet") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

pub mod canonical;
