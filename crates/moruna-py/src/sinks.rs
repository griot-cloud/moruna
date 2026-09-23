//! The sink handles: `ParquetSink`, `TensorSink`, `ArrowIpcSink` (d.2).
//!
//! Each is a frozen `#[pyclass]` (PY-I7) holding the configuration component 8 builds from. The
//! two sizes a user may pass, `row_group_bytes` and `file_bytes`, are rows of the preamble's
//! configuration table whose owner is `user`, so they are clamped here, once, and each clamp is
//! carried as a note into the run report (PY-I10, f.3).

use std::path::{Path, PathBuf};

use moruna_sinks::{ArrowIpcSinkConfig, ParquetSinkConfig, TensorFormat, TensorSinkConfig};
use parquet::basic::{Compression, ZstdLevel};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyModule};

use crate::handles::SinkSpec;
use crate::sources::type_name;
use crate::translate::{SizeArg, clamp_file_bytes, clamp_row_group_bytes};

/// A sink URL with a scheme, from what the user wrote (d.2).
///
/// A user writes the destination the way they write it for `pathlib` or `pyarrow`: a bare
/// filesystem path. The reactor reads a string with no `://` as a bare key in the default S3
/// bucket (06 d.1), so a bare path reaches it as an object nobody configured a bucket for and
/// every write fails. The source side already treats a bare path as local (07 util `is_local`),
/// so the surface owes the sink side the same reading, and it belongs here, once, rather than in
/// every caller. A string that already carries a scheme is passed through untouched, so
/// `s3://bucket/out` and `file:///data/out` mean what they say.
fn local_url(url: String) -> String {
    if url.contains("://") {
        return url;
    }
    let path = Path::new(&url);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    format!("file://{}", absolute.display())
}

/// A sink that writes Parquet files under a prefix.
#[pyclass(frozen, module = "moruna._core", name = "ParquetSink")]
pub struct PyParquetSink {
    pub(crate) cfg: ParquetSinkConfig,
    pub(crate) notes: Vec<String>,
}

#[pymethods]
impl PyParquetSink {
    #[new]
    #[pyo3(signature = (url, *, row_group_bytes = None, file_bytes = None, compression = "zstd"))]
    fn new(
        url: String,
        row_group_bytes: Option<&Bound<'_, PyAny>>,
        file_bytes: Option<&Bound<'_, PyAny>>,
        compression: &str,
    ) -> PyResult<PyParquetSink> {
        let mut notes = Vec::new();
        let row_group_bytes = match row_group_bytes {
            Some(v) => clamp_row_group_bytes(size(v, "sink.row_group_bytes")?, &mut notes),
            None => 128 << 20,
        };
        let file_bytes = match file_bytes {
            Some(v) => clamp_file_bytes(size(v, "sink.file_bytes")?, &mut notes),
            None => 1 << 30,
        };
        Ok(PyParquetSink {
            cfg: ParquetSinkConfig {
                url: local_url(url),
                row_group_bytes,
                file_bytes,
                compression: parse_compression(compression)?,
                writer_props: None,
            },
            notes,
        })
    }

    fn __repr__(&self) -> String {
        format!("ParquetSink({})", self.cfg.url)
    }
}

/// A sink that writes tensors, in the aligned binary format or safetensors.
#[pyclass(frozen, module = "moruna._core", name = "TensorSink")]
pub struct PyTensorSink {
    pub(crate) cfg: TensorSinkConfig,
}

#[pymethods]
impl PyTensorSink {
    #[new]
    #[pyo3(signature = (path, *, format = "mrb1", one_file_per_morsel = false, name = "tensor"))]
    fn new(
        path: String,
        format: &str,
        one_file_per_morsel: bool,
        name: &str,
    ) -> PyResult<PyTensorSink> {
        let format = match format.to_ascii_lowercase().as_str() {
            "mrb1" => TensorFormat::Amb1,
            "safetensors" => TensorFormat::SafeTensors,
            other => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown tensor format `{other}` (use \"mrb1\" or \"safetensors\")"
                )));
            }
        };
        Ok(PyTensorSink {
            cfg: TensorSinkConfig {
                path: PathBuf::from(path),
                format,
                one_file_per_morsel,
                name: name.to_string(),
            },
        })
    }

    fn __repr__(&self) -> String {
        format!("TensorSink({})", self.cfg.path.display())
    }
}

