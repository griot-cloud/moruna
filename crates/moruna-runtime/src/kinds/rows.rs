//! Rows made by the run itself as a source, as a database's `generate_series` makes them: the
//! only code that knows the `rows` kind. It reads nothing, so a job that uses it needs no file,
//! volume or object store, which is what a smoke test, a measurement or a sizing run wants.
//!
//! `{"kind": "rows", "rows": N}` gives N rows of two `Int64` columns: `id`, the row's number
//! from 0, and `value`, `id % 1000`. They come in splits of [`SPLIT_ROWS`] rows, the last one
//! shorter, and a split is read whole or by any range of its rows, the same rows every time.

use std::sync::Arc;

use moruna_kernel::arrow::array::{ArrayData, ArrayRef, make_array};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Allocator, BoxFuture, MorunaError, Payload, Result, RowRange, Source, SourceSchema, Split,
    SplitId, Tier,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{BuiltSource, Context, Entry, SourceKind};
use crate::spec::SourceSpec;
use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, Role, settings_schema};

/// The rows of one split: 1 Mi, 16 MiB of the two columns.
pub const SPLIT_ROWS: u64 = 1 << 20;

/// What `value` repeats over: `id % VALUES`.
pub const VALUES: i64 = 1000;

/// The `rows` kind, a source.
pub struct Rows;

/// A `rows` source's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// How many rows the source makes; at least one.
    pub rows: u64,
}

impl SourceKind for Rows {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "rows".into(),
            role: Role::Source,
            summary: "Rows the run makes itself, `id` from 0 and `value` = `id % 1000`, as \
                      generate_series makes them: no input"
                .into(),
            settings: settings_schema::<Settings>(
                "rows",
                vec![json!({"rows": 1000}), json!({"rows": 1_000_000_000u64})],
            ),
            inputs: Flow::Nothing,
            outputs: Flow::Batches,
            needs: Needs::nothing(),
            deterministic: Fact::Yes,
            preserves_columns: Fact::Yes,
            isolation: Isolation::Builtin,
            served: None,
            title: None,
            description: None,
            takes: None,
            gives: None,
            requires: Vec::new(),
            provenance: None,
        }
    }

    fn source(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSource> {
        let settings: Settings = entry.settings()?;
        if settings.rows == 0 {
            return Err(entry.refuse("rows", "0: a rows source makes at least one row"));
        }
        if settings.rows > i64::MAX as u64 {
            return Err(entry.refuse(
                "rows",
                format!("{}: at most {} rows, an `id` each", settings.rows, i64::MAX),
            ));
        }
        let source: Arc<dyn Source> = Arc::new(RowsSource::new(settings.rows));
        Ok(BuiltSource {
            spec: SourceSpec::Built(source),
            targets: Vec::new(),
        })
    }
}

/// The source the `rows` kind builds: `rows` rows, made as they are read.
pub struct RowsSource {
    rows: u64,
    schema: SchemaRef,
}

impl RowsSource {
    /// A source of `rows` rows.
    pub fn new(rows: u64) -> RowsSource {
        RowsSource {
            rows,
            schema: Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("value", DataType::Int64, false),
            ])),
        }
    }

    /// The column whose row `i` of `range` in `split` is `value(id)`, in a buffer from `alloc`.
    fn column(
        split: &Split,
        range: RowRange,
        alloc: &dyn Allocator,
        tier: Tier,
        value: impl Fn(i64) -> i64,
    ) -> Result<ArrayRef> {
        let first = split.id as u64 * SPLIT_ROWS + range.start;
        let count = (range.end - range.start) as usize;
        let mut buffer = alloc.alloc((count * 8).max(1), tier)?;
        for (i, slot) in buffer[..count * 8]
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .enumerate()
        {
            *slot = value((first + i as u64) as i64).to_le_bytes();
        }
        let bytes = buffer.into_arrow_buffer()?;
        let data = ArrayData::builder(DataType::Int64)
            .len(count)
            .add_buffer(bytes.slice_with_length(0, count * 8))
            .build()
            .map_err(|e| MorunaError::Source {
                split: split.id,
                msg: e.to_string(),
            })?;
        Ok(make_array(data))
    }

    // As in the reactor: the error type's size is the contract's.
    #[allow(clippy::result_large_err)]
    fn payload(
        &self,
        split: &Split,
        rows: Option<RowRange>,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> Result<Payload> {
        let range = rows.unwrap_or(RowRange {
            start: 0,
            end: split.rows,
        });
        if range.start > range.end || range.end > split.rows {
            return Err(MorunaError::Source {
                split: split.id,
                msg: format!(
                    "rows {}..{} are not within the split's {}",
                    range.start, range.end, split.rows
                ),
            });
        }
        let id = Self::column(split, range, alloc, tier, |id| id)?;
        let value = Self::column(split, range, alloc, tier, |id| id % VALUES)?;
        let batch =
            RecordBatch::try_new(Arc::clone(&self.schema), vec![id, value]).map_err(|e| {
                MorunaError::Source {
                    split: split.id,
                    msg: e.to_string(),
                }
            })?;
        Payload::table(batch)
    }
}

