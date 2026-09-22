//! What `ParquetSource`, `TensorSource`, `IteratorSource`, `ParquetSink`, `TensorSink` and
//! `ArrowIpcSink` carry from Python into the facade (d.2, b).
//!
//! Each Python handle holds a configuration, not a built component: every source and sink
//! constructor in components 7 and 8 takes the run's `Arc<dyn Reactor>` and `Arc<dyn Allocator>`,
//! and those exist only inside `Runtime::run` (f.1, "sources built with the reactor"). The
//! escalation in the report covers the consequence for `RunSpec` (d.1 declares `source:
//! Arc<dyn Source>`, which no caller outside the facade can build).

use std::path::Path;

use amoru_sinks::{ArrowIpcSinkConfig, ParquetSinkConfig, TensorSinkConfig};
use amoru_sources::{ParquetSourceConfig, TensorSourceConfig};

/// Which source a run reads, and with what settings.
pub enum SourceSpec {
    /// `amoru.ParquetSource`.
    Parquet(ParquetSourceConfig),
    /// `amoru.TensorSource`.
    Tensor(TensorSourceConfig),
    /// `amoru.IteratorSource`: the iterable and the schema it promises. The object is held by
    /// the Python handle and taken by the module's `run` (f.1 sets `set_staging(0, true)` for it).
    Iterator {
        /// The schema the user declared, as an Arrow schema or a tensor shape.
        schema: IteratorSchema,
    },
}

/// The schema an `IteratorSource` was given.
pub enum IteratorSchema {
    /// A `pyarrow.Schema`.
    Table(arrow::datatypes::SchemaRef),
    /// A `(dtype, shape)` tuple.
    Tensor {
        /// Element type name as Python spelled it.
        dtype: String,
        /// Shape, with a leading `-1` for the batch dimension.
        shape: Vec<i64>,
    },
}

/// Which sink a run writes, and with what settings.
///
/// The Parquet variant is the widest by some way (`ParquetSinkConfig` carries the writer
/// properties); the enum is built once per run and moved once, so the difference costs a
/// stack copy on a path that already reads a Parquet footer.
#[allow(clippy::large_enum_variant)]
pub enum SinkSpec {
    /// `amoru.ParquetSink`.
    Parquet(ParquetSinkConfig),
    /// `amoru.TensorSink`.
    Tensor(TensorSinkConfig),
    /// `amoru.ArrowIpcSink`.
    ArrowIpc(ArrowIpcSinkConfig),
}

impl SourceSpec {
    /// Every URL or path this source reads, for the sink equals source rule (f.3).
    pub fn targets(&self) -> Vec<String> {
        match self {
            SourceSpec::Parquet(cfg) => cfg.urls.clone(),
            SourceSpec::Tensor(cfg) => cfg
                .paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            SourceSpec::Iterator { .. } => Vec::new(),
        }
    }
}

impl SinkSpec {
    /// Where this sink writes, for the sink equals source rule (f.3).
    pub fn target(&self) -> String {
        match self {
            SinkSpec::Parquet(cfg) => cfg.url.clone(),
            SinkSpec::Tensor(cfg) => path_string(&cfg.path),
            SinkSpec::ArrowIpc(cfg) => path_string(&cfg.path),
        }
    }

    /// The sink's name in a report note or an error message.
    pub fn kind_name(&self) -> &'static str {
        match self {
            SinkSpec::Parquet(_) => "ParquetSink",
            SinkSpec::Tensor(_) => "TensorSink",
            SinkSpec::ArrowIpc(_) => "ArrowIpcSink",
        }
    }
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
