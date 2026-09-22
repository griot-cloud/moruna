//! `PyIteratorSource`: a Python iterator of `pyarrow.RecordBatch` objects (f.5).
//!
//! The one source that cannot promise SO-I6 or SO-I8: it plans one split per pulled batch and
//! cannot re-pull, so it reports `sub_splittable = false` and `repeatable() == false`, the run
//! over it is not resumable, and the facade disables Q0 eviction for it (PL-I6).
//!
//! Two departures from f.5, both reported. The batch is converted with `pyo3-arrow`, because
//! taking an `FFI_ArrowArray` out of a PyCapsule by hand needs `unsafe`, which section l does
//! not allow this crate; `pyo3-arrow` is in the preamble's dependency table for components 5
//! and 12 and this adds 7 to that row. And the DLPack half is a `Plan` error rather than a
//! tensor: importing a `__dlpack__` capsule needs the same capsule unwrapping, and `dlpark`'s
//! own pyo3 integration is a feature of a dependency that `amoru-kernel` pins without it.

use std::sync::{Arc, Mutex};

use pyo3::prelude::*;
use pyo3::types::PyAnyMethods;

use amoru_kernel::{
    Allocator, BoxFuture, Payload, Reactor, Result, RowRange, Source, SourceSchema, Split, Tier,
};

use crate::stats::{Counters, SourceStats};
use crate::util::{plan_err, source_err};

/// The single split id an iterator source uses: it is a stream, not a set of splits (f.5).
const ONLY_SPLIT: amoru_kernel::SplitId = 0;

struct State {
    /// The batch `plan` pulled and no read has taken yet.
    ahead: Option<arrow::array::RecordBatch>,
    /// Set once the iterator has raised `StopIteration`.
    exhausted: bool,
    /// Set once a read has seen the exhausted iterator and returned its empty payload.
    reported: bool,
}

/// A source over a Python iterator of `pyarrow.RecordBatch` objects (d.1).
pub struct PyIteratorSource {
    iter: pyo3::Py<pyo3::PyAny>,
    schema: SourceSchema,
    #[allow(dead_code)]
    reactor: Arc<dyn Reactor>,
    state: Mutex<State>,
    plan: Mutex<Vec<Split>>,
    counters: Counters,
}

impl PyIteratorSource {
    /// `iter` yields `pyarrow.RecordBatch` objects. The reactor is taken like every other
    /// source's; in v1 the iterator source issues no reactor operation (f.5).
    pub fn new(
        iter: pyo3::Py<pyo3::PyAny>,
        schema: SourceSchema,
        reactor: Arc<dyn Reactor>,
    ) -> Result<PyIteratorSource> {
        if let SourceSchema::Tensor { .. } = schema {
            return Err(plan_err(
                "PyIteratorSource over a tensor schema needs the DLPack capsule import, which \
                 needs `unsafe` that 07 section l does not allow this crate (escalation)",
            ));
        }
        Ok(PyIteratorSource {
            iter,
            schema,
            reactor,
            state: Mutex::new(State {
                ahead: None,
                exhausted: false,
                reported: false,
            }),
            plan: Mutex::new(Vec::new()),
            counters: Counters::default(),
        })
    }

