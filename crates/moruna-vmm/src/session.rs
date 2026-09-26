//! `boot_and_run`: boot a guest, hand Moruna one spec over vsock, relay what it says, return
//! how it ended. This is the library seam `moruna run --vm` calls.
//!
//! The host protocol (MH 4.3) is F8.1's, in `moruna-runtime::host`. `moruna-vmm` depends on
//! `moruna-kernel` only (H-Q7), so it does not import those types; it speaks the one message it
//! must, the `spec` of `moruna_runtime::host::protocol::Inbound::Spec`,
//! `{"type":"spec","spec":<the spec file's JSON>}` and a newline, and relays every line Moruna
//! sends back (`hello`, heartbeats, `report`, `exit`) to the caller untouched. If the protocol
//! types move into `moruna-kernel`, [`spec_message`] is the one function to replace.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use moruna_kernel::MorunaError;

use crate::config::{DiskConfig, VmConfig};
use crate::error::{Result, VmmError};
use crate::machine::GUEST_AGENT_PORT;

/// How long to keep trying to reach Moruna in a booting guest.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Pause between connection attempts while the guest boots.
pub const CONNECT_RETRY: Duration = Duration::from_millis(20);

/// The resources the guest is booted with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Memory at boot, bytes.
    pub memory_bytes: u64,
    /// The most memory the host may grow the guest to, bytes.
    pub memory_max_bytes: u64,
    /// vCPUs at boot.
    pub cpus: u32,
    /// The most vCPUs the host may grow the guest to.
    pub cpus_max: u32,
}

/// How a VM run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitStatus {
    /// The monitor's exit code: Moruna's own code when it reported one (MH 4.2), else 2, 6, 7
    /// or 8 as [`crate::error`] defines them.
    pub code: i32,
    /// Lines Moruna sent over the host protocol, in order.
    pub messages: usize,
}

