//! `PySource`: a user's `moruna.Source` subclass, planned and read through the interpreter (e.6,
//! f.7).
//!
//! The user's object plans splits with their row counts (`plan()`), may declare its schema
//! (`schema()`), and reads any row range of a split (`read(split_id, start, end)`). Because a
//! read takes a range, this source is sub-splittable and, unless the object says otherwise
//! through `repeatable = False`, repeatable: it keeps SO-I6 and SO-I8, so a run over it has
//! look-ahead, Q0 eviction and resume, which `PyIteratorSource` (f.5) cannot offer.
//!
//! The route is `PyIteratorSource`'s: every call into the object is made under interpreter
//! attachment on the thread that calls `plan` or `read`, the returned `pyarrow.RecordBatch` is
//! converted with `pyo3-arrow` (07 section l allows this crate no `unsafe`), and the one copy
//! into the arena happens after detaching (AD-I4, SO-I5).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use pyo3::prelude::*;
use pyo3::types::PyAnyMethods;

use moruna_kernel::{
    Allocator, BoxFuture, Payload, Reactor, Result, RowRange, Source, SourceSchema, Split, SplitId,
    Tier,
};

use crate::stats::{Counters, SourceStats};
use crate::util::{plan_err, source_err};

/// Rows the plan reads from the first non-empty split when a split gives no byte estimate, to
/// learn bytes per row (f.7). Small enough to cost nothing, large enough that a string column's
/// average is not one row's.
pub const SAMPLE_ROWS: u64 = 1024;

/// One split as the user's `plan()` described it: `moruna.Split(id, rows, bytes=None)`.
struct Planned {
    id: SplitId,
    rows: u64,
    bytes: Option<u64>,
}

/// A source over a user's `moruna.Source` subclass (d.1).
pub struct PySource {
    object: Py<PyAny>,
    /// The name of the user's class, for every message.
    name: String,
    schema: SchemaRef,
    plan: Vec<Split>,
    /// Split id to its position in `plan` (SO-I1).
    index: HashMap<SplitId, usize>,
    repeatable: bool,
    /// Taken before attaching and held across one call into the object, so the user's code is
    /// never entered from two threads at once, whatever the interpreter build (g).
    calls: Mutex<()>,
    #[allow(dead_code)]
    reactor: Arc<dyn Reactor>,
    counters: Counters,
}

impl PySource {
    /// Plan the user's object: call `plan()`, and `schema()` when it defines one; read a sample
    /// of the first non-empty split when the schema or any split's byte estimate is missing
    /// (f.7). The reactor is taken like every other source's; this source issues no reactor
    /// operation, its bytes come from the interpreter.
    pub fn new(object: Py<PyAny>, reactor: Arc<dyn Reactor>) -> Result<PySource> {
        let (name, repeatable, planned, declared) = Python::attach(|py| {
            let bound = object.bind(py);
            let name = class_name(bound);
            let repeatable = match bound.getattr(pyo3::intern!(py, "repeatable")) {
                Ok(value) => value.extract::<bool>().map_err(|_| {
                    plan_err(format!(
                        "{name}.repeatable must be True or False, not {}",
                        class_name(&value)
                    ))
                })?,
                Err(_) => true,
            };
            let planned = plan_of(py, bound, &name)?;
            let declared = schema_of(py, bound, &name)?;
            Ok::<_, moruna_kernel::MorunaError>((name, repeatable, planned, declared))
        })?;

        let mut index = HashMap::with_capacity(planned.len());
        for (at, split) in planned.iter().enumerate() {
            if index.insert(split.id, at).is_some() {
                return Err(plan_err(format!(
                    "{name}.plan() names split {} twice; split ids are unique within a run",
                    split.id
                )));
            }
        }

        let mut source = PySource {
            object,
            name,
            schema: Arc::new(arrow::datatypes::Schema::empty()),
            plan: Vec::new(),
            index,
            repeatable,
            calls: Mutex::new(()),
            reactor,
            counters: Counters::default(),
        };

        let needs_sample = declared.is_none() || planned.iter().any(|s| s.bytes.is_none());
        let sample = if needs_sample {
            source.sample(&planned)?
        } else {
            None
        };
        source.schema = match (declared, &sample) {
            (Some(schema), _) => schema,
            (None, Some((_, _, batch))) => batch.schema(),
            (None, None) => {
                return Err(plan_err(format!(
                    "{}.plan() returned no splits and the class defines no schema(); a source \
                     with nothing to read must declare its schema",
                    source.name
                )));
            }
        };
        if let Some((id, end, batch)) = &sample {
            source.conform(batch, *id, 0, *end)?;
        }
        source.plan = splits_of(&planned, &source.schema, sample.as_ref().map(|(_, _, b)| b));
        Counters::add(&source.counters.splits, source.plan.len() as u64);
        Counters::add(
            &source.counters.bytes_planned,
            source.plan.iter().map(|s| s.uncompressed_bytes).sum(),
        );
        Ok(source)
    }

