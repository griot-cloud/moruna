//! The `moruna` command line (MH 4.2): `run`, `serve`, `--version`. The binary is a
//! thin `main` over [`main`]; the Python adapter's binary passes a loader that imports Python
//! kernels, and a build without it passes [`crate::job::NoKernels`].

use std::path::Path;
use std::time::Duration;

use moruna_kernel::CancelToken;

use super::session::{self, HEARTBEAT, HostOptions, Outcome};
use super::transport::Address;
use super::{VERSION, exit};
use crate::job::KernelLoader;
use crate::job::build::process_env;

/// What `moruna` prints for a command line it does not understand.
pub const USAGE: &str = "usage: moruna run <spec.json> [--strict]
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
            for arg in rest {
                match arg {
                    "--strict" => strict = true,
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
            Ok(Command::Run { spec, strict })
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