    /// What this source has done so far (j).
    pub fn stats(&self) -> SourceStats {
        self.counters.snapshot()
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pull the next batch under interpreter attachment. `StopIteration` is exhaustion; any
    /// other exception is a `Source` error naming the exception's type (f.5).
    fn pull(&self) -> Result<Option<arrow::array::RecordBatch>> {
        pyo3::Python::attach(|py| {
            let iter = self.iter.bind(py);
            match iter.call_method0(pyo3::intern!(py, "__next__")) {
                Ok(object) => {
                    let batch: pyo3_arrow::PyRecordBatch = object.extract().map_err(|e| {
                        source_err(ONLY_SPLIT, format!("{}: {e}", exception_type(py, &e)))
                    })?;
                    Ok(Some(batch.into_inner()))
                }
                Err(e) if e.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => Ok(None),
                Err(e) => Err(source_err(
                    ONLY_SPLIT,
                    format!("{}: {e}", exception_type(py, &e)),
                )),
            }
        })
    }

    /// One split describing the batch just pulled: estimated, never sub splittable (f.5).
    fn split_of(batch: &arrow::array::RecordBatch) -> Split {
        Split {
            id: ONLY_SPLIT,
            rows: batch.num_rows() as u64,
            uncompressed_bytes: batch.get_array_memory_size() as u64,
            estimated: true,
            column_bytes: batch
                .columns()
                .iter()
                .map(|c| c.get_array_memory_size() as u64)
                .collect(),
            null_counts: batch
                .columns()
                .iter()
                .map(|c| Some(c.null_count() as u64))
                .collect(),
            sub_splittable: false,
        }
    }
}

/// The Python type name of an exception, for the message f.5 specifies.
fn exception_type(py: pyo3::Python<'_>, error: &pyo3::PyErr) -> String {
    error.get_type(py).name().map_or_else(
        |_| "Exception".to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

impl Source for PyIteratorSource {
    fn schema(&self) -> SourceSchema {
        self.schema.clone()
    }

    fn plan(&self) -> Result<Vec<Split>> {
        {
            let plan = PyIteratorSource::lock(&self.plan);
            if !plan.is_empty() {
                return Ok(plan.clone());
            }
        }
        let pulled = self.pull()?;
        let mut state = PyIteratorSource::lock(&self.state);
        let splits = match pulled {
            Some(batch) => {
                let split = PyIteratorSource::split_of(&batch);
                state.ahead = Some(batch);
                Counters::add(&self.counters.splits, 1);
                Counters::add(&self.counters.bytes_planned, split.uncompressed_bytes);
                vec![split]
            }
            None => {
                state.exhausted = true;
                Vec::new()
            }
        };
        *PyIteratorSource::lock(&self.plan) = splits.clone();
        Ok(splits)
    }

    fn read(
        &self,
        split: &Split,
        _rows: Option<RowRange>,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'_, Result<Payload>> {
        let outcome = self.read_now(split, alloc, tier);
        Box::pin(async move { outcome })
    }

    fn repeatable(&self) -> bool {
        false
    }
}

impl PyIteratorSource {
    /// The pull and the decode copy. The interpreter attachment is held for the pull and the
    /// conversion only, never across the arena allocation's failure path (f.5).
    fn read_now(&self, split: &Split, alloc: &dyn Allocator, tier: Tier) -> Result<Payload> {
        Counters::add(&self.counters.reads, 1);
        let taken = {
            let mut state = PyIteratorSource::lock(&self.state);
            match state.ahead.take() {
                Some(batch) => Some(batch),
                None if state.exhausted => {
                    if state.reported {
                        return Err(source_err(split.id, "exhausted"));
                    }
                    state.reported = true;
                    None
                }
                None => None,
            }
        };
        let batch = match taken {
            Some(batch) => batch,
            None => match self.pull()? {
                Some(batch) => batch,
                None => {
                    let mut state = PyIteratorSource::lock(&self.state);
                    if state.reported {
                        return Err(source_err(split.id, "exhausted"));
                    }
                    state.exhausted = true;
                    state.reported = true;
                    drop(state);
                    return empty(&self.schema, alloc, tier);
                }
            },
        };
        let (batch, copied) = crate::parquet::decode_copy_for_iterator(&batch, alloc, tier)?;
        Counters::add(&self.counters.decode_bytes, copied);
        // The interpreter is this source's decoder, so its copy into the arena is the decode
        // copy G-I2 grants (SO-I5).
        alloc.note_payload_copy(copied);
        Payload::table(batch)
    }
}

/// The zero-row payload an exhausted iterator returns once, before `Source { "exhausted" }`.
fn empty(schema: &SourceSchema, alloc: &dyn Allocator, tier: Tier) -> Result<Payload> {
    let SourceSchema::Table(schema) = schema else {
        return Err(plan_err("an iterator source has a table schema"));
    };
    let columns: Vec<arrow::array::ArrayRef> = schema
        .fields()
        .iter()
        .map(|f| arrow::array::new_empty_array(f.data_type()))
        .collect();
    let batch = arrow::array::RecordBatch::try_new_with_options(
        Arc::clone(schema),
        columns,
        &arrow::array::RecordBatchOptions::new().with_row_count(Some(0)),
    )
    .map_err(|e| plan_err(format!("an empty batch: {e}")))?;
    let (batch, _) = crate::parquet::decode_copy_for_iterator(&batch, alloc, tier)?;
    Payload::table(batch)
}