    /// What this source has done so far (j).
    pub fn stats(&self) -> SourceStats {
        self.counters.snapshot()
    }

    /// The sample of f.7: up to `SAMPLE_ROWS` rows of the first non-empty split, or the zero-row
    /// range of the first split when every split is empty (which still yields a schema). `None`
    /// when there are no splits at all.
    fn sample(&self, planned: &[Planned]) -> Result<Option<(SplitId, u64, RecordBatch)>> {
        let (id, end) = match planned.iter().find(|s| s.rows > 0) {
            Some(split) => (split.id, split.rows.min(SAMPLE_ROWS)),
            None => match planned.first() {
                Some(split) => (split.id, 0),
                None => return Ok(None),
            },
        };
        let batch = self.call_read(id, 0, end)?;
        self.check_rows(&batch, id, 0, end)?;
        Ok(Some((id, end, batch)))
    }

    /// Call the user's `read(split_id, start, end)` under attachment and take the batch out of
    /// Python. An exception, or a value that is not a `pyarrow.RecordBatch`, is a `Source` error
    /// naming the split and carrying the Python message (h).
    fn call_read(&self, id: SplitId, start: u64, end: u64) -> Result<RecordBatch> {
        let _serial = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        Python::attach(|py| {
            let returned = self
                .object
                .bind(py)
                .call_method1(pyo3::intern!(py, "read"), (id, start, end))
                .map_err(|e| {
                    source_err(id, format!("{}.read({id}, {start}, {end}): {e}", self.name))
                })?;
            let batch: pyo3_arrow::PyRecordBatch = returned.extract().map_err(|_| {
                source_err(
                    id,
                    format!(
                        "{}.read({id}, {start}, {end}) returned {}, not a pyarrow.RecordBatch",
                        self.name,
                        class_name(&returned)
                    ),
                )
            })?;
            Ok(batch.into_inner())
        })
    }

    /// SO-I4 for a user's read: exactly `end - start` rows.
    fn check_rows(&self, batch: &RecordBatch, id: SplitId, start: u64, end: u64) -> Result<()> {
        let got = batch.num_rows() as u64;
        if got != end - start {
            return Err(source_err(
                id,
                format!(
                    "{}.read({id}, {start}, {end}) returned {got} rows; it must return exactly \
                     the {} rows of [{start}, {end})",
                    self.name,
                    end - start
                ),
            ));
        }
        Ok(())
    }

    /// Every batch has the source's schema: the same column names and types in the same order
    /// (h). Returns the batch under the source's own schema, so field metadata the user's batch
    /// did not carry cannot make two morsels of one run differ.
    fn conform(
        &self,
        batch: &RecordBatch,
        id: SplitId,
        start: u64,
        end: u64,
    ) -> Result<RecordBatch> {
        let want = self.schema.fields();
        let got = batch.schema();
        let same = want.len() == got.fields().len()
            && want
                .iter()
                .zip(got.fields().iter())
                .all(|(w, g)| w.name() == g.name() && w.data_type() == g.data_type());
        let mismatch = || {
            source_err(
                id,
                format!(
                    "{}.read({id}, {start}, {end}) returned a batch whose schema differs from \
                     the source's: expected [{}], got [{}]",
                    self.name,
                    describe(want.iter().map(|f| f.as_ref())),
                    describe(got.fields().iter().map(|f| f.as_ref())),
                ),
            )
        };
        if !same {
            return Err(mismatch());
        }
        RecordBatch::try_new(Arc::clone(&self.schema), batch.columns().to_vec()).map_err(|e| {
            source_err(
                id,
                format!(
                    "{}.read({id}, {start}, {end}) returned a batch the source's schema refuses: \
                     {e}",
                    self.name
                ),
            )
        })
    }

