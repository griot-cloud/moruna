//! `TraceView`: a read-only view over everything recorded so far, the in-memory chunks plus
//! whatever went to the overflow file or, after `finish`, the final Arrow IPC file (d.1, e.2).

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    Array, Float32Array, Int64Array, ListArray, RecordBatch, StringArray, UInt8Array, UInt16Array,
    UInt64Array,
};
use arrow::ipc::reader::{FileReader, StreamReader};
use moruna_kernel::{MorunaError, Outcome, StageId, TraceRecord};

use crate::Result;
use crate::writer::Shared;

/// The chunks a reader was handed at the moment it took its view (writer `chunk_set`).
pub(crate) struct ChunkSet {
    /// In-memory chunks, oldest first, with the builder's partial chunk last.
    pub(crate) memory: Vec<Arc<RecordBatch>>,
    /// How many chunks the overflow file holds. Chunks beyond this may be half written.
    pub(crate) overflow_chunks: usize,
    /// Every row recorded so far, in memory and in the overflow file.
    pub(crate) rows: u64,
}

/// A read-only view over the trace (d.1). Cheap to take: it clones `Arc`s, never records.
pub struct TraceView {
    set: ChunkSet,
    shared: Arc<Shared>,
    overflow_failed: bool,
    late_records: u64,
    /// True for the view `finish` returns: the final file, when one was given, is complete
    /// and holds every chunk, so the view reads it instead of memory plus overflow (e.2).
    finished: bool,
}

impl TraceView {
    pub(crate) fn from_parts(
        set: ChunkSet,
        shared: Arc<Shared>,
        overflow_failed: bool,
        late_records: u64,
        finished: bool,
    ) -> TraceView {
        TraceView {
            set,
            shared,
            overflow_failed,
            late_records,
            finished,
        }
    }

    /// Records in the trace.
    pub fn len(&self) -> u64 {
        self.set.rows
    }

    /// True when nothing was recorded (an empty source, h).
    pub fn is_empty(&self) -> bool {
        self.set.rows == 0
    }

    /// Whether a chunk could not be written to the overflow file (h, failures). Surfaced in
    /// the report so a run whose trace outgrew memory on a full disk says so.
    pub fn overflow_failed(&self) -> bool {
        self.overflow_failed
    }

    /// Records that arrived after `finish`, or after the writer thread failed (e.1, h).
    pub fn late_records(&self) -> u64 {
        self.late_records
    }

    /// Every chunk of the trace, oldest first. Reads the overflow file (or the final file)
    /// lazily, one message at a time, so a trace larger than memory is still readable.
    pub fn batches(&self) -> Batches {
        let memory = self.set.memory.clone();
        if self.finished
            && let Some(path) = self.shared.final_path.as_ref()
            && let Ok(file) = File::open(path)
            && let Ok(reader) = FileReader::try_new(BufReader::new(file), None)
        {
            return Batches {
                reader: Reader::Final(Box::new(reader)),
                memory: Vec::new().into_iter(),
            };
        }
        let reader = if self.set.overflow_chunks > 0 {
            match File::open(&self.shared.overflow_path)
                .map_err(|_| ())
                .and_then(|f| StreamReader::try_new(BufReader::new(f), None).map_err(|_| ()))
            {
                Ok(r) => Reader::Overflow {
                    reader: Box::new(r),
                    left: self.set.overflow_chunks,
                },
                Err(()) => Reader::None,
            }
        } else {
            Reader::None
        };
        Batches {
            reader,
            memory: memory.into_iter(),
        }
    }

    /// The last `n` records for `stage`, oldest last, walked over the in-memory chunks the
    /// view holds: the same walk as `TraceTail::tail` (f.3).
    pub fn tail(&self, stage: StageId, n: usize) -> Vec<TraceRecord> {
        if n == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        for batch in self.set.memory.iter().rev() {
            for row in (0..batch.num_rows()).rev() {
                let Some(rec) = record_at(batch, row) else {
                    continue;
                };
                if rec.stage != stage {
                    continue;
                }
                out.push(rec);
                if out.len() == n {
                    out.reverse();
                    return out;
                }
            }
        }
        out.reverse();
        out
    }

    /// Write the whole trace as an Arrow IPC file (random access), schema per contracts e.5.
    pub fn to_ipc_file(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| MorunaError::Io {
                op: "trace_dir",
                target: dir.display().to_string(),
                msg: e.to_string(),
            })?;
        }
        let file = File::create(path).map_err(|e| MorunaError::Io {
            op: "trace_create",
            target: path.display().to_string(),
            msg: e.to_string(),
        })?;
        let mut w =
            arrow::ipc::writer::FileWriter::try_new(file, &self.shared.schema).map_err(|e| {
                MorunaError::Io {
                    op: "trace_ipc",
                    target: path.display().to_string(),
                    msg: e.to_string(),
                }
            })?;
        for batch in self.batches() {
            w.write(&batch).map_err(|e| MorunaError::Io {
                op: "trace_ipc",
                target: path.display().to_string(),
                msg: e.to_string(),
            })?;
        }
        w.finish().map_err(|e| MorunaError::Io {
            op: "trace_ipc",
            target: path.display().to_string(),
            msg: e.to_string(),
        })
    }

    /// Every record of the trace, oldest first. Used by the tests and by any consumer that
    /// wants rows rather than batches; the report walks `batches` instead.
    pub fn records(&self) -> Vec<TraceRecord> {
        let mut out = Vec::with_capacity(self.set.rows as usize);
        for batch in self.batches() {
            for row in 0..batch.num_rows() {
                if let Some(r) = record_at(&batch, row) {
                    out.push(r);
                }
            }
        }
        out
    }
}

