//! The command line (MH 4.8.4).
//!
//! ```text
//! moruna-vmm boot   --image <dir-or-oci> [--disk <path>[:ro|:rw]]... --memory <bytes>
//!                   [--memory-max <bytes>] --cpus <n> [--cpus-max <n>] --vsock <cid>
//!                   [--vsock-uds <path>] [--control <path>]
//! moruna-vmm resize (--control <path> | --vsock <cid>) (--memory <bytes> | --cpus <n>)
//! moruna-vmm status (--control <path> | --vsock <cid>)
//! moruna-vmm stop   (--control <path> | --vsock <cid>)
//! ```
//!
//! Parsed by hand: the preamble's dependency table has no argument parser, and the grammar is
//! a dozen flags (the bench agent made the same choice, preamble 6.2).

use std::path::PathBuf;

use crate::config::{DiskConfig, VmConfig, VsockConfig, default_control_path, default_vsock_path};
use crate::control::Request;
use crate::error::{Result, VmmError};

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Boot one guest and block until it exits.
    Boot(VmConfig),
    /// Send one request to a running monitor's control socket.
    Control {
        /// The control socket.
        socket: PathBuf,
        /// The request.
        request: Request,
    },
    /// Print usage and exit 0.
    Help,
    /// Print the version and exit 0.
    Version,
}

/// The usage text printed by `--help` and after a parse error.
pub const USAGE: &str = "\
usage:
  moruna-vmm boot   --image <dir-or-oci> [--disk <path>[:ro|:rw]]... --memory <bytes>
                    [--memory-max <bytes>] --cpus <n> [--cpus-max <n>] --vsock <cid>
                    [--vsock-uds <path>] [--control <path>]
  moruna-vmm resize (--control <path> | --vsock <cid>) (--memory <bytes> | --cpus <n>)
  moruna-vmm status (--control <path> | --vsock <cid>)
  moruna-vmm stop   (--control <path> | --vsock <cid>)
bytes accept a K, M, G or T suffix (binary: 1G = 1073741824), with or without \"iB\".
exit: the guest's code; 2 bad configuration or no /dev/kvm; 6 guest kernel panic;
      7 guest stopped without reporting a code; 8 monitor or hypervisor failure.";

/// Parse `args` (without the program name).
pub fn parse(args: &[String]) -> Result<Command> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(VmmError::config(
            "command",
            "missing; one of boot, resize, status, stop",
        ));
    };
    match sub.as_str() {
        "boot" => parse_boot(rest).map(Command::Boot),
        "resize" => parse_resize(rest),
        "status" => parse_simple(rest, Request::Status, "status"),
        "stop" => parse_simple(rest, Request::Stop, "stop"),
        "-h" | "--help" | "help" => Ok(Command::Help),
        "-V" | "--version" => Ok(Command::Version),
        other => Err(VmmError::config(
            "command",
            format!("unknown command {other:?}; one of boot, resize, status, stop"),
        )),
    }
}

/// Flag/value pairs, in order; every flag takes exactly one value.
fn pairs(args: &[String]) -> Result<Vec<(&str, &str)>> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        if !flag.starts_with("--") {
            return Err(VmmError::config(
                "arguments",
                format!("unexpected {flag:?}; every argument is a --flag with a value"),
            ));
        }
        let (name, value) = match flag.split_once('=') {
            Some((n, v)) => (n, v),
            None => {
                let v = it.next().ok_or_else(|| {
                    VmmError::config("arguments", format!("{flag} needs a value"))
                })?;
                (flag.as_str(), v.as_str())
            }
        };
        out.push((name, value));
    }
    Ok(out)
}

fn once<'a>(seen: &mut Vec<&'a str>, flag: &'a str) -> Result<()> {
    if seen.contains(&flag) {
        return Err(VmmError::config("arguments", format!("{flag} given twice")));
    }
    seen.push(flag);
    Ok(())
}

fn parse_boot(args: &[String]) -> Result<VmConfig> {
    let mut image = None;
    let mut disks = Vec::new();
    let mut memory = None;
    let mut memory_max = None;
    let mut cpus = None;
    let mut cpus_max = None;
    let mut cid = None;
    let mut uds = None;
    let mut control = None;
    let mut seen = Vec::new();
    for (flag, value) in pairs(args)? {
        if flag != "--disk" {
            once(&mut seen, flag)?;
        }
        match flag {
            "--image" => image = Some(PathBuf::from(value)),
            "--disk" => disks.push(parse_disk(value)?),
            "--memory" => memory = Some(parse_bytes("--memory", value)?),
            "--memory-max" => memory_max = Some(parse_bytes("--memory-max", value)?),
            "--cpus" => cpus = Some(parse_u32("--cpus", value)?),
            "--cpus-max" => cpus_max = Some(parse_u32("--cpus-max", value)?),
            "--vsock" => cid = Some(parse_u32("--vsock", value)?),
            "--vsock-uds" => uds = Some(PathBuf::from(value)),
            "--control" => control = Some(PathBuf::from(value)),
            other => {
                return Err(VmmError::config(
                    "arguments",
                    format!("unknown flag {other} for boot"),
                ));
            }
        }
    }
    let image = image.ok_or_else(|| VmmError::config("--image", "required"))?;
    let memory = memory.ok_or_else(|| VmmError::config("--memory", "required"))?;
    let cpus = cpus.ok_or_else(|| VmmError::config("--cpus", "required"))?;
    let cid = cid.ok_or_else(|| VmmError::config("--vsock", "required"))?;
    Ok(VmConfig {
        image,
        disks,
        memory_bytes: memory,
        memory_max_bytes: memory_max.unwrap_or(memory),
        cpus,
        cpus_max: cpus_max.unwrap_or(cpus),
        vsock: VsockConfig {
            cid,
            uds_path: uds.unwrap_or_else(|| default_vsock_path(cid)),
        },
        control_socket: control.unwrap_or_else(|| default_control_path(cid)),
    })
}

