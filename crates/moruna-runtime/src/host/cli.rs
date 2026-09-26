//! The `moruna` command line (MH 4.2, 4.8.4): `run`, `run --vm`, `serve`, `--version`. The
//! binary is a thin `main` over [`main`]; the Python adapter's binary passes a loader that
//! imports Python kernels, and a build without it passes [`crate::job::NoKernels`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use moruna_vmm::config::{DiskConfig, VmConfig};

use moruna_kernel::CancelToken;

use super::session::{self, HEARTBEAT, HostOptions, Outcome};
use super::transport::Address;
use super::{VERSION, exit};
use crate::job::KernelLoader;
use crate::job::build::process_env;

/// What `moruna` prints for a command line it does not understand.
pub const USAGE: &str = "usage: moruna run <spec.json> [--strict]
       moruna run --vm <spec.json> --image <dir-or-oci> [--disk <path>[:ro|:rw]]...
       moruna serve --listen <unix:///path | vsock://CID:PORT> [--no-strict]
       moruna --version";

/// A parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `moruna run <spec.json> [--strict]`.
    Run {
        /// The document.
        spec: String,
        /// Refuse any field the environment would fill.
        strict: bool,
    },
    /// `moruna run --vm <spec.json> --image <path> [--disk <path>[:ro]]...` (MH 4.8.4): boot
    /// the guest with the disks attached, send it the document, relay what it says, exit with
    /// its code. The guest's `moruna serve` is strict, so there is no `--strict` here.
    RunVm {
        /// The document.
        spec: String,
        /// The guest image: a directory or an OCI layout.
        image: PathBuf,
        /// The disks the guest sees, in order.
        disks: Vec<DiskConfig>,
    },
    /// `moruna serve --listen <address> [--no-strict]`; strict unless told otherwise, because
    /// serve is the hosted path (MH 4.1).
    Serve {
        /// Where to wait for the peer.
        listen: Address,
        /// Refuse any field the environment would fill.
        strict: bool,
    },
    /// `moruna --version`.
    Version,
    /// `moruna --help`.
    Help,
}

