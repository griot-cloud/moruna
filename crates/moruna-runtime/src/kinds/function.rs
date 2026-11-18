//! A registered function as a task, the kind `function` (system design 3E and 5C; item 3.6): a
//! function task names a function registered on the node, `{"function": NAME}`, with the
//! `params` it is given as `MORUNA_PARAMS` (its thresholds and options), and runs the file the
//! node fetched and verified for it, which `node-admission` gives the engine as a read-only
//! memory object ([`moruna_micro::functions`]). Its needs are the check's, not its settings'.
//!
//! What the file is, its content decides (PM, item 3.6):
//!
//! - an ELF is a native program of the kernel's user target, which the stage starts itself on
//!   the data plane's rings, the kernel loading it from the function's memory object;
//! - a Moruna image archive ([`moruna_image::archive`]) is an image: its layers are attached in
//!   place by the image filesystem as the task's root, and the personality runs its command,
//!   found in its own files, as an `image` task's, with the image's own config;
//! - anything else is refused, naming the function.
//!
//! The file is not hashed again: `node-admission` checked its SHA-256 against the registration.
//! Only a Moruna node gives the engine functions; anywhere else the stage is refused.

use std::collections::BTreeMap;

use moruna_image::Launch;
use moruna_image::archive::{self, Archive};
use moruna_kernel::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, Role, settings_schema};

use super::image::{ARGS_ENV, ENTRY_ENV, PARAMS_ENV, ROLE_ENV};
use super::{Context, Entry, RunnableKind, Stage};

/// The `function` kind: a function task.
pub struct Function;

/// The kind's name, which a job's task names as its kind (system design 3E): the job's own
/// [`moruna_job::FUNCTION_KIND`], by which the check finds the functions a job uses.
const KIND: &str = moruna_job::FUNCTION_KIND;

/// A `function` entry's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The function's registered name (`POST /v1/functions`): 1 to 64 of lowercase letters,
    /// digits, `_`, `-` and `.`, a letter first. The node refuses a name it has not registered.
    pub function: String,
    /// The function's params, data only, given to its process as `MORUNA_PARAMS`, as an
    /// `image` task's are, whether the function is native or an image.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
}

/// Why a function task is refused anywhere but on a Moruna node.
const ON_A_NODE_ONLY: &str = "a registered function runs on a Moruna node, whose admission gives \
                              the engine the function's file: this engine was given none";

/// The first bytes of an ELF file.
const ELF_MAGIC: &[u8] = b"\x7fELF";
/// The bytes of a program's head read to tell a native program from a Linux one: its ELF
/// header and its program headers (as `moruna_micro::task::program` reads).
const ELF_HEAD: usize = 64 + 64 * 56;

/// What a registered function's file is, by its content (PM, item 3.6).
#[derive(Debug)]
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
pub(crate) enum Content {
    /// A native program of the kernel's user target, checked where it lies.
    Native,
    /// A Moruna image archive, its index, its layers' headers and its image's identity checked
    /// ([`Archive::parse`]).
    Image(Box<Archive>),
}

/// What the file `bytes` of the function `name` is: an ELF must be a program of the kernel's
/// user target ([`moruna_micro::layout::native_elf`]), an archive must be one; anything else is
/// refused, naming the function.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
pub(crate) fn content(name: &str, bytes: &[u8]) -> std::result::Result<Content, String> {
    if bytes.starts_with(ELF_MAGIC) {
        moruna_micro::layout::native_elf(bytes).map_err(|why| {
            format!(
                "the function `{name}` is an ELF program, and not one of the Moruna kernel's \
                 user target, which is the only program a function's file may be: {why}"
            )
        })?;
        return Ok(Content::Native);
    }
    if archive::is_archive(bytes) {
        return Archive::parse(bytes)
            .map(|a| Content::Image(Box::new(a)))
            .map_err(|e| {
                format!("the function `{name}` is a Moruna image archive that is not one: {e}")
            });
    }
    let head: String = bytes.iter().take(8).map(|b| format!("{b:02x}")).collect();
    Err(format!(
        "the function `{name}` is neither a program of the Moruna kernel's user target (an \
         ELF) nor a Moruna image archive (MRNIMAG1): its file begins {}",
        if head.is_empty() {
            String::from("with nothing")
        } else {
            format!("0x{head}")
        }
    ))
}

/// An image function's process, as its archive says.
#[derive(Debug)]
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
pub(crate) struct ImageProgram {
    /// The image's manifest digest.
    pub digest: String,
    /// Its command, environment and identity, from its config, with the runner's environment.
    pub launch: Launch,
    /// The program its command names, found in the image's own files.
    pub path: String,
    /// Each layer's offset, length in pages and seal, lowest first: what the image filesystem
    /// attaches.
    pub layers: Vec<(u64, u64, [u8; 32])>,
}

/// The environment the runner gives a function's process, as an `image` task's: its role, its
/// params and the run's arguments. The image's own config gives its entry (`MORUNA_ENTRY`).
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
pub(crate) fn runner_env(
    params: &Map<String, Value>,
    args: &Map<String, Value>,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        (ROLE_ENV.to_string(), Role::Kernel.as_str().to_string()),
        (
            PARAMS_ENV.to_string(),
            moruna_job::canonical_json(&Value::Object(params.clone())),
        ),
        (
            ARGS_ENV.to_string(),
            moruna_job::canonical_json(&Value::Object(args.clone())),
        ),
    ])
}

