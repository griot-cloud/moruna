//! Tensor files as a source (07) and as a sink (08): the only code that knows the tensor kinds.

use std::sync::Arc;

use moruna_kernel::{Result, Source};
use moruna_sinks::{TensorFormat, TensorSink, TensorSinkConfig};
use moruna_sources::{TensorSource, TensorSourceConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{BuiltSink, BuiltSource, Context, Entry, SinkKind, SourceKind};
use crate::job::translate;
use crate::spec::{SinkSpec, SourceSpec};
use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, Role, Urls, settings_schema};

/// The `tensor` kind, a source and a sink.
pub struct Tensor;

/// A `tensor` source's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceSettings {
    /// Local paths or `file://` URLs of safetensors or aligned binary tensor files.
    pub url: Urls,
    /// Which tensors.
    #[serde(default)]
    pub options: SourceOptions,
}

/// `source.options` for `tensor`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SourceOptions {
    /// Tensor names; absent is every tensor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tensors: Option<Vec<String>>,
}

/// A `tensor` sink's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SinkSettings {
    /// A local directory.
    pub url: String,
    /// Format and naming.
    #[serde(default)]
    pub options: SinkOptions,
}

/// `sink.options` for `tensor`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SinkOptions {
    /// `mrb1` (absent) or `safetensors`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// One file per morsel rather than one per size bound.
    pub one_file_per_morsel: bool,
    /// The tensor name; absent is `tensor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl SourceKind for Tensor {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "tensor".into(),
            role: Role::Source,
            summary: "Safetensors or aligned binary tensor files on a local disk".into(),
            settings: settings_schema::<SourceSettings>(
                "tensor",
                vec![
                    json!({"url": "/data/weights.safetensors"}),
                    json!({"url": ["/data/a.safetensors", "file:///data/b.safetensors"],
                           "options": {"tensors": ["w"]}}),
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
            return Err(entry.refuse("url", "empty: a source needs at least one path"));
        }
        let cfg = TensorSourceConfig {
            paths: settings
                .url
                .0
                .iter()
                .map(|u| translate::local_path(u))
                .collect(),
            tensors: settings.options.tensors,
            slice_rows_hint: None,
        };
        Ok(BuiltSource {
            spec: SourceSpec::Build(Box::new(move |ctx| {
                Ok(Arc::new(TensorSource::new(cfg, ctx.reactor.clone())?) as Arc<dyn Source>)
            })),
            targets: settings.url.0,
            memory: None,
        })
    }
}

impl SinkKind for Tensor {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "tensor".into(),
            role: Role::Sink,
            summary: "Tensor files in a local directory, `mrb1` or safetensors".into(),
            settings: settings_schema::<SinkSettings>(
                "tensor",
                vec![
                    json!({"url": "/data/out"}),
                    json!({"url": "file:///data/out",
                           "options": {"format": "safetensors", "one_file_per_morsel": true,
                                       "name": "embeddings"}}),
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

    fn sink(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSink> {
        let settings: SinkSettings = entry.settings()?;
        let format = parse_tensor_format(settings.options.format.as_deref().unwrap_or("mrb1"))
            .map_err(|reason| entry.refuse("options.format", reason))?;
        let cfg = TensorSinkConfig {
            path: translate::local_path(&settings.url),
            format,
            one_file_per_morsel: settings.options.one_file_per_morsel,
            name: settings
                .options
                .name
                .unwrap_or_else(|| "tensor".to_string()),
        };
        Ok(BuiltSink {
            spec: SinkSpec::Build(Box::new(move |ctx| {
                Ok(Box::new(TensorSink::new(
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

/// A tensor file format by the name a document and `TensorSink(format=)` use.
pub fn parse_tensor_format(name: &str) -> core::result::Result<TensorFormat, String> {
    match name.to_ascii_lowercase().as_str() {
        "mrb1" => Ok(TensorFormat::Amb1),
        "safetensors" => Ok(TensorFormat::SafeTensors),
        other => Err(format!(
            "unknown tensor format `{other}` (use \"mrb1\" or \"safetensors\")"
        )),
    }
}

/// The name [`parse_tensor_format`] reads for a format.
pub fn tensor_format_name(format: TensorFormat) -> &'static str {
    match format {
        TensorFormat::Amb1 => "mrb1",
        TensorFormat::SafeTensors => "safetensors",
    }
}
