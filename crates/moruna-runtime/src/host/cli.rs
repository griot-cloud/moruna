//! The `moruna` command line (MH 4.2, 4.8.4, SDD 18 d.3, SDD 22 d.3, SDD 26 d.5): `run`,
//! `run --vm`, `serve`, `kinds`, `check-job`, `explain`, `--version`. The binary is a thin `main` over [`main`] with the registry of the kinds it can
//! run: the Python adapter's binary adds the Python kinds to [`Registry::standard`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use moruna_kernel::CancelToken;

use super::events::EventOptions;
use super::session::{self, HEARTBEAT, HostOptions, Outcome};
use super::transport::Address;
use super::{VERSION, exit};
use crate::job::build::process_env;
use crate::kinds::Registry;
use crate::packaged::PackageHost;

/// What `moruna` prints for a command line it does not understand.
pub const USAGE: &str = "usage: moruna run <spec.json> [--arg NAME=VALUE]... [--resolver MODULE:ATTRIBUTE] [--plan <plan.mplan>] [--strict] [--events-json] [--cost-store <dir>] [--cost-model <name>] [--sandbox-python <path>] [--sandbox-cgroup <dir>] [--kinds <kinds.json>]...
       moruna run <spec.json> --output <dir> [--report <file>] [--input NAME=URL]... [--memory-gib N | --memory BYTES] [--scratch-gib N] [--node-kit <dir>] [--image-stack <dir>] [--launch-witness <dir>] [--vcpu-cpus <list>]
       moruna run --vm <spec.json> [--image <node-image.json>] [the options above]
       moruna serve --listen <unix:///path | vsock://CID:PORT> [--no-strict] [--cost-store <dir>] [--cost-model <name>] [--sandbox-python <path>] [--sandbox-cgroup <dir>] [--kinds <kinds.json>]...
       moruna kinds [--kinds <kinds.json>]...
       moruna check-job <spec.json> [--kinds <kinds.json>]...
       moruna explain <spec.json> [--planner <name>] [--out <plan.mplan>] [--kinds <kinds.json>]...
       moruna --version

--kinds names a file of the kinds the program running Moruna serves: a JSON array of
declarations, each with `isolation: embedder` and `served` (SDD 18 e.4).";

/// The commands that take `--kinds`: every one that reads or runs a document, and `kinds`.
const TAKES_KINDS: [&str; 5] = ["run", "serve", "kinds", "check-job", "explain"];

/// `--kinds <file>` taken out of a command line, wherever it stands: the files, in order, and
/// the rest of the line. A command other than [`TAKES_KINDS`], and `run --vm`, whose guest names
/// its own kinds, are refused it.
pub fn split_kinds(args: &[String]) -> Result<(Vec<PathBuf>, Vec<String>), String> {
    let mut files = Vec::new();
    let mut rest = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--kinds" {
            let path = it
                .next()
                .ok_or_else(|| "moruna: --kinds needs a file of declarations".to_string())?;
            files.push(PathBuf::from(path));
        } else {
            rest.push(arg.clone());
        }
    }
    if files.is_empty() {
        return Ok((files, rest));
    }
    let command = rest.first().map(String::as_str).unwrap_or("");
    if !TAKES_KINDS.contains(&command) {
        return Err(format!(
            "moruna: --kinds is taken by {}, and not by `{command}`",
            TAKES_KINDS.join(", ")
        ));
    }
    if command == "run" && rest.iter().any(|a| a == "--vm") {
        return Err(
            "moruna run --vm: the guest's `moruna serve` is given its own kinds; drop --kinds"
                .to_string(),
        );
    }
    Ok((files, rest))
}

/// `kinds` with every served kind the files declare (SDD 18 KD-I11), or why one could not be:
/// a file not there, not a JSON array of declarations, or a declaration `Registry::declare`
/// refuses, each naming the file.
pub fn with_declared(kinds: &Registry, files: &[PathBuf]) -> Result<Registry, String> {
    let mut extended = kinds.clone();
    for file in files {
        let named = |why: String| format!("moruna: --kinds {}: {why}", file.display());
        let text = std::fs::read_to_string(file).map_err(|e| named(e.to_string()))?;
        let declarations: Vec<moruna_job::Declaration> = serde_json::from_str(&text)
            .map_err(|e| named(format!("not a JSON array of declarations: {e}")))?;
        for declaration in declarations {
            extended
                .declare(declaration)
                .map_err(|e| named(e.to_string()))?;
        }
    }
    Ok(extended)
}

/// What a run in a VM is given beside its document (stream D, `moruna_vmm::data`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VmData {
    /// `--input NAME=URL`: the host file or remote object behind a job's `disk://NAME`.
    pub inputs: Vec<(String, String)>,
    /// `--output DIR`: where the run's output layer is unpacked, a new or empty directory.
    pub output: Option<PathBuf>,
    /// `--vcpu-cpus LIST`: the host CPU of each vCPU, vCPU 0 first (`moruna_vmm::placement`).
    pub vcpu_cpus: Vec<u32>,
    /// `--report FILE`: the run's report as JSON, the VM's figures beside the engine's report.
    pub report: Option<PathBuf>,
    /// `--memory BYTES` or `--memory-gib N`: the VM's memory, for a node built from the kit.
    pub memory_bytes: Option<u64>,
    /// `--scratch-gib N`: the engine's scratch, and the room for outputs on the output disk
    /// (stream C hook, D's file: the sinks write there directly).
    pub scratch_bytes: Option<u64>,
    /// `--node-kit DIR`: what a node is built from (`kernel/node/HOST.md`).
    pub node_kit: Option<PathBuf>,
    /// `--image-stack DIR`: the image layers the node attaches, in place of the kit's own.
    pub image_stack: Option<PathBuf>,
}

/// A parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `moruna run <spec.json> [--plan <plan.mplan>] [--strict] [--events-json]`.
    Run {
        /// The document.
        spec: String,
        /// The plan to run it by (SDD 26); absent: the default planner's.
        plan: Option<PathBuf>,
        /// Refuse any field the environment would fill.
        strict: bool,
        /// What a `.pex` package's sandbox needs (17 e.6).
        packages: PackageHost,
        /// Print every event of the run to standard output as a line of JSON, rendered from its
        /// record (MH 4.3 version 7).
        events_json: bool,
        /// The run's arguments, `--arg NAME=VALUE` each, read by the types the job declares.
        args: Vec<(String, String)>,
    },
    /// `moruna run --vm <spec.json> --image <node-image.json> --launch-witness <durable-directory> [--plan <plan>]`:
    /// boot the checked native node, send the document/plan, relay actual events,
    /// and return its explicit job result. The guest's `moruna serve` is strict, so there is no `--strict` here.
    RunVm {
        /// The document.
        spec: String,
        /// Exact checked native node-image.json descriptor; directories and OCI
        /// layouts are not interpreted as microkernel images. Absent: a node is built for the
        /// run from the node kit.
        image: Option<PathBuf>,
        /// Existing durable run-claim directory; never a temporary default. Absent: the
        /// user's state directory's `moruna/launches`.
        witness: Option<PathBuf>,
        /// Optional exact plan delivered through the canonical Spec message.
        plan: Option<PathBuf>,
        /// The run's data disks and vCPU placement.
        data: VmData,
    },
    /// `moruna serve --listen <address> [--no-strict]`; strict unless told otherwise, because
    /// serve is the hosted path (MH 4.1).
    Serve {
        /// Where to wait for the peer.
        listen: Address,
        /// Refuse any field the environment would fill.
        strict: bool,
        /// What a `.pex` package's sandbox needs (17 e.6).
        packages: PackageHost,
    },
    /// Read actual governed historical rows through the host's scoped native reader.
    TaskOutput {
        /// Exact immutable manifest metadata reference, never a live source query.
        manifest: PathBuf,
        /// One complete Arrow stream, or None to print the manifest facts.
        chunk: Option<usize>,
        /// New destination file for a requested Arrow stream.
        out: Option<PathBuf>,
        /// Explicit maximum retained-read capacity.
        max_bytes: Option<u64>,
    },
    /// `moruna kinds` (SDD 18 e.3): every kind this binary can run, its declaration, as JSON.
    Kinds,
    /// `moruna check-job <spec.json>` (SDD 22 d.3): read a document and check it against the
    /// kinds this binary has, without running it.
    CheckJob {
        /// The document.
        spec: String,
    },
    /// `moruna explain <spec.json> [--planner <name>] [--out <plan.mplan>]` (SDD 26 d.5): the
    /// document's plan, as JSON on standard output, and as the plan document at `out`.
    Explain {
        /// The document.
        spec: String,
        /// The planner, by name; absent: the default.
        planner: Option<String>,
        /// Where the plan document goes.
        out: Option<PathBuf>,
    },
    /// `moruna --version`.
    Version,
    /// `moruna --help`.
    Help,
}

/// Parse the arguments after the program name. The error is the line to print before
/// [`USAGE`].
pub fn parse(args: &[String]) -> Result<Command, String> {
    let (args, _) = cost_options(args)?;
    parse_command(&args)
}

