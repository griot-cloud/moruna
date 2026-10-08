//! Refused rows set aside (MH 4.1 `refused_rows`): a row a kernel refuses ([`Kernel::judge`])
//! is written, with where it was and why, to the run's set-aside output, and the rows the kernel
//! kept go on to the sink.
//!
//! The output is a directory with one Arrow IPC file per stage that refused a row,
//! `stage-<n>.arrow`. Each row of a file is one refused row: the morsel it was in
//! (`refused_seq`), its index in the stage's input (`refused_row`), its row in the source when
//! the stage reads the source's rows (`refused_source_row`, stage 1 only: the rows of every
//! earlier split plus its row in its own), the column it was refused for (`refused_column`) and
//! why (`refused_cause`), followed by the refused row's own columns as the stage was given them,
//! when that input was a table in host memory (null otherwise). A worker only builds what is set
//! aside; the sink drive writes it, as it writes everything else (SC-I1), and finishes every
//! file before the sink commits.
//!
//! [`Kernel::judge`]: moruna_kernel::Kernel::judge

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use moruna_kernel::arrow::array::{
    Array, ArrayRef, RecordBatch, RecordBatchOptions, StringArray, UInt64Array, new_null_array,
};
use moruna_kernel::arrow::compute::take_record_batch;
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use moruna_kernel::arrow::ipc::writer::FileWriter;
use moruna_kernel::{MorunaError, Origin, Result, RowRefusal, Seq, Split, SplitId, StageId};

/// The most refused rows a summary names; past it the summary counts them, and the files
/// still hold every one.
pub const MAX_LISTED: usize = 100_000;

/// The columns a set-aside file puts before the refused row's own.
pub const COLUMNS: [&str; 5] = [
    "refused_seq",
    "refused_row",
    "refused_source_row",
    "refused_column",
    "refused_cause",
];

/// One refused row, as a run's summary names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedRow {
    /// The stage whose kernel refused it.
    pub stage: StageId,
    /// The morsel it was in.
    pub seq: Seq,
    /// Its index in the stage's input morsel.
    pub row: u64,
    /// Its row in the source, counted from 0 over every split in plan order: known at stage 1,
    /// whose input is the source's rows.
    pub source_row: Option<u64>,
    /// The column it was refused for, when one.
    pub column: Option<String>,
    /// Why.
    pub cause: String,
}

/// What a run set aside.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetAsideSummary {
    /// The set-aside output.
    pub dir: PathBuf,
    /// Every row set aside.
    pub rows: u64,
    /// The files written, one per stage that refused a row.
    pub files: Vec<PathBuf>,
    /// The rows set aside, the first [`MAX_LISTED`] of them in the order they were set aside.
    pub refused: Vec<RefusedRow>,
}

/// One stage's file.
struct StageFile {
    path: PathBuf,
    schema: SchemaRef,
    /// The columns of the refused rows' own, after [`COLUMNS`].
    values: usize,
    /// Opened by the sink drive at its first write.
    writer: Option<FileWriter<File>>,
}

struct Inner {
    files: BTreeMap<StageId, StageFile>,
    /// Built by the workers, not yet written.
    pending: Vec<(StageId, RecordBatch)>,
    summary: SetAsideSummary,
    finished: bool,
}

/// The set-aside output of one run.
pub(crate) struct SetAside {
    dir: PathBuf,
    /// The rows of every split before each split, in plan order.
    before: HashMap<SplitId, u64>,
    inner: Mutex<Inner>,
}

fn io(op: &'static str, target: &Path, e: impl std::fmt::Display) -> MorunaError {
    MorunaError::Io {
        op,
        target: target.display().to_string(),
        msg: e.to_string(),
    }
}

impl SetAside {
    /// The output at `dir`, over `plan`. The directory is made now, so a run that cannot set
    /// rows aside is refused before it reads one.
    pub(crate) fn new(dir: PathBuf, plan: &[Split]) -> Result<Arc<SetAside>> {
        std::fs::create_dir_all(&dir).map_err(|e| io("set_aside.create", &dir, e))?;
        let mut before = HashMap::with_capacity(plan.len());
        let mut rows = 0u64;
        for split in plan {
            before.insert(split.id, rows);
            rows = rows.saturating_add(split.rows);
        }
        Ok(Arc::new(SetAside {
            inner: Mutex::new(Inner {
                files: BTreeMap::new(),
                pending: Vec::new(),
                summary: SetAsideSummary {
                    dir: dir.clone(),
                    ..SetAsideSummary::default()
                },
                finished: false,
            }),
            dir,
            before,
        }))
    }

