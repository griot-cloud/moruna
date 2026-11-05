//! An OCI image as a task of every role, the kind `image` (SDD 19): a source, a function or a
//! sink whose code is an image's command, run as a confined process whatever language it is
//! written in. A function's batches cross the data plane (SDD 29), as Arrow IPC over the
//! process's standard input and output, which the engine carries onto the plane, or by the
//! plane's three calls on its descriptor 3 (`plane`); a source writes one Arrow IPC stream on its
//! standard output, and a sink reads one on its standard input and writes back its commit.
//!
//! The task names its image as the job's spec does (`moruna_image::ImageRef`): a build key, and
//! once built, the manifest's digest and where the image is. A draft, an image that is not the
//! one named, an image not pulled, are refused naming the task. The runner gives the process
//! its entry, its role, its params, the run's arguments and the names of its ports in its
//! environment (`MORUNA_ENTRY`, `MORUNA_ROLE`, `MORUNA_PARAMS`, `MORUNA_ARGS`, `MORUNA_PORTS`),
//! and serves its needs as ports on a socket (`moruna_sandbox::ports`). The command runs in a
//! confined process on Linux (`moruna-sandbox`), and a function also on the Moruna kernel.

use std::collections::BTreeMap;

use moruna_kernel::{Result, Route};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use moruna_job::{
    Declaration, Fact, Flow, Isolation, Needs, PortDoc, Role, is_port_name, settings_schema,
};

use super::{BuiltSink, BuiltSource, Context, Entry, RunnableKind, SinkKind, SourceKind, Stage};

/// The `image` kind, of every role.
pub struct Image;

/// The environment variable that holds the task's entry, which the image's entry point reads.
pub const ENTRY_ENV: &str = "MORUNA_ENTRY";
/// The environment variable that holds the task's role: `source`, `function` or `sink`.
pub const ROLE_ENV: &str = "MORUNA_ROLE";
/// The environment variable that holds the task's params, a JSON object.
pub const PARAMS_ENV: &str = "MORUNA_PARAMS";
/// The environment variable that holds the run's arguments, a JSON object.
pub const ARGS_ENV: &str = "MORUNA_ARGS";

/// Where an image is, as a document names it ([`moruna_image::ImageRef`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageDoc {
    /// The build key of an image Moruna builds: `sha256:` and 64 hex digits over its code, its
    /// lock, its runtime, its shim, its platform and its entry; absent for an image it did not
    /// build.
    #[serde(default)]
    pub build_key: Option<String>,
    /// The manifest's digest, `sha256:...`; absent until the image is built.
    #[serde(default)]
    pub digest: Option<String>,
    /// Where the image is: an OCI layout's absolute path, or a registry reference
    /// `HOST/REPOSITORY@sha256:...`; absent until the image is built.
    #[serde(default)]
    pub location: Option<String>,
}

impl ImageDoc {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn as_ref(&self) -> moruna_image::ImageRef {
        moruna_image::ImageRef {
            build_key: self.build_key.clone(),
            digest: self.digest.clone(),
            location: self.location.clone(),
        }
    }
}

/// An `image` entry's settings (SDD 19 d.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The image the task runs.
    pub image: ImageDoc,
    /// The entry the image's entry point runs, given to the process as `MORUNA_ENTRY`; Moruna
    /// does not read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// The task's params, data only, given to the process as `MORUNA_PARAMS`, checked against
    /// the JSON Schema of its definition.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
    /// What the runner must supply the task: each a port it serves, which the process calls on
    /// its port socket.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// The command to run, replacing the image's entrypoint and cmd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Environment added to the image's own; the process gets nothing else. A name the runner
    /// gives (`MORUNA_...`) is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    /// Where unpacked images are kept; `images` beside the run's output when it is a directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    /// A function's command makes the data plane's calls (`batch_get`, `batch_put`) on its
    /// descriptor 3 instead of reading Arrow IPC on its standard input and writing it on its
    /// standard output.
    #[serde(default)]
    pub plane: bool,
    /// Ports the command may call besides its needs, each routed as the document says (MH
    /// 4.10): to the host protocol's peer or to a handler the program registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<BTreeMap<String, PortDoc>>,
}