fn parse_command(args: &[String]) -> Result<Command, String> {
    let mut rest = args.iter().map(String::as_str);
    match rest.next() {
        Some("run") => {
            let mut spec = None;
            let mut plan = None;
            let mut strict = false;
            let mut vm = false;
            let mut events_json = false;
            let mut image = None;
            let mut witness = None;
            let mut packages = PackageHost::default();
            let mut args: Vec<(String, String)> = Vec::new();
            let mut data = VmData::default();
            let mut vcpus = false;
            while let Some(arg) = rest.next() {
                if sandbox_option(arg, &mut rest, &mut packages, "moruna run")? {
                    continue;
                }
                match arg {
                    "--arg" => {
                        let given = rest
                            .next()
                            .ok_or_else(|| "moruna run: --arg needs NAME=VALUE".to_string())?;
                        let (name, value) = given.split_once('=').ok_or_else(|| {
                            format!("moruna run: --arg {given}: an argument is NAME=VALUE")
                        })?;
                        if args.iter().any(|(n, _)| n == name) {
                            return Err(format!("moruna run: --arg {name} is given twice"));
                        }
                        args.push((name.to_string(), value.to_string()));
                    }
                    "--strict" => strict = true,
                    "--events-json" => events_json = true,
                    "--vm" => vm = true,
                    "--plan" => {
                        let path = rest
                            .next()
                            .ok_or_else(|| "moruna run: --plan needs a path".to_string())?;
                        plan = Some(PathBuf::from(path));
                    }
                    "--image" => {
                        let path = rest
                            .next()
                            .ok_or_else(|| "moruna run: --image needs a path".to_string())?;
                        image = Some(PathBuf::from(path));
                    }
                    "--launch-witness" => {
                        let path = rest.next().ok_or("--launch-witness needs a directory")?;
                        if witness.replace(PathBuf::from(path)).is_some() {
                            return Err("duplicate --launch-witness".into());
                        }
                    }
                    "--input" => {
                        let given = rest
                            .next()
                            .ok_or("moruna run --vm: --input needs NAME=URL")?;
                        let (name, url) = super::inputs::parse_binding(given)
                            .map_err(|e| format!("moruna run --vm: {e}"))?;
                        if data.inputs.iter().any(|(n, _)| *n == name) {
                            return Err(format!("moruna run --vm: input {name} is bound twice"));
                        }
                        data.inputs.push((name, url));
                    }
                    "--output" => {
                        let path = rest
                            .next()
                            .ok_or("moruna run --vm: --output needs a directory")?;
                        if data.output.replace(PathBuf::from(path)).is_some() {
                            return Err("moruna run --vm: --output is given twice".into());
                        }
                    }
                    "--report" => {
                        let path = rest.next().ok_or("moruna run: --report needs a file")?;
                        if data.report.replace(PathBuf::from(path)).is_some() {
                            return Err("moruna run: --report is given twice".into());
                        }
                    }
                    "--memory" | "--memory-gib" | "--scratch-gib" => {
                        let value = rest
                            .next()
                            .ok_or_else(|| format!("moruna run: {arg} needs a number"))?;
                        let bytes = if arg == "--memory" {
                            value.parse::<u64>().ok()
                        } else {
                            value
                                .parse::<f64>()
                                .ok()
                                .filter(|g| g.is_finite() && *g > 0.0 && *g < 1024.0)
                                .map(|g| (g * f64::from(1u32 << 30)) as u64)
                        }
                        .filter(|b| *b > 0)
                        .ok_or_else(|| format!("moruna run: {arg} {value} is not a size"))?;
                        let slot = if arg == "--scratch-gib" {
                            &mut data.scratch_bytes
                        } else {
                            &mut data.memory_bytes
                        };
                        if slot.replace(bytes).is_some() {
                            return Err(format!("moruna run: {arg} is given twice"));
                        }
                    }
                    "--node-kit" | "--image-stack" => {
                        let path = rest
                            .next()
                            .ok_or_else(|| format!("moruna run: {arg} needs a directory"))?;
                        let slot = if arg == "--node-kit" {
                            &mut data.node_kit
                        } else {
                            &mut data.image_stack
                        };
                        if slot.replace(PathBuf::from(path)).is_some() {
                            return Err(format!("moruna run: {arg} is given twice"));
                        }
                    }
                    "--vcpu-cpus" => {
                        let list = rest
                            .next()
                            .ok_or("moruna run --vm: --vcpu-cpus needs a CPU list")?;
                        if vcpus {
                            return Err("moruna run --vm: --vcpu-cpus is given twice".into());
                        }
                        vcpus = true;
                        data.vcpu_cpus = moruna_vmm::placement::parse_cpu_list(list)
                            .map_err(|e| format!("moruna run --vm: --vcpu-cpus: {e}"))?;
                    }
                    "--disk" => return Err(
                        "moruna run --vm: --disk is retired; node-image.json defines exact devices"
                            .into(),
                    ),
                    flag if flag.starts_with("--") => {
                        return Err(format!("moruna run: unknown option `{flag}`"));
                    }
                    path if spec.is_none() => spec = Some(path.to_string()),
                    extra => {
                        return Err(format!(
                            "moruna run: one document, and `{extra}` is a second"
                        ));
                    }
                }
            }
            let spec = spec.ok_or_else(|| "moruna run: which document?".to_string())?;
            // A run that names its output directory runs in a VM (stream D): a run on this host
            // writes where its sink says.
            let vm = vm || data.output.is_some();
            if !vm {
                if image.is_some() || witness.is_some() || data != VmData::default() || vcpus {
                    return Err(
                        "moruna run: --image, --launch-witness, --input, --report, --memory, \
                         --node-kit and --vcpu-cpus describe a guest, and need --vm or --output"
                            .to_string(),
                    );
                }
                return Ok(Command::Run {
                    spec,
                    plan,
                    strict,
                    packages,
                    events_json,
                    args,
                });
            }
            if events_json {
                return Err(
                    "moruna run --vm: the guest's events are printed as JSON already; drop \
                     --events-json"
                        .to_string(),
                );
            }
            if strict {
                return Err(
                    "moruna run --vm: the guest's `moruna serve` is always strict; drop --strict"
                        .to_string(),
                );
            }
            if packages != PackageHost::default() {
                return Err(
                    "moruna run --vm: the guest names its own sandbox; drop --sandbox-python and \
                     --sandbox-cgroup"
                        .to_string(),
                );
            }
            if image.is_some()
                && (data.memory_bytes.is_some()
                    || data.scratch_bytes.is_some()
                    || data.node_kit.is_some()
                    || data.image_stack.is_some())
            {
                return Err(
                    "moruna run --vm: --memory, --scratch-gib, --node-kit and --image-stack size \
                     and build a node from the kit; --image names one already built"
                        .to_string(),
                );
            }
            Ok(Command::RunVm {
                spec,
                image,
                plan,
                witness,
                data,
            })
        }
        Some("serve") => {
            let mut listen = None;
            let mut strict = true;
            let mut packages = PackageHost::default();
            while let Some(arg) = rest.next() {
                if sandbox_option(arg, &mut rest, &mut packages, "moruna serve")? {
                    continue;
                }
                match arg {
                    "--listen" => {
                        let text = rest
                            .next()
                            .ok_or_else(|| "moruna serve: --listen needs an address".to_string())?;
                        listen =
                            Some(Address::parse(text).map_err(|e| format!("moruna serve: {e}"))?);
                    }
                    "--no-strict" => strict = false,
                    "--strict" => strict = true,
                    other => return Err(format!("moruna serve: unknown argument `{other}`")),
                }
            }
            let listen = listen.ok_or_else(|| "moruna serve: --listen is required".to_string())?;
            Ok(Command::Serve {
                listen,
                strict,
                packages,
            })
        }
        Some("task-output") => {
            let manifest = rest
                .next()
                .filter(|value| !value.starts_with("--"))
                .ok_or_else(|| "moruna task-output: which pinned manifest reference?".to_owned())?;
            let mut chunk = None;
            let mut out = None;
            let mut max_bytes = None;
            while let Some(arg) = rest.next() {
                let value = rest
                    .next()
                    .ok_or_else(|| format!("moruna task-output: {arg} needs a value"))?;
                match arg {
                    "--chunk" if chunk.is_none() => {
                        chunk = Some(value.parse::<usize>().map_err(|_| {
                            "moruna task-output: chunk is a nonnegative index".to_owned()
                        })?)
                    }
                    "--out" if out.is_none() => out = Some(PathBuf::from(value)),
                    "--max-bytes" if max_bytes.is_none() => {
                        max_bytes = Some(value.parse::<u64>().map_err(|_| {
                            "moruna task-output: max-bytes is a positive byte budget".to_owned()
                        })?)
                    }
                    _ => {
                        return Err(format!(
                            "moruna task-output: unknown or repeated option {arg}"
                        ));
                    }
                }
            }
            if (chunk.is_some() && (out.is_none() || max_bytes.is_none_or(|bytes| bytes == 0)))
                || (chunk.is_none() && (out.is_some() || max_bytes.is_some()))
            {
                return Err(
                    "moruna task-output: a chunk requires --out and positive --max-bytes".into(),
                );
            }
            Ok(Command::TaskOutput {
                manifest: PathBuf::from(manifest),
                chunk,
                out,
                max_bytes,
            })
        }
        Some("kinds") => match rest.next() {
            None => Ok(Command::Kinds),
            Some(extra) => Err(format!(
                "moruna kinds: takes no arguments, and was given `{extra}`"
            )),
        },
        Some("check-job") => {
            let spec = rest
                .next()
                .ok_or_else(|| "moruna check-job: which document?".to_string())?;
            match rest.next() {
                None => Ok(Command::CheckJob {
                    spec: spec.to_string(),
                }),
                Some(extra) => Err(format!(
                    "moruna check-job: one document, and `{extra}` is a second"
                )),
            }
        }
        Some("explain") => {
            let mut spec = None;
            let mut planner = None;
            let mut out = None;
            while let Some(arg) = rest.next() {
                match arg {
                    "--planner" => {
                        let name = rest
                            .next()
                            .ok_or_else(|| "moruna explain: --planner needs a name".to_string())?;
                        planner = Some(name.to_string());
                    }
                    "--out" => {
                        let path = rest
                            .next()
                            .ok_or_else(|| "moruna explain: --out needs a path".to_string())?;
                        out = Some(PathBuf::from(path));
                    }
                    flag if flag.starts_with("--") => {
                        return Err(format!("moruna explain: unknown option `{flag}`"));
                    }
                    path if spec.is_none() => spec = Some(path.to_string()),
                    extra => {
                        return Err(format!(
                            "moruna explain: one document, and `{extra}` is a second"
                        ));
                    }
                }
            }
            let spec = spec.ok_or_else(|| "moruna explain: which document?".to_string())?;
            Ok(Command::Explain { spec, planner, out })
        }
        Some("--version" | "-V" | "version") => Ok(Command::Version),
        Some("--help" | "-h" | "help") => Ok(Command::Help),
        Some(other) => Err(format!("moruna: unknown command `{other}`")),
        None => Err("moruna: which command?".to_string()),
    }
}

/// `--sandbox-python <path>` and `--sandbox-cgroup <dir>` (17 e.6): true when `arg` was one.
fn sandbox_option<'a>(
    arg: &str,
    rest: &mut impl Iterator<Item = &'a str>,
    packages: &mut PackageHost,
    command: &str,
) -> Result<bool, String> {
    let slot = match arg {
        "--sandbox-python" => &mut packages.sandbox_python,
        "--sandbox-cgroup" => &mut packages.sandbox_cgroup,
        _ => return Ok(false),
    };
    let value = rest
        .next()
        .ok_or_else(|| format!("{command}: {arg} needs a path"))?;
    *slot = Some(PathBuf::from(value));
    Ok(true)
}

/// Strip the cost configuration once at the surface; the same settings open registration's
/// observation store for live scheduling. No model/store alias or environment fallback.
fn cost_options(
    args: &[String],
) -> Result<(Vec<String>, Option<crate::cost::CostSettings>), String> {
    let mut clean = Vec::new();
    let mut store = None;
    let mut model = None;
    let mut at = 0;
    while at < args.len() {
        let slot = match args[at].as_str() {
            "--cost-store" => Some(&mut store),
            "--cost-model" => Some(&mut model),
            _ => None,
        };
        if let Some(slot) = slot {
            if slot.is_some() {
                return Err(format!("{} is repeated", args[at]));
            }
            at += 1;
            let value = args
                .get(at)
                .filter(|value| !value.is_empty() && !value.starts_with("--"))
                .ok_or_else(|| {
                    "cost options require a nonempty store path or model name".to_owned()
                })?;
            *slot = Some(value.clone());
        } else {
            clean.push(args[at].clone());
        }
        at += 1;
    }
    if store.is_none() && model.is_none() {
        return Ok((clean, None));
    }
    if !matches!(clean.first().map(String::as_str), Some("run" | "serve"))
        || clean.iter().any(|arg| arg == "--vm")
    {
        return Err("cost options configure native run/serve; a VM guest requires its own explicit configuration".into());
    }
    let directory = store
        .map(PathBuf::from)
        .or_else(crate::cost::default_store_dir)
        .ok_or_else(|| {
            "--cost-store is required when no default observation directory exists".to_owned()
        })?;
    let mut settings = crate::cost::CostSettings::directory(directory);
    if let Some(model) = model {
        settings.model = model;
    }
    Ok((clean, Some(settings)))
}

/// Run a command line and return the process's exit code (MH 4.2). `cancel` is the surface's
/// (a signal handler's) way in; a `cancel` message from the peer uses the same token.
pub fn main(args: &[String], kinds: &Registry, cancel: CancelToken) -> i32 {
    // stream E hook (D's file): SIGINT and SIGTERM cancel the run, as a peer's `cancel`
    // does. Before this neither was handled: started in the background, `moruna run`
    // inherited SIGINT ignored, and a SIGTERM or SIGKILL left its VM running (2026-11-17). On
    // the Moruna kernel the engine is cancelled through its host, not by signals.
    #[cfg(unix)]
    if !moruna_micro::on_kernel() {
        signals::cancel_on(&cancel);
    }
    main_as(args, kinds, cancel, &process_env, HEARTBEAT, true)
}

/// stream E hook (D's file): the process's SIGINT and SIGTERM as a cancel.
#[cfg(unix)]
mod signals {
    use std::sync::atomic::{AtomicBool, Ordering};

    use moruna_kernel::CancelToken;

    static SIGNALLED: AtomicBool = AtomicBool::new(false);

    /// The handler: it only stores to an atomic, which is async-signal-safe.
    extern "C" fn on_signal(_: libc::c_int) {
        SIGNALLED.store(true, Ordering::Release);
    }

    /// Handle SIGINT and SIGTERM (over an inherited "ignore" too) and cancel `cancel` when
    /// either comes, from a thread that waits for it.
    pub(super) fn cancel_on(cancel: &CancelToken) {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: a zeroed sigaction with an empty mask and a handler that only stores to
            // an atomic; sigaction copies it.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
        let cancel = cancel.clone();
        let _ = std::thread::Builder::new()
            .name("moruna-signals".into())
            .spawn(move || {
                while !SIGNALLED.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                eprintln!("moruna: a signal asked the run to stop: cancelling it");
                cancel.cancel();
            });
    }

    #[cfg(test)]
    pub(super) fn raised() -> bool {
        SIGNALLED.load(Ordering::Acquire)
    }
}