impl Source for RowsSource {
    fn schema(&self) -> SourceSchema {
        SourceSchema::Table(Arc::clone(&self.schema))
    }

    fn plan(&self) -> Result<Vec<Split>> {
        let splits = self.rows.div_ceil(SPLIT_ROWS);
        if splits > SplitId::MAX as u64 {
            return Err(MorunaError::Plan(format!(
                "{} rows are more splits than a run numbers",
                self.rows
            )));
        }
        Ok((0..splits)
            .map(|i| {
                let rows = SPLIT_ROWS.min(self.rows - i * SPLIT_ROWS);
                Split {
                    id: i as SplitId,
                    rows,
                    uncompressed_bytes: rows * 16,
                    estimated: false,
                    column_bytes: vec![rows * 8, rows * 8],
                    null_counts: vec![Some(0), Some(0)],
                    sub_splittable: true,
                }
            })
            .collect())
    }

    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        let outcome = self.payload(split, rows, alloc, tier);
        Box::pin(async move { outcome })
    }
}

#[cfg(test)]
mod tests {
    use moruna_kernel::arrow::array::{Array, Int64Array};
    use moruna_kernel::{PayloadKind, Sink};
    use moruna_testkit::FakeAllocator;

    use super::super::count::{Count, CountSink};
    use super::super::{SinkKind, Standalone};
    use super::*;
    use moruna_job::KindDoc;

    fn block_on<T>(f: BoxFuture<'_, T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn built(settings: serde_json::Value) -> Result<BuiltSource> {
        let doc = KindDoc::from_value(settings).expect("an entry");
        Rows.source(
            &Entry::new("source", None, &doc),
            &mut Context::new(Arc::new(Standalone)),
        )
    }

    fn ints(payload: &Payload, name: &str) -> Vec<i64> {
        let Payload::Table(batch, _) = payload else {
            panic!("a table");
        };
        let column = batch.column_by_name(name).expect("the column");
        let ints = column.as_any().downcast_ref::<Int64Array>().expect("Int64");
        ints.values().to_vec()
    }

