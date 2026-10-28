//! Parquet files as a source and as a sink (07, 08): the only code that knows Parquet's kinds.

use std::sync::Arc;

use moruna_kernel::{Result, Source};
use moruna_sinks::{ParquetSink, ParquetSinkConfig};
use moruna_sources::{ParquetSource, ParquetSourceConfig, RowFilter, ScalarValue};
use parquet::basic::{Compression, ZstdLevel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    BuiltSink, BuiltSource, Context, Declaration, Entry, Fact, Flow, Isolation, Needs, Role,
    SinkKind, SourceKind, Urls, settings_schema,
};
use crate::job::translate;
use crate::spec::{SinkSpec, SourceSpec};

/// The `parquet` kind, a source and a sink.
pub struct Parquet;

/// A `parquet` source's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceSettings {
    /// `file://`, `s3://`, `gs://`, `az://` or a bare local path: files or prefixes.
    pub url: Urls,
    /// Projection and row-group pruning.
    #[serde(default)]
    pub options: SourceOptions,
}

/// `source.options` for `parquet`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SourceOptions {
    /// The projection; absent is every column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    /// Row-group predicates, each `[column, op, value]` with `op` one of `>`, `<`, `==`.
    pub filters: Vec<Filter>,
}

/// One row-group predicate: `[column, op, value]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Filter(pub String, pub String, pub serde_json::Value);

/// A `parquet` sink's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SinkSettings {
    /// `file://`, `s3://`, `gs://`, `az://` or a bare local directory.
    pub url: String,
    /// Sizes and compression.
    #[serde(default)]
    pub options: SinkOptions,
}

/// `sink.options` for `parquet`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SinkOptions {
    /// `sink.row_group_bytes`; absent is 128 MiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_group_bytes: Option<u64>,
    /// `sink.file_bytes`; absent is 1 GiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_bytes: Option<u64>,
    /// `zstd` (absent), `snappy`, `gzip`, `lz4` or `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression: Option<String>,
}

impl SourceKind for Parquet {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "parquet".into(),
            role: Role::Source,
            summary: "Parquet files or prefixes, local or in an object store".into(),
            settings: settings_schema::<SourceSettings>(
                "parquet",
                vec![
                    json!({"url": "file:///data/in/"}),
                    json!({"url": ["/data/a.parquet", "s3://bucket/b/"],
                           "options": {"columns": ["id"], "filters": [["id", ">", 5]]}}),
                ],
            ),
            inputs: Flow::Files,
            outputs: Flow::Batches,
            needs: Needs {
                network: Fact::BySettings,
                ..Needs::nothing()
            },
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
        let mut filters = Vec::with_capacity(settings.options.filters.len());
        for (i, filter) in settings.options.filters.iter().enumerate() {
            filters.push(row_filter(entry, i, filter)?);
        }
        let cfg = ParquetSourceConfig {
            urls: settings.url.0.clone(),
            columns: settings.options.columns,
            filters,
            batch_rows_hint: None,
        };
        Ok(BuiltSource {
            spec: SourceSpec::Build(Box::new(move |ctx| {
                let meta = ctx.object_metadata()?;
                // With the arena, so an object URL's footer can be read (MH 4.6).
                Ok(Arc::new(ParquetSource::with_allocator(
                    cfg,
                    ctx.reactor.clone(),
                    meta,
                    ctx.alloc.clone(),
                )?) as Arc<dyn Source>)
            })),
            targets: settings.url.0,
            memory: None,
        })
    }
}

