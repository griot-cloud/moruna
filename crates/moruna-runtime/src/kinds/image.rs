//! An OCI image's command as the runnable kind `image` (SDD 19): a sandboxed process that
//! exchanges Arrow IPC with Moruna over standard input and output, whatever language it is
//! written in. The layout is read, verified and unpacked into a cache (`moruna-image`); the
//! command runs in a confined process on Linux (`moruna-sandbox`).

use std::collections::BTreeMap;

use moruna_kernel::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Context, Declaration, Entry, Fact, Flow, Isolation, Needs, Role, RunnableKind, Stage,
    settings_schema,
};

/// The `image` kind.
pub struct Image;

/// An `image` entry's settings (SDD 19 d.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// A local OCI image layout: a directory holding `oci-layout`, `index.json` and `blobs/`.
    pub layout: String,
    /// The image to run: a manifest digest (`sha256:...`) or the name the layout's `index.json`
    /// gives it (`org.opencontainers.image.ref.name`).
    pub reference: String,
    /// The command to run, replacing the image's entrypoint and cmd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Environment added to the image's own; the process gets nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    /// Where unpacked images are kept; `images` beside the run's output when it is a directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
}

impl RunnableKind for Image {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "image".into(),
            role: Role::Kernel,
            summary: "An OCI image's command in a sandboxed process, Arrow IPC over its \
                      standard input and output"
                .into(),
            settings: settings_schema::<Settings>(
                "image",
                vec![json!({
                    "layout": "/disk/images/scorer",
                    "reference": "scorer:1",
                    "command": ["/usr/bin/scorer", "--column", "text"],
                    "env": {"MODEL": "small"}
                })],
            ),
            inputs: Flow::Batches,
            outputs: Flow::Batches,
            needs: Needs::nothing(),
            deterministic: Fact::No,
            preserves_columns: Fact::No,
            isolation: Isolation::Confined,
        }
    }

    fn stage(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<Stage> {
        let settings: Settings = entry.settings()?;
        stage(entry, settings, cx)
    }
}

#[cfg(not(target_os = "linux"))]
fn stage(_entry: &Entry<'_>, _settings: Settings, _cx: &mut Context) -> Result<Stage> {
    Err(moruna_sandbox::unavailable())
}

/// Where unpacked images go when the settings do not say: `images` under the run's output, when
/// that is a directory on this machine.
#[cfg(target_os = "linux")]
fn default_cache(entry: &Entry<'_>, sink: &str) -> Result<std::path::PathBuf> {
    let path = sink.strip_prefix("file://").unwrap_or(sink);
    if path.is_empty() || path.contains("://") {
        return Err(entry.refuse(
            "cache",
            format!(
                "is needed: the run's output `{sink}` is not a directory on this machine to keep \
                 unpacked images beside"
            ),
        ));
    }
    Ok(std::path::Path::new(path).join("images"))
}

#[cfg(target_os = "linux")]
fn stage(entry: &Entry<'_>, settings: Settings, cx: &mut Context) -> Result<Stage> {
    use std::sync::Arc;

    let cgroup = cx.package_host().sandbox_cgroup.clone().ok_or_else(|| {
        moruna_kernel::MorunaError::Config {
            name: "sandbox",
            msg: format!(
                "{}: an image runs in the sandbox, which needs --sandbox-cgroup, a cgroup v2 \
                 directory with memory and cpu delegated (SDD 19 f.5)",
                settings.reference
            ),
        }
    })?;
    let cache = match &settings.cache {
        Some(cache) => std::path::PathBuf::from(cache),
        None => default_cache(entry, cx.sink_target())?,
    };
    let source = moruna_image::OciLayout::open(&settings.layout)
        .map_err(|e| entry.refuse("layout", e.to_string()))?;
    let image = moruna_image::unpack(&source, &settings.reference, &cache)
        .map_err(|e| entry.refuse("reference", e.to_string()))?;
    if image.skipped > 0 {
        cx.notes().push(format!(
            "image {}: {} device nodes or fifos in its layers are not in its root",
            settings.reference, image.skipped
        ));
    }
    let kernel = Arc::new(moruna_sandbox::ImageKernel::new(
        &settings.reference,
        &image,
        settings.command,
        &settings.env.unwrap_or_default(),
        cgroup,
    )?);
    let mut stage = Stage::of(kernel.clone());
    stage.packaged = Some(crate::packaged::Packaged::Image(kernel));
    Ok(stage)
}