/// Parse the arguments after the program name. The error is the line to print before
/// [`USAGE`].
pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut rest = args.iter().map(String::as_str);
    match rest.next() {
        Some("run") => {
            let mut spec = None;
            let mut strict = false;
            let mut vm = false;
            let mut image = None;
            let mut disks = Vec::new();
            while let Some(arg) = rest.next() {
                match arg {
                    "--strict" => strict = true,
                    "--vm" => vm = true,
                    "--image" => {
                        let path = rest
                            .next()
                            .ok_or_else(|| "moruna run: --image needs a path".to_string())?;
                        image = Some(PathBuf::from(path));
                    }
                    "--disk" => {
                        let disk = rest
                            .next()
                            .ok_or_else(|| "moruna run: --disk needs a path".to_string())?;
                        disks.push(
                            moruna_vmm::cli::parse_disk(disk)
                                .map_err(|e| format!("moruna run: {e}"))?,
                        );
                    }
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
                if image.is_some() || !disks.is_empty() {
                    return Err(
                        "moruna run: --image and --disk describe a guest, and need --vm"
                            .to_string(),
                    );
                }
                return Ok(Command::Run { spec, strict });
            }
            if strict {
                return Err(
                    "moruna run --vm: the guest's `moruna serve` is always strict; drop --strict"
                        .to_string(),
                );
            }
            let image = image.ok_or_else(|| "moruna run --vm: --image is required".to_string())?;
            Ok(Command::RunVm { spec, image, disks })
        }
        Some("serve") => {
            let mut listen = None;
            let mut strict = true;
            while let Some(arg) = rest.next() {
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
            Ok(Command::Serve { listen, strict })
        }
        Some("--version" | "-V" | "version") => Ok(Command::Version),
        Some("--help" | "-h" | "help") => Ok(Command::Help),
        Some(other) => Err(format!("moruna: unknown command `{other}`")),
        None => Err("moruna: which command?".to_string()),
    }
}

/// Run a command line and return the process's exit code (MH 4.2). `cancel` is the surface's
/// (a signal handler's) way in; a `cancel` message from the peer uses the same token.
pub fn main(args: &[String], loader: &dyn KernelLoader, cancel: CancelToken) -> i32 {
    main_with(args, loader, cancel, &process_env, HEARTBEAT)
}

/// [`main`] with the environment and the heartbeat cadence supplied, for tests.
pub fn main_with(
    args: &[String],
    loader: &dyn KernelLoader,
    cancel: CancelToken,
    env: &dyn Fn(&str) -> Option<String>,
    heartbeat: Duration,
) -> i32 {
    let command = match parse(args) {
        Ok(command) => command,
        Err(line) => {
            eprintln!("{line}\n{USAGE}");
            return exit::SPEC_REFUSED;
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
        Command::RunVm { spec, image, disks } => {
            return run_vm(Path::new(&spec), &image, &disks);
        }
        Command::Run { spec, strict } => session::run_file(
            Path::new(&spec),
            loader,
            &HostOptions {
                strict,
                env,
                cancel,
                heartbeat,
            },
        ),
        Command::Serve { listen, strict } => session::serve(
            &listen,
            loader,
            &HostOptions {
                strict,
                env,
                cancel,
                heartbeat,
            },
        ),
    };
    eprintln!("{}", summary(&outcome));
    outcome.code
}

/// `moruna run --vm` (MH 4.8.4) on the monitor this build has: KVM, which is Linux-only.
#[cfg(target_os = "linux")]
fn run_vm(spec: &Path, image: &Path, disks: &[DiskConfig]) -> i32 {
    run_vm_with(
        spec,
        image,
        disks,
        moruna_vmm::session::default_cid(),
        &mut std::io::stdout(),
        moruna_vmm::boot,
    )
}

/// `moruna run --vm` where there is no KVM: refused by name, before anything is read.
#[cfg(not(target_os = "linux"))]
fn run_vm(_spec: &Path, _image: &Path, _disks: &[DiskConfig]) -> i32 {
    eprintln!(
        "moruna run --vm: the microVM runs on KVM, which is Linux-only, and this is {}",
        std::env::consts::OS
    );
    exit::FAILED
}

/// The guest's budget, from the document (MH 4.8.4): it boots with `budget.memory_bytes` and
/// `budget.cpu` rounded up to whole vCPUs, and may be grown to `budget.elastic`'s maxima. A
/// document that leaves either to discovery is refused: a guest has no machine to discover
/// until it is booted with one.
pub fn vm_budget(spec: &crate::job::JobSpec) -> Result<moruna_vmm::Budget, crate::job::SpecError> {
    use crate::job::SpecError;
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

/// [`run_vm`] with the guest's CID, the relay's output and the monitor chosen by the caller:
/// the real monitor is [`moruna_vmm::boot`], and a test passes one that plays the guest.
pub fn run_vm_with(
    spec: &Path,
    image: &Path,
    disks: &[DiskConfig],
    cid: u32,
    out: &mut dyn Write,
    monitor: fn(&VmConfig) -> moruna_vmm::error::Result<i32>,
) -> i32 {
    let doc = match std::fs::read_to_string(spec)
        .map_err(|e| crate::job::SpecError::new("spec", format!("{}: {e}", spec.display())))
        .and_then(|text| crate::job::JobSpec::from_json(&text))
        .and_then(|doc| vm_budget(&doc))
    {
        Ok(budget) => budget,
        Err(refused) => {
            eprintln!("moruna run --vm: {refused}");
            return exit::SPEC_REFUSED;
        }
    };
    match moruna_vmm::session::boot_and_run_with(spec, image, disks, doc, cid, out, monitor) {
        Ok(status) => {
            eprintln!(
                "moruna run --vm: exit {} after {} messages from the guest",
                status.code, status.messages
            );
            status.code
        }
        Err(error) => {
            eprintln!("moruna run --vm: {error}");
            error.exit_code()
        }
    }
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
                strict: false
            })
        );
        assert_eq!(
            parse(&args(&["run", "--strict", "job.json"])),
            Ok(Command::Run {
                spec: "job.json".into(),
                strict: true
            })
        );
        assert_eq!(
            parse(&args(&["serve", "--listen", "vsock://-1:5000"])),
            Ok(Command::Serve {
                listen: Address::parse("vsock://-1:5000").expect("address"),
                strict: true
            })
        );
        assert_eq!(
            parse(&args(&["serve", "--no-strict", "--listen", "/tmp/m.sock"])),
            Ok(Command::Serve {
                listen: Address::parse("/tmp/m.sock").expect("address"),
                strict: false
            })
        );
        assert_eq!(
            parse(&args(&["serve", "--strict", "--listen", "/tmp/m.sock"]))
                .map(|c| matches!(c, Command::Serve { strict: true, .. })),
            Ok(true)
        );
        assert_eq!(
            parse(&args(&[
                "run", "--vm", "job.json", "--image", "/img", "--disk", "/d1:ro", "--disk", "/d2",
            ])),
            Ok(Command::RunVm {
                spec: "job.json".into(),
                image: "/img".into(),
                disks: vec![
                    DiskConfig {
                        path: "/d1".into(),
                        read_only: true
                    },
                    DiskConfig {
                        path: "/d2".into(),
                        read_only: false
                    },
                ],
            })
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
        let loader = crate::job::NoKernels;
        let env = |_: &str| None;
        let run = |a: &[&str]| main_with(&args(a), &loader, CancelToken::new(), &env, HEARTBEAT);
        assert_eq!(run(&["--version"]), exit::COMPLETED);
        assert_eq!(run(&["--help"]), exit::COMPLETED);
        assert_eq!(run(&["frobnicate"]), exit::SPEC_REFUSED);
        assert_eq!(
            main(
                &args(&["run", "/no/such/moruna/spec.json"]),
                &loader,
                CancelToken::new()
            ),
            exit::SPEC_REFUSED
        );
    }

    /// A monitor that plays the guest: it serves the vsock device's host socket the way the
    /// muxer does, checks the budget it was booted with and the disk it was given, takes the
    /// `spec` line and answers as Moruna would, with exit code 4.
    fn guest(c: &VmConfig) -> moruna_vmm::error::Result<i32> {
        use std::io::{BufRead, BufReader, Write};
        assert_eq!(c.memory_bytes, 512 << 20);
        assert_eq!(c.memory_max_bytes, 1 << 30);
        assert_eq!((c.cpus, c.cpus_max), (2, 4));
        assert_eq!(c.disks.len(), 1);
        assert!(c.disks[0].read_only);
        let listener = std::os::unix::net::UnixListener::bind(&c.vsock.uds_path).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut conn = BufReader::new(stream);
        let mut line = String::new();
        conn.read_line(&mut line).unwrap();
        assert_eq!(line, "CONNECT 5000\n");
        conn.get_mut().write_all(b"OK 1073741824\n").unwrap();
        line.clear();
        conn.read_line(&mut line).unwrap();
        let spec: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(spec["type"], "spec");
        assert_eq!(spec["spec"]["budget"]["memory_bytes"], 512 << 20);
        conn.get_mut()
            .write_all(b"{\"type\":\"hello\"}\n{\"type\":\"exit\",\"code\":4}\n")
            .unwrap();
        let _ = std::fs::remove_file(&c.vsock.uds_path);
        Ok(4)
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("moruna-vm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("img")).unwrap();
        dir
    }

    /// `moruna run --vm` boots the guest at the document's budget with the disks it was given,
    /// sends the document, relays what Moruna says and exits with Moruna's own code (MH 4.8.4),
    /// through the same seam the KVM monitor is called through.
    #[test]
    fn run_vm_boots_at_the_documents_budget_and_relays_the_guest() {
        let dir = scratch("relay");
        let spec = dir.join("job.json");
        std::fs::write(
            &spec,
            serde_json::json!({
                "moruna_spec": 1,
                "source": {"kind": "parquet", "url": "/data/in.parquet"},
                "sink": {"kind": "parquet", "url": "/data/out"},
                "budget": {"memory_bytes": 512u64 << 20, "cpu": 1.5,
                           "elastic": {"memory_max_bytes": 1u64 << 30, "cpu_max": 4}},
            })
            .to_string(),
        )
        .unwrap();
        let disk = dir.join("data.img");
        std::fs::write(&disk, vec![0u8; 4096]).unwrap();
        let disks = vec![DiskConfig {
            path: disk,
            read_only: true,
        }];
        let mut out = Vec::new();
        let cid = 300_000 + std::process::id();
        let code = run_vm_with(&spec, &dir.join("img"), &disks, cid, &mut out, guest);
        assert_eq!(code, 4, "Moruna's own code");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"type\":\"hello\"}\n{\"type\":\"exit\",\"code\":4}\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A document that leaves the budget to discovery, or is not a document, is refused before
    /// a guest is booted, by the field; a monitor that fails reports its own code.
    #[test]
    fn run_vm_refuses_what_it_cannot_boot() {
        fn never(_: &VmConfig) -> moruna_vmm::error::Result<i32> {
            panic!("no guest is booted for a refused document")
        }
        fn fails(_: &VmConfig) -> moruna_vmm::error::Result<i32> {
            Err(moruna_vmm::error::VmmError::NoKvm("absent".into()))
        }
        let dir = scratch("refuse");
        let spec = dir.join("job.json");
        let doc = |budget: serde_json::Value| {
            serde_json::json!({
                "moruna_spec": 1,
                "source": {"kind": "parquet", "url": "/data/in.parquet"},
                "sink": {"kind": "parquet", "url": "/data/out"},
                "budget": budget,
            })
            .to_string()
        };
        let img = dir.join("img");
        let mut out = Vec::new();
        for (budget, field) in [
            (serde_json::json!({"cpu": 1.0}), "budget.memory_bytes"),
            (
                serde_json::json!({"memory_bytes": 512u64 << 20}),
                "budget.cpu",
            ),
            (
                serde_json::json!({"memory_bytes": 512u64 << 20, "cpu": 0.0}),
                "budget.cpu",
            ),
        ] {
            std::fs::write(&spec, doc(budget)).unwrap();
            let parsed =
                crate::job::JobSpec::from_json(&std::fs::read_to_string(&spec).unwrap()).unwrap();
            assert_eq!(vm_budget(&parsed).unwrap_err().field, field);
            assert_eq!(
                run_vm_with(&spec, &img, &[], 1, &mut out, never),
                exit::SPEC_REFUSED
            );
        }
        std::fs::write(&spec, "not json").unwrap();
        assert_eq!(
            run_vm_with(&spec, &img, &[], 1, &mut out, never),
            exit::SPEC_REFUSED
        );
        assert_eq!(
            run_vm_with(&dir.join("absent.json"), &img, &[], 1, &mut out, never),
            exit::SPEC_REFUSED
        );
        std::fs::write(
            &spec,
            doc(serde_json::json!({"memory_bytes": 512u64 << 20, "cpu": 1.0})),
        )
        .unwrap();
        let cid = 400_000 + std::process::id();
        assert_eq!(
            run_vm_with(&spec, &img, &[], cid, &mut out, fails),
            moruna_vmm::error::EXIT_CONFIG
        );
        assert!(out.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Off Linux, `moruna run --vm` is refused by name before anything is read.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn run_vm_off_linux_is_refused_by_name() {
        let loader = crate::job::NoKernels;
        let env = |_: &str| None;
        let code = main_with(
            &args(&["run", "--vm", "/no/such/spec.json", "--image", "/no/img"]),
            &loader,
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