/// stream E hook (D's file): how long a cancelled VM run's guest has to stop and report before
/// the run is given up on and its QEMU killed.
pub const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// [`main`] with the environment and the heartbeat cadence supplied, for tests: the test's
/// process is not the run's alone, so its own output is left where it goes.
pub fn main_with(
    args: &[String],
    kinds: &Registry,
    cancel: CancelToken,
    env: &dyn Fn(&str) -> Option<String>,
    heartbeat: Duration,
) -> i32 {
    main_as(args, kinds, cancel, env, heartbeat, false)
}

fn main_as(
    args: &[String],
    kinds: &Registry,
    cancel: CancelToken,
    env: &dyn Fn(&str) -> Option<String>,
    heartbeat: Duration,
    capture_process: bool,
) -> i32 {
    let (files, args) = match split_kinds(args) {
        Ok(split) => split,
        Err(line) => {
            eprintln!("{line}\n{USAGE}");
            return exit::SPEC_REFUSED;
        }
    };
    let (args, cost) = match cost_options(&args) {
        Ok(configured) => configured,
        Err(reason) => {
            eprintln!("{reason}\n{USAGE}");
            return exit::SPEC_REFUSED;
        }
    };
    let command = match parse_command(&args) {
        Ok(command) => command,
        Err(line) => {
            eprintln!("{line}\n{USAGE}");
            return exit::SPEC_REFUSED;
        }
    };
    // SDD 18 KD-I11: the kinds the program running Moruna serves, given for this invocation.
    let declared;
    let kinds = if files.is_empty() {
        kinds
    } else {
        match with_declared(kinds, &files) {
            Ok(extended) => {
                declared = extended;
                &declared
            }
            Err(line) => {
                eprintln!("{line}");
                return exit::SPEC_REFUSED;
            }
        }
    };
    let outcome = match command {
        Command::Version => {
            println!("moruna {VERSION}");
            return exit::COMPLETED;
        }
        Command::Help => {
            println!("{USAGE}");
            return exit::COMPLETED;
        }
        Command::TaskOutput {
            manifest,
            chunk,
            out,
            max_bytes,
        } => {
            match task_output(
                kinds,
                &manifest,
                chunk,
                out.as_deref(),
                max_bytes,
                &mut std::io::stdout().lock(),
            ) {
                Ok(()) => return exit::COMPLETED,
                Err(reason) => {
                    eprintln!("moruna task-output: {reason}");
                    return exit::SPEC_REFUSED;
                }
            }
        }
        Command::Kinds => {
            println!("{}", kinds.listing().to_json());
            return exit::COMPLETED;
        }
        Command::CheckJob { spec } => {
            return check_job(Path::new(&spec), kinds, &mut std::io::stderr().lock());
        }
        Command::Explain { spec, planner, out } => {
            return explain(
                Path::new(&spec),
                planner.as_deref(),
                out.as_deref(),
                kinds,
                &mut std::io::stdout().lock(),
                &mut std::io::stderr().lock(),
            );
        }
        Command::RunVm {
            spec,
            image,
            plan,
            witness,
            data,
        } => {
            return run_vm(
                Path::new(&spec),
                image.as_deref(),
                plan.as_deref(),
                witness.as_deref(),
                &data,
                cancel,
            );
        }
        Command::Run {
            spec,
            plan,
            strict,
            packages,
            events_json,
            args,
        } => {
            let events = if events_json {
                match events_stream() {
                    Ok(json) => EventOptions {
                        ring: None,
                        json: Some(json),
                    },
                    Err(error) => {
                        eprintln!(
                            "moruna run: --events-json: standard output cannot be written to: {error}"
                        );
                        return exit::FAILED;
                    }
                }
            } else {
                EventOptions::default()
            };
            session::run_file(
                Path::new(&spec),
                plan.as_deref(),
                kinds,
                &HostOptions {
                    cost: cost.clone(),
                    strict,
                    env,
                    cancel,
                    heartbeat,
                    packages,
                    capture_process,
                    events,
                    args,
                    pulled_images: pulled_images(),
                },
            )
        }
        Command::Serve {
            listen,
            strict,
            packages,
        } => session::serve(
            &listen,
            kinds,
            &HostOptions {
                cost: cost.clone(),
                strict,
                env,
                cancel,
                heartbeat,
                packages,
                capture_process,
                events: EventOptions::default(),
                args: Vec::new(),
                pulled_images: pulled_images(),
            },
        ),
    };
    eprintln!("{}", summary(&outcome));
    outcome.code
}

/// A handle on the process's standard output that stays its own while the run captures
/// descriptor 1 as the engine's output: the stream `--events-json` prints to.
fn events_stream() -> std::io::Result<std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>> {
    use std::os::fd::AsFd;
    let own = std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    Ok(std::sync::Arc::new(std::sync::Mutex::new(Box::new(own))))
}

/// `moruna check-job` (SDD 22 d.3): the document at `spec` against the kinds of `kinds`, every
/// refusal written to `out` as `<field>: <message>`. Exits 0 when there is none, with the exit
/// code of a refused document (2) when there is one or the document cannot be read, and 1 when
/// the kinds themselves cannot be checked against.
pub fn check_job(spec: &Path, kinds: &Registry, out: &mut dyn Write) -> i32 {
    let read = std::fs::read(spec)
        .map_err(|e| moruna_job::SpecError::new("spec", format!("{}: {e}", spec.display())))
        .and_then(|bytes| super::session::read_document(&bytes).map_err(|(_, refused)| refused));
    let job = match read {
        Ok(job) => job,
        Err(refused) => {
            let _ = writeln!(out, "{refused}");
            return exit::SPEC_REFUSED;
        }
    };
    let catalogue = match moruna_job::Catalogue::from_listing(kinds.listing()) {
        Ok(catalogue) => catalogue,
        Err(e) => {
            let _ = writeln!(
                out,
                "moruna check-job: the kinds of this binary cannot be checked against: {e}"
            );
            return exit::FAILED;
        }
    };
    match moruna_job::check(&job, &catalogue) {
        Ok(()) => exit::COMPLETED,
        Err(refusals) => {
            for refusal in &refusals {
                let _ = writeln!(out, "{refusal}");
            }
            exit::SPEC_REFUSED
        }
    }
}

/// `moruna explain` (SDD 26 d.5): the plan of the document at `spec` by the planner `planner`
/// names (the default when it names none), for the run's resources (the document's budget, what
/// discovery finds where it leaves one to discovery), the document checked against the kinds of
/// `kinds` first. The plan's JSON goes to `out`, and with `plan_file`, the plan document is
/// written there for `moruna run --plan`. Exits 0 with a plan; 2 when the document, its kinds or
/// the planner's name is refused, each refusal written to `err`; 1 when the kinds cannot be
/// checked against, a planner fails, or the plan cannot be written.
pub fn explain(
    spec: &Path,
    planner: Option<&str>,
    plan_file: Option<&Path>,
    kinds: &Registry,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let read = std::fs::read(spec)
        .map_err(|e| moruna_job::SpecError::new("spec", format!("{}: {e}", spec.display())))
        .and_then(|bytes| super::session::read_document(&bytes).map_err(|(_, refused)| refused));
    let job = match read {
        Ok(job) => job,
        Err(refused) => {
            let _ = writeln!(err, "{refused}");
            return exit::SPEC_REFUSED;
        }
    };
    let plan = match crate::job::plan::explain(&job, planner, kinds) {
        Ok(plan) => plan,
        Err(crate::job::plan::ExplainError::Plan(moruna_plan::PlanError::Job(refusals))) => {
            for refusal in &refusals {
                let _ = writeln!(err, "{refusal}");
            }
            return exit::SPEC_REFUSED;
        }
        Err(
            e @ crate::job::plan::ExplainError::Plan(moruna_plan::PlanError::UnknownPlanner {
                ..
            }),
        ) => {
            let _ = writeln!(err, "moruna explain: {e}");
            return exit::SPEC_REFUSED;
        }
        Err(e) => {
            let _ = writeln!(err, "moruna explain: {e}");
            return exit::FAILED;
        }
    };
    if let Some(path) = plan_file
        && let Err(e) = std::fs::write(path, plan.to_flatbuffer())
    {
        let _ = writeln!(
            err,
            "moruna explain: the plan could not be written to {}: {e}",
            path.display()
        );
        return exit::FAILED;
    }
    let _ = writeln!(out, "{}", plan.to_json());
    exit::COMPLETED
}

#[cfg(target_os = "linux")]
struct StopWatcher<'a>(&'a std::sync::atomic::AtomicBool);
#[cfg(target_os = "linux")]
impl Drop for StopWatcher<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// The run's private host directory for its staged inputs and its output disk, removed with
/// everything in it when the run ends, whatever the end (contract C7: nothing kept between runs).
#[cfg(target_os = "linux")]
struct Staging(PathBuf);
#[cfg(target_os = "linux")]
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How long a VM run took, part by part, in milliseconds: the line `moruna run --vm` prints on
/// standard error (`arrow-core-0.7.md` §9: VM boot reported on its own).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmTiming {
    /// Inputs fetched to the host.
    pub stage_ms: u64,
    /// Artifacts checked and copied, disks opened, QEMU spawned and placed.
    pub launch_ms: u64,
    /// QEMU spawned to the guest's runtime saying hello: the boot.
    pub boot_ms: u64,
    /// Hello to the job's exit.
    pub job_ms: u64,
    /// The VM stopped and its outputs unpacked.
    pub outputs_ms: u64,
}

impl VmTiming {
    /// The line printed: `moruna run --vm: timing {...}`.
    pub fn line(&self) -> String {
        format!(
            "moruna run --vm: timing {{\"stage_ms\":{},\"launch_ms\":{},\"boot_ms\":{},\
             \"job_ms\":{},\"outputs_ms\":{}}}",
            self.stage_ms, self.launch_ms, self.boot_ms, self.job_ms, self.outputs_ms
        )
    }
}

/// The run's output directory, refused before anything boots when it cannot take the outputs:
/// named exactly when the image has an output disk, and new or empty.
pub fn check_output(declared: u64, output: Option<&Path>) -> Result<(), String> {
    match (declared > 0, output) {
        (true, None) => Err(
            "the image writes the job's outputs to an output disk; name the directory they go \
             to with --output DIR"
                .into(),
        ),
        (false, Some(_)) => Err("--output: the image has no output disk".into()),
        (false, None) => Ok(()),
        (true, Some(dir)) => match std::fs::symlink_metadata(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(m) if m.is_dir() => match std::fs::read_dir(dir).map(|mut d| d.next().is_none()) {
                Ok(true) => Ok(()),
                Ok(false) => Err(format!("--output {}: not empty", dir.display())),
                Err(e) => Err(format!("--output {}: {e}", dir.display())),
            },
            Ok(_) => Err(format!("--output {}: not a directory", dir.display())),
            Err(e) => Err(format!("--output {}: {e}", dir.display())),
        },
    }
}

/// The engine's scratch a node built from the kit gets when the run names none, and the room for
/// outputs on its output disk (stream C hook, D's file: the sinks write there directly).
pub const SCRATCH_DEFAULT: u64 = 1 << 30;

// stream C hook (D's file): the output disk holds the layer as the sinks write it.
/// The output disk of a node built from the kit, for `data` bytes of outputs (the scratch's
/// size; the outputs no longer pass through it): the data, the 16 MiB the layer keeps before it
/// for its tables (`moruna_micro::blk::out::RESERVE`), its hash tree (a page per 127 pages of
/// what is before it) and a margin, in whole pages.
pub fn output_disk_for(data: u64) -> u64 {
    const RESERVE: u64 = 16 << 20;
    const MARGIN: u64 = 64 << 20;
    let content = data.next_multiple_of(4096) + RESERVE;
    (content + content.div_ceil(4096 * 127) * 4096 + MARGIN).div_ceil(4096) * 4096
}

/// The node kit a run builds its node from: `--node-kit`, else `MORUNA_NODE_KIT`, else
/// `share/moruna/node-kit` beside the executable's `bin` (`kernel/node/HOST.md`).
pub fn node_kit(
    given: Option<&Path>,
    env: Option<std::ffi::OsString>,
    exe: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(kit) = given {
        return Ok(kit.to_path_buf());
    }
    if let Some(kit) = env.filter(|k| !k.is_empty()) {
        return Ok(PathBuf::from(kit));
    }
    if let Some(kit) = exe
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|prefix| prefix.join("share/moruna/node-kit"))
        .filter(|kit| kit.is_dir())
    {
        return Ok(kit);
    }
    Err(
        "no node to boot: name one with --image, or a node kit with --node-kit, \
         MORUNA_NODE_KIT or share/moruna/node-kit beside the executable's bin"
            .into(),
    )
}

