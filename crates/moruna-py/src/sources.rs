//! The source handles: `ParquetSource`, `TensorSource`, `IteratorSource` (d.2).
//!
//! Each is a frozen `#[pyclass]` (PY-I7) holding the configuration component 7 builds from, and
//! the arguments are validated where the user can see the traceback: a projection that is not a
//! list of strings, a filter tuple that is not a triple, a schema that is neither a
//! `pyarrow.Schema` nor a `(dtype, shape)` pair.

use std::path::PathBuf;
use std::sync::Arc;

use moruna_sources::{ParquetSourceConfig, RowFilter, ScalarValue, TensorSourceConfig};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyList, PyModule, PySequence, PyString, PyTuple};

use crate::handles::{IteratorSchema, SourceSpec};

/// A source over Parquet files or prefixes.
#[pyclass(frozen, module = "moruna._core", name = "ParquetSource")]
pub struct PyParquetSource {
    pub(crate) cfg: ParquetSourceConfig,
}

#[pymethods]
impl PyParquetSource {
    #[new]
    #[pyo3(signature = (urls, *, columns = None, filters = None))]
    fn new(
        urls: &Bound<'_, PyAny>,
        columns: Option<Vec<String>>,
        filters: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyParquetSource> {
        Ok(PyParquetSource {
            cfg: ParquetSourceConfig {
                urls: string_or_list(urls, "urls")?,
                columns,
                filters: match filters {
                    Some(f) => row_filters(f)?,
                    None => Vec::new(),
                },
                batch_rows_hint: None,
            },
        })
    }

    fn __repr__(&self) -> String {
        format!("ParquetSource({} url(s))", self.cfg.urls.len())
    }
}

/// A source over safetensors or aligned binary tensor files.
#[pyclass(frozen, module = "moruna._core", name = "TensorSource")]
pub struct PyTensorSource {
    pub(crate) cfg: TensorSourceConfig,
}

#[pymethods]
impl PyTensorSource {
    #[new]
    #[pyo3(signature = (paths, *, tensors = None))]
    fn new(paths: &Bound<'_, PyAny>, tensors: Option<Vec<String>>) -> PyResult<PyTensorSource> {
        Ok(PyTensorSource {
            cfg: TensorSourceConfig {
                paths: string_or_list(paths, "paths")?
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
                tensors,
                slice_rows_hint: None,
            },
        })
    }

    fn __repr__(&self) -> String {
        format!("TensorSource({} path(s))", self.cfg.paths.len())
    }
}

/// A source over a Python iterable of `pyarrow.RecordBatch` objects.
#[pyclass(frozen, module = "moruna._core", name = "IteratorSource")]
pub struct PyIteratorSourceHandle {
    /// The iterator the run pulls from; `iter()` is called here so a list is accepted too.
    pub(crate) iterator: Py<PyAny>,
    pub(crate) schema: IteratorSchema,
}

#[pymethods]
impl PyIteratorSourceHandle {
    #[new]
    #[pyo3(signature = (iterable, *, schema))]
    fn new(iterable: &Bound<'_, PyAny>, schema: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(PyIteratorSourceHandle {
            iterator: iterable.try_iter()?.into_any().unbind(),
            schema: iterator_schema(schema)?,
        })
    }

    fn __repr__(&self) -> String {
        "IteratorSource(...)".to_string()
    }
}

/// The `SourceSpec` a handle carries, for translation and for the facade.
pub fn spec_of(handle: &Bound<'_, PyAny>) -> PyResult<SourceSpec> {
    if let Ok(p) = handle.cast::<PyParquetSource>() {
        return Ok(SourceSpec::Parquet(clone_parquet(&p.get().cfg)));
    }
    if let Ok(t) = handle.cast::<PyTensorSource>() {
        let cfg = &t.get().cfg;
        return Ok(SourceSpec::Tensor(TensorSourceConfig {
            paths: cfg.paths.clone(),
            tensors: cfg.tensors.clone(),
            slice_rows_hint: cfg.slice_rows_hint,
        }));
    }
    if let Ok(i) = handle.cast::<PyIteratorSourceHandle>() {
        return Ok(SourceSpec::Iterator {
            schema: clone_schema(&i.get().schema),
        });
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "source must be an moruna.ParquetSource, moruna.TensorSource or moruna.IteratorSource, not {}",
        type_name(handle)
    )))
}

fn clone_parquet(cfg: &ParquetSourceConfig) -> ParquetSourceConfig {
    ParquetSourceConfig {
        urls: cfg.urls.clone(),
        columns: cfg.columns.clone(),
        filters: cfg.filters.clone(),
        batch_rows_hint: cfg.batch_rows_hint,
    }
}

