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
       moruna run --vm <spec.json> --image <node-image.json> --launch-witness <durable-directory>
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
        /// layouts are not interpreted as microkernel images.
        image: PathBuf,
        /// Existing durable run-claim directory; never a temporary default.
        witness: PathBuf,
        /// Optional exact plan delivered through the canonical Spec message.
        plan: Option<PathBuf>,
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
            if !vm {
                if image.is_some() || witness.is_some() {
                    return Err(
                        "moruna run: --image and --disk describe a guest, and need --vm"
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
            let image = image.ok_or_else(|| "moruna run --vm: --image is required".to_string())?;
            let witness = witness.ok_or("moruna run --vm: --launch-witness is required")?;
            Ok(Command::RunVm {
                spec,
                image,
                plan,
                witness,
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
    main_as(args, kinds, cancel, &process_env, HEARTBEAT, true)
}

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
        } => {
            return run_vm(Path::new(&spec), &image, plan.as_deref(), &witness, cancel);
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

/// Moruna 1.0 boots only the checked native microkernel artifact descriptor.
#[cfg(target_os = "linux")]
fn run_vm(
    spec: &Path,
    image: &Path,
    plan: Option<&Path>,
    witness: &Path,
    cancel: CancelToken,
) -> i32 {
    let result = (|| -> moruna_vmm::error::Result<moruna_vmm::ExitStatus> {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        std::fs::File::open(spec)
            .map_err(|e| moruna_vmm::error::VmmError::io("open", spec.display().to_string(), &e))?
            .take(super::protocol::MAX_SPEC_LINE as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| moruna_vmm::error::VmmError::io("read", spec.display().to_string(), &e))?;
        if bytes.len() > super::protocol::MAX_SPEC_LINE {
            return Err(moruna_vmm::error::VmmError::config(
                "spec",
                "exceeds canonical protocol ceiling",
            ));
        }
        let document = super::session::read_document(&bytes)
            .map_err(|(_, e)| moruna_vmm::error::VmmError::config("spec", e.to_string()))?;
        let budget = vm_budget(&document)
            .map_err(|e| moruna_vmm::error::VmmError::config("budget", e.to_string()))?;
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|e| moruna_vmm::error::VmmError::config("run directory", e.to_string()))?;
        let name: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("moruna-node-{name}"));
        let executable = std::env::current_exe()
            .map_err(|e| moruna_vmm::error::VmmError::io("resolve", "runtime executable", &e))?;
        let backend = executable
            .parent()
            .ok_or_else(|| {
                moruna_vmm::error::VmmError::config("backend", "executable parent unavailable")
            })?
            .join("moruna-vhost-vsock");
        let plan = plan
            .map(|path| {
                use base64::Engine as _;
                let metadata = std::fs::metadata(path)
                    .map_err(|e| moruna_vmm::error::VmmError::io("metadata", "plan", &e))?;
                if metadata.len() > super::protocol::MAX_SPEC_LINE as u64 {
                    return Err(moruna_vmm::error::VmmError::config(
                        "plan",
                        "exceeds canonical control ceiling",
                    ));
                }
                let bytes = std::fs::read(path)
                    .map_err(|e| moruna_vmm::error::VmmError::io("read", "plan", &e))?;
                super::session::read_plan(&bytes)
                    .map_err(|e| moruna_vmm::error::VmmError::config("plan", e.to_string()))?;
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
                moruna_vmm::error::VmmError::config(
                    "run",
                    "native admission requires canonical document id",
                )
            })?;
        let mut run = super::native::NativeRun::start(
            super::launch_witness::Admission {
                run_id: assigned,
                node_boot: moruna_kernel::RunId(nonce),
                attempt: 0,
                directory: witness,
            },
            image,
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
        )?;
        let control = run.control();
        let done = std::sync::atomic::AtomicBool::new(false);
        let result = std::thread::scope(|scope| {
            let _stop_watcher = StopWatcher(&done);
            let watcher = scope.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    if cancel.is_cancelled() {
                        let _ = control.send(&super::protocol::Inbound::Cancel);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            let outcome = (|| {
                let mut messages = 0usize;
                loop {
                    let message = run.next(deadline)?;
                    let message = match message {
                        super::native::Message::Control(message) => message,
                        super::native::Message::Event(event) => {
                            std::io::stdout()
                                .write_all(event.json_line().as_bytes())
                                .map_err(|e| {
                                    moruna_vmm::error::VmmError::io(
                                        "write",
                                        "guest event output",
                                        &e,
                                    )
                                })?;
                            messages += 1;
                            continue;
                        }
                    };
                    std::io::stdout()
                        .write_all(message.line().as_bytes())
                        .map_err(|e| {
                            moruna_vmm::error::VmmError::io("write", "guest output", &e)
                        })?;
                    messages += 1;
                    match message {
                        super::protocol::Outbound::PortCall { v, call, port, .. } => {
                            if v != super::protocol::PORTS_VERSION {
                                return Err(moruna_vmm::error::VmmError::config(
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
                        super::protocol::Outbound::Exit { code, .. } => {
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
        let cleanup = run.shutdown();
        let result = result.and_then(|status| cleanup.map(|()| status));
        // Preserve actual serial/backend diagnostics for a failed run.
        if result.is_ok() {
            let _ = std::fs::remove_dir_all(&directory);
        } else {
            eprintln!(
                "moruna run --vm: diagnostics retained at {}",
                directory.display()
            );
        }
        result
    })();
    match result {
        Ok(status) => status.code,
        Err(error) => {
            eprintln!("moruna run --vm: {error}");
            error.exit_code()
        }
    }
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
    _image: &Path,
    _plan: Option<&Path>,
    _witness: &Path,
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
                image: "/node-image.json".into(),
                plan: Some("/job.mplan".into()),
                witness: "/launches".into()
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
            &["run", "--vm", "a"][..],
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