/// `[column, op, value]` to a row-group predicate (07 d.1).
fn row_filter(entry: &Entry<'_>, index: usize, filter: &Filter) -> Result<RowFilter> {
    let field = format!("options.filters[{index}]");
    let Filter(column, op, value) = filter;
    let scalar = match value {
        serde_json::Value::Bool(b) => ScalarValue::Bool(*b),
        serde_json::Value::String(s) => ScalarValue::Str(s.clone()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ScalarValue::I64(i)
            } else if let Some(u) = n.as_u64() {
                ScalarValue::U64(u)
            } else {
                ScalarValue::F64(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        other => {
            return Err(entry.refuse(
                &field,
                format!("a filter value is a number, a string or a boolean, not {other}"),
            ));
        }
    };
    Ok(match op.as_str() {
        ">" | "gt" => RowFilter::Gt(column.clone(), scalar),
        "<" | "lt" => RowFilter::Lt(column.clone(), scalar),
        "==" | "=" | "eq" => RowFilter::Eq(column.clone(), scalar),
        other => {
            return Err(entry.refuse(
                &field,
                format!("unknown operator `{other}` (use \">\", \"<\" or \"==\")"),
            ));
        }
    })
}

impl SinkKind for Parquet {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "parquet".into(),
            role: Role::Sink,
            summary: "Parquet files under a prefix, local or in an object store".into(),
            settings: settings_schema::<SinkSettings>(
                "parquet",
                vec![
                    json!({"url": "file:///data/out/"}),
                    json!({"url": "s3://bucket/out/",
                           "options": {"row_group_bytes": 33554432, "compression": "snappy"}}),
                ],
            ),
            inputs: Flow::Batches,
            outputs: Flow::Files,
            needs: Needs {
                network: Fact::BySettings,
                ..Needs::nothing()
            },
            deterministic: Fact::Yes,
            preserves_columns: Fact::Yes,
            isolation: Isolation::Builtin,
        }
    }

    fn sink(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSink> {
        let settings: SinkSettings = entry.settings()?;
        let url = translate::local_url(&settings.url);
        let row_group_bytes = translate::clamp_row_group_bytes(
            settings
                .options
                .row_group_bytes
                .unwrap_or(translate::DEFAULT_ROW_GROUP_BYTES),
            cx.notes(),
        );
        let file_bytes = translate::clamp_file_bytes(
            settings
                .options
                .file_bytes
                .unwrap_or(translate::DEFAULT_FILE_BYTES),
            cx.notes(),
        );
        let compression =
            parse_compression(settings.options.compression.as_deref().unwrap_or("zstd"))
                .map_err(|reason| entry.refuse("options.compression", reason))?;
        let target = url.clone();
        Ok(BuiltSink {
            spec: SinkSpec::Build(Box::new(move |ctx| {
                // The sink encodes each file in the arena, in one of two buffers: together they
                // are the share the facade sets aside for it.
                let cfg = ParquetSinkConfig {
                    url,
                    row_group_bytes,
                    file_bytes: file_bytes.min(ctx.claim_file_buffer() / 2),
                    compression,
                    writer_props: None,
                };
                // The run id goes into every file's footer (08 e.2).
                let sink = ParquetSink::new(cfg, ctx.reactor.clone(), ctx.alloc.clone())?
                    .with_run_id(ctx.run_id);
                Ok(Box::new(sink) as Box<dyn moruna_kernel::Sink>)
            })),
            target,
            memory: None,
        })
    }
}

/// A Parquet compression by the name a document and `ParquetSink(compression=)` use.
pub fn parse_compression(name: &str) -> core::result::Result<Compression, String> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "zstd" => Compression::ZSTD(ZstdLevel::default()),
        "snappy" => Compression::SNAPPY,
        "gzip" => Compression::GZIP(Default::default()),
        "lz4" => Compression::LZ4_RAW,
        "none" | "uncompressed" => Compression::UNCOMPRESSED,
        other => {
            return Err(format!(
                "unknown compression `{other}` (use \"zstd\", \"snappy\", \"gzip\", \"lz4\" or \"none\")"
            ));
        }
    })
}

/// The name [`parse_compression`] reads for a compression, so a built sink can be described.
pub fn compression_name(compression: Compression) -> &'static str {
    match compression {
        Compression::SNAPPY => "snappy",
        Compression::GZIP(_) => "gzip",
        Compression::LZ4_RAW | Compression::LZ4 => "lz4",
        Compression::UNCOMPRESSED => "none",
        _ => "zstd",
    }
}