/// The whole environment of the native function `name`'s process (item 5.5), as an image
/// function's process gets the same four variables: its entry, which is its registered name,
/// and the runner's ([`runner_env`]). Each `NAME=VALUE`, in name order; the kernel gives them as
/// the process's start slot `env`.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
pub(crate) fn native_env(
    name: &str,
    params: &Map<String, Value>,
    args: &Map<String, Value>,
) -> Vec<String> {
    let mut env = runner_env(params, args);
    env.insert(ENTRY_ENV.to_string(), name.to_string());
    env.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// The process of the image function `name` whose archive `archive` is the file `bytes`: its
/// config's command with `env` added, the program it names found in the image's own files (as
/// the personality will find it on that root), and refused as an `image` task's would be when
/// the personality cannot run it as the image says, or its program is a native one.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
pub(crate) fn image_program(
    name: &str,
    bytes: &[u8],
    archive: &Archive,
    env: &BTreeMap<String, String>,
) -> std::result::Result<ImageProgram, String> {
    let digest = archive.image.digest.to_string();
    let fail = |why: String| format!("the function `{name}` (the image {digest}): {why}");
    let launch =
        Launch::new(&digest, &archive.image.config, None, env).map_err(|e| fail(e.to_string()))?;
    if let Some(why) = super::image::personality_refusal(&launch) {
        return Err(fail(why));
    }
    let mut files = archive.files(bytes).map_err(|e| fail(e.to_string()))?;
    let candidates: Vec<String> = launch
        .candidates()
        .iter()
        .map(|c| {
            std::path::Path::new(&launch.working_dir)
                .join(c)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let path = candidates
        .iter()
        .find(|p| files.is_file(p))
        .cloned()
        .ok_or_else(|| {
            fail(format!(
                "`{}` is not in the image: tried {}",
                launch.command[0],
                candidates.join(", ")
            ))
        })?;
    let head = files
        .head(&path, ELF_HEAD)
        .map_err(|e| fail(e.to_string()))?;
    if moruna_micro::layout::native_elf(&head).is_ok() {
        return Err(fail(format!(
            "`{path}` is a program of the Moruna kernel's user target, which the Linux \
             personality does not run: register the program itself as the function"
        )));
    }
    let layers = archive
        .layers
        .iter()
        .map(|l| (l.span.offset, l.span.pages, l.seal))
        .collect();
    Ok(ImageProgram {
        digest,
        launch,
        path,
        layers,
    })
}

impl RunnableKind for Function {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: KIND.into(),
            role: Role::Kernel,
            summary: "A function registered on the node, by its name: a native program of the \
                      Moruna kernel's user target on the data plane's rings, or a Moruna image \
                      archive whose command runs on its own files, its batches as Arrow IPC"
                .into(),
            settings: settings_schema::<Settings>(
                KIND,
                vec![
                    json!({"function": "taxi.clean"}),
                    json!({"function": "taxi.score", "params": {"threshold": 0.5}}),
                ],
            ),
            inputs: Flow::Batches,
            outputs: Flow::Batches,
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

    fn stage(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<Stage> {
        let settings: Settings = entry.settings()?;
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if moruna_micro::on_kernel() {
            return on_kernel(entry, settings, cx);
        }
        let _ = (settings, cx);
        Err(entry.refuse("function", ON_A_NODE_ONLY))
    }
}

/// The stage on a Moruna node: the function's file, as its content says, as a task of the
/// data plane.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn on_kernel(entry: &Entry<'_>, settings: Settings, cx: &mut Context) -> Result<Stage> {
    use std::sync::Arc;

    use moruna_micro::functions;
    use moruna_micro::task::{ImageTask, Root};

    let name = &settings.function;
    let function = functions::function(name).map_err(|why| entry.refuse("function", why))?;
    let sha256 = functions::hex(&function.entry);
    let reference = format!("{name}@sha256:{sha256}");
    let made = |why: String| moruna_kernel::MorunaError::Config {
        name: "function",
        msg: format!("{reference}: {why}"),
    };
    let (task, what) =
        match content(name, function.bytes()).map_err(|why| entry.refuse("function", why))? {
            Content::Native => {
                let env = native_env(name, &settings.params, cx.args());
                let identity = json!({
                    "function": name, "sha256": sha256, "abi": "moruna", "env": env,
                });
                let task = ImageTask::new(
                    &reference,
                    &identity.to_string(),
                    format!("/{}", function.slot),
                    vec![name.clone()],
                    env,
                    Vec::new(),
                )
                .map_err(made)?
                .with_native_function(Arc::clone(&function));
                (task, String::from("a native program"))
            }
            Content::Image(archive) => {
                let env = runner_env(&settings.params, cx.args());
                let program = image_program(name, function.bytes(), &archive, &env)
                    .map_err(|why| entry.refuse("function", why))?;
                let env = program.launch.env_entries();
                let identity = json!({
                    "function": name, "sha256": sha256, "image": program.digest,
                    "path": program.path, "command": program.launch.command, "env": env,
                });
                let layers = program.layers.len();
                let task = ImageTask::new(
                    &reference,
                    &identity.to_string(),
                    program.path,
                    program.launch.command,
                    env,
                    Vec::new(),
                )
                .map_err(made)?
                .with_root(Root::new(Arc::clone(&function), program.layers));
                (
                    task,
                    format!("the image {} of {layers} layers", program.digest),
                )
            }
        };
    cx.notes().push(format!(
        "function {name}: {what}, sha256:{sha256}, {} bytes, as the node gave it",
        function.entry.size
    ));
    let task = Arc::new(task);
    let mut stage = Stage::of(task.clone());
    let bound = task.clone();
    // A function task calls no port: its stage knows it has none (MH 4.10).
    stage.ports = Some(super::ports::StageCalls {
        routes: BTreeMap::new(),
        bind: Box::new(move |ports| bound.bind_ports(ports)),
    });
    stage.packaged = Some(crate::packaged::Packaged::Task(task));
    Ok(stage)
}

#[cfg(test)]
#[path = "function_tests.rs"]
mod tests;