/// Where a run's launch claims are kept when `--launch-witness` names nowhere: the user's state
/// directory's `moruna/launches` (`$XDG_STATE_HOME`, else `~/.local/state`), made private.
pub fn default_witness(
    state: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, String> {
    let base = match (
        state.filter(|s| !s.is_empty()),
        home.filter(|h| !h.is_empty()),
    ) {
        (Some(state), _) => PathBuf::from(state),
        (None, Some(home)) => PathBuf::from(home).join(".local/state"),
        (None, None) => return Err("no --launch-witness, and no HOME for its default".into()),
    };
    let dir = base.join("moruna/launches");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// What a node built from a kit is: its CPUs, its memory (exact when asked), the engine's
/// scratch, its data disks and the image layers it attaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeOrder {
    /// vCPUs, the job's.
    pub cpus: u32,
    /// The VM's memory; absent: the job's, its scratch and the node's services.
    pub memory_bytes: Option<u64>,
    /// The engine's scratch.
    pub scratch_bytes: u64,
    /// The inputs' names, in device order.
    pub inputs: Vec<String>,
    /// The image layers in place of the kit's own.
    pub image_stack: Option<PathBuf>,
}

impl NodeOrder {
    /// The arguments of `node-for-job.py` for this order.
    pub fn args(&self, kit: &Path, job: &Path, out: &Path) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = vec![
            "-I".into(),
            kit.join("node/tools/node-for-job.py").into(),
            "--build".into(),
            kit.into(),
            "--job".into(),
            job.into(),
            "--out".into(),
            out.into(),
            "--cpus".into(),
            self.cpus.to_string().into(),
            "--scratch".into(),
            self.scratch_bytes.to_string().into(),
            "--output-bytes".into(),
            output_disk_for(self.scratch_bytes).to_string().into(),
        ];
        for name in &self.inputs {
            args.push("--input".into());
            args.push(name.into());
        }
        if let Some(memory) = self.memory_bytes {
            args.push("--node-memory".into());
            args.push(
                memory
                    .div_ceil(2 << 20)
                    .saturating_mul(2 << 20)
                    .to_string()
                    .into(),
            );
        }
        if let Some(stack) = &self.image_stack {
            args.push("--image-stack".into());
            args.push(stack.into());
        }
        args
    }
}

/// Build the node one run boots from the kit (`kernel/node/tools/node-for-job.py`, started by
/// exec, no shell) in `out`: the path of its `node-image.json`.
pub fn build_node(
    kit: &Path,
    job: &Path,
    out: &Path,
    order: &NodeOrder,
) -> Result<PathBuf, String> {
    if !kit.join("node/tools/node-for-job.py").is_file() {
        return Err(format!(
            "{} is not a node kit: it has no node/tools/node-for-job.py",
            kit.display()
        ));
    }
    let done = std::process::Command::new("python3")
        .args(order.args(kit, job, out))
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("python3, to build the node: {e}"))?;
    if !done.status.success() {
        let log = std::fs::read(out.join("prepare.log")).unwrap_or_default();
        let tail =
            |b: &[u8]| String::from_utf8_lossy(&b[b.len().saturating_sub(2048)..]).into_owned();
        return Err(format!(
            "building the node failed ({}): {}{}",
            done.status,
            tail(&done.stderr).trim(),
            tail(&log).trim()
        ));
    }
    let printed = String::from_utf8_lossy(&done.stdout);
    let path = printed
        .lines()
        .last()
        .map(|l| PathBuf::from(l.trim()))
        .filter(|p| p.is_file())
        .ok_or_else(|| format!("the node builder named no descriptor: {printed}"))?;
    Ok(path)
}

/// The most of each node log a run that did not complete keeps.
pub const DIAGNOSTIC_BYTES: u64 = 1 << 20;

