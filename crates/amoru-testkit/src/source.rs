//! `FakeSource`, the `Source` fake of contracts d.15.

use std::sync::{Arc, Mutex};

use amoru_kernel::arrow::array::{ArrayData, ArrayRef, make_array};
use amoru_kernel::arrow::datatypes::{DataType, Field, Schema};
use amoru_kernel::arrow::record_batch::RecordBatch;
use amoru_kernel::{
    Allocator, AmoruError, BoxFuture, DType, ManagedTensor, Payload, Result, RowRange, Source,
    SourceSchema, Split, SplitId, Tier,
};

#[derive(Default)]
struct State {
    reads: Vec<(SplitId, Option<RowRange>)>,
}

struct Inner {
    state: Mutex<State>,
    splits: Vec<Split>,
    schema: SourceSchema,
    sub_splittable: bool,
    repeatable: bool,
    failing: Vec<SplitId>,
}

/// The source as a test sees it. Its content is deterministic: row `i` of split `s` has the
/// value `s * 1_000_000 + i` (d.15), so a test can check what came back without a fixture file.
///
/// Knobs: `splits(n, rows_each, bytes_each)`, `schema(SourceSchema)`, `sub_splittable(bool)`,
/// `repeatable(bool)`, `fail_split(id)`. Observables: `reads()`.
#[derive(Clone)]
pub struct FakeSource {
    inner: Arc<Inner>,
}

impl Default for FakeSource {
    fn default() -> Self {
        FakeSource::new()
    }
}

impl FakeSource {
    /// A source of one split of 8 rows, over a single `Int64` column named `value`.
    pub fn new() -> FakeSource {
        FakeSource {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                splits: Vec::new(),
                schema: SourceSchema::Table(Arc::new(Schema::new(vec![Field::new(
                    "value",
                    DataType::Int64,
                    false,
                )]))),
                sub_splittable: true,
                repeatable: true,
                failing: Vec::new(),
            }),
        }
        .splits(1, 8, 64)
    }

    /// Knob: `n` splits of `rows_each` rows and `bytes_each` uncompressed bytes.
    pub fn splits(self, n: u32, rows_each: u64, bytes_each: u64) -> FakeSource {
        let splits = (0..n)
            .map(|id| Split {
                id,
                rows: rows_each,
                uncompressed_bytes: bytes_each,
                estimated: false,
                column_bytes: vec![bytes_each],
                null_counts: vec![Some(0)],
                sub_splittable: self.inner.sub_splittable,
            })
            .collect();
        self.rebuild(|b| b.splits = splits)
    }

    /// Knob: what `schema()` reports and what `read` produces (a table or a tensor).
    pub fn schema(self, schema: SourceSchema) -> FakeSource {
        self.rebuild(|b| b.schema = schema)
    }

    /// Knob: whether `read` accepts a `RowRange` narrower than the split.
    pub fn sub_splittable(self, sub_splittable: bool) -> FakeSource {
        let source = self.rebuild(|b| b.sub_splittable = sub_splittable);
        let splits: Vec<Split> = source
            .inner
            .splits
            .iter()
            .map(|split| Split {
                sub_splittable,
                ..split.clone()
            })
            .collect();
        source.rebuild(|b| b.splits = splits)
    }

    /// Knob: whether the source satisfies CT-I12 (a one-shot iterator does not).
    pub fn repeatable(self, repeatable: bool) -> FakeSource {
        self.rebuild(|b| b.repeatable = repeatable)
    }

    /// Knob: reading this split fails with `Source`.
    pub fn fail_split(self, id: SplitId) -> FakeSource {
        let mut failing = self.inner.failing.clone();
        failing.push(id);
        self.rebuild(|b| b.failing = failing)
    }

    /// Observable: every read, in call order.
    pub fn reads(&self) -> Vec<(SplitId, Option<RowRange>)> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reads
            .clone()
    }

    /// The value row `i` of split `s` holds: `s * 1_000_000 + i` (d.15).
    pub fn value_at(split: SplitId, row: u64) -> i64 {
        split as i64 * 1_000_000 + row as i64
    }

    fn rebuild(self, f: impl FnOnce(&mut Builder)) -> FakeSource {
        let mut builder = Builder {
            splits: self.inner.splits.clone(),
            schema: self.inner.schema.clone(),
            sub_splittable: self.inner.sub_splittable,
            repeatable: self.inner.repeatable,
            failing: self.inner.failing.clone(),
        };
        f(&mut builder);
        let state =
            std::mem::take(&mut *self.inner.state.lock().unwrap_or_else(|e| e.into_inner()));
        FakeSource {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                splits: builder.splits,
                schema: builder.schema,
                sub_splittable: builder.sub_splittable,
                repeatable: builder.repeatable,
                failing: builder.failing,
            }),
        }
    }

    // As in `reactor.rs`: the error type's size is the contract's, not this fake's.
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
        if !self.inner.sub_splittable && rows.is_some() {
            return Err(AmoruError::Source {
                split: split.id,
                msg: "this source does not sub-split".to_string(),
            });
        }
        let count = range.end.saturating_sub(range.start);
        let values: Vec<i64> = (range.start..range.start + count)
            .map(|row| FakeSource::value_at(split.id, row))
            .collect();
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut buffer = alloc.alloc(bytes.len().max(1), tier)?;
        buffer[..bytes.len()].copy_from_slice(&bytes);
        match &self.inner.schema {
            SourceSchema::Table(schema) => {
                let arrow_buffer = buffer.into_arrow_buffer()?;
                let data = ArrayData::builder(DataType::Int64)
                    .len(values.len())
                    .add_buffer(arrow_buffer.slice_with_length(0, bytes.len()))
                    .build()
                    .map_err(|e| AmoruError::Source {
                        split: split.id,
                        msg: e.to_string(),
                    })?;
                let column: ArrayRef = make_array(data);
                let batch =
                    RecordBatch::try_new(Arc::clone(schema), vec![column]).map_err(|e| {
                        AmoruError::Source {
                            split: split.id,
                            msg: e.to_string(),
                        }
                    })?;
                Payload::table(batch)
            }
            SourceSchema::Tensor { dtype, .. } => {
                let tensor = ManagedTensor::from_buffer(
                    buffer,
                    0,
                    *dtype,
                    vec![elements(*dtype, bytes.len())],
                )?;
                Payload::tensor(tensor)
            }
        }
    }
}

fn elements(dtype: DType, bytes: usize) -> i64 {
    (bytes / dtype.item_size()) as i64
}

struct Builder {
    splits: Vec<Split>,
    schema: SourceSchema,
    sub_splittable: bool,
    repeatable: bool,
    failing: Vec<SplitId>,
}

impl Source for FakeSource {
    fn schema(&self) -> SourceSchema {
        self.inner.schema.clone()
    }

    fn plan(&self) -> Result<Vec<Split>> {
        Ok(self.inner.splits.clone())
    }

    fn read(
        &self,
        split: &Split,
        rows: Option<RowRange>,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'_, Result<Payload>> {
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.reads.push((split.id, rows));
        }
        let outcome = if self.inner.failing.contains(&split.id) {
            Err(AmoruError::Source {
                split: split.id,
                msg: "FakeSource::fail_split".to_string(),
            })
        } else {
            self.payload(split, rows, alloc, tier)
        };
        Box::pin(async move { outcome })
    }

    fn repeatable(&self) -> bool {
        self.inner.repeatable
    }
}