    /// The read of f.7: range checks, the user's read, the row and schema checks, then the
    /// decode copy into the arena after detaching.
    fn read_now(
        &self,
        split: &Split,
        rows: Option<RowRange>,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> Result<Payload> {
        Counters::add(&self.counters.reads, 1);
        let Some(&at) = self.index.get(&split.id) else {
            return Err(source_err(
                split.id,
                format!("split {} is not in {}'s plan", split.id, self.name),
            ));
        };
        let planned = &self.plan[at];
        let range = rows.unwrap_or(RowRange {
            start: 0,
            end: planned.rows,
        });
        if range.start > range.end || range.end > planned.rows {
            return Err(source_err(
                split.id,
                format!(
                    "rows [{}, {}) are outside split {} of {} rows",
                    range.start, range.end, split.id, planned.rows
                ),
            ));
        }
        let batch = self.call_read(split.id, range.start, range.end)?;
        self.check_rows(&batch, split.id, range.start, range.end)?;
        let batch = self.conform(&batch, split.id, range.start, range.end)?;
        let (batch, compacted) = compact(&batch)?;
        Counters::add(&self.counters.compacted_bytes, compacted);
        let (batch, copied) = crate::parquet::decode_copy::copy_batch(&batch, alloc, tier)
            .map_err(|e| match e {
                moruna_kernel::MorunaError::Alloc { .. } => e,
                other => source_err(split.id, other.to_string()),
            })?;
        Counters::add(&self.counters.decode_bytes, copied);
        // The interpreter is this source's decoder, so its copy into the arena is the decode
        // copy G-I2 grants (SO-I5).
        alloc.note_payload_copy(copied);
        Payload::table(batch)
    }
}

impl Source for PySource {
    fn schema(&self) -> SourceSchema {
        SourceSchema::Table(Arc::clone(&self.schema))
    }

    fn plan(&self) -> Result<Vec<Split>> {
        Ok(self.plan.clone())
    }

    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        let outcome = self.read_now(split, rows, alloc, tier);
        Box::pin(async move { outcome })
    }

    fn repeatable(&self) -> bool {
        self.repeatable
    }
}

/// The user's `plan()`: a list of objects with `id`, `rows` and optionally `bytes` (d.1). An
/// exception, or an entry without those attributes, is a `Plan` error naming the entry.
fn plan_of(py: Python<'_>, object: &Bound<'_, PyAny>, name: &str) -> Result<Vec<Planned>> {
    let returned = object
        .call_method0(pyo3::intern!(py, "plan"))
        .map_err(|e| plan_err(format!("{name}.plan(): {e}")))?;
    let items = returned.try_iter().map_err(|_| {
        plan_err(format!(
            "{name}.plan() returned {}, not a list of moruna.Split",
            class_name(&returned)
        ))
    })?;
    let mut out = Vec::new();
    for (at, item) in items.enumerate() {
        let item = item.map_err(|e| plan_err(format!("{name}.plan(): {e}")))?;
        let field = |attr: &str| {
            item.getattr(attr).map_err(|_| {
                plan_err(format!(
                    "{name}.plan()[{at}] is {}, which has no `{attr}`; return moruna.Split(id, \
                     rows, bytes=None)",
                    class_name(&item)
                ))
            })
        };
        let whole = |attr: &str, value: &Bound<'_, PyAny>| {
            plan_err(format!(
                "{name}.plan()[{at}].{attr} must be a non-negative int, not {}",
                value
                    .repr()
                    .map_or_else(|_| class_name(value), |r| r.to_string())
            ))
        };
        let id = field("id")?;
        let id = id.extract::<SplitId>().map_err(|_| whole("id", &id))?;
        let rows = field("rows")?;
        let rows = rows.extract::<u64>().map_err(|_| whole("rows", &rows))?;
        let bytes = match item.getattr("bytes") {
            Ok(value) if value.is_none() => None,
            Ok(value) => Some(value.extract::<u64>().map_err(|_| whole("bytes", &value))?),
            Err(_) => None,
        };
        out.push(Planned { id, rows, bytes });
    }
    Ok(out)
}

/// The user's `schema()`, when the class defines one and it returns a schema; `None` otherwise.
fn schema_of(py: Python<'_>, object: &Bound<'_, PyAny>, name: &str) -> Result<Option<SchemaRef>> {
    if !object.hasattr(pyo3::intern!(py, "schema")).unwrap_or(false) {
        return Ok(None);
    }
    let returned = object
        .call_method0(pyo3::intern!(py, "schema"))
        .map_err(|e| plan_err(format!("{name}.schema(): {e}")))?;
    if returned.is_none() {
        return Ok(None);
    }
    let schema: pyo3_arrow::PySchema = returned.extract().map_err(|_| {
        plan_err(format!(
            "{name}.schema() returned {}, not a pyarrow.Schema",
            class_name(&returned)
        ))
    })?;
    Ok(Some(schema.into_inner()))
}