/// Copy the last [`DIAGNOSTIC_BYTES`] of the node's logs in `from` (`serial.log`, `qemu.log`,
/// `backend.log`) into `to`, a new private directory.
pub fn keep_diagnostics(from: &Path, to: &Path) -> Result<(), String> {
    use std::io::{Read as _, Seek as _};
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(to)
        .map_err(|e| format!("{}: {e}", to.display()))?;
    for name in ["serial.log", "qemu.log", "backend.log"] {
        let Ok(mut file) = std::fs::File::open(from.join(name)) else {
            continue;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = file.seek(std::io::SeekFrom::Start(
            len.saturating_sub(DIAGNOSTIC_BYTES),
        ));
        let mut tail = Vec::new();
        file.take(DIAGNOSTIC_BYTES)
            .read_to_end(&mut tail)
            .map_err(|e| format!("{name}: {e}"))?;
        std::fs::write(to.join(name), tail).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

/// stream E hook (D's file): the lines of a node's console that say why it never answered, the
/// last four of them: a critical service that ended or panicked, an account or quota refused, a
/// process that failed. `None` when the console names nothing.
pub fn console_cause(serial: &str) -> Option<String> {
    const SIGNS: [&str; 8] = [
        "critical service",
        "panicked",
        "account",
        "Quota",
        "quota",
        "refused",
        "failed",
        "exited with code",
    ];
    let lines: Vec<&str> = serial
        .lines()
        .map(str::trim)
        .filter(|l| SIGNS.iter().any(|s| l.contains(s)) && !l.contains("exited with code 0"))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(lines[lines.len().saturating_sub(4)..].join("; "))
}

/// stream E hook (D's file): a node that never answered its runtime keeps its logs, as a run
/// that did not complete keeps them (in `keep`, the run's `MORUNA_KEEP_NODE_LOGS`, when given), and
/// its error says what its console says. Before this a job too big for its node ended with
/// "native service connect deadline" and nothing kept.
pub fn connect_failure(
    error: moruna_vmm::error::VmmError,
    runtime_dir: &Path,
    keep: Option<std::ffi::OsString>,
) -> moruna_vmm::error::VmmError {
    use moruna_vmm::error::VmmError;
    let kept = match keep {
        Some(keep) => PathBuf::from(keep),
        None => {
            let name = runtime_dir
                .file_name()
                .map(|n| {
                    n.to_string_lossy()
                        .trim_start_matches("moruna-node-")
                        .to_string()
                })
                .unwrap_or_default();
            std::env::temp_dir().join(format!("moruna-diagnostics-{name}"))
        }
    };
    match keep_diagnostics(runtime_dir, &kept) {
        Ok(()) => eprintln!(
            "moruna run --vm: the node's logs are kept in {}",
            kept.display()
        ),
        Err(why) => eprintln!("moruna run --vm: the node's logs were not kept: {why}"),
    }
    let serial = std::fs::read(kept.join("serial.log")).unwrap_or_default();
    let Some(cause) = console_cause(&String::from_utf8_lossy(&serial)) else {
        return error;
    };
    match error {
        VmmError::Control(msg) => VmmError::Control(format!("{msg}; the node's console: {cause}")),
        VmmError::Io { op, target, msg } => VmmError::Io {
            op,
            target,
            msg: format!("{msg}; the node's console: {cause}"),
        },
        other => other,
    }
}

/// stream E hook (D's file): a run that was cancelled and did not complete exits
/// [`exit::CANCELLED`], its diagnostic saying so; any other run keeps its code.
pub fn cancelled_code(code: i32, cancelled: bool, diagnostic: &mut Option<String>) -> i32 {
    if !cancelled || code == exit::COMPLETED || code == exit::CANCELLED {
        return code;
    }
    let said = diagnostic.take();
    *diagnostic = Some(match said {
        Some(said) => format!("cancelled: {said}"),
        None => "cancelled".to_string(),
    });
    exit::CANCELLED
}

/// The status a run's exit code says, as the engine's report words it.
pub fn status_of(code: i32) -> &'static str {
    match code {
        exit::COMPLETED => "completed",
        exit::CANCELLED => "cancelled",
        exit::SPEC_REFUSED | exit::BUDGET_REFUSED | exit::RESUME_REFUSED => "refused",
        _ => "failed",
    }
}

/// What a VM run's report file holds: the engine's own report, every field as it wrote it, its
/// `exit` (an object) kept as `engine_exit`; then `exit` (the run's status, `completed` when it
/// completed), `code`, `diagnostic`, `version`, and `vm`, the VM's figures in seconds, its boot
/// apart (`arrow-core-0.7.md` §9), its memory and CPUs, its inputs and its outputs.
pub fn vm_report(
    engine: Option<serde_json::Value>,
    code: i32,
    diagnostic: Option<String>,
    vm: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let mut report = match engine {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    let engine_exit = report.remove("exit");
    let status = match engine_exit
        .as_ref()
        .and_then(|e| e.get("status"))
        .and_then(serde_json::Value::as_str)
    {
        _ if code == exit::COMPLETED => "completed".to_string(),
        Some(said) => said.to_string(),
        None => status_of(code).to_string(),
    };
    if let Some(e) = engine_exit {
        report.insert("engine_exit".into(), e);
    }
    report.insert("exit".into(), status.into());
    report.insert("code".into(), code.into());
    report.insert("diagnostic".into(), diagnostic.into());
    report
        .entry("version")
        .or_insert_with(|| serde_json::Value::String(super::VERSION.to_string()));
    report.insert("vm".into(), serde_json::Value::Object(vm));
    serde_json::Value::Object(report)
}

/// Write `report` to `path` whole: a sibling written, then renamed over it.
pub fn write_report(path: &Path, report: &serde_json::Value) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Moruna 1.0 boots only the checked native microkernel artifact descriptor: `--image`, or a
/// node built for the run from the node kit. The document is adapted to the VM (a `run_id`, its
/// local inputs as data disks, its local sink as the VM's output root; `inputs::adapt`), its
/// inputs are attached as read-only data disks, its outputs read back from its output disk into
/// the output directory, and its report written with the VM's figures (stream D).
#[cfg(target_os = "linux")]
fn run_vm(
    spec: &Path,
    image: Option<&Path>,
    plan: Option<&Path>,
    witness: Option<&Path>,
    data: &VmData,
    cancel: CancelToken,
) -> i32 {
    use moruna_vmm::error::VmmError;
    let begun = std::time::Instant::now();
    let mut timing = VmTiming::default();
    let mut output_disk: Option<PathBuf> = None;
    let mut output_dir: Option<PathBuf> = data.output.clone();
    let mut staging: Option<Staging> = None;
    let mut engine_report: Option<serde_json::Value> = None;
    let mut diagnostic: Option<String> = None;
    let mut facts = serde_json::Map::new();
    let result = (|| -> moruna_vmm::error::Result<moruna_vmm::ExitStatus> {
        use std::io::Read as _;
        let read_bounded = |path: &Path, limit: usize| -> moruna_vmm::error::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .map_err(|e| VmmError::io("open", path.display().to_string(), &e))?
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| VmmError::io("read", path.display().to_string(), &e))?;
            if bytes.len() > limit {
                return Err(VmmError::config(
                    "spec",
                    format!("{} exceeds its {limit} bytes", path.display()),
                ));
            }
            Ok(bytes)
        };
        let raw = read_bounded(spec, super::protocol::MAX_SPEC_LINE)?;
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|e| VmmError::config("run directory", e.to_string()))?;
        let name: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        // The run's own host directory: its document as sent, its node, its fetched inputs and
        // its output disk, gone with the run.
        let stage_dir = std::env::temp_dir().join(format!("moruna-data-{name}"));
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
            .create(&stage_dir)
            .map_err(|e| VmmError::io("create", stage_dir.display().to_string(), &e))?;
        let stage_dir = staging.insert(Staging(stage_dir)).0.clone();
        let sidecar_path = super::inputs::sidecar_of(spec);
        let sidecar = match sidecar_path.is_file() {
            true => Some(read_bounded(&sidecar_path, 1 << 20)?),
            false => None,
        };
        let adapted = super::inputs::adapt(&raw, sidecar.as_deref(), || {
            crate::run::mint_run_id()
                .map(|id| id.to_hex())
                .unwrap_or_else(|_| name.clone())
        })
        .map_err(|e| VmmError::config("spec", e))?;
        if output_dir.is_none() {
            output_dir = adapted.output.clone();
        }
        let bytes = adapted.bytes;
        if bytes.len() > super::protocol::MAX_SPEC_LINE {
            return Err(VmmError::config(
                "spec",
                "exceeds canonical protocol ceiling",
            ));
        }
        let job_path = stage_dir.join("job.json");
        std::fs::write(&job_path, &bytes)
            .map_err(|e| VmmError::io("write", job_path.display().to_string(), &e))?;
        let document = super::session::read_document(&bytes)
            .map_err(|(_, e)| VmmError::config("spec", e.to_string()))?;
        let budget = vm_budget(&document).map_err(|e| VmmError::config("budget", e.to_string()))?;
        let mut bindings = adapted.bindings;
        for (name, url) in &data.inputs {
            if bindings.iter().any(|(n, _)| n == name) {
                return Err(VmmError::config(
                    "input",
                    format!("input {name} is bound twice"),
                ));
            }
            bindings.push((name.clone(), url.clone()));
        }
        facts.insert(
            "inputs".into(),
            bindings
                .iter()
                .map(|(n, u)| (n.clone(), serde_json::Value::String(u.clone())))
                .collect::<serde_json::Map<_, _>>()
                .into(),
        );
        // The node: the one named, or one built for this run from the kit.
        let building = std::time::Instant::now();
        let descriptor = match image {
            Some(image) => image.to_path_buf(),
            None => {
                let kit = node_kit(
                    data.node_kit.as_deref(),
                    std::env::var_os("MORUNA_NODE_KIT"),
                    std::env::current_exe().ok().as_deref(),
                )
                .map_err(|e| VmmError::config("node", e))?;
                let order = NodeOrder {
                    cpus: budget.cpus,
                    memory_bytes: data.memory_bytes,
                    scratch_bytes: data.scratch_bytes.unwrap_or(SCRATCH_DEFAULT),
                    inputs: bindings.iter().map(|(n, _)| n.clone()).collect(),
                    image_stack: data.image_stack.clone(),
                };
                build_node(&kit, &job_path, &stage_dir.join("node"), &order)
                    .map_err(|e| VmmError::config("node", e))?
            }
        };
        facts.insert("node_s".into(), building.elapsed().as_secs_f64().into());
        // What the node declares for data disks, checked against the job and the command line
        // before anything is fetched or booted.
        let artifacts = moruna_vmm::microkernel::inspect(&descriptor)?;
        facts.insert(
            "memory_bytes".into(),
            artifacts.resources.node_memory_bytes.into(),
        );
        facts.insert("cpus".into(), artifacts.resources.cpus.into());
        let declared = artifacts.data_disks;
        let used = super::inputs::names_used(&document).map_err(|e| VmmError::config("spec", e))?;
        super::inputs::check_used(&used, &declared.inputs)
            .map_err(|e| VmmError::config("spec", e))?;
        check_output(declared.output_bytes, output_dir.as_deref())
            .map_err(|e| VmmError::config("output", e))?;
        let directory = std::env::temp_dir().join(format!("moruna-node-{name}"));
        let executable = std::env::current_exe()
            .map_err(|e| VmmError::io("resolve", "runtime executable", &e))?;
        let backend = executable
            .parent()
            .ok_or_else(|| VmmError::config("backend", "executable parent unavailable"))?
            .join("moruna-vhost-vsock");
        let plan = plan
            .map(|path| {
                use base64::Engine as _;
                let metadata =
                    std::fs::metadata(path).map_err(|e| VmmError::io("metadata", "plan", &e))?;
                if metadata.len() > super::protocol::MAX_SPEC_LINE as u64 {
                    return Err(VmmError::config(
                        "plan",
                        "exceeds canonical control ceiling",
                    ));
                }
                let bytes = std::fs::read(path).map_err(|e| VmmError::io("read", "plan", &e))?;
                super::session::read_plan(&bytes)
                    .map_err(|e| VmmError::config("plan", e.to_string()))?;
                Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
            })
            .transpose()?;
        let spec = moruna_vmm::session::spec_value(&bytes)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(24 * 60 * 60);
        let assigned = document
            .id
            .as_deref()
            .and_then(moruna_kernel::RunId::from_hex)
            .ok_or_else(|| {
                VmmError::config("run", "native admission requires canonical document id")
            })?;
        let witness = match witness {
            Some(w) => w.to_path_buf(),
            None => default_witness(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
                .map_err(|e| VmmError::config("launch witness", e))?,
        };
        let fetching = std::time::Instant::now();
        let inputs = super::inputs::stage(&bindings, &stage_dir, &document.object_store)
            .map_err(|e| VmmError::config("input", e))?;
        timing.stage_ms = fetching.elapsed().as_millis() as u64;
        let devices = moruna_vmm::data::RunDevices {
            inputs,
            output: (declared.output_bytes > 0).then(|| stage_dir.join("output.disk")),
            vcpu_cpus: data.vcpu_cpus.clone(),
        };
        output_disk = devices.output.clone();
        // A build with the explicit TCG proof (tests only) boots without KVM when asked; a
        // production build has no such start.
        #[cfg(feature = "native-tcg-proof")]
        let tcg = std::env::var_os("MORUNA_TCG_PROOF").is_some_and(|v| v == "1");
        #[cfg(not(feature = "native-tcg-proof"))]
        let tcg = false;
        if tcg {
            eprintln!("moruna run --vm: the explicit TCG proof boot (tests only): no KVM figure");
            facts.insert("accel".into(), "tcg-proof".into());
        }
        let start = match tcg {
            #[cfg(feature = "native-tcg-proof")]
            true => super::native::NativeRun::start_tcg_proof_with,
            _ => super::native::NativeRun::start_with,
        };
        let mut run = start(
            super::launch_witness::Admission {
                run_id: assigned,
                node_boot: moruna_kernel::RunId(nonce),
                attempt: 0,
                directory: &witness,
            },
            &descriptor,
            budget,
            &directory,
            super::native::Tools {
                qemu: Path::new("qemu-system-x86_64"),
                vsock: &backend,
                // `moruna run --vm` is not told of a delegated cgroup; its VM keeps its fixed
                // memory and CPUs, and its jail's namespaces.
                cgroup: &moruna_vmm::jail::Cgroup::Undelegated,
            },
            super::protocol::Inbound::Spec { spec, plan },
            deadline,
            &devices,
        )?;
        let launched = run.timing();
        timing.launch_ms = launched.launch.as_millis() as u64;
        if let Some(placed) = run.placed() {
            facts.insert(
                "vcpu_cpus".into(),
                placed
                    .vcpus
                    .iter()
                    .map(|(_, _, cpu)| serde_json::Value::from(*cpu))
                    .collect::<Vec<_>>()
                    .into(),
            );
            for (vcpu, tid, cpu) in &placed.vcpus {
                eprintln!(
                    "moruna run --vm: vCPU {vcpu} (host thread {tid}) runs on host CPU {cpu}"
                );
            }
            eprintln!(
                "moruna run --vm: {} other threads of the run kept off those CPUs",
                placed.others
            );
        }
        let spawned = run.launched_at();
        let control = run.control();
        let done = std::sync::atomic::AtomicBool::new(false);
        let result = std::thread::scope(|scope| {
            let _stop_watcher = StopWatcher(&done);
            let watcher = scope.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    if cancel.is_cancelled() {
                        let _ = control.send(&super::protocol::Inbound::Cancel);
                        // stream E hook (D's file): the guest has CANCEL_GRACE to stop; a
                        // guest that has not is given up on, and the node shut down below.
                        let until = std::time::Instant::now() + CANCEL_GRACE;
                        while !done.load(std::sync::atomic::Ordering::Acquire)
                            && std::time::Instant::now() < until
                        {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        if !done.load(std::sync::atomic::Ordering::Acquire) {
                            eprintln!(
                                "moruna run --vm: the guest did not stop within {} s of the \
                                 cancel: its VM is shut down",
                                CANCEL_GRACE.as_secs()
                            );
                            control.abandon();
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            let outcome = (|| {
                let mut messages = 0usize;
                let mut hello: Option<std::time::Instant> = None;
                loop {
                    let message = run.next(deadline)?;
                    let message = match message {
                        super::native::Message::Control(message) => message,
                        super::native::Message::Event(event) => {
                            std::io::stdout()
                                .write_all(event.json_line().as_bytes())
                                .map_err(|e| VmmError::io("write", "guest event output", &e))?;
                            messages += 1;
                            continue;
                        }
                    };
                    if hello.is_none() {
                        let now = std::time::Instant::now();
                        timing.boot_ms = now.saturating_duration_since(spawned).as_millis() as u64;
                        hello = Some(now);
                    }
                    std::io::stdout()
                        .write_all(message.line().as_bytes())
                        .map_err(|e| VmmError::io("write", "guest output", &e))?;
                    messages += 1;
                    if let super::protocol::Outbound::Report { report, .. } = &message {
                        engine_report = Some(report.clone());
                    }
                    match message {
                        super::protocol::Outbound::PortCall { v, call, port, .. } => {
                            if v != super::protocol::PORTS_VERSION {
                                return Err(VmmError::config(
                                    "port",
                                    "unsupported guest port version",
                                ));
                            }
                            control.send(&super::protocol::Inbound::PortReply {
                                v,
                                call,
                                ok: None,
                                error: Some(format!(
                                    "host port {port} is not configured by this CLI"
                                )),
                            })?;
                        }
                        super::protocol::Outbound::Exit {
                            code,
                            diagnostic: said,
                        } => {
                            diagnostic = said;
                            timing.job_ms = hello.map_or(0, |h| h.elapsed().as_millis() as u64);
                            return Ok(moruna_vmm::ExitStatus { code, messages });
                        }
                        _ => {}
                    }
                }
            })();
            done.store(true, std::sync::atomic::Ordering::Release);
            let _ = watcher.join();
            outcome
        });
        // A run that did not complete keeps its node's logs (the guest's serial console, QEMU's
        // and the vsock back end's), since the run's directory goes with the node.
        // The gate keeps every run's node logs beside its results (stream E): a directory in
        // MORUNA_KEEP_NODE_LOGS gets them whatever the run's end.
        if let Some(keep) = std::env::var_os("MORUNA_KEEP_NODE_LOGS") {
            if let Err(why) = keep_diagnostics(run.runtime_dir(), std::path::Path::new(&keep)) {
                eprintln!("moruna run --vm: the node's logs were not kept: {why}");
            }
        } else if !matches!(result, Ok(ref s) if s.code == exit::COMPLETED) {
            let kept = std::env::temp_dir().join(format!("moruna-diagnostics-{name}"));
            match keep_diagnostics(run.runtime_dir(), &kept) {
                Ok(()) => eprintln!(
                    "moruna run --vm: the node's logs are kept in {}",
                    kept.display()
                ),
                Err(why) => eprintln!("moruna run --vm: the node's logs were not kept: {why}"),
            }
        }
        let stopping = std::time::Instant::now();
        let cleanup = run.shutdown();
        timing.outputs_ms = stopping.elapsed().as_millis() as u64;
        let _ = std::fs::remove_dir_all(&directory);
        result.and_then(|status| cleanup.map(|()| status))
    })();
    let code = match result {
        Ok(status) => status.code,
        Err(error) => {
            eprintln!("moruna run --vm: {error}");
            diagnostic.get_or_insert_with(|| error.to_string());
            error.exit_code()
        }
    };
    // stream E hook (D's file): a cancelled run that did not complete ends cancelled, whether
    // its guest stopped or was given up on.
    let code = cancelled_code(code, cancel.is_cancelled(), &mut diagnostic);
    // C3: a completed run's outputs, from its output disk into the job's output directory.
    let code = match (code, &output_disk, &output_dir) {
        (exit::COMPLETED, Some(disk), Some(out)) => {
            let unpacking = std::time::Instant::now();
            let unpacked = super::outputs::unpack(disk, out);
            timing.outputs_ms += unpacking.elapsed().as_millis() as u64;
            match unpacked {
                Ok(done) => {
                    facts.insert(
                        "outputs".into(),
                        serde_json::json!({"directory": out, "files": done.files,
                            "directories": done.directories, "bytes": done.bytes,
                            "links_skipped": done.links_skipped}),
                    );
                    eprintln!(
                        "moruna run --vm: outputs in {}: {} files, {} directories, {} bytes{}",
                        out.display(),
                        done.files,
                        done.directories,
                        done.bytes,
                        if done.links_skipped > 0 {
                            format!(", {} symbolic links left out", done.links_skipped)
                        } else {
                            String::new()
                        }
                    );
                    code
                }
                Err(why) => {
                    eprintln!("moruna run --vm: the outputs were not read back: {why}");
                    diagnostic = Some(format!("the outputs were not read back: {why}"));
                    exit::FAILED
                }
            }
        }
        _ => code,
    };
    drop(staging);
    if timing != VmTiming::default() {
        eprintln!("{}", timing.line());
    }
    if let Some(path) = &data.report {
        let seconds = |ms: u64| serde_json::Value::from(ms as f64 / 1000.0);
        for (key, ms) in [
            ("stage_s", timing.stage_ms),
            ("launch_s", timing.launch_ms),
            ("boot_s", timing.boot_ms),
            ("job_s", timing.job_ms),
            ("outputs_s", timing.outputs_ms),
        ] {
            facts.insert(key.into(), seconds(ms));
        }
        facts.insert("total_s".into(), begun.elapsed().as_secs_f64().into());
        let report = vm_report(engine_report, code, diagnostic, facts);
        if let Err(why) = write_report(path, &report) {
            eprintln!("moruna run --vm: the report was not written: {why}");
            return if code == exit::COMPLETED {
                exit::FAILED
            } else {
                code
            };
        }
    }
    code
}

/// Native historical read adapter used by the CLI. SDK/MCP embedders call `ReadPort` directly;
/// scope/version/hash/encoding decisions stay entirely in the sole governed storage reader.
pub fn task_output(
    kinds: &Registry,
    manifest: &Path,
    chunk: Option<usize>,
    out: Option<&Path>,
    max_bytes: Option<u64>,
    printed: &mut dyn Write,
) -> Result<(), String> {
    let reader = kinds
        .embedding()
        .historical_reader()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "this host has no authorized historical output reader".to_owned())?;
    if std::fs::metadata(manifest)
        .map_err(|error| error.to_string())?
        .len()
        > 64 * 1024
    {
        return Err("pinned manifest reference exceeds its metadata budget".into());
    }
    let bytes = std::fs::read(manifest).map_err(|error| error.to_string())?;
    let pinned: moruna_storage::Metadata = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid pinned manifest reference: {error}"))?;
    let runtime =
        moruna_reactor::configure_runtime(&mut tokio::runtime::Builder::new_current_thread())
            .build()
            .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let manifest = reader
            .manifest(&pinned)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(index) = chunk {
            let budget = max_bytes
                .filter(|bytes| *bytes > 0)
                .ok_or_else(|| "an explicit read byte budget is required".to_owned())?;
            let path = out.ok_or_else(|| "an output file is required".to_owned())?;
            let bytes = reader
                .chunk(&manifest, index, budget)
                .await
                .map_err(|error| error.to_string())?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|error| error.to_string())?;
            file.write_all(bytes.as_ref())
                .map_err(|error| error.to_string())?;
        } else {
            serde_json::to_writer_pretty(&mut *printed, &manifest)
                .map_err(|error| error.to_string())?;
            writeln!(printed).map_err(|error| error.to_string())?;
        }
        Ok(())
    })
}