fn clone_schema(schema: &IteratorSchema) -> IteratorSchema {
    match schema {
        IteratorSchema::Table(s) => IteratorSchema::Table(Arc::clone(s)),
        IteratorSchema::Tensor { dtype, shape } => IteratorSchema::Tensor {
            dtype: dtype.clone(),
            shape: shape.clone(),
        },
    }
}

pub(crate) fn type_name(value: &Bound<'_, PyAny>) -> String {
    value
        .get_type()
        .name()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "object".to_string())
}

/// `urls: str | list[str]` (d.2).
fn string_or_list(value: &Bound<'_, PyAny>, name: &str) -> PyResult<Vec<String>> {
    if let Ok(s) = value.cast::<PyString>() {
        return Ok(vec![s.to_string()]);
    }
    let seq = value.cast::<PySequence>().map_err(|_| {
        pyo3::exceptions::PyTypeError::new_err(format!(
            "{name} must be a string or a list of strings, not {}",
            type_name(value)
        ))
    })?;
    let mut out = Vec::new();
    for item in seq.try_iter()? {
        out.push(item?.extract::<String>()?);
    }
    if out.is_empty() {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "{name} is empty: a source needs at least one url"
        )));
    }
    Ok(out)
}

/// `filters=[(column, op, value), ...]`, the row-group predicates of 07 d.1.
fn row_filters(value: &Bound<'_, PyAny>) -> PyResult<Vec<RowFilter>> {
    let list = value.cast::<PyList>().map_err(|_| {
        pyo3::exceptions::PyTypeError::new_err(
            "filters must be a list of (column, op, value) tuples",
        )
    })?;
    let mut out = Vec::new();
    for item in list.iter() {
        let t = item.cast::<PyTuple>().map_err(|_| {
            pyo3::exceptions::PyTypeError::new_err(
                "each filter must be a (column, op, value) tuple",
            )
        })?;
        if t.len() != 3 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "each filter must be a (column, op, value) tuple",
            ));
        }
        let column: String = t.get_item(0)?.extract()?;
        let op: String = t.get_item(1)?.extract()?;
        let scalar = scalar(&t.get_item(2)?)?;
        out.push(match op.as_str() {
            ">" | "gt" => RowFilter::Gt(column, scalar),
            "<" | "lt" => RowFilter::Lt(column, scalar),
            "==" | "=" | "eq" => RowFilter::Eq(column, scalar),
            other => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown filter operator `{other}` (use \">\", \"<\" or \"==\")"
                )));
            }
        });
    }
    Ok(out)
}

fn scalar(value: &Bound<'_, PyAny>) -> PyResult<ScalarValue> {
    if let Ok(b) = value.extract::<bool>() {
        return Ok(ScalarValue::Bool(b));
    }
    if let Ok(i) = value.extract::<i64>() {
        return Ok(ScalarValue::I64(i));
    }
    if let Ok(u) = value.extract::<u64>() {
        return Ok(ScalarValue::U64(u));
    }
    if let Ok(f) = value.extract::<f64>() {
        return Ok(ScalarValue::F64(f));
    }
    if let Ok(s) = value.extract::<String>() {
        return Ok(ScalarValue::Str(s));
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "a filter value must be an int, float, str or bool, not {}",
        type_name(value)
    )))
}

/// `schema: pyarrow.Schema | tuple[str, tuple[int, ...]]` (d.2).
fn iterator_schema(value: &Bound<'_, PyAny>) -> PyResult<IteratorSchema> {
    if let Ok(t) = value.cast::<PyTuple>() {
        if t.len() != 2 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "a tensor schema is a (dtype, shape) pair",
            ));
        }
        let dtype: String = t.get_item(0)?.extract()?;
        let shape: Vec<i64> = t.get_item(1)?.extract()?;
        return Ok(IteratorSchema::Tensor { dtype, shape });
    }
    let schema: pyo3_arrow::PySchema = value.extract().map_err(|_| {
        pyo3::exceptions::PyTypeError::new_err(format!(
            "schema must be a pyarrow.Schema or a (dtype, shape) pair, not {}",
            type_name(value)
        ))
    })?;
    Ok(IteratorSchema::Table(schema.into_inner()))
}

/// Add the three source classes to the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyParquetSource>()?;
    m.add_class::<PyTensorSource>()?;
    m.add_class::<PyIteratorSourceHandle>()?;
    Ok(())
}
