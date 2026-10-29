//! Arrow IPC files as a source (07 e.9) and as a sink (08): the only code that knows the
//! `arrow_ipc` kind.

use std::sync::Arc;

use moruna_kernel::ipc::Compression;
use moruna_kernel::{Result, Source};
use moruna_sinks::{ArrowIpcSink, ArrowIpcSinkConfig};
use moruna_sources::{ArrowIpcSource, ArrowIpcSourceConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{BuiltSink, BuiltSource, Context, Entry, SinkKind, SourceKind};
use crate::job::translate;
use crate::spec::{SinkSpec, SourceSpec};
use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, Role, Urls, settings_schema};

/// The `arrow_ipc` kind, a source and a sink.
pub struct ArrowIpc;

/// An `arrow_ipc` source's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceSettings {
    /// Local files, or directories whose `*.arrow` files are read in name order: a bare path or
    /// `file://`.
    pub url: Urls,
    /// Projection and split size.
    #[serde(default)]
    pub options: SourceOptions,
}

/// `source.options` for `arrow_ipc`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SourceOptions {
    /// The projection; absent is every column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    /// The rows a split is cut toward, from whole record batches; absent is one split per
    /// record batch (07 e.9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split_rows: Option<u64>,
}

/// An `arrow_ipc` sink's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SinkSettings {
    /// A local directory.
    pub url: String,
    /// Sizes.
    #[serde(default)]
    pub options: SinkOptions,
}

/// `sink.options` for `arrow_ipc`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SinkOptions {
    /// `sink.file_bytes`; absent is 1 GiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_bytes: Option<u64>,
    /// `none` (absent), `zstd` or `lz4`; a compressed file is 8-byte aligned, not page aligned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression: Option<String>,
    /// The zstd level (absent is 3); `zstd` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<i32>,
}

impl SourceKind for ArrowIpc {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "arrow_ipc".into(),
            role: Role::Source,
            summary: "Arrow IPC files (the file format) in local paths or directories".into(),
            settings: settings_schema::<SourceSettings>(
                "arrow_ipc",
                vec![
                    json!({"url": "file:///data/in/"}),
                    json!({"url": ["/data/a.arrow", "/data/more/"],
                           "options": {"columns": ["id"], "split_rows": 65536}}),
                ],
            ),
            inputs: Flow::Files,
            outputs: Flow::Batches,
            needs: Needs::nothing(),
            deterministic: Fact::Yes,
            preserves_columns: Fact::BySettings,
            isolation: Isolation::Builtin,
        }
    }

    fn source(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSource> {
        let settings: SourceSettings = entry.settings()?;
        if settings.url.0.is_empty() {
            return Err(entry.refuse("url", "empty: a source needs at least one url"));
        }
        if settings.options.split_rows == Some(0) {
            return Err(entry.refuse(
                "options.split_rows",
                "0: a split holds at least one record batch",
            ));
        }
        let cfg = ArrowIpcSourceConfig {
            urls: settings.url.0.clone(),
            columns: settings.options.columns,
            split_rows: settings.options.split_rows,
        };
        Ok(BuiltSource {
            spec: SourceSpec::Build(Box::new(move |ctx| {
                Ok(Arc::new(ArrowIpcSource::new(cfg, ctx.reactor.clone())?) as Arc<dyn Source>)
            })),
            targets: settings.url.0,
            memory: None,
        })
    }
}

impl SinkKind for ArrowIpc {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "arrow_ipc".into(),
            role: Role::Sink,
            summary: "Arrow IPC files in a local directory, page-aligned or compressed".into(),
            settings: settings_schema::<SinkSettings>(
                "arrow_ipc",
                vec![
                    json!({"url": "/data/out"}),
                    json!({"url": "file:///data/out", "options": {"file_bytes": 268435456}}),
                    json!({"url": "/data/out", "options": {"compression": "zstd", "level": 9}}),
                ],
            ),
            inputs: Flow::Batches,
            outputs: Flow::Files,
            needs: Needs::nothing(),
            deterministic: Fact::Yes,
            preserves_columns: Fact::Yes,
            isolation: Isolation::Builtin,
        }
    }

    fn sink(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSink> {
        let settings: SinkSettings = entry.settings()?;
        let file_bytes = translate::clamp_file_bytes(
            settings
                .options
                .file_bytes
                .unwrap_or(translate::DEFAULT_FILE_BYTES),
            cx.notes(),
        );
        let compression = Compression::parse(
            settings.options.compression.as_deref().unwrap_or("none"),
            settings.options.level,
        )
        .map_err(|reason| entry.refuse("options.compression", reason))?;
        let cfg = ArrowIpcSinkConfig {
            path: translate::local_path(&settings.url),
            file_bytes,
            compression,
        };
        Ok(BuiltSink {
            spec: SinkSpec::Build(Box::new(move |ctx| {
                Ok(Box::new(ArrowIpcSink::new(
                    cfg,
                    ctx.reactor.clone(),
                    ctx.alloc.clone(),
                )?) as Box<dyn moruna_kernel::Sink>)
            })),
            target: settings.url,
            memory: None,
        })
    }
}
