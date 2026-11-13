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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    fn socket(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("m-vs-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// A vsock helper that answers each `CONNECT` with one scripted reply.
    fn helper(path: &Path, replies: Vec<&'static [u8]>) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            for reply in replies {
                let (mut s, _) = listener.accept().unwrap();
                let mut asked = [0u8; 64];
                let n = s.read(&mut asked).unwrap();
                assert!(asked[..n].starts_with(b"CONNECT 5000\n"));
                s.write_all(reply).unwrap();
                if reply.ends_with(b"\n") && reply.starts_with(b"OK ") {
                    s.write_all(b"hello\n").unwrap();
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        })
    }

    #[test]
    fn spec_bytes_are_json_or_canonical_binary() {
        assert_eq!(spec_value(br#"{"a":1}"#).unwrap()["a"], 1);
        assert!(spec_value(b"not json").is_err());
        let mut binary = vec![0u8; 16];
        binary[4..8].copy_from_slice(b"MJB2");
        assert!(spec_value(&binary).unwrap().is_string());
        assert!(
            ExitStatus {
                code: 0,
                messages: 1
            }
            .success()
        );
        assert!(
            !ExitStatus {
                code: 2,
                messages: 0
            }
            .success()
        );
    }

    #[test]
    fn connect_retries_until_the_guest_answers_ok() {
        let path = socket("ok");
        // First the guest is not listening (the helper closes), then a split OK line.
        let server = helper(&path, vec![b"", b"OK 1234\n"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut r = connect_guest(&path, 5000, deadline, &|| true).expect("connected");
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert_eq!(line, "hello\n");
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_refused_connect_an_overlong_answer_or_a_dead_owner_never_connects() {
        let path = socket("refused");
        let long: &'static [u8] = Box::leak(vec![b'x'; 300].into_boxed_slice());
        let server = helper(&path, vec![b"ERR no\n", long]);
        // Each refusal is retried until the short deadline; the owner is alive throughout.
        let deadline = Instant::now() + Duration::from_millis(400);
        assert!(connect_guest(&path, 5000, deadline, &|| true).is_err());
        drop(server);
        let _ = std::fs::remove_file(&path);
        // A VM that has gone is reported at once, with no socket at all.
        let missing = socket("missing");
        let gone = connect_guest(
            &missing,
            5000,
            Instant::now() + Duration::from_secs(5),
            &|| false,
        );
        assert!(matches!(gone, Err(VmmError::Control(_))));
        // An expired deadline with a live owner and no helper is a connect failure.
        assert!(connect_guest(&missing, 5000, Instant::now(), &|| true).is_err());
    }
}