fn control_target(flag: &str, value: &str, socket: &mut Option<PathBuf>) -> Result<bool> {
    match flag {
        "--control" => *socket = Some(PathBuf::from(value)),
        "--vsock" => *socket = Some(default_control_path(parse_u32("--vsock", value)?)),
        _ => return Ok(false),
    }
    Ok(true)
}

fn parse_resize(args: &[String]) -> Result<Command> {
    let mut socket = None;
    let mut request = None;
    let mut seen = Vec::new();
    for (flag, value) in pairs(args)? {
        once(&mut seen, flag)?;
        if control_target(flag, value, &mut socket)? {
            continue;
        }
        let r = match flag {
            "--memory" => Request::ResizeMemory {
                bytes: parse_bytes("--memory", value)?,
            },
            "--cpus" => Request::ResizeCpus {
                cpus: parse_u32("--cpus", value)?,
            },
            other => {
                return Err(VmmError::config(
                    "arguments",
                    format!("unknown flag {other} for resize"),
                ));
            }
        };
        if request.replace(r).is_some() {
            return Err(VmmError::config(
                "resize",
                "one of --memory or --cpus per request, not both",
            ));
        }
    }
    Ok(Command::Control {
        socket: socket
            .ok_or_else(|| VmmError::config("--control", "required (or --vsock <cid>)"))?,
        request: request.ok_or_else(|| VmmError::config("resize", "--memory or --cpus"))?,
    })
}

fn parse_simple(args: &[String], request: Request, name: &str) -> Result<Command> {
    let mut socket = None;
    let mut seen = Vec::new();
    for (flag, value) in pairs(args)? {
        once(&mut seen, flag)?;
        if !control_target(flag, value, &mut socket)? {
            return Err(VmmError::config(
                "arguments",
                format!("unknown flag {flag} for {name}"),
            ));
        }
    }
    Ok(Command::Control {
        socket: socket
            .ok_or_else(|| VmmError::config("--control", "required (or --vsock <cid>)"))?,
        request,
    })
}

/// `path`, `path:ro` or `path:rw`; the default is read-write.
pub fn parse_disk(v: &str) -> Result<DiskConfig> {
    let (path, read_only) = if let Some(p) = v.strip_suffix(":ro") {
        (p, true)
    } else if let Some(p) = v.strip_suffix(":rw") {
        (p, false)
    } else {
        (v, false)
    };
    if path.is_empty() {
        return Err(VmmError::config("--disk", format!("{v:?} names no path")));
    }
    Ok(DiskConfig {
        path: PathBuf::from(path),
        read_only,
    })
}

/// A byte count: digits, optionally followed by `K`, `M`, `G` or `T` (binary multiples), with
/// or without `iB` or `B`.
pub fn parse_bytes(field: &'static str, v: &str) -> Result<u64> {
    let t = v.trim();
    let digits_end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, suffix) = t.split_at(digits_end);
    let n: u64 = num
        .parse()
        .map_err(|_| VmmError::config(field, format!("{v:?} is not a byte count")))?;
    let shift = match suffix {
        "" | "B" => 0,
        "K" | "KiB" | "KB" | "k" => 10,
        "M" | "MiB" | "MB" => 20,
        "G" | "GiB" | "GB" => 30,
        "T" | "TiB" | "TB" => 40,
        _ => {
            return Err(VmmError::config(
                field,
                format!("{v:?} has an unknown suffix {suffix:?}"),
            ));
        }
    };
    n.checked_mul(1u64 << shift)
        .ok_or_else(|| VmmError::config(field, format!("{v:?} overflows 64 bits")))
}