/// `moruna run --vm` where there is no KVM: refused by name, before anything is read.
#[cfg(not(target_os = "linux"))]
fn run_vm(
    _spec: &Path,
    _image: Option<&Path>,
    _plan: Option<&Path>,
    _witness: Option<&Path>,
    _data: &VmData,
    _cancel: CancelToken,
) -> i32 {
    eprintln!(
        "moruna run --vm: the microVM runs on KVM, which is Linux-only, and this is {}",
        std::env::consts::OS
    );
    exit::FAILED
}

/// Requested job limits from the document, with fractional CPU rounded upward.
/// Native admission checks these against the descriptor's fixed engine contract;
/// physical node RAM/CPUs are distinct and never inferred by changing QEMU -m.
/// Missing explicit job memory/CPU is refused before launch.
pub fn vm_budget(spec: &moruna_job::Job) -> Result<moruna_vmm::Budget, moruna_job::SpecError> {
    use moruna_job::SpecError;
    let memory = spec.budget.memory_bytes.ok_or_else(|| {
        SpecError::new(
            "budget.memory_bytes",
            "a guest is booted with the memory the document gives, and this one gives none",
        )
    })?;
    let cpu = spec.budget.cpu.ok_or_else(|| {
        SpecError::new(
            "budget.cpu",
            "a guest is booted with the CPUs the document gives, and this one gives none",
        )
    })?;
    if !cpu.is_finite() || cpu <= 0.0 {
        return Err(SpecError::new(
            "budget.cpu",
            format!("{cpu} is not a CPU count"),
        ));
    }
    let cpus = cpu.ceil().min(f64::from(u32::MAX)) as u32;
    let elastic = spec.budget.elastic.clone().unwrap_or_default();
    Ok(moruna_vmm::Budget {
        memory_bytes: memory,
        memory_max_bytes: elastic.memory_max_bytes.unwrap_or(memory).max(memory),
        cpus,
        cpus_max: elastic.cpu_max.unwrap_or(cpus).max(cpus),
    })
}

