//! Page-aligned Arrow IPC files as a sink (08): the only code that knows the `arrow_ipc` kind.

use moruna_kernel::Result;
use moruna_sinks::{ArrowIpcSink, ArrowIpcSinkConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{BuiltSink, Context, Entry, SinkKind};
use crate::job::translate;
use crate::spec::SinkSpec;
use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, Role, settings_schema};

/// The `arrow_ipc` kind, a sink.
pub struct ArrowIpc;

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
}

impl SinkKind for ArrowIpc {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "arrow_ipc".into(),
            role: Role::Sink,
            summary: "Page-aligned Arrow IPC files in a local directory".into(),
            settings: settings_schema::<SinkSettings>(
                "arrow_ipc",
                vec![
                    json!({"url": "/data/out"}),
                    json!({"url": "file:///data/out", "options": {"file_bytes": 268435456}}),
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
        let cfg = ArrowIpcSinkConfig {
            path: translate::local_path(&settings.url),
            file_bytes,
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