fn parse_u32(field: &'static str, v: &str) -> Result<u32> {
    v.trim()
        .parse()
        .map_err(|_| VmmError::config(field, format!("{v:?} is not a whole number")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GIB, MIB};

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn field_of(r: Result<Command>) -> &'static str {
        match r {
            Err(VmmError::Config { field, .. }) => field,
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn vm_t3_boot_parses_every_flag() {
        let c = parse(&args(
            "boot --image /img --disk /d1:ro --disk /d2 --disk /d3:rw --memory 1G \
             --memory-max 4GiB --cpus 2 --cpus-max 8 --vsock 42 --vsock-uds /tmp/v \
             --control=/tmp/c",
        ))
        .unwrap();
        let Command::Boot(c) = c else { panic!() };
        assert_eq!(c.image, PathBuf::from("/img"));
        assert_eq!(
            c.disks,
            vec![
                DiskConfig {
                    path: "/d1".into(),
                    read_only: true
                },
                DiskConfig {
                    path: "/d2".into(),
                    read_only: false
                },
                DiskConfig {
                    path: "/d3".into(),
                    read_only: false
                },
            ]
        );
        assert_eq!((c.memory_bytes, c.memory_max_bytes), (GIB, 4 * GIB));
        assert_eq!((c.cpus, c.cpus_max), (2, 8));
        assert_eq!(c.vsock.cid, 42);
        assert_eq!(c.vsock.uds_path, PathBuf::from("/tmp/v"));
        assert_eq!(c.control_socket, PathBuf::from("/tmp/c"));
    }

    #[test]
    fn vm_t3_boot_defaults_and_refusals() {
        let Command::Boot(c) =
            parse(&args("boot --image i --memory 512M --cpus 1 --vsock 3")).unwrap()
        else {
            panic!()
        };
        assert_eq!(c.memory_max_bytes, 512 * MIB);
        assert_eq!(c.cpus_max, 1);
        assert_eq!(c.vsock.uds_path, default_vsock_path(3));
        assert_eq!(c.control_socket, default_control_path(3));

        assert_eq!(
            field_of(parse(&args("boot --memory 1G --cpus 1 --vsock 3"))),
            "--image"
        );
        assert_eq!(
            field_of(parse(&args("boot --image i --cpus 1 --vsock 3"))),
            "--memory"
        );
        assert_eq!(
            field_of(parse(&args("boot --image i --memory 1G --vsock 3"))),
            "--cpus"
        );
        assert_eq!(
            field_of(parse(&args("boot --image i --memory 1G --cpus 1"))),
            "--vsock"
        );
        assert_eq!(
            field_of(parse(&args("boot --image i --net tap0"))),
            "arguments"
        );
        assert_eq!(
            field_of(parse(&args("boot --image i --image j"))),
            "arguments"
        );
        assert_eq!(field_of(parse(&args("boot --image"))), "arguments");
        assert_eq!(field_of(parse(&args("boot image"))), "arguments");
        assert_eq!(field_of(parse(&args("boot --cpus x"))), "--cpus");
        assert_eq!(field_of(parse(&args("boot --disk :ro"))), "--disk");
        assert_eq!(field_of(parse(&args(""))), "command");
        assert_eq!(field_of(parse(&args("start"))), "command");
    }

    #[test]
    fn vm_t3_resize_status_stop() {
        assert_eq!(
            parse(&args("resize --control /c --memory 2G")).unwrap(),
            Command::Control {
                socket: "/c".into(),
                request: Request::ResizeMemory { bytes: 2 * GIB }
            }
        );
        assert_eq!(
            parse(&args("resize --vsock 9 --cpus 3")).unwrap(),
            Command::Control {
                socket: default_control_path(9),
                request: Request::ResizeCpus { cpus: 3 }
            }
        );
        assert_eq!(
            parse(&args("status --vsock 9")).unwrap(),
            Command::Control {
                socket: default_control_path(9),
                request: Request::Status
            }
        );
        assert_eq!(
            parse(&args("stop --control /c")).unwrap(),
            Command::Control {
                socket: "/c".into(),
                request: Request::Stop
            }
        );
        assert_eq!(
            field_of(parse(&args("resize --control /c --memory 1G --cpus 2"))),
            "resize"
        );
        assert_eq!(field_of(parse(&args("resize --control /c"))), "resize");
        assert_eq!(field_of(parse(&args("resize --cpus 2"))), "--control");
        assert_eq!(
            field_of(parse(&args("resize --control /c --disk x"))),
            "arguments"
        );
        assert_eq!(field_of(parse(&args("status"))), "--control");
        assert_eq!(field_of(parse(&args("status --cpus 2"))), "arguments");
        assert_eq!(parse(&args("--help")).unwrap(), Command::Help);
        assert_eq!(parse(&args("-V")).unwrap(), Command::Version);
        assert!(USAGE.contains("resize"));
    }

    #[test]
    fn vm_t3_byte_counts() {
        for (s, v) in [
            ("4096", 4096),
            ("1K", 1024),
            ("1KiB", 1024),
            ("3M", 3 * MIB),
            ("2GiB", 2 * GIB),
            ("2GB", 2 * GIB),
            ("1T", 1 << 40),
            ("7B", 7),
        ] {
            assert_eq!(parse_bytes("--memory", s).unwrap(), v, "{s}");
        }
        for s in ["", "G", "1X", "-1", "99999999999999T"] {
            assert!(parse_bytes("--memory", s).is_err(), "{s}");
        }
    }
}
