//! An OCI image's command as the runnable kind `image` (SDD 19): a sandboxed process whose
//! batches cross the data plane (SDD 29), whatever language it is written in: as Arrow IPC over
//! its standard input and output, which the engine carries onto the plane, or by the plane's
//! three calls on its descriptor 3 (`plane`). The layout is read, verified and unpacked into a
//! cache (`moruna-image`); the command runs in a confined process on Linux (`moruna-sandbox`).

use std::collections::BTreeMap;

use moruna_kernel::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, PortDoc, Role, settings_schema};

use super::{Context, Entry, RunnableKind, Stage};

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
    /// The command to run, replacing the image's entrypoint and cmd. On the Moruna kernel the
    /// reference is a manifest digest and the image's config, once verified, gives the command
    /// when this does not (SDD 30 f.12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Environment added to the image's own; the process gets nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    /// Where unpacked images are kept; `images` beside the run's output when it is a directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    /// The command makes the data plane's calls (`batch_get`, `batch_put`) on its descriptor 3
    /// instead of reading Arrow IPC on its standard input and writing it on its standard output.
    #[serde(default)]
    pub plane: bool,
    /// The ports the command may call (MH 4.10), on the Moruna kernel only: its task's port `n`
    /// is the `n`th of these in name order, from 1 (SDD 30 f.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<BTreeMap<String, PortDoc>>,
}

impl RunnableKind for Image {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "image".into(),
            role: Role::Kernel,
            summary: "An OCI image's command in a sandboxed process, its batches on the data \
                      plane: Arrow IPC over its standard input and output, or the plane's calls"
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
            served: None,
        }
    }

    fn stage(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<Stage> {
        let settings: Settings = entry.settings()?;
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if moruna_micro::on_kernel() {
            return on_kernel(entry, settings, cx);
        }
        if settings.ports.is_some() {
            return Err(entry.refuse(
                "ports",
                "an image calls ports on the Moruna kernel only, through its task's port (SDD 30 \
                 f.7): on a Linux node its process has none",
            ));
        }
        sandboxed(entry, settings, cx)
    }
}

/// The stage on the Moruna kernel (SDD 30 f.5, f.12): the command as a task of the data plane,
/// on the layers the launcher attached and the personality's image filesystem serves. Before
/// the task is made the entry's reference is checked against them: the attached image's record
/// (its manifest, config and layers) must be the image the reference names, and the layers
/// served must be the record's, or the stage is refused naming both sides. The command and the
/// environment are then the verified config's, amended by the entry's as on a Linux node.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn on_kernel(entry: &Entry<'_>, settings: Settings, cx: &mut Context) -> Result<Stage> {
    use std::sync::Arc;

    if settings.cache.is_some() {
        return Err(entry.refuse(
            "cache",
            "the Moruna kernel unpacks no image: the launcher attaches its layers",
        ));
    }
    let attached = moruna_micro::image::attached().map_err(|why| entry.refuse("reference", why))?;
    let verified = moruna_image::record::Record::parse(&attached.record)
        .and_then(|record| record.verify(&settings.reference, &attached.seals))
        .map_err(|e| entry.refuse("reference", e.to_string()))?;
    let launch = moruna_image::Launch::new(
        &settings.reference,
        &verified.config,
        settings.command.clone(),
        &settings.env.clone().unwrap_or_default(),
    )
    .map_err(|e| entry.refuse("command", e.to_string()))?;
    if (launch.uid, launch.gid) != (0, 0) {
        return Err(entry.refuse(
            "reference",
            format!(
                "the image runs as uid {} and gid {}, and the Linux personality starts every \
                 program as root (SDD 28): it cannot run as its image says",
                launch.uid, launch.gid
            ),
        ));
    }
    if launch.working_dir != "/" {
        return Err(entry.refuse(
            "reference",
            format!(
                "the image's working directory is {}, and the Linux personality starts every \
                 program in / (SDD 28): it cannot run as its image says",
                launch.working_dir
            ),
        ));
    }
    // The engine runs on the same image filesystem as its tasks, so its own view of the image
    // is where the command's PATH is searched.
    let candidates = launch.candidates();
    let path = candidates
        .iter()
        .map(|c| std::path::Path::new(&launch.working_dir).join(c))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| {
            entry.refuse(
                "command",
                format!(
                    "`{}` is not in the image {}: tried {}",
                    launch.command[0],
                    verified.digest,
                    candidates.join(", ")
                ),
            )
        })?;
    let routes = match &settings.ports {
        Some(listed) => super::ports::routes(entry, listed, cx)?,
        None => BTreeMap::new(),
    };
    let env = launch.env_entries();
    let identity = json!({
        "image": verified.digest.to_string(), "path": path, "command": launch.command,
        "env": env, "ports": routes.keys().collect::<Vec<_>>(),
    });
    let task = moruna_micro::task::ImageTask::new(
        &settings.reference,
        &identity.to_string(),
        path,
        launch.command,
        env,
        routes.keys().cloned().collect(),
    )
    .map_err(|why| moruna_kernel::MorunaError::Config {
        name: "image",
        msg: format!("{}: {why}", settings.reference),
    })?;
    cx.notes().push(format!(
        "image {}: verified on the Moruna kernel against the {} layers it attached; the layout \
         {} is a Linux node's and is not read here",
        verified.digest,
        attached.seals.len(),
        settings.layout
    ));
    let task = Arc::new(task);
    let mut stage = Stage::of(task.clone());
    let bound = task.clone();
    // MH 4.10: the stage knows its ports, none listed or some, so a call to a number its entry
    // does not list is refused.
    stage.ports = Some(super::ports::StageCalls {
        routes,
        bind: Box::new(move |ports| bound.bind_ports(ports)),
    });
    stage.packaged = Some(crate::packaged::Packaged::Task(task));
    Ok(stage)
}

#[cfg(not(target_os = "linux"))]
fn sandboxed(_entry: &Entry<'_>, _settings: Settings, _cx: &mut Context) -> Result<Stage> {
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
fn sandboxed(entry: &Entry<'_>, settings: Settings, cx: &mut Context) -> Result<Stage> {
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
        settings.plane,
    )?);
    let mut stage = Stage::of(kernel.clone());
    stage.packaged = Some(crate::packaged::Packaged::Image(kernel));
    Ok(stage)
}
