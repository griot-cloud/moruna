//! Shared native session resource types and the actual vsock-helper CONNECT transport.
use crate::error::{Result, VmmError};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};
/// Maximum native service connection wait; caller deadlines may shorten it.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounded delay between real guest connection attempts.
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

/// Convert the already captured canonical job bytes without rereading a mutable
/// file between budget admission and guest delivery.
pub fn spec_value(bytes: &[u8]) -> Result<serde_json::Value> {
    if bytes.get(4..8) == Some(b"MJB2".as_slice()) {
        use base64::Engine as _;
        return Ok(serde_json::Value::String(
            base64::engine::general_purpose::STANDARD.encode(bytes),
        ));
    }
    serde_json::from_slice(bytes)
        .map_err(|e| VmmError::config("spec", format!("document is not JSON: {e}")))
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
        if Instant::now() >= deadline || !alive() {
            return Err(VmmError::Control(
                "native service connect deadline/owner expired".into(),
            ));
        }
        if let Some(s) = try_connect(uds, port, deadline) {
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
fn try_connect(uds: &Path, port: u32, deadline: Instant) -> Option<BufReader<UnixStream>> {
    let mut s = UnixStream::connect(uds).ok()?;
    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(5));
    if timeout.is_zero() {
        return None;
    }
    s.set_read_timeout(Some(timeout)).ok()?;
    s.set_write_timeout(Some(timeout)).ok()?;
    s.write_all(format!("CONNECT {port}\n").as_bytes()).ok()?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    loop {
        let bytes = r.fill_buf().ok()?;
        if bytes.is_empty() {
            return None;
        }
        let take = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        if line.len() + take > 256 {
            return None;
        }
        let done = bytes[take - 1] == b'\n';
        line.push_str(core::str::from_utf8(&bytes[..take]).ok()?);
        r.consume(take);
        if done {
            break;
        }
    }
    if !line.starts_with("OK ") {
        return None;
    }
    r.get_ref().set_read_timeout(None).ok()?;
    Some(r)
}