impl ExitStatus {
    /// True when Moruna completed its run.
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

/// The `spec` message for a spec file's JSON.
pub fn spec_message(spec: &serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::json!({"type": "spec", "spec": spec})
        .to_string()
        .into_bytes();
    line.push(b'\n');
    line
}

/// Read and parse the spec file.
pub fn read_spec(path: &Path) -> Result<serde_json::Value> {
    let bytes = std::fs::read(path)
        .map_err(|e| VmmError::config("spec", format!("{}: {e}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| VmmError::config("spec", format!("{} is not JSON: {e}", path.display())))
}

/// Connect to guest port `port` through the vsock device's host socket, retrying while the
/// guest boots, until `deadline` or until `alive` says the VM has gone.
pub fn connect_guest(
    uds: &Path,
    port: u32,
    deadline: Instant,
    alive: &dyn Fn() -> bool,
) -> Result<BufReader<UnixStream>> {
    loop {
        if let Some(s) = try_connect(uds, port) {
            return Ok(s);
        }
        if Instant::now() >= deadline || !alive() {
            return Err(VmmError::Io {
                op: "connect",
                target: format!("{} port {port}", uds.display()),
                msg: "Moruna did not start listening in the guest".into(),
            });
        }
        std::thread::sleep(CONNECT_RETRY);
    }
}

/// One attempt: `CONNECT <port>` and an `OK` line back, or nothing (the guest is not
/// listening yet, so the monitor closed the stream).
fn try_connect(uds: &Path, port: u32) -> Option<BufReader<UnixStream>> {
    let mut s = UnixStream::connect(uds).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(format!("CONNECT {port}\n").as_bytes()).ok()?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    if !line.starts_with("OK ") {
        return None;
    }
    r.get_ref().set_read_timeout(None).ok()?;
    Some(r)
}

/// Send the spec and relay every line to `out` until Moruna closes the connection; returns
/// the number of lines relayed.
pub fn drive(
    mut conn: BufReader<UnixStream>,
    spec: &serde_json::Value,
    out: &mut dyn Write,
) -> Result<usize> {
    conn.get_mut()
        .write_all(&spec_message(spec))
        .map_err(|e| VmmError::io("write", "spec", &e))?;
    let mut n = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        match conn.read_until(b'\n', &mut line) {
            Ok(0) => return Ok(n),
            Ok(_) => {
                out.write_all(&line)
                    .map_err(|e| VmmError::io("write", "relay", &e))?;
                n += 1;
            }
            Err(e) => return Err(VmmError::io("read", "guest", &e)),
        }
    }
}

/// A guest CID for this process: 3 plus the process id, which is unique on the host while
/// this process lives.
pub fn default_cid() -> u32 {
    3 + std::process::id()
}

/// Boot a guest with `image` and `disks` at `budget`, run the spec at `spec_path` in it, relay
/// Moruna's messages to stdout, and return how the run ended. The seam for `moruna run --vm`.
pub fn boot_and_run(
    spec_path: &Path,
    image: &Path,
    disks: &[DiskConfig],
    budget: Budget,
) -> std::result::Result<ExitStatus, MorunaError> {
    boot_and_run_with(
        spec_path,
        image,
        disks,
        budget,
        default_cid(),
        &mut std::io::stdout(),
        crate::boot,
    )
    .map_err(MorunaError::from)
}

/// [`boot_and_run`] with the CID, the output and the monitor chosen by the caller (tests pass
/// a fake monitor; the real one is [`crate::boot`]).
pub fn boot_and_run_with(
    spec_path: &Path,
    image: &Path,
    disks: &[DiskConfig],
    budget: Budget,
    cid: u32,
    out: &mut dyn Write,
    monitor: fn(&VmConfig) -> Result<i32>,
) -> Result<ExitStatus> {
    let spec = read_spec(spec_path)?;
    let mut config = VmConfig::with_defaults(
        PathBuf::from(image),
        disks.to_vec(),
        budget.memory_bytes,
        budget.cpus,
        cid,
    );
    config.memory_max_bytes = budget.memory_max_bytes;
    config.cpus_max = budget.cpus_max;
    config.validate()?;

    let uds = config.vsock.uds_path.clone();
    let vm = std::thread::Builder::new()
        .name("moruna-vmm".into())
        .spawn(move || monitor(&config))
        .map_err(|e| VmmError::io("spawn", "monitor", &e))?;
    let alive = || !vm.is_finished();
    let relayed = connect_guest(
        &uds,
        GUEST_AGENT_PORT,
        Instant::now() + CONNECT_TIMEOUT,
        &alive,
    )
    .and_then(|conn| drive(conn, &spec, out));
    let code = vm
        .join()
        .map_err(|_| VmmError::Control("the monitor thread panicked".into()))??;
    Ok(ExitStatus {
        code,
        messages: relayed.unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MIB;
    use crate::testing::scratch_dir;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    /// A fake monitor: serves the vsock host socket the way the muxer does, refusing the
    /// first connection (the guest is "still booting"), then plays Moruna.
    fn fake_monitor(c: &VmConfig) -> Result<i32> {
        let l = UnixListener::bind(&c.vsock.uds_path).unwrap();
        // Booting: the first attempt is closed without an OK.
        let (mut s, _) = l.accept().unwrap();
        let mut b = [0u8; 13];
        s.read_exact(&mut b).unwrap();
        drop(s);
        let (s, _) = l.accept().unwrap();
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert_eq!(line, "CONNECT 5000\n");
        r.get_mut().write_all(b"OK 1073741824\n").unwrap();
        line.clear();
        r.read_line(&mut line).unwrap();
        let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(msg["type"], "spec");
        assert_eq!(msg["spec"]["moruna_spec"], 1);
        r.get_mut()
            .write_all(b"{\"type\":\"hello\"}\n{\"type\":\"exit\",\"code\":4}\n")
            .unwrap();
        Ok(4)
    }

    fn never_listens(_: &VmConfig) -> Result<i32> {
        std::thread::sleep(Duration::from_millis(50));
        Ok(crate::error::EXIT_NO_CODE)
    }

    fn fails(_: &VmConfig) -> Result<i32> {
        Err(VmmError::NoKvm("absent".into()))
    }

    fn setup() -> (PathBuf, PathBuf, Budget) {
        let dir = scratch_dir("session");
        let spec = dir.join("spec.json");
        std::fs::write(&spec, br#"{"moruna_spec": 1}"#).unwrap();
        let img = dir.join("img");
        std::fs::create_dir_all(&img).unwrap();
        let budget = Budget {
            memory_bytes: 512 * MIB,
            memory_max_bytes: 1024 * MIB,
            cpus: 1,
            cpus_max: 2,
        };
        (spec, img, budget)
    }

    #[test]
    fn vm_t29_boot_and_run_sends_the_spec_and_relays_moruna() {
        let (spec, img, budget) = setup();
        let mut out = Vec::new();
        let cid = 100_000 + std::process::id();
        let st = boot_and_run_with(&spec, &img, &[], budget, cid, &mut out, fake_monitor).unwrap();
        assert_eq!(
            st,
            ExitStatus {
                code: 4,
                messages: 2
            }
        );
        assert!(!st.success());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"type\":\"hello\"}\n{\"type\":\"exit\",\"code\":4}\n"
        );
    }

    #[test]
    fn vm_t29_a_guest_that_never_listens_still_returns_the_monitor_code() {
        let (spec, img, budget) = setup();
        let cid = 200_000 + std::process::id();
        let st = boot_and_run_with(
            &spec,
            &img,
            &[],
            budget,
            cid,
            &mut Vec::new(),
            never_listens,
        )
        .unwrap();
        assert_eq!(st.code, crate::error::EXIT_NO_CODE);
        assert_eq!(st.messages, 0);
        // A monitor that fails reports its error.
        let e = boot_and_run_with(&spec, &img, &[], budget, cid + 1, &mut Vec::new(), fails)
            .unwrap_err();
        assert_eq!(e.exit_code(), crate::error::EXIT_CONFIG);
    }

    #[test]
    fn vm_t29_spec_and_budget_are_checked_first() {
        let (spec, img, budget) = setup();
        let bad = spec.with_file_name("bad.json");
        std::fs::write(&bad, b"not json").unwrap();
        for p in [bad.as_path(), Path::new("/nonexistent/spec.json")] {
            let e = boot_and_run(p, &img, &[], budget).unwrap_err();
            assert!(e.to_string().contains("config spec"), "{e}");
        }
        let small = Budget {
            memory_bytes: MIB,
            ..budget
        };
        let e = boot_and_run(&spec, &img, &[], small).unwrap_err();
        assert!(e.to_string().contains("--memory"), "{e}");
        assert_eq!(
            spec_message(&serde_json::json!({"a": 1})),
            b"{\"spec\":{\"a\":1},\"type\":\"spec\"}\n".to_vec()
        );
        assert!(default_cid() > 2);
    }

    #[test]
    fn vm_t29_connect_gives_up_when_the_vm_is_gone() {
        let dir = scratch_dir("conn");
        let uds = dir.join("absent.sock");
        let e = connect_guest(&uds, 5000, Instant::now() + Duration::from_secs(5), &|| {
            false
        })
        .unwrap_err();
        assert!(e.to_string().contains("did not start listening"));
        let e = connect_guest(&uds, 5000, Instant::now(), &|| true).unwrap_err();
        assert!(matches!(e, VmmError::Io { .. }));
        // A reply that is not OK is a refusal.
        let l = UnixListener::bind(dir.join("v.sock")).unwrap();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut b = [0u8; 13];
            s.read_exact(&mut b).unwrap();
            s.write_all(b"NO\n").unwrap();
        });
        assert!(try_connect(&dir.join("v.sock"), 5000).is_none());
        t.join().unwrap();
    }
}