/// The splits of the contract (d.6) from what the user planned: exact row counts, the user's
/// byte estimate or the sample's bytes per row times the rows, `estimated` always (the figure is
/// the user's, not a file's), and `sub_splittable` always, because `read` takes a range.
fn splits_of(planned: &[Planned], schema: &SchemaRef, sample: Option<&RecordBatch>) -> Vec<Split> {
    let columns = schema.fields().len();
    // Bytes per row, per column, from the sample; an even share of the split's bytes otherwise.
    let per_row: Option<Vec<f64>> = sample.filter(|b| b.num_rows() > 0).map(|b| {
        let rows = b.num_rows() as f64;
        b.columns()
            .iter()
            .map(|c| c.get_array_memory_size() as f64 / rows)
            .collect()
    });
    planned
        .iter()
        .map(|p| {
            let column_bytes: Vec<u64> = match (&per_row, p.bytes) {
                (Some(rates), None) => rates
                    .iter()
                    .map(|rate| (rate * p.rows as f64).ceil() as u64)
                    .collect(),
                (Some(rates), Some(total)) => {
                    let sum: f64 = rates.iter().sum();
                    rates
                        .iter()
                        .map(|rate| {
                            if sum > 0.0 {
                                (total as f64 * rate / sum).round() as u64
                            } else {
                                total / columns.max(1) as u64
                            }
                        })
                        .collect()
                }
                (None, total) => {
                    let total = total.unwrap_or(0);
                    (0..columns).map(|_| total / columns as u64).collect()
                }
            };
            let uncompressed_bytes = p.bytes.unwrap_or_else(|| column_bytes.iter().sum());
            Split {
                id: p.id,
                rows: p.rows,
                uncompressed_bytes,
                estimated: true,
                column_bytes,
                null_counts: vec![None; columns],
                sub_splittable: true,
            }
        })
        .collect()
}

/// A column whose buffers hold much more than its rows need, which is what a slice of a larger
/// batch is (`table.slice(start, n)`, the natural way to write `read`), is compacted to its own
/// rows before the decode copy. `copy_batch` keeps an array's offset and copies whole buffers, so
/// without this every morsel read from a sliced table would put the whole table's column into
/// the arena. Slack below this is copied as it is: compacting costs a copy of its own.
const COMPACT_SLACK_BYTES: usize = 64 * 1024;

/// Compact the columns of `batch` that reference more than twice their slice's bytes (plus
/// `COMPACT_SLACK_BYTES`) into buffers of their own, returning the batch and the bytes compacted.
///
/// The compaction is a CPU copy of the slice's bytes into heap memory before the decode copy
/// into the arena, the price of returning a view; a batch that owns its buffers pays nothing
/// here (f.7).
fn compact(batch: &RecordBatch) -> Result<(RecordBatch, u64)> {
    let mut compacted = 0u64;
    let mut columns = Vec::with_capacity(batch.num_columns());
    for column in batch.columns() {
        let data = column.to_data();
        let held = column.get_buffer_memory_size();
        let needed = data.get_slice_memory_size().unwrap_or(held);
        if held > needed.saturating_mul(2).saturating_add(COMPACT_SLACK_BYTES) {
            let mut copy = arrow::array::MutableArrayData::new(vec![&data], false, data.len());
            copy.try_extend(0, 0, data.len())
                .map_err(|e| plan_err(format!("compacting a sliced batch: {e}")))?;
            let own = arrow::array::make_array(copy.freeze());
            compacted += own.get_buffer_memory_size() as u64;
            columns.push(own);
        } else {
            columns.push(Arc::clone(column));
        }
    }
    if compacted == 0 {
        return Ok((batch.clone(), 0));
    }
    let batch = RecordBatch::try_new_with_options(
        batch.schema(),
        columns,
        &arrow::array::RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
    )
    .map_err(|e| plan_err(format!("compacting a sliced batch: {e}")))?;
    Ok((batch, compacted))
}

/// `name: type` for each field, for a schema mismatch message.
fn describe<'a>(fields: impl Iterator<Item = &'a arrow::datatypes::Field>) -> String {
    fields
        .map(|f| format!("{}: {}", f.name(), f.data_type()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The Python class name of a value, for messages.
fn class_name(value: &Bound<'_, PyAny>) -> String {
    value
        .get_type()
        .name()
        .map_or_else(|_| "object".to_string(), |n| n.to_string())
}