/// A sink that writes page-aligned Arrow IPC files.
#[pyclass(frozen, module = "moruna._core", name = "ArrowIpcSink")]
pub struct PyArrowIpcSink {
    pub(crate) cfg: ArrowIpcSinkConfig,
    pub(crate) notes: Vec<String>,
}

#[pymethods]
impl PyArrowIpcSink {
    #[new]
    #[pyo3(signature = (path, *, file_bytes = None))]
    fn new(path: String, file_bytes: Option<&Bound<'_, PyAny>>) -> PyResult<PyArrowIpcSink> {
        let mut notes = Vec::new();
        let file_bytes = match file_bytes {
            Some(v) => clamp_file_bytes(size(v, "sink.file_bytes")?, &mut notes),
            None => 1 << 30,
        };
        Ok(PyArrowIpcSink {
            cfg: ArrowIpcSinkConfig {
                path: PathBuf::from(path),
                file_bytes,
            },
            notes,
        })
    }

    fn __repr__(&self) -> String {
        format!("ArrowIpcSink({})", self.cfg.path.display())
    }
}

/// The `SinkSpec` a handle carries, and the clamps its constructor already reported.
pub fn spec_of(handle: &Bound<'_, PyAny>) -> PyResult<(SinkSpec, Vec<String>)> {
    if let Ok(p) = handle.cast::<PyParquetSink>() {
        let h = p.get();
        return Ok((
            SinkSpec::Parquet(ParquetSinkConfig {
                url: h.cfg.url.clone(),
                row_group_bytes: h.cfg.row_group_bytes,
                file_bytes: h.cfg.file_bytes,
                compression: h.cfg.compression,
                writer_props: h.cfg.writer_props.clone(),
            }),
            h.notes.clone(),
        ));
    }
    if let Ok(t) = handle.cast::<PyTensorSink>() {
        let h = t.get();
        return Ok((
            SinkSpec::Tensor(TensorSinkConfig {
                path: h.cfg.path.clone(),
                format: h.cfg.format,
                one_file_per_morsel: h.cfg.one_file_per_morsel,
                name: h.cfg.name.clone(),
            }),
            Vec::new(),
        ));
    }
    if let Ok(a) = handle.cast::<PyArrowIpcSink>() {
        let h = a.get();
        return Ok((
            SinkSpec::ArrowIpc(ArrowIpcSinkConfig {
                path: h.cfg.path.clone(),
                file_bytes: h.cfg.file_bytes,
            }),
            h.notes.clone(),
        ));
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "sink must be an moruna.ParquetSink, moruna.TensorSink or moruna.ArrowIpcSink, not {}",
        type_name(handle)
    )))
}

fn size(value: &Bound<'_, PyAny>, name: &'static str) -> PyResult<u64> {
    let arg = if let Ok(n) = value.extract::<u64>() {
        SizeArg::Bytes(n)
    } else if let Ok(s) = value.extract::<String>() {
        SizeArg::Text(s)
    } else {
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "{name} must be an int or a size string such as \"128MiB\", not {}",
            type_name(value)
        )));
    };
    arg.bytes(name)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}

fn parse_compression(name: &str) -> PyResult<Compression> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "zstd" => Compression::ZSTD(ZstdLevel::default()),
        "snappy" => Compression::SNAPPY,
        "gzip" => Compression::GZIP(Default::default()),
        "lz4" => Compression::LZ4_RAW,
        "none" | "uncompressed" => Compression::UNCOMPRESSED,
        other => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown compression `{other}` (use \"zstd\", \"snappy\", \"gzip\", \"lz4\" or \"none\")"
            )));
        }
    })
}

/// Add the three sink classes to the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyParquetSink>()?;
    m.add_class::<PyTensorSink>()?;
    m.add_class::<PyArrowIpcSink>()?;
    Ok(())
}
