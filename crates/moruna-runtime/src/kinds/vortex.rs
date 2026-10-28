//! Vortex files as a source (07 e.7) and as a sink (08 f.11): the only code that knows Vortex's
//! kinds.

use std::sync::Arc;

use moruna_kernel::{Result, Source};
use moruna_sinks::{VortexSink, VortexSinkConfig};
use moruna_sources::{VortexSource, VortexSourceConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{BuiltSink, BuiltSource, Context, Entry, SinkKind, SourceKind};
use crate::job::translate;
use crate::spec::{SinkSpec, SourceSpec};
use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, Role, Urls, settings_schema};

/// The `vortex` kind, a source and a sink.
pub struct Vortex;

/// A `vortex` source's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceSettings {
    /// `file://`, `s3://`, `gs://`, `az://` or a bare local path: files or prefixes.
    pub url: Urls,
    /// Projection and split size.
    #[serde(default)]
    pub options: SourceOptions,
}

/// `source.options` for `vortex`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SourceOptions {
    /// The projection; absent is every column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    /// The uncompressed bytes a split is cut toward; absent is 128 MiB (07 e.7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split_bytes: Option<u64>,
}

/// A `vortex` sink's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SinkSettings {
    /// `file://`, `s3://`, `gs://`, `az://` or a bare local directory.
    pub url: String,
    /// Sizes.
    #[serde(default)]
    pub options: SinkOptions,
}

/// `sink.options` for `vortex`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SinkOptions {
    /// `sink.file_bytes`; absent is 1 GiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_bytes: Option<u64>,
}

impl SourceKind for Vortex {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "vortex".into(),
            role: Role::Source,
            summary: "Vortex files or prefixes, local or in an object store".into(),
            settings: settings_schema::<SourceSettings>(
                "vortex",
                vec![
                    json!({"url": "file:///data/in/"}),
                    json!({"url": ["/data/a.vortex", "s3://bucket/in/"],
                           "options": {"columns": ["id"], "split_bytes": 134217728}}),
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
        if settings.options.split_bytes == Some(0) {
            return Err(entry.refuse("options.split_bytes", "0: a split holds at least one zone"));
        }
        let cfg = VortexSourceConfig {
            urls: settings.url.0.clone(),
            columns: settings.options.columns,
            split_bytes: settings.options.split_bytes,
        };
        Ok(BuiltSource {
            spec: SourceSpec::Build(Box::new(move |ctx| {
                let meta = ctx.object_metadata()?;
                // With the arena, so an object URL's footer and zone maps can be read.
                Ok(Arc::new(VortexSource::with_allocator(
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

impl SinkKind for Vortex {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "vortex".into(),
            role: Role::Sink,
            summary: "Vortex files under a prefix, local or in an object store".into(),
            settings: settings_schema::<SinkSettings>(
                "vortex",
                vec![
                    json!({"url": "file:///data/out/"}),
                    json!({"url": "/data/out", "options": {"file_bytes": 1073741824}}),
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
        let file_bytes = translate::clamp_file_bytes(
            settings
                .options
                .file_bytes
                .unwrap_or(translate::DEFAULT_FILE_BYTES),
            cx.notes(),
        );
        let target = url.clone();
        Ok(BuiltSink {
            spec: SinkSpec::Build(Box::new(move |ctx| {
                // As for Parquet: the sink encodes each file in the arena, in one of two buffers,
                // which together are the share the facade sets aside for it.
                let cfg = VortexSinkConfig {
                    url,
                    file_bytes: file_bytes.min(ctx.claim_file_buffer() / 2),
                };
                let sink = VortexSink::new(cfg, ctx.reactor.clone(), ctx.alloc.clone())?
                    .with_run_id(ctx.run_id);
                Ok(Box::new(sink) as Box<dyn moruna_kernel::Sink>)
            })),
            target,
            memory: None,
        })
    }
}