fn declaration_of(role: Role) -> Declaration {
    let (summary, inputs, outputs) = match role {
        Role::Source => (
            "An OCI image's command as a source, in a sandboxed process that writes one Arrow \
             IPC stream on its standard output",
            Flow::Nothing,
            Flow::Batches,
        ),
        Role::Kernel => (
            "An OCI image's command in a sandboxed process, its batches on the data plane: \
             Arrow IPC over its standard input and output, or the plane's calls",
            Flow::Batches,
            Flow::Batches,
        ),
        Role::Sink => (
            "An OCI image's command as a sink, in a sandboxed process that reads one Arrow IPC \
             stream on its standard input and writes back its commit",
            Flow::Batches,
            Flow::Nothing,
        ),
    };
    let key = format!("sha256:{}", "0".repeat(64));
    Declaration {
        name: "image".into(),
        role,
        summary: summary.into(),
        settings: settings_schema::<Settings>(
            "image",
            vec![json!({
                "image": {"build_key": key, "digest": null, "location": null},
                "entry": "scoring.functions:Score",
                "params": {"column": "text"},
                "needs": ["model"]
            })],
        ),
        inputs,
        outputs,
        needs: Needs::nothing(),
        deterministic: Fact::No,
        preserves_columns: Fact::No,
        isolation: Isolation::Confined,
        served: None,
        title: None,
        description: None,
        takes: None,
        gives: None,
        requires: Vec::new(),
        provenance: None,
    }
}

/// The environment the runner gives a task's process: the settings' `env`, then its entry, its
/// role, its params, the run's arguments and its port names.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn task_env(
    entry: &Entry<'_>,
    settings: &Settings,
    role: Role,
    ports: &[String],
    cx: &Context,
) -> Result<BTreeMap<String, String>> {
    let mut env = settings.env.clone().unwrap_or_default();
    if let Some(reserved) = env.keys().find(|k| k.starts_with("MORUNA_")) {
        return Err(entry.refuse(
            "env",
            format!(
                "`{reserved}` is the runner's to give: a name starting MORUNA_ is not a setting"
            ),
        ));
    }
    if let Some(name) = &settings.entry {
        env.insert(ENTRY_ENV.into(), name.clone());
    }
    env.insert(ROLE_ENV.into(), role.as_str().into());
    env.insert(
        PARAMS_ENV.into(),
        moruna_job::canonical_json(&Value::Object(settings.params.clone())),
    );
    env.insert(
        ARGS_ENV.into(),
        moruna_job::canonical_json(&Value::Object(cx.args().clone())),
    );
    if !ports.is_empty() {
        env.insert(
            moruna_sandbox::ports::PORTS_ENV.into(),
            moruna_job::canonical_json(&json!(ports)),
        );
    }
    Ok(env)
}

/// The routes of a task's ports: those its settings route, and each of its needs, which the
/// runner serves: the handler of that name the program running the document registered, else
/// the host protocol's peer.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn routes(entry: &Entry<'_>, settings: &Settings, cx: &Context) -> Result<BTreeMap<String, Route>> {
    let mut routes = match &settings.ports {
        Some(listed) => super::ports::routes(entry, listed, cx)?,
        None => BTreeMap::new(),
    };
    for need in &settings.needs {
        if !is_port_name(need) {
            return Err(entry.refuse(
                "needs",
                format!(
                    "{need:?} is not a need's name: 1 to 64 of ASCII letters, digits, `_`, `-`, \
                     `.` and `:`, starting with a letter or a digit"
                ),
            ));
        }
        if routes.contains_key(need) {
            return Err(entry.refuse(
                "needs",
                format!("`{need}` is listed in `ports` too; a name is routed once"),
            ));
        }
        let port = if cx.embedding().guest_port(need).is_some() {
            PortDoc {
                host: None,
                guest: Some(need.clone()),
                deadline_ms: None,
            }
        } else if cx.host_port_hub().is_some() {
            PortDoc {
                host: Some(need.clone()),
                guest: None,
                deadline_ms: None,
            }
        } else {
            return Err(entry.refuse(
                "needs",
                format!(
                    "`{need}` is a need the runner serves, and nothing serves it here: the \
                     program running the document has no handler of that name and there is no \
                     host peer"
                ),
            ));
        };
        let route = super::ports::route(&entry.at(&format!("needs.{need}")), &port, cx)?;
        routes.insert(need.clone(), route);
    }
    Ok(routes)
}

impl RunnableKind for Image {
    fn declaration(&self) -> Declaration {
        declaration_of(Role::Kernel)
    }

    fn stage(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<Stage> {
        let settings: Settings = entry.settings()?;
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if moruna_micro::on_kernel() {
            return on_kernel(entry, settings, cx);
        }
        sandboxed(entry, settings, cx)
    }
}

impl SourceKind for Image {
    fn declaration(&self) -> Declaration {
        declaration_of(Role::Source)
    }

    fn source(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSource> {
        let settings: Settings = entry.settings()?;
        process_task(entry, settings, cx, Role::Source).map(|built| match built {
            Built::Source(source) => source,
            Built::Sink(_) => unreachable!("a source is built as one"),
        })
    }
}

impl SinkKind for Image {
    fn declaration(&self) -> Declaration {
        declaration_of(Role::Sink)
    }