    /// Set aside the rows `refused` names of the `rows`-row input of `stage` in morsel `seq`;
    /// `input` is that input when it was a table in host memory. A worker's: it builds what is
    /// set aside and writes nothing.
    pub(crate) fn put(
        &self,
        stage: StageId,
        seq: Seq,
        origin: &Origin,
        rows: u64,
        input: Option<&RecordBatch>,
        refused: &[RowRefusal],
    ) -> Result<()> {
        let mut seen = BTreeSet::new();
        for r in refused {
            if r.row >= rows || !seen.insert(r.row) {
                return Err(MorunaError::Kernel {
                    stage,
                    seq,
                    msg: format!(
                        "the kernel refused row {} of an input of {rows} rows{}; a kernel \
                         refuses rows of its input, each at most once",
                        r.row,
                        if r.row < rows { " twice" } else { "" }
                    ),
                    features: None,
                });
            }
        }
        let source_row = |row: u64| {
            (stage == 1)
                .then(|| self.before.get(&origin.split))
                .flatten()
                .map(|before| before + origin.row_start + row)
        };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.finished {
            return Err(MorunaError::Staging(format!(
                "stage {stage} refused rows of morsel {seq} after the set-aside output was \
                 finished"
            )));
        }
        if !inner.files.contains_key(&stage) {
            let file = self.file(stage, input.map(RecordBatch::schema))?;
            inner.summary.files.push(file.path.clone());
            inner.files.insert(stage, file);
        }
        let file = inner.files.get(&stage).expect("made above");
        let indices = UInt64Array::from(refused.iter().map(|r| r.row).collect::<Vec<_>>());
        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(vec![seq; refused.len()])),
            Arc::new(indices.clone()),
            Arc::new(UInt64Array::from(
                refused
                    .iter()
                    .map(|r| source_row(r.row))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                refused
                    .iter()
                    .map(|r| r.column.as_deref())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                refused.iter().map(|r| r.cause.as_str()).collect::<Vec<_>>(),
            )),
        ];
        let taken = match input {
            Some(batch) if batch.num_columns() == file.values => Some(
                take_record_batch(batch, &indices)
                    .map_err(|e| io("set_aside.take", &file.path, e))?,
            ),
            _ => None,
        };
        for (i, field) in file.schema.fields().iter().skip(COLUMNS.len()).enumerate() {
            columns.push(match &taken {
                Some(t) if t.column(i).data_type() == field.data_type() => t.column(i).clone(),
                _ => new_null_array(field.data_type(), refused.len()),
            });
        }
        let batch = RecordBatch::try_new_with_options(
            file.schema.clone(),
            columns,
            &RecordBatchOptions::new().with_row_count(Some(refused.len())),
        )
        .map_err(|e| io("set_aside.build", &file.path, e))?;
        inner.pending.push((stage, batch));
        let summary = &mut inner.summary;
        summary.rows += refused.len() as u64;
        for r in refused {
            if summary.refused.len() >= MAX_LISTED {
                break;
            }
            summary.refused.push(RefusedRow {
                stage,
                seq,
                row: r.row,
                source_row: source_row(r.row),
                column: r.column.clone(),
                cause: r.cause.clone(),
            });
        }
        Ok(())
    }

    /// Stage `stage`'s file: the refusal's columns, then `values`' own, each nullable.
    fn file(&self, stage: StageId, values: Option<SchemaRef>) -> Result<StageFile> {
        let mut fields = vec![
            Field::new(COLUMNS[0], DataType::UInt64, false),
            Field::new(COLUMNS[1], DataType::UInt64, false),
            Field::new(COLUMNS[2], DataType::UInt64, true),
            Field::new(COLUMNS[3], DataType::Utf8, true),
            Field::new(COLUMNS[4], DataType::Utf8, false),
        ];
        let mut count = 0;
        if let Some(values) = values {
            for field in values.fields() {
                if COLUMNS.contains(&field.name().as_str()) {
                    return Err(MorunaError::Plan(format!(
                        "stage {stage}'s input has a column `{}`, which the set-aside output \
                         names its own",
                        field.name()
                    )));
                }
                fields.push(field.as_ref().clone().with_nullable(true));
                count += 1;
            }
        }
        Ok(StageFile {
            path: self.dir.join(format!("stage-{stage}.arrow")),
            schema: Arc::new(Schema::new(fields)),
            values: count,
            writer: None,
        })
    }

    /// Write what the workers set aside since the last call: the sink drive's, between its
    /// writes.
    pub(crate) fn flush(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Inner { files, pending, .. } = &mut *inner;
        for (stage, batch) in pending.drain(..) {
            let file = files
                .get_mut(&stage)
                .expect("a pending batch's file is made");
            if file.writer.is_none() {
                let created = File::create_new(&file.path)
                    .map_err(|e| io("set_aside.create", &file.path, e))?;
                file.writer = Some(
                    FileWriter::try_new(created, &file.schema)
                        .map_err(|e| io("set_aside.create", &file.path, e))?,
                );
            }
            file.writer
                .as_mut()
                .expect("opened above")
                .write(&batch)
                .map_err(|e| io("set_aside.write", &file.path, e))?;
        }
        Ok(())
    }

    /// Write what is left and finish every file: the output is complete, and nothing more is
    /// set aside. The sink drive's, before the sink commits.
    pub(crate) fn finish(&self) -> Result<()> {
        self.flush()?;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.finished = true;
        for file in inner.files.values_mut() {
            if let Some(writer) = file.writer.as_mut() {
                writer
                    .finish()
                    .map_err(|e| io("set_aside.finish", &file.path, e))?;
            }
        }
        Ok(())
    }

    /// What was set aside so far.
    pub(crate) fn summary(&self) -> SetAsideSummary {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .summary
            .clone()
    }
}