/// The one line `moruna` prints to stderr when a job ends.
pub fn summary(outcome: &Outcome) -> String {
    let what = match &outcome.diagnostic {
        Some(diagnostic) => format!("exit {}: {diagnostic}", outcome.code),
        None => format!("exit {}", outcome.code),
    };
    match &outcome.report_file {
        Some(path) => format!("moruna: {what}; report: {}", path.display()),
        None => format!("moruna: {what}; no report file"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_lines_parse() {
        assert_eq!(
            parse(&args(&["run", "job.json"])),
            Ok(Command::Run {
                spec: "job.json".into(),
                plan: None,
                strict: false,
                packages: PackageHost::default(),
                events_json: false,
                args: Vec::new(),
            })
        );
        assert_eq!(
            parse(&args(&["run", "--strict", "job.json"])),
            Ok(Command::Run {
                spec: "job.json".into(),
                plan: None,
                strict: true,
                packages: PackageHost::default(),
                events_json: false,
                args: Vec::new(),
            })
        );
        assert_eq!(
            parse(&args(&["run", "job.json", "--events-json"])),
            Ok(Command::Run {
                spec: "job.json".into(),
                plan: None,
                strict: false,
                packages: PackageHost::default(),
                events_json: true,
                args: Vec::new(),
            })
        );
        assert!(
            parse(&args(&[
                "run",
                "--vm",
                "job.json",
                "--image",
                "/i",
                "--events-json"
            ]))
            .unwrap_err()
            .contains("printed as JSON already")
        );
        assert_eq!(
            parse(&args(&["serve", "--listen", "vsock://-1:5000"])),
            Ok(Command::Serve {
                listen: Address::parse("vsock://-1:5000").expect("address"),
                strict: true,
                packages: PackageHost::default(),
            })
        );
        assert_eq!(
            parse(&args(&["serve", "--no-strict", "--listen", "/tmp/m.sock"])),
            Ok(Command::Serve {
                listen: Address::parse("/tmp/m.sock").expect("address"),
                strict: false,
                packages: PackageHost::default(),
            })
        );
        // 17 e.6: the sandbox's interpreter and cgroup, on run and serve, and not on run --vm.
        let host = PackageHost {
            sandbox_python: Some("/opt/py/bin/python3".into()),
            sandbox_cgroup: Some("/sys/fs/cgroup/sb".into()),
        };
        assert_eq!(
            parse(&args(&[
                "run",
                "job.json",
                "--sandbox-python",
                "/opt/py/bin/python3",
                "--sandbox-cgroup",
                "/sys/fs/cgroup/sb",
            ])),
            Ok(Command::Run {
                spec: "job.json".into(),
                plan: None,
                strict: false,
                packages: host.clone(),
                events_json: false,
                args: Vec::new(),
            })
        );
        assert_eq!(
            parse(&args(&[
                "serve",
                "--listen",
                "/tmp/m.sock",
                "--sandbox-cgroup",
                "/sys/fs/cgroup/sb",
                "--sandbox-python",
                "/opt/py/bin/python3",
            ])),
            Ok(Command::Serve {
                listen: Address::parse("/tmp/m.sock").expect("address"),
                strict: true,
                packages: host,
            })
        );
        assert!(
            parse(&args(&["run", "job.json", "--sandbox-python"]))
                .unwrap_err()
                .contains("--sandbox-python needs a path")
        );
        assert!(
            parse(&args(&[
                "run",
                "--vm",
                "job.json",
                "--image",
                "/i",
                "--sandbox-cgroup",
                "/c"
            ]))
            .unwrap_err()
            .contains("the guest names its own sandbox")
        );
        assert_eq!(
            parse(&args(&["serve", "--strict", "--listen", "/tmp/m.sock"]))
                .map(|c| matches!(c, Command::Serve { strict: true, .. })),
            Ok(true)
        );
        assert_eq!(
            parse(&args(&[
                "run",
                "--vm",
                "job.json",
                "--image",
                "/node-image.json",
                "--plan",
                "/job.mplan",
                "--launch-witness",
                "/launches"
            ])),
            Ok(Command::RunVm {
                spec: "job.json".into(),
                image: Some("/node-image.json".into()),
                plan: Some("/job.mplan".into()),
                witness: Some("/launches".into()),
                data: VmData::default(),
            })
        );
        assert!(
            parse(&args(&[
                "run",
                "--vm",
                "job.json",
                "--image",
                "/node-image.json",
                "--disk",
                "/d"
            ]))
            .unwrap_err()
            .contains("retired")
        );
        assert_eq!(parse(&args(&["--version"])), Ok(Command::Version));
        assert_eq!(parse(&args(&["help"])), Ok(Command::Help));
        for bad in [
            &[][..],
            &["run"][..],
            &["run", "a", "b"][..],
            &["run", "--fast", "a"][..],
            &["serve"][..],
            &["serve", "--listen"][..],
            &["serve", "--listen", "tcp://x"][..],
            &["serve", "--port", "1"][..],
            &["check", "k.py"][..],
            &["run", "a", "--image", "/img"][..],
            &["run", "a", "--disk", "/d"][..],
            &["run", "a", "--report", "/r.json"][..],
            &["run", "--vm", "a", "--image", "/i", "--memory-gib", "8"][..],
            &["run", "--vm", "--strict", "a", "--image", "/i"][..],
            &["run", "--vm", "a", "--image"][..],
            &["run", "--vm", "a", "--image", "/i", "--disk"][..],
            &["run", "--vm", "a", "--image", "/i", "--disk", ":ro"][..],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn version_help_and_nonsense_exit_as_documented() {
        let kinds = Registry::standard();
        let env = |_: &str| None;
        let run = |a: &[&str]| main_with(&args(a), &kinds, CancelToken::new(), &env, HEARTBEAT);
        assert_eq!(run(&["--version"]), exit::COMPLETED);
        assert_eq!(run(&["--help"]), exit::COMPLETED);
        assert_eq!(run(&["frobnicate"]), exit::SPEC_REFUSED);
        // KD-T6: `moruna kinds` prints the listing and takes nothing else.
        assert_eq!(parse(&args(&["kinds"])), Ok(Command::Kinds));
        assert_eq!(run(&["kinds"]), exit::COMPLETED);
        assert_eq!(run(&["kinds", "--json"]), exit::SPEC_REFUSED);
        assert_eq!(
            main(
                &args(&["run", "/no/such/moruna/spec.json"]),
                &kinds,
                CancelToken::new()
            ),
            exit::SPEC_REFUSED
        );
    }

    #[test]
    fn native_vm_budget_requires_explicit_memory_and_cpu() {
        let document = |budget: serde_json::Value| {
            moruna_job::Job::from_v1(&moruna_job::JobSpec::from_json(&serde_json::json!({
            "moruna_spec": 1, "source": {"kind":"parquet","url":"/in"}, "sink":{"kind":"parquet","url":"/out"}, "budget":budget
        }).to_string()).unwrap())
        };
        assert_eq!(
            vm_budget(&document(serde_json::json!({"cpu":1.0})))
                .unwrap_err()
                .field,
            "budget.memory_bytes"
        );
        assert_eq!(
            vm_budget(&document(serde_json::json!({"memory_bytes":536870912})))
                .unwrap_err()
                .field,
            "budget.cpu"
        );
        let budget = vm_budget(&document(
            serde_json::json!({"memory_bytes":536870912,"cpu":1.5}),
        ))
        .unwrap();
        assert_eq!(budget.memory_bytes, 536870912);
        assert_eq!(budget.cpus, 2);
    }

    /// Off Linux, `moruna run --vm` is refused by name before anything is read.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn run_vm_off_linux_is_refused_by_name() {
        let kinds = Registry::standard();
        let env = |_: &str| None;
        let code = main_with(
            &args(&[
                "run",
                "--vm",
                "/no/such/spec.json",
                "--image",
                "/no/img",
                "--launch-witness",
                "/launches",
            ]),
            &kinds,
            CancelToken::new(),
            &env,
            HEARTBEAT,
        );
        assert_eq!(code, exit::FAILED);
    }

    /// Stream D: a VM run binds the image's inputs, names its output directory and may place
    /// its vCPUs; none of these is a host run's.
    #[test]
    fn a_vm_run_names_its_data_disks_and_placement() {
        let base = [
            "run",
            "--vm",
            "job.json",
            "--image",
            "/i.json",
            "--launch-witness",
            "/w",
        ];
        let with = |extra: &[&str]| {
            let mut all: Vec<&str> = base.to_vec();
            all.extend_from_slice(extra);
            parse(&args(&all))
        };
        let parsed = with(&[
            "--input",
            "nyc=/data/nyc.parquet",
            "--input",
            "zones=s3://b/zones.parquet",
            "--output",
            "/out",
            "--vcpu-cpus",
            "2-3",
        ])
        .unwrap();
        let Command::RunVm { data, .. } = parsed else {
            panic!("{parsed:?}")
        };
        assert_eq!(
            data,
            VmData {
                inputs: vec![
                    ("nyc".into(), "/data/nyc.parquet".into()),
                    ("zones".into(), "s3://b/zones.parquet".into())
                ],
                output: Some("/out".into()),
                vcpu_cpus: vec![2, 3],
                ..VmData::default()
            }
        );
        for (extra, why) in [
            (
                &["--input", "nyc=/a", "--input", "nyc=/b"][..],
                "bound twice",
            ),
            (&["--input", "Bad=/a"][..], "is not one"),
            (&["--input"][..], "needs NAME=URL"),
            (&["--output", "/a", "--output", "/b"][..], "given twice"),
            (&["--output"][..], "needs a directory"),
            (&["--vcpu-cpus", "3-1"][..], "runs backwards"),
            (&["--vcpu-cpus", "1", "--vcpu-cpus", "2"][..], "given twice"),
            (&["--vcpu-cpus"][..], "needs a CPU list"),
        ] {
            let e = with(extra).unwrap_err();
            assert!(e.contains(why), "{extra:?}: {e}");
        }
        for host in [
            &["run", "job.json", "--input", "a=/a"][..],
            &["run", "job.json", "--vcpu-cpus", "1"][..],
            &["run", "job.json", "--report", "/r"][..],
        ] {
            assert!(
                parse(&args(host))
                    .unwrap_err()
                    .contains("need --vm or --output"),
                "{host:?}"
            );
        }
        // The gate's command line: the output directory makes it a VM run, its node built from
        // the kit, its witness the default.
        let gate = parse(&args(&[
            "run",
            "job.json",
            "--output",
            "/out",
            "--report",
            "/r.json",
            "--memory-gib",
            "24",
            "--scratch-gib",
            "2.5",
            "--node-kit",
            "/kit",
            "--image-stack",
            "/stack",
        ]))
        .unwrap();
        assert_eq!(
            gate,
            Command::RunVm {
                spec: "job.json".into(),
                image: None,
                plan: None,
                witness: None,
                data: VmData {
                    output: Some("/out".into()),
                    report: Some("/r.json".into()),
                    memory_bytes: Some(24 << 30),
                    scratch_bytes: Some(5 << 29),
                    node_kit: Some("/kit".into()),
                    image_stack: Some("/stack".into()),
                    ..VmData::default()
                },
            }
        );
        let kit = |extra: &[&str]| {
            let mut all = vec!["run", "job.json", "--output", "/out"];
            all.extend_from_slice(extra);
            parse(&args(&all))
        };
        assert_eq!(
            kit(&["--memory", "1048576"]).map(|c| match c {
                Command::RunVm { data, .. } => data.memory_bytes,
                _ => None,
            }),
            Ok(Some(1 << 20))
        );
        for (extra, why) in [
            (&["--memory-gib", "0"][..], "not a size"),
            (&["--memory-gib", "x"][..], "not a size"),
            (&["--memory", "-1"][..], "not a size"),
            (&["--memory"][..], "needs a number"),
            (&["--memory-gib", "1", "--memory", "2"][..], "given twice"),
            (&["--report", "/a", "--report", "/b"][..], "given twice"),
            (&["--report"][..], "needs a file"),
            (&["--node-kit"][..], "needs a directory"),
            (&["--node-kit", "/a", "--node-kit", "/b"][..], "given twice"),
            (&["--image", "/i", "--memory-gib", "8"][..], "already built"),
        ] {
            let e = kit(extra).unwrap_err();
            assert!(e.contains(why), "{extra:?}: {e}");
        }
    }

    #[test]
    fn a_vm_run_finds_its_node_kit_and_its_witness() {
        let dir = std::env::temp_dir().join(format!("m-kit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            node_kit(Some(Path::new("/k")), Some("/e".into()), None).unwrap(),
            Path::new("/k")
        );
        assert_eq!(
            node_kit(None, Some("/e".into()), None).unwrap(),
            Path::new("/e")
        );
        assert!(node_kit(None, Some("".into()), None).is_err());
        let installed = dir.join("share/moruna/node-kit");
        std::fs::create_dir_all(&installed).unwrap();
        assert_eq!(
            node_kit(None, None, Some(&dir.join("bin/moruna"))).unwrap(),
            installed
        );
        assert!(node_kit(None, None, Some(Path::new("/nowhere/bin/moruna"))).is_err());
        let state = default_witness(Some(dir.join("state").into()), None).unwrap();
        assert_eq!(state, dir.join("state/moruna/launches"));
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            default_witness(None, Some(dir.join("home").into())).unwrap(),
            dir.join("home/.local/state/moruna/launches")
        );
        assert!(default_witness(None, None).is_err());
        // The node builder's arguments, and a kit that is not one.
        let order = NodeOrder {
            cpus: 8,
            memory_bytes: Some((24 << 30) + 1),
            scratch_bytes: 1 << 30,
            inputs: vec!["a".into(), "b".into()],
            image_stack: Some("/stack".into()),
        };
        let args: Vec<String> = order
            .args(Path::new("/kit"), Path::new("/j.json"), Path::new("/o"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(args[1], "/kit/node/tools/node-for-job.py");
        assert_eq!(at("--cpus"), "8");
        assert_eq!(
            at("--node-memory"),
            ((24u64 << 30) + (2 << 20)).to_string(),
            "whole 2 MiB pages"
        );
        assert_eq!(at("--output-bytes"), output_disk_for(1 << 30).to_string());
        assert_eq!(at("--image-stack"), "/stack");
        assert_eq!(args.iter().filter(|a| *a == "--input").count(), 2);
        assert!(output_disk_for(1 << 30) > (1 << 30));
        assert_eq!(output_disk_for(1 << 30) % 4096, 0);
        // stream C hook (D's file): the data, the tables' 16 MiB, the tree, the margin.
        let gib = output_disk_for(1 << 30);
        let content = (1u64 << 30) + (16 << 20);
        assert!(gib >= content + content / 127 + (64 << 20), "{gib}");
        assert!(gib < content + content / 64 + (64 << 20) + 4096, "{gib}");
        let none = output_disk_for(0);
        assert!(
            none > (16 << 20) + (64 << 20) && none.is_multiple_of(4096),
            "{none}"
        );
        let e = build_node(&dir, Path::new("/j"), &dir.join("o"), &order).unwrap_err();
        assert!(e.contains("not a node kit"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// stream E hook (D's file): a cancelled run that did not complete exits cancelled.
    #[test]
    fn a_cancelled_run_exits_cancelled() {
        let mut said = Some("control: job deadline expired".to_string());
        assert_eq!(
            cancelled_code(exit::FAILED, true, &mut said),
            exit::CANCELLED
        );
        assert_eq!(
            said.as_deref(),
            Some("cancelled: control: job deadline expired")
        );
        let mut none = None;
        assert_eq!(cancelled_code(8, true, &mut none), exit::CANCELLED);
        assert_eq!(none.as_deref(), Some("cancelled"));
        let mut kept = Some("x".to_string());
        assert_eq!(
            cancelled_code(exit::COMPLETED, true, &mut kept),
            exit::COMPLETED
        );
        assert_eq!(cancelled_code(exit::FAILED, false, &mut kept), exit::FAILED);
        assert_eq!(kept.as_deref(), Some("x"));
    }

    /// stream E hook (D's file): SIGTERM, even over an inherited "ignore", cancels the token
    /// instead of killing the process.
    #[cfg(unix)]
    #[test]
    fn a_sigterm_cancels_the_run() {
        let cancel = CancelToken::new();
        signals::cancel_on(&cancel);
        // SAFETY: raise delivers SIGTERM to this process, whose handler is now ours.
        unsafe { libc::raise(libc::SIGTERM) };
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while !cancel.is_cancelled() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(signals::raised());
        assert!(cancel.is_cancelled());
    }

    /// stream E hook (D's file): a node that never answered says why, from its console, and
    /// keeps its logs.
    #[test]
    fn a_node_that_never_answered_names_its_console_cause() {
        let console = "kk: 8 CPUs online\n\
            KD2 engine: run 1 of 1: starting /moruna/bin/moruna-engine\n\
            kk: process task.a exited with code 0 (it ran on cpus 1)\n\
            admission: an account of 23353884672 bytes: QuotaExceeded\n\
            kk: process admission exited with code 1 (it ran on cpus 2)\n\
            node: critical service 5 terminated; stopping node\n";
        let cause = console_cause(console).unwrap();
        assert!(cause.starts_with("admission: an account"), "{cause}");
        assert!(cause.ends_with("node: critical service 5 terminated; stopping node"));
        assert!(!cause.contains("code 0"));
        assert_eq!(console_cause("kk: 8 CPUs online\nlinux: up\n"), None);
        let many = "x failed\n".repeat(9);
        assert_eq!(console_cause(&many).unwrap().matches("failed").count(), 4);

        let dir = std::env::temp_dir().join(format!("m-never-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let run = dir.join("moruna-node-abc");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("serial.log"), console).unwrap();
        let error = connect_failure(
            moruna_vmm::error::VmmError::Control(
                "native service connect deadline/owner expired".into(),
            ),
            &run,
            Some(dir.join("kept").into_os_string()),
        );
        let said = error.to_string();
        assert!(said.contains("connect deadline"), "{said}");
        assert!(
            said.contains("the node's console: admission: an account"),
            "{said}"
        );
        assert!(dir.join("kept/serial.log").is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_that_did_not_complete_keeps_the_tail_of_its_node_logs() {
        let dir = std::env::temp_dir().join(format!("m-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run")).unwrap();
        let mut serial = vec![b'a'; DIAGNOSTIC_BYTES as usize];
        serial.extend_from_slice(b"panic: the end");
        std::fs::write(dir.join("run/serial.log"), &serial).unwrap();
        std::fs::write(dir.join("run/qemu.log"), b"qemu says").unwrap();
        keep_diagnostics(&dir.join("run"), &dir.join("kept")).unwrap();
        let kept = std::fs::read(dir.join("kept/serial.log")).unwrap();
        assert_eq!(kept.len() as u64, DIAGNOSTIC_BYTES);
        assert!(kept.ends_with(b"panic: the end"));
        assert_eq!(
            std::fs::read(dir.join("kept/qemu.log")).unwrap(),
            b"qemu says"
        );
        assert!(!dir.join("kept/backend.log").exists());
        assert!(
            keep_diagnostics(&dir.join("run"), &dir.join("kept")).is_err(),
            "never over"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_vm_runs_report_is_the_engines_with_the_vms_figures() {
        let engine = serde_json::json!({"tasks": [{"role": "function", "busy_ms": 5}],
            "exit": {"status": "completed", "code": 0}, "peak_anon_bytes": 7});
        let mut vm = serde_json::Map::new();
        vm.insert("boot_s".into(), 1.5.into());
        let report = vm_report(Some(engine.clone()), 0, None, vm.clone());
        assert_eq!(report["exit"], "completed");
        assert_eq!(report["engine_exit"]["status"], "completed");
        assert_eq!(report["vm"]["boot_s"], 1.5);
        assert_eq!(report["tasks"], engine["tasks"]);
        assert_eq!(report["peak_anon_bytes"], 7);
        assert_eq!(report["version"], VERSION);
        let failed = serde_json::json!({"exit": {"status": "terminated", "code": 4}});
        let report = vm_report(Some(failed), 4, Some("why".into()), vm.clone());
        assert_eq!(report["exit"], "terminated");
        assert_eq!(report["diagnostic"], "why");
        let none = vm_report(None, 8, None, vm);
        assert_eq!(none["exit"], "failed");
        assert_eq!(none["code"], 8);
        for (code, status) in [
            (0, "completed"),
            (130, "cancelled"),
            (2, "refused"),
            (3, "refused"),
            (5, "refused"),
            (4, "failed"),
        ] {
            assert_eq!(status_of(code), status);
        }
        let dir = std::env::temp_dir().join(format!("m-report-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.json");
        write_report(&path, &none).unwrap();
        let read: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read, none);
        assert!(write_report(&dir.join("no/such/r.json"), &none).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_vm_run_reports_its_timing_and_checks_its_output_directory() {
        let t = VmTiming {
            stage_ms: 1,
            launch_ms: 2,
            boot_ms: 3,
            job_ms: 4,
            outputs_ms: 5,
        };
        assert_eq!(
            t.line(),
            "moruna run --vm: timing {\"stage_ms\":1,\"launch_ms\":2,\"boot_ms\":3,\"job_ms\":4,\"outputs_ms\":5}"
        );
        let json: serde_json::Value =
            serde_json::from_str(t.line().split_once("timing ").unwrap().1).unwrap();
        assert_eq!(json["boot_ms"], 3);
        let dir = std::env::temp_dir().join(format!("m-check-output-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        check_output(0, None).unwrap();
        assert!(
            check_output(4096, None)
                .unwrap_err()
                .contains("--output DIR")
        );
        assert!(
            check_output(0, Some(&dir))
                .unwrap_err()
                .contains("no output disk")
        );
        check_output(4096, Some(&dir)).unwrap();
        std::fs::create_dir(&dir).unwrap();
        check_output(4096, Some(&dir)).unwrap();
        std::fs::write(dir.join("x"), b"x").unwrap();
        assert!(
            check_output(4096, Some(&dir))
                .unwrap_err()
                .contains("not empty")
        );
        assert!(
            check_output(4096, Some(&dir.join("x")))
                .unwrap_err()
                .contains("not a directory")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_summary_names_the_file() {
        let mut outcome = Outcome {
            code: 0,
            diagnostic: None,
            report: None,
            report_file: Some("/tmp/r.json".into()),
            notes: Vec::new(),
            plan: None,
        };
        assert_eq!(summary(&outcome), "moruna: exit 0; report: /tmp/r.json");
        outcome.code = 2;
        outcome.diagnostic = Some("spec refused: x: y".into());
        outcome.report_file = None;
        assert_eq!(
            summary(&outcome),
            "moruna: exit 2: spec refused: x: y; no report file"
        );
    }
}

#[cfg(test)]
mod historical_cost_options_tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }
    #[test]
    fn explicit_cost_store_model_and_pinned_historical_reads_parse_or_refuse() {
        let (clean, cost) = cost_options(&args(&[
            "run",
            "job.json",
            "--cost-store",
            "/tmp/explicit-history",
            "--cost-model",
            "loglinear",
        ]))
        .unwrap();
        assert_eq!(clean, args(&["run", "job.json"]));
        assert_eq!(cost.unwrap().model, "loglinear");
        assert!(cost_options(&args(&["run", "job.json", "--cost-model"])).is_err());
        assert!(
            cost_options(&args(&[
                "run",
                "--vm",
                "job.json",
                "--cost-store",
                "/tmp/store"
            ]))
            .is_err()
        );
        assert!(matches!(
            parse(&args(&["task-output", "pin.json"])).unwrap(),
            Command::TaskOutput { chunk: None, .. }
        ));
        assert!(parse(&args(&["task-output", "pin.json", "--chunk", "0"])).is_err());
        assert!(
            parse(&args(&[
                "task-output",
                "pin.json",
                "--chunk",
                "0",
                "--out",
                "rows.arrow",
                "--max-bytes",
                "0"
            ]))
            .is_err()
        );
        assert!(matches!(
            parse(&args(&[
                "task-output",
                "pin.json",
                "--chunk",
                "0",
                "--out",
                "rows.arrow",
                "--max-bytes",
                "4096"
            ]))
            .unwrap(),
            Command::TaskOutput { chunk: Some(0), .. }
        ));
    }
}

/// Where the package's `moruna run` pulls a job's registry images to before it starts
/// (`$XDG_CACHE_HOME/moruna/images`, else `~/.cache/moruna/images`), when it is there.
pub fn pulled_images() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let dir = base.join("moruna").join("images");
    dir.join("oci-layout").is_file().then_some(dir)
}

#[cfg(test)]
mod surface_refusal_tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }
    fn refused(values: &[&str]) -> String {
        parse(&args(values)).expect_err("refused")
    }
    fn no_env(_: &str) -> Option<String> {
        None
    }
    fn exit_of(values: &[&str]) -> i32 {
        main_with(
            &args(values),
            &Registry::standard(),
            CancelToken::new(),
            &no_env,
            Duration::from_secs(1),
        )
    }

    /// Every malformed command line is refused with the option it is about named.
    #[test]
    fn malformed_command_lines_are_refused_by_name() {
        assert!(refused(&["run", "j.json", "--arg"]).contains("--arg needs NAME=VALUE"));
        assert!(refused(&["run", "j.json", "--arg", "x"]).contains("is NAME=VALUE"));
        assert!(refused(&["run", "j.json", "--arg", "x=1", "--arg", "x=2"]).contains("twice"));
        assert!(refused(&["run", "j.json", "--plan"]).contains("--plan needs a path"));
        assert!(refused(&["run", "j.json", "--image"]).contains("--image needs a path"));
        assert!(refused(&["run", "j.json", "--sandbox-python"]).contains("needs a path"));
        assert!(
            refused(&[
                "run",
                "j.json",
                "--launch-witness",
                "/a",
                "--launch-witness",
                "/b"
            ])
            .contains("duplicate")
        );
        assert!(refused(&["task-output", "p.json", "--chunk"]).contains("needs a value"));
        assert!(refused(&["task-output", "p.json", "--chunk", "x"]).contains("nonnegative"));
        assert!(refused(&["task-output", "p.json", "--max-bytes", "-1"]).contains("byte budget"));
        assert!(
            refused(&["task-output", "p.json", "--out", "a", "--out", "b"]).contains("repeated")
        );
        assert!(refused(&["task-output", "p.json", "--out", "a"]).contains("requires --out"));
        assert!(refused(&["kinds", "extra"]).contains("takes no arguments"));
        assert!(refused(&["check-job"]).contains("which document"));
        assert!(refused(&["check-job", "a", "b"]).contains("is a second"));
        assert!(refused(&["explain"]).contains("which document"));
        assert!(refused(&["explain", "a", "b"]).contains("is a second"));
        assert!(refused(&["explain", "a", "--planner"]).contains("needs a name"));
        assert!(refused(&["explain", "a", "--out"]).contains("needs a path"));
        assert!(refused(&["explain", "a", "--bogus"]).contains("unknown option"));
        assert!(refused(&["frobnicate"]).contains("unknown command"));
        assert!(refused(&[]).contains("which command"));
        assert_eq!(
            parse(&args(&[
                "explain",
                "a",
                "--planner",
                "levels",
                "--out",
                "o"
            ])),
            Ok(Command::Explain {
                spec: "a".into(),
                planner: Some("levels".into()),
                out: Some(PathBuf::from("o")),
            })
        );
        for version in ["--version", "-V", "version"] {
            assert_eq!(parse(&args(&[version])), Ok(Command::Version));
        }
        for help in ["--help", "-h", "help"] {
            assert_eq!(parse(&args(&[help])), Ok(Command::Help));
        }
    }

    /// The surface's exits: what needs no run completes, a refused line or option is a refusal,
    /// and a task-output read without an authorized reader is refused before anything is read.
    #[test]
    fn the_surface_exits_by_command() {
        assert_eq!(exit_of(&["--version"]), exit::COMPLETED);
        assert_eq!(exit_of(&["help"]), exit::COMPLETED);
        assert_eq!(exit_of(&["kinds"]), exit::COMPLETED);
        assert_eq!(exit_of(&["frobnicate"]), exit::SPEC_REFUSED);
        assert_eq!(
            exit_of(&["run", "j.json", "--cost-store", "a", "--cost-store", "b"]),
            exit::SPEC_REFUSED
        );
        assert_eq!(
            exit_of(&["task-output", "/nonexistent/pin.json"]),
            exit::SPEC_REFUSED
        );
        let mut printed = Vec::new();
        let refused = task_output(
            &Registry::standard(),
            Path::new("/nonexistent/pin.json"),
            None,
            None,
            None,
            &mut printed,
        )
        .unwrap_err();
        assert!(refused.contains("historical output reader"), "{refused}");
        assert!(printed.is_empty());
        assert_ne!(
            exit_of(&["check-job", "/nonexistent/job.json"]),
            exit::COMPLETED
        );
    }

    /// The guest's budget is the document's own: absent memory or CPU, or a CPU count that is
    /// not one, is refused naming the field; a fractional CPU rounds up.
    #[test]
    fn a_vm_budget_comes_only_from_the_document() {
        let mut job = moruna_job::Job::from_v1(&moruna_job::JobSpec::new(
            moruna_job::KindDoc::new("parquet", serde_json::Map::new()),
            Vec::new(),
            moruna_job::KindDoc::new("parquet", serde_json::Map::new()),
        ));
        job.budget.memory_bytes = None;
        job.budget.cpu = Some(1.0);
        assert_eq!(vm_budget(&job).unwrap_err().field, "budget.memory_bytes");
        job.budget.memory_bytes = Some(1 << 30);
        job.budget.cpu = None;
        assert_eq!(vm_budget(&job).unwrap_err().field, "budget.cpu");
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            job.budget.cpu = Some(bad);
            assert_eq!(vm_budget(&job).unwrap_err().field, "budget.cpu");
        }
        job.budget.cpu = Some(1.5);
        let budget = vm_budget(&job).unwrap();
        assert_eq!(budget.cpus, 2);
    }
}

/// `moruna run --vm` refuses what it cannot boot before any VM, jail or run directory exists.
#[cfg(all(test, target_os = "linux"))]
mod run_vm_refusals {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("m-runvm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn refused(spec: &Path, plan: Option<&Path>) -> i32 {
        let dir = spec.parent().unwrap();
        run_vm(
            spec,
            Some(&dir.join("no-image.json")),
            plan,
            Some(&dir.join("witness")),
            &VmData::default(),
            CancelToken::new(),
        )
    }

    #[test]
    fn a_missing_oversized_or_invalid_spec_is_refused_before_boot() {
        let dir = scratch("spec");
        assert_ne!(refused(&dir.join("absent.json"), None), 0);
        let big = dir.join("big.json");
        std::fs::write(&big, vec![b' '; super::super::protocol::MAX_SPEC_LINE + 1]).unwrap();
        assert_ne!(refused(&big, None), 0);
        let garbage = dir.join("garbage.json");
        std::fs::write(&garbage, b"{not json").unwrap();
        assert_ne!(refused(&garbage, None), 0);
        assert!(
            std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .flatten()
                .all(
                    |e| !e.file_name().to_string_lossy().starts_with("moruna-node-")
                        || e.path().metadata().is_ok()
                ),
            "no run directory is left behind for a refused spec"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
