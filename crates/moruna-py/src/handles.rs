//! What `ParquetSource`, `TensorSource`, `IteratorSource`, `ParquetSink`, `TensorSink`,
//! `ArrowIpcSink` and the user's `moruna.Source` and `moruna.Sink` subclasses carry from Python
//! into the facade (d.2, b).
//!
//! Each Python handle holds a configuration, not a built component: every source and sink
//! constructor in components 7 and 8 takes the run's `Arc<dyn Reactor>` and `Arc<dyn Allocator>`,
//! and those exist only inside `Runtime::run` (f.1, "sources built with the reactor"). The
//! escalation in the report covers the consequence for `RunSpec` (d.1 declares `source:
//! Arc<dyn Source>`, which no caller outside the facade can build).

use std::path::Path;

use moruna_sinks::{ArrowIpcSinkConfig, ParquetSinkConfig, TensorSinkConfig};
use moruna_sources::{ParquetSourceConfig, TensorSourceConfig};

/// Which source a run reads, and with what settings.
pub enum SourceSpec {
    /// `moruna.ParquetSource`.
    Parquet(ParquetSourceConfig),
    /// `moruna.TensorSource`.
    Tensor(TensorSourceConfig),
    /// `moruna.IteratorSource`: the iterable and the schema it promises. The object is held by
    /// the Python handle and taken by the module's `run` (f.1 sets `set_staging(0, true)` for it).
    Iterator {
        /// The schema the user declared, as an Arrow schema or a tensor shape.
        schema: IteratorSchema,
    },
    /// A `moruna.Source` subclass (07 e.6). The object is held by `moruna.run`, which hands it to
    /// the runtime through the library loader, as it does the iterable.
    Python,
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
    /// `moruna.ParquetSink`.
    Parquet(ParquetSinkConfig),
    /// `moruna.TensorSink`.
    Tensor(TensorSinkConfig),
    /// `moruna.ArrowIpcSink`.
    ArrowIpc(ArrowIpcSinkConfig),
    /// A `moruna.Sink` subclass (08 f.10), held by `moruna.run` like a `moruna.Source`.
    Python,
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
            SourceSpec::Iterator { .. } | SourceSpec::Python => Vec::new(),
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
            // A user's sink names no location; the empty target is what the rule skips.
            SinkSpec::Python => String::new(),
        }
    }

    /// The sink's name in a report note or an error message.
    pub fn kind_name(&self) -> &'static str {
        match self {
            SinkSpec::Parquet(_) => "ParquetSink",
            SinkSpec::Tensor(_) => "TensorSink",
            SinkSpec::ArrowIpc(_) => "ArrowIpcSink",
            SinkSpec::Python => "Sink",
        }
    }
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