enum Reader {
    None,
    Overflow {
        reader: Box<StreamReader<BufReader<File>>>,
        left: usize,
    },
    Final(Box<FileReader<BufReader<File>>>),
}

/// The iterator `TraceView::batches` returns: the spilled chunks first, then what is still
/// in memory.
pub struct Batches {
    reader: Reader,
    memory: std::vec::IntoIter<Arc<RecordBatch>>,
}

impl Iterator for Batches {
    type Item = RecordBatch;

    fn next(&mut self) -> Option<RecordBatch> {
        match &mut self.reader {
            Reader::Overflow { reader, left } if *left > 0 => match reader.next() {
                Some(Ok(b)) => {
                    *left -= 1;
                    return Some(b);
                }
                // A truncated or unreadable overflow file ends the spilled part; what is
                // still in memory is returned regardless.
                _ => self.reader = Reader::None,
            },
            Reader::Final(reader) => {
                return match reader.next() {
                    Some(Ok(b)) => Some(b),
                    _ => None,
                };
            }
            _ => self.reader = Reader::None,
        }
        self.memory.next().map(|b| b.as_ref().clone())
    }
}

macro_rules! col {
    ($batch:expr, $idx:expr, $ty:ty) => {
        $batch.column($idx).as_any().downcast_ref::<$ty>()?
    };
}

fn list_values(a: &ListArray, row: usize) -> Vec<u64> {
    let values = a.value(row);
    match values.as_any().downcast_ref::<UInt64Array>() {
        Some(v) => v.values().to_vec(),
        None => Vec::new(),
    }
}

/// Decode one row of a trace chunk back into a `TraceRecord`. `None` when a column is not
/// the type the schema pins, which `check_schema` makes impossible at start (TR-I6).
pub(crate) fn record_at(batch: &RecordBatch, row: usize) -> Option<TraceRecord> {
    if row >= batch.num_rows() || batch.num_columns() < 31 {
        return None;
    }
    let error_col = col!(batch, 30, StringArray);
    Some(TraceRecord {
        seq: col!(batch, 0, UInt64Array).value(row),
        stage: col!(batch, 1, UInt16Array).value(row),
        worker: col!(batch, 2, UInt16Array).value(row),
        instance: col!(batch, 3, UInt16Array).value(row),
        t_start_ns: col!(batch, 4, UInt64Array).value(row),
        t_end_ns: col!(batch, 5, UInt64Array).value(row),
        rows_in: col!(batch, 6, UInt64Array).value(row),
        bytes_in: col!(batch, 7, UInt64Array).value(row),
        rows_out: col!(batch, 8, UInt64Array).value(row),
        bytes_out: col!(batch, 9, UInt64Array).value(row),
        tier_in: col!(batch, 10, UInt8Array).value(row),
        tier_out: col!(batch, 11, UInt8Array).value(row),
        feat_mean_string_len: col!(batch, 12, Float32Array).value(row),
        feat_null_ratio: col!(batch, 13, Float32Array).value(row),
        feat_column_bytes: list_values(col!(batch, 14, ListArray), row),
        knob_morsel_target: col!(batch, 15, UInt64Array).value(row),
        knob_active_workers: col!(batch, 16, UInt16Array).value(row),
        knob_read_ahead: col!(batch, 17, UInt16Array).value(row),
        mem_anon_before: col!(batch, 18, UInt64Array).value(row),
        mem_anon_peak: col!(batch, 19, UInt64Array).value(row),
        dev_mem_peak: col!(batch, 20, UInt64Array).value(row),
        cpu_time_us: col!(batch, 21, UInt64Array).value(row),
        throttled_delta_us: col!(batch, 22, UInt64Array).value(row),
        q_bytes_before: list_values(col!(batch, 23, ListArray), row),
        q_bytes_after: list_values(col!(batch, 24, ListArray), row),
        staging_bytes_delta: col!(batch, 25, Int64Array).value(row),
        placement_miss_wait_us: col!(batch, 26, UInt64Array).value(row),
        state_bytes: col!(batch, 27, UInt64Array).value(row),
        sizer: col!(batch, 28, UInt8Array).value(row),
        outcome: Outcome::from_code(col!(batch, 29, UInt8Array).value(row))?,
        error: if error_col.is_null(row) {
            None
        } else {
            Some(error_col.value(row).to_string())
        },
    })
}