    /// The source is planned in splits of 1 Mi rows, the last one shorter, and every row is its
    /// number and that number modulo 1000, however a split is read.
    #[test]
    fn rows_are_numbered_from_zero_in_splits_of_a_mebirow() {
        assert!(Rows.declaration().check(Role::Source).is_ok());
        let built = built(json!({"kind": "rows", "rows": 3_000_000})).expect("built");
        assert!(built.targets.is_empty(), "it reads no location");
        let SourceSpec::Built(source) = built.spec else {
            panic!("built at once");
        };
        let SourceSchema::Table(schema) = source.schema() else {
            panic!("a table");
        };
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, ["id", "value"]);
        let plan = source.plan().expect("a plan");
        let rows: Vec<u64> = plan.iter().map(|s| s.rows).collect();
        assert_eq!(rows, [SPLIT_ROWS, SPLIT_ROWS, 3_000_000 - 2 * SPLIT_ROWS]);
        assert_eq!(plan.iter().map(|s| s.rows).sum::<u64>(), 3_000_000);
        assert_eq!(plan[2].uncompressed_bytes, plan[2].rows * 16);
        assert!(plan.iter().all(|s| s.sub_splittable && !s.estimated));
        let alloc = FakeAllocator::new();
        let last = block_on(source.read(&plan[2], None, &alloc, Tier::Host)).expect("read");
        assert_eq!(last.kind(), PayloadKind::Table);
        assert_eq!(last.rows(), plan[2].rows);
        let ids = ints(&last, "id");
        assert_eq!(ids[0], 2 * SPLIT_ROWS as i64);
        assert_eq!(*ids.last().unwrap(), 2_999_999);
        assert!(
            ints(&last, "value")
                .iter()
                .zip(&ids)
                .all(|(v, id)| *v == id % VALUES)
        );
        // A range of a split, and the same rows again.
        let range = RowRange { start: 10, end: 13 };
        let part = block_on(source.read(&plan[1], Some(range), &alloc, Tier::Host)).unwrap();
        let first = SPLIT_ROWS as i64;
        assert_eq!(ints(&part, "id"), [first + 10, first + 11, first + 12]);
        let again = block_on(source.read(&plan[1], Some(range), &alloc, Tier::Host)).unwrap();
        assert_eq!(ints(&again, "value"), ints(&part, "value"));
        assert!(source.repeatable() && !source.streaming());
        // A range past the split is the source's error.
        let past = RowRange {
            start: 0,
            end: plan[2].rows + 1,
        };
        assert!(block_on(source.read(&plan[2], Some(past), &alloc, Tier::Host)).is_err());
    }

    #[test]
    fn a_rows_source_makes_at_least_one_row_and_takes_rows_alone() {
        for (settings, field) in [
            (json!({"kind": "rows", "rows": 0}), "source.rows"),
            (json!({"kind": "rows"}), "source"),
            (json!({"kind": "rows", "rows": 5, "columns": 3}), "source"),
            (json!({"kind": "rows", "rows": u64::MAX}), "source.rows"),
        ] {
            let error = built(settings.clone()).err().expect("refused");
            assert_eq!(
                super::super::conformance::refused_field(&error).as_deref(),
                Some(field),
                "{settings}: {error}"
            );
        }
        let one = built(json!({"kind": "rows", "rows": 1})).expect("one row");
        let SourceSpec::Built(source) = one.spec else {
            panic!("built");
        };
        assert_eq!(source.plan().unwrap().len(), 1);
    }

    /// The sink keeps nothing and says what reached it.
    #[test]
    fn a_count_sink_counts_and_keeps_nothing() {
        assert!(Count.declaration().check(Role::Sink).is_ok());
        let doc = KindDoc::from_value(json!({"kind": "count"})).unwrap();
        let mut cx = Context::new(Arc::new(Standalone));
        let built = Count
            .sink(&Entry::new("sink", None, &doc), &mut cx)
            .expect("built");
        assert!(built.target.is_empty(), "it writes no location");
        let extra = KindDoc::from_value(json!({"kind": "count", "url": "/x"})).unwrap();
        assert!(
            Count
                .sink(&Entry::new("sink", None, &extra), &mut cx)
                .is_err()
        );

        let source = RowsSource::new(5);
        let split = &source.plan().unwrap()[0];
        let alloc = FakeAllocator::new();
        let mut sink = CountSink::default();
        sink.open(&source.schema()).unwrap();
        assert_eq!(sink.accepts().kind, PayloadKind::Either);
        for seq in 0..3 {
            let payload = block_on(source.read(split, None, &alloc, Tier::Host)).unwrap();
            block_on(sink.write(seq, payload)).unwrap();
        }
        let bytes = block_on(source.read(split, None, &alloc, Tier::Host))
            .unwrap()
            .bytes();
        let summary = sink.finish().unwrap();
        assert_eq!((summary.rows, summary.bytes), (15, 3 * bytes));
        assert!(summary.files.is_empty() && summary.commit.is_none());
    }
}