    fn sink(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSink> {
        let settings: Settings = entry.settings()?;
        process_task(entry, settings, cx, Role::Sink).map(|built| match built {
            Built::Sink(sink) => sink,
            Built::Source(_) => unreachable!("a sink is built as one"),
        })
    }
}

/// A source or a sink, built.
#[allow(dead_code, clippy::large_enum_variant)]
enum Built {
    Source(BuiltSource),
    Sink(BuiltSink),
}

/// The stage on the Moruna kernel (SDD 30 f.5, f.12): the command as a task of the data plane,
/// on the layers the launcher attached and the personality's image filesystem serves. Before
/// the task is made the entry's image is checked against them: the attached image's record
/// (its manifest, config and layers) must be the manifest the image's digest names, carrying
/// its build key, and the layers served must be the record's, or the stage is refused naming
/// both sides. The command and the environment are then the verified config's, amended by the
/// entry's as on a Linux node; its needs are ports its task calls by number, in name order.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn on_kernel(entry: &Entry<'_>, settings: Settings, cx: &mut Context) -> Result<Stage> {
    use std::sync::Arc;

    if settings.cache.is_some() {
        return Err(entry.refuse(
            "cache",
            "the Moruna kernel unpacks no image: the launcher attaches its layers",
        ));
    }
    let image = settings.image.as_ref();
    let Some((digest, _)) = image
        .check()
        .map_err(|e| entry.refuse("image", e.to_string()))?
    else {
        return Err(entry.refuse("image", "the image is not built: the document is a draft"));
    };
    let attached = moruna_micro::image::attached().map_err(|why| entry.refuse("image", why))?;
    let record = moruna_image::record::Record::parse(&attached.record)
        .map_err(|e| entry.refuse("image", e.to_string()))?;
    let verified = match &image.build_key {
        Some(key) => {
            let key = key
                .parse()
                .map_err(|e: moruna_image::Error| entry.refuse("image", e.to_string()))?;
            record.verify_name(&moruna_image::ImageName::Key(key), &attached.seals)
        }
        None => record.verify(&digest.to_string(), &attached.seals),
    }
    .map_err(|e| entry.refuse("image", e.to_string()))?;
    if verified.digest != digest {
        return Err(entry.refuse(
            "image",
            format!(
                "the document names the manifest {digest}, and the image attached is {}",
                verified.digest
            ),
        ));
    }
    let routes = routes(entry, &settings, cx)?;
    let names: Vec<String> = routes.keys().cloned().collect();
    let env = task_env(entry, &settings, Role::Kernel, &names, cx)?;
    let launch = moruna_image::Launch::new(
        &verified.digest.to_string(),
        &verified.config,
        settings.command.clone(),
        &env,
    )
    .map_err(|e| entry.refuse("command", e.to_string()))?;
    if (launch.uid, launch.gid) != (0, 0) {
        return Err(entry.refuse(
            "image",
            format!(
                "the image runs as uid {} and gid {}, and the Linux personality starts every \
                 program as root (SDD 28): it cannot run as its image says",
                launch.uid, launch.gid
            ),
        ));
    }
    if launch.working_dir != "/" {
        return Err(entry.refuse(
            "image",
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
    let env = launch.env_entries();
    let identity = json!({
        "image": verified.digest.to_string(), "path": path, "command": launch.command,
        "env": env, "ports": names,
    });
    let reference = verified.digest.to_string();
    let task = moruna_micro::task::ImageTask::new(
        &reference,
        &identity.to_string(),
        path,
        launch.command,
        env,
        names,
    )
    .map_err(|why| moruna_kernel::MorunaError::Config {
        name: "image",
        msg: format!("{reference}: {why}"),
    })?;
    cx.notes().push(format!(
        "image {}: verified on the Moruna kernel against the {} layers it attached",
        verified.digest,
        attached.seals.len(),
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

#[cfg(not(target_os = "linux"))]
fn process_task(
    _entry: &Entry<'_>,
    _settings: Settings,
    _cx: &mut Context,
    _role: Role,
) -> Result<Built> {
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

/// The task's image, found, checked against what the document names and unpacked; and the
/// sandbox's cgroup.
#[cfg(target_os = "linux")]
fn unpacked(
    entry: &Entry<'_>,
    settings: &Settings,
    cx: &mut Context,
) -> Result<(moruna_image::Image, std::path::PathBuf)> {
    let cgroup = cx.package_host().sandbox_cgroup.clone().ok_or_else(|| {
        moruna_kernel::MorunaError::Config {
            name: "sandbox",
            msg: format!(
                "{}: an image runs in the sandbox, which needs --sandbox-cgroup, a cgroup v2 \
                 directory with memory and cpu delegated (SDD 19 f.5)",
                entry.field()
            ),
        }
    })?;
    let cache = match &settings.cache {
        Some(cache) => std::path::PathBuf::from(cache),
        None => default_cache(entry, cx.sink_target())?,
    };
    let (layout, manifest) = settings
        .image
        .as_ref()
        .locate(cx.pulled_images())
        .map_err(|e| entry.refuse("image", e.to_string()))?;
    let image = moruna_image::unpack(&layout, &manifest.digest.to_string(), &cache)
        .map_err(|e| entry.refuse("image", e.to_string()))?;
    if image.skipped > 0 {
        cx.notes().push(format!(
            "image {}: {} device nodes or fifos in its layers are not in its root",
            manifest.digest, image.skipped
        ));
    }
    Ok((image, cgroup))
}

#[cfg(target_os = "linux")]
fn sandboxed(entry: &Entry<'_>, settings: Settings, cx: &mut Context) -> Result<Stage> {
    use std::sync::Arc;

    let (image, cgroup) = unpacked(entry, &settings, cx)?;
    let routes = routes(entry, &settings, cx)?;
    let names: Vec<String> = routes.keys().cloned().collect();
    let env = task_env(entry, &settings, Role::Kernel, &names, cx)?;
    let kernel = Arc::new(moruna_sandbox::ImageKernel::new(
        entry.field(),
        &image,
        settings.command,
        &env,
        cgroup,
        settings.plane,
    )?);
    let mut stage = Stage::of(kernel.clone());
    let bound = kernel.clone();
    stage.ports = Some(super::ports::StageCalls {
        routes,
        bind: Box::new(move |ports| bound.bind_ports(ports)),
    });
    stage.packaged = Some(crate::packaged::Packaged::Image(kernel));
    Ok(stage)
}

/// A source or a sink of an image on a Linux node: its process is started when the run builds
/// its part, limited to the run's memory ceiling and its cores, and its stream is read or
/// written as the run goes.
#[cfg(target_os = "linux")]
fn process_task(
    entry: &Entry<'_>,
    settings: Settings,
    cx: &mut Context,
    role: Role,
) -> Result<Built> {
    use std::sync::Arc;

    if settings.plane {
        return Err(entry.refuse(
            "plane",
            "a source or a sink of an image speaks Arrow IPC on its standard streams",
        ));
    }
    let (image, cgroup) = unpacked(entry, &settings, cx)?;
    let routes = routes(entry, &settings, cx)?;
    let names: Vec<String> = routes.keys().cloned().collect();
    let env = task_env(entry, &settings, role, &names, cx)?;
    let launch = moruna_image::Launch::new(entry.field(), &image.config, settings.command, &env)
        .map_err(|e| entry.refuse("command", e.to_string()))?;
    let task = entry
        .index()
        .ok_or_else(|| entry.refuse("image", "a task of a job has a place in it"))?;
    let ports = (!routes.is_empty()).then(|| cx.task_calls(task, routes));
    let name = entry.field().to_string();
    match role {
        Role::Source => Ok(Built::Source(BuiltSource {
            spec: crate::SourceSpec::Build(Box::new(move |ctx| {
                let cores = ctx.discovered.limits.cpu_quota.max(1.0);
                let started = moruna_sandbox::image::start_process(
                    &name,
                    &image,
                    &launch,
                    &cgroup,
                    (ctx.discovered.limits.memory_ceiling, cores),
                    ports,
                    false,
                )?;
                let stdout = started
                    .stdout
                    .ok_or_else(|| moruna_kernel::MorunaError::Config {
                        name: "image",
                        msg: format!("{name}: the process has no standard output"),
                    })?;
                let end = started.end;
                Ok(Arc::new(moruna_sources::ArrowStreamSource::new(
                    &name,
                    Box::new(stdout),
                    Box::new(move |fault| end.wait(fault).map(drop)),
                )?) as Arc<dyn moruna_kernel::Source>)
            })),
            targets: Vec::new(),
            memory: None,
        })),
        Role::Sink => Ok(Built::Sink(BuiltSink {
            spec: crate::SinkSpec::Build(Box::new(move |ctx| {
                let cores = ctx.discovered.limits.cpu_quota.max(1.0);
                let started = moruna_sandbox::image::start_process(
                    &name,
                    &image,
                    &launch,
                    &cgroup,
                    (ctx.discovered.limits.memory_ceiling, cores),
                    ports,
                    true,
                )?;
                let stdin = started
                    .stdin
                    .ok_or_else(|| moruna_kernel::MorunaError::Config {
                        name: "image",
                        msg: format!("{name}: the process has no standard input"),
                    })?;
                let end = started.end;
                Ok(Box::new(moruna_sinks::ArrowStreamSink::new(
                    &name,
                    Box::new(stdin),
                    Box::new(move || end.wait(None)),
                )) as Box<dyn moruna_kernel::Sink>)
            })),
            target: String::new(),
            memory: None,
        })),
        Role::Kernel => Err(entry.refuse("image", "a function is a stage, not a process task")),
    }
}
