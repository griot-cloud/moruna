//! `moruna.Source`, `moruna.Sink` and `moruna.Split`: the classes a user subclasses to bring a
//! source or a sink of their own (d.2, 07 e.6, 08 f.10), 2026-09-29.
//!
//! The bases hold nothing: a subclass keeps its own state in its instance, and the runtime calls
//! it through `moruna_sources::PySource` and `moruna_sinks::PySink`, which read it by attribute.
//! Each base is a frozen `#[pyclass]` (PY-I7); the instance dictionary a Python subclass gets is
//! the user's object, not this module's. What a subclass must define is checked here, where the
//! user sees the traceback, before anything starts.

use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyModule, PyTuple, PyType};

use crate::sources::type_name;

/// One unit of a `moruna.Source`'s plan: an id unique within the run, the exact row count (the
/// source contract gives a split its rows before any read, contracts d.6) and an optional byte
/// estimate, which the source measures from a sample read when it is absent.
#[pyclass(
    frozen,
    eq,
    skip_from_py_object,
    module = "moruna._core",
    name = "Split"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PySplit {
    /// Split id, unique within the plan.
    #[pyo3(get)]
    pub id: u32,
    /// Rows in the split.
    #[pyo3(get)]
    pub rows: u64,
    /// Estimated bytes of the split's rows in Arrow form, or `None`.
    #[pyo3(get)]
    pub bytes: Option<u64>,
}

#[pymethods]
impl PySplit {
    #[new]
    #[pyo3(signature = (id, rows, bytes = None))]
    fn new(
        id: &Bound<'_, PyAny>,
        rows: &Bound<'_, PyAny>,
        bytes: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PySplit> {
        Ok(PySplit {
            id: whole(id, "id")?,
            rows: whole(rows, "rows")?,
            bytes: match bytes {
                Some(b) if !b.is_none() => Some(whole(b, "bytes")?),
                _ => None,
            },
        })
    }

    fn __repr__(&self) -> String {
        match self.bytes {
            Some(bytes) => format!("Split(id={}, rows={}, bytes={bytes})", self.id, self.rows),
            None => format!("Split(id={}, rows={})", self.id, self.rows),
        }
    }
}

/// A non-negative int that fits `T`, or a `ValueError` naming the argument.
fn whole<'py, T>(value: &Bound<'py, PyAny>, name: &str) -> PyResult<T>
where
    T: for<'a> FromPyObject<'a, 'py>,
{
    value.extract::<T>().map_err(|_| {
        pyo3::exceptions::PyValueError::new_err(format!(
            "Split({name}=...) must be a non-negative int, not {}",
            value
                .repr()
                .map_or_else(|_| type_name(value), |r| r.to_string())
        ))
    })
}

/// The base of a user's source. A subclass defines `plan(self) -> list[moruna.Split]` and
/// `read(self, split_id, start, end) -> pyarrow.RecordBatch` returning exactly rows
/// `[start, end)` of the split, and may define `schema(self) -> pyarrow.Schema`. It is repeatable
/// unless it sets `repeatable = False`.
#[pyclass(frozen, subclass, module = "moruna._core", name = "Source")]
pub struct PySourceBase;

#[pymethods]
impl PySourceBase {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> PySourceBase {
        PySourceBase
    }

    /// True when `read` returns the same rows for the same arguments for the life of the run,
    /// which is what resume and Q0 eviction both rely on (07 SO-I8).
    #[classattr]
    fn repeatable() -> bool {
        true
    }

    /// `None`: the schema is the first read's. Override to declare it, which saves a read at
    /// plan time and is required for a source whose plan is empty.
    fn schema(&self) -> Option<Py<PyAny>> {
        None
    }
}

/// The base of a user's sink. A subclass defines `write(self, batch)`, and may define
/// `finish(self)`, and `checkpoint(self) -> bytes | None` with `restore(self, state)` to take
/// part in resume.
#[pyclass(frozen, subclass, module = "moruna._core", name = "Sink")]
pub struct PySinkBase;

#[pymethods]
impl PySinkBase {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> PySinkBase {
        PySinkBase
    }

    /// Called once, after the last `write`, when the run completes. Nothing by default.
    fn finish(&self) {}

    /// `None`: this sink does not checkpoint, so a run into it is not resumable, and the report
    /// says so. Return bytes that describe every batch written so far to take part in resume.
    fn checkpoint(&self) -> Option<Py<PyAny>> {
        None
    }

    /// Reached only when a subclass returns bytes from `checkpoint` and defines no `restore`,
    /// which `moruna.run` refuses before anything starts.
    fn restore(slf: &Bound<'_, Self>, _state: &Bound<'_, PyAny>) -> PyResult<()> {
        Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "{} defines checkpoint() but not restore(state)",
            type_name(slf.as_any())
        )))
    }
}

/// True when `handle` is an instance of a `moruna.Source` subclass that defines what a source
/// must; a `TypeError` naming the missing method when it is one that does not.
pub fn is_source(handle: &Bound<'_, PyAny>) -> PyResult<bool> {
    if handle.cast::<PySourceBase>().is_err() {
        return Ok(false);
    }
    let class = handle.get_type();
    for method in ["plan", "read"] {
        if !class.hasattr(method)? {
            return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                "{} is a moruna.Source and must define {method}(); see help(moruna.Source)",
                type_name(handle)
            )));
        }
    }
    Ok(true)
}

/// True when `handle` is an instance of a `moruna.Sink` subclass that defines what a sink must;
/// a `TypeError` naming what is missing otherwise.
pub fn is_sink(handle: &Bound<'_, PyAny>) -> PyResult<bool> {
    if handle.cast::<PySinkBase>().is_err() {
        return Ok(false);
    }
    let py = handle.py();
    let class = handle.get_type();
    let base = py.get_type::<PySinkBase>();
    if !class.hasattr("write")? {
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "{} is a moruna.Sink and must define write(batch); see help(moruna.Sink)",
            type_name(handle)
        )));
    }
    if overrides(&class, &base, "checkpoint")? && !overrides(&class, &base, "restore")? {
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "{} defines checkpoint() but not restore(state); a sink that checkpoints must \
             restore, or a resumed run could not continue it",
            type_name(handle)
        )));
    }
    Ok(true)
}

/// True when `class` defines `name` itself rather than taking the base's.
fn overrides(class: &Bound<'_, PyType>, base: &Bound<'_, PyType>, name: &str) -> PyResult<bool> {
    Ok(!class.getattr(name)?.is(&base.getattr(name)?))
}

/// Add the three classes to the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PySplit>()?;
    m.add_class::<PySourceBase>()?;
    m.add_class::<PySinkBase>()?;
    Ok(())
}
