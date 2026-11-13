//! Explicit native descriptor launcher. No Linux image/OCI/resize compatibility path.
use crate::{
    Budget,
    error::{Result, VmmError},
    jail::Cgroup,
    microkernel,
};
use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};
/// Exact native command inputs.
#[derive(Debug, PartialEq, Eq)]
pub struct Boot {
    /// Canonical checked native descriptor.
    pub descriptor: PathBuf,
    /// New private owned output directory.
    pub runtime_dir: PathBuf,
    /// Explicit installed QEMU executable.
    pub qemu: PathBuf,
    /// Explicit installed native vhost-vsock helper.
    pub vsock_backend: PathBuf,
    /// Finite node lifetime; termination signals can stop it earlier.
    pub lifetime: Duration,
    /// The delegated cgroup the run's leaf is made under, or none.
    pub cgroup: Cgroup,
}
/// The supported production command.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Boot the actual native artifact and supervise its processes.
    Boot(Boot),
    /// Print native usage.
    Help,
    /// Print this binary's version.
    Version,
}
/// Production usage; every artifact/tool/owner path is explicit.
pub const USAGE: &str = "moruna-vmm boot --descriptor FILE --runtime-dir NEW_DIR --qemu FILE --vsock-backend FILE --lifetime-seconds N --cgroup DIR|none\n";
fn bad(message: impl Into<String>) -> VmmError {
    VmmError::config("native command", message)
}
/// Parse only the native contract. Retired options are errors, never guessed aliases.
pub fn parse(args: &[String]) -> Result<Command> {
    if args == ["--help"] || args == ["help"] {
        return Ok(Command::Help);
    }
    if args == ["--version"] {
        return Ok(Command::Version);
    }
    if args.first().map(String::as_str) != Some("boot") {
        return Err(bad("expected native boot command"));
    }
    let mut fields = std::collections::BTreeMap::new();
    for pair in args[1..].chunks(2) {
        if pair.len() != 2
            || ![
                "--descriptor",
                "--runtime-dir",
                "--qemu",
                "--vsock-backend",
                "--lifetime-seconds",
                "--cgroup",
            ]
            .contains(&pair[0].as_str())
            || pair[1].is_empty()
            || fields.insert(pair[0].as_str(), pair[1].as_str()).is_some()
        {
            return Err(bad("unknown, duplicate or incomplete native option"));
        }
    }
    let get = |name| {
        fields
            .get(name)
            .copied()
            .ok_or_else(|| bad(format!("required {name}")))
    };
    let seconds = get("--lifetime-seconds")?
        .parse::<u64>()
        .map_err(|_| bad("invalid lifetime"))?;
    if !(1..=86_400).contains(&seconds) {
        return Err(bad("lifetime must be 1..86400 seconds"));
    }
    Ok(Command::Boot(Boot {
        descriptor: get("--descriptor")?.into(),
        runtime_dir: get("--runtime-dir")?.into(),
        qemu: get("--qemu")?.into(),
        vsock_backend: get("--vsock-backend")?.into(),
        lifetime: Duration::from_secs(seconds),
        cgroup: Cgroup::parse(get("--cgroup")?)?,
    }))
}
fn boot(command: Boot, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    let termination = crate::sys::Termination::install()
        .map_err(|e| VmmError::io("install", "owned CLI signals", &e))?;
    let descriptor = microkernel::inspect(&command.descriptor)?;
    let resources = descriptor.resources;
    let budget = Budget {
        memory_bytes: resources.job_memory_bytes,
        memory_max_bytes: resources.job_memory_bytes,
        cpus: resources.cpus,
        cpus_max: resources.cpus,
    };
    let mut node = microkernel::launch(
        &command.descriptor,
        budget,
        &command.runtime_dir,
        &command.qemu,
        &command.vsock_backend,
        &command.cgroup,
    )?;
    let deadline = Instant::now() + command.lifetime;
    loop {
        if let Some(status) = node.try_wait()? {
            // The run's directory, logs included, goes with the run: say what they hold first.
            write!(err, "{}", node.logs()).map_err(|e| VmmError::io("write", "node logs", &e))?;
            node.shutdown()?;
            let code = status
                .code()
                .ok_or_else(|| bad("node emulator terminated by signal"))?;
            writeln!(out, "node emulator exited with code {code}")
                .map_err(|e| VmmError::io("write", "node outcome", &e))?;
            return Ok(code);
        }
        if let Some(signal) = termination.signal() {
            node.shutdown()?;
            writeln!(out, "node stopped after signal {signal}")
                .map_err(|e| VmmError::io("write", "node outcome", &e))?;
            return Ok(128 + signal);
        }
        if Instant::now() >= deadline {
            node.shutdown()?;
            writeln!(out, "node stopped at configured lifetime")
                .map_err(|e| VmmError::io("write", "node outcome", &e))?;
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
/// Execute the actual native command; no guest job result is inferred from emulator exit.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match parse(args) {
        Ok(Command::Help) => return if write!(out, "{USAGE}").is_ok() { 0 } else { 1 },
        Ok(Command::Version) => {
            return if writeln!(out, "{}", env!("CARGO_PKG_VERSION")).is_ok() {
                0
            } else {
                1
            };
        }
        Ok(Command::Boot(command)) => boot(command, out, err),
        Err(error) => Err(error),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            error.exit_code()
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }
    #[test]
    fn run_answers_help_and_version_and_refuses_with_a_named_error() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&args("--help"), &mut out, &mut err), 0);
        assert_eq!(String::from_utf8(out.clone()).unwrap(), USAGE);
        out.clear();
        assert_eq!(run(&args("--version"), &mut out, &mut err), 0);
        assert_eq!(
            String::from_utf8(out.clone()).unwrap().trim(),
            env!("CARGO_PKG_VERSION")
        );
        out.clear();
        assert_ne!(run(&args("start --now"), &mut out, &mut err), 0);
        assert!(
            String::from_utf8(err.clone())
                .unwrap()
                .contains("expected native boot command")
        );
        err.clear();
        // A descriptor that does not exist is refused before any emulator or directory exists.
        let missing =
            std::env::temp_dir().join(format!("m-cli-absent-{}.json", std::process::id()));
        let runtime = std::env::temp_dir().join(format!("m-cli-run-{}", std::process::id()));
        let line = format!(
            "boot --descriptor {} --runtime-dir {} --qemu /bin/false --vsock-backend /bin/false --lifetime-seconds 5 --cgroup none",
            missing.display(),
            runtime.display()
        );
        let code = run(&args(&line), &mut out, &mut err);
        assert_ne!(code, 0);
        assert!(!err.is_empty(), "the refusal is named");
        assert!(!runtime.exists(), "nothing was created for a refused boot");
    }

    #[test]
    fn options_outside_the_contract_and_bad_lifetimes_are_refused() {
        for line in [
            "boot --descriptor a --descriptor b",
            "boot --descriptor",
            "boot --memory 4G",
            "boot --descriptor a --runtime-dir b --qemu c --vsock-backend d --lifetime-seconds 0 --cgroup none",
            "boot --descriptor a --runtime-dir b --qemu c --vsock-backend d --lifetime-seconds 86401 --cgroup none",
            "boot --descriptor a --runtime-dir b --qemu c --vsock-backend d --lifetime-seconds soon --cgroup none",
            "boot --runtime-dir b --qemu c --vsock-backend d --lifetime-seconds 5 --cgroup none",
        ] {
            assert!(parse(&args(line)).is_err(), "{line}");
        }
        assert_eq!(parse(&args("help")).unwrap(), Command::Help);
    }

    #[test]
    fn native_command_requires_actual_artifacts_tools_owner_and_finite_lifetime() {
        let valid = "boot --descriptor node.json --runtime-dir new --qemu qemu --vsock-backend helper --lifetime-seconds 30 --cgroup none";
        assert!(matches!(
            parse(&args(valid)).unwrap(),
            Command::Boot(Boot {
                cgroup: Cgroup::Undelegated,
                ..
            })
        ));
        let delegated = valid.replace("--cgroup none", "--cgroup /sys/fs/cgroup/griot.service");
        assert!(matches!(
            parse(&args(&delegated)).unwrap(),
            Command::Boot(Boot { cgroup: Cgroup::Delegated(path), .. }) if path == std::path::Path::new("/sys/fs/cgroup/griot.service")
        ));
        // Where the run is accounted is always said: never assumed, never relative.
        assert!(parse(&args(&valid.replace(" --cgroup none", ""))).is_err());
        assert!(
            parse(&args(
                &valid.replace("--cgroup none", "--cgroup griot.service")
            ))
            .is_err()
        );
        for suffix in [
            "--image old",
            "--disk root",
            "--descriptor duplicate",
            "--memory 123",
        ] {
            assert!(parse(&args(&format!("{valid} {suffix}"))).is_err());
        }
        for command in [
            "boot --image old",
            "resize --cpus 2",
            "stop --control old",
            "boot --descriptor node.json",
        ] {
            assert!(parse(&args(command)).is_err());
        }
        assert!(parse(&args(&valid.replace("seconds 30", "seconds 0"))).is_err());
    }
}
