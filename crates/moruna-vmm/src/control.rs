//! The monitor's control socket: newline-delimited JSON over a Unix socket, one request per
//! line, one response per request.
//!
//! ```text
//! -> {"op":"resize_memory","bytes":4294967296}
//! <- {"ok":true,"status":{"memory_bytes":...,"memory_max_bytes":...,...}}
//! -> {"op":"resize_cpus","cpus":4}
//! -> {"op":"status"}
//! -> {"op":"stop"}
//! <- {"ok":false,"error":"..."}
//! ```
//!
//! What a peer here can do is bounded by what boot fixed (MH 4.8.4): resize within
//! `--memory-max` and `--cpus-max`, read the status, stop the guest. There is no request that
//! adds a device, and none that reads or writes guest memory or vsock traffic.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::devices::StopSignal;
use crate::error::{Result, VmmError};

/// Longest request line accepted; a longer one is refused and the connection closed.
pub const LINE_MAX: usize = 4096;
/// How long the client waits for the monitor's answer, and the server for a request line.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the server checks whether the guest has stopped while idle.
pub const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// A request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Set the guest's total memory to `bytes` (boot memory plus hot-plugged).
    ResizeMemory {
        /// The new total, bytes.
        bytes: u64,
    },
    /// Set the number of vCPUs to `cpus` (hot-add only).
    ResizeCpus {
        /// The new count.
        cpus: u32,
    },
    /// Report the current state.
    Status,
    /// Stop the guest now; `boot` returns 7.
    Stop,
}

/// What the monitor reports about the guest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// Boot memory plus the memory the host asked the guest to hold.
    pub memory_bytes: u64,
    /// Boot memory plus what the guest has actually plugged.
    pub memory_plugged_bytes: u64,
    /// The boot maximum.
    pub memory_max_bytes: u64,
    /// vCPUs the host asked for.
    pub cpus: u32,
    /// The boot maximum.
    pub cpus_max: u32,
    /// True once the guest has stopped.
    pub stopped: bool,
}

/// A response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// Whether the request was carried out.
    pub ok: bool,
    /// The state after the request, when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// Why not, when not `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    fn from_result(r: Result<Status>) -> Self {
        match r {
            Ok(s) => Response {
                ok: true,
                status: Some(s),
                error: None,
            },
            Err(e) => Response {
                ok: false,
                status: None,
                error: Some(e.to_string()),
            },
        }
    }
}

/// What the control socket drives: the running VM, or a fake in tests.
pub trait Controller: Send + Sync {
    /// Resize memory to a total of `bytes`.
    fn resize_memory(&self, bytes: u64) -> Result<Status>;
    /// Resize to `cpus` vCPUs.
    fn resize_cpus(&self, cpus: u32) -> Result<Status>;
    /// The current state.
    fn status(&self) -> Status;
    /// Stop the guest.
    fn stop(&self);
}

/// Carry out one request.
pub fn handle(c: &dyn Controller, r: &Request) -> Response {
    Response::from_result(match r {
        Request::ResizeMemory { bytes } => c.resize_memory(*bytes),
        Request::ResizeCpus { cpus } => c.resize_cpus(*cpus),
        Request::Status => Ok(c.status()),
        Request::Stop => {
            c.stop();
            Ok(c.status())
        }
    })
}

/// The control socket server.
pub struct ControlServer {
    listener: UnixListener,
    path: PathBuf,
}

impl ControlServer {
    /// Bind at `path`, replacing a stale socket file; refuses a path that is not a socket and
    /// a socket another monitor is serving.
    pub fn bind(path: &Path) -> Result<Self> {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            use std::os::unix::fs::FileTypeExt;
            if !meta.file_type().is_socket() {
                return Err(VmmError::config(
                    "--control",
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
            if UnixStream::connect(path).is_ok() {
                return Err(VmmError::config(
                    "--control",
                    format!("{} is in use by another monitor", path.display()),
                ));
            }
            std::fs::remove_file(path)
                .map_err(|e| VmmError::io("remove", path.display().to_string(), &e))?;
        }
        let listener = UnixListener::bind(path)
            .map_err(|e| VmmError::config("--control", format!("{}: {e}", path.display())))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| VmmError::io("nonblocking", path.display().to_string(), &e))?;
        Ok(ControlServer {
            listener,
            path: path.to_path_buf(),
        })
    }

    /// Serve until `stop` is signalled; one client at a time.
    pub fn serve(&self, c: &dyn Controller, stop: &Arc<StopSignal>) {
        while !stop.is_stopped() {
            match self.listener.accept() {
                Ok((stream, _)) => serve_client(stream, c),
                Err(_) => std::thread::sleep(ACCEPT_POLL),
            }
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn serve_client(stream: UnixStream, c: &dyn Controller) {
    if stream.set_nonblocking(false).is_err()
        || stream.set_read_timeout(Some(CLIENT_TIMEOUT)).is_err()
    {
        return;
    }
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = Vec::new();
        match (&mut reader)
            .take(LINE_MAX as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let too_long = line.len() > LINE_MAX;
        let resp = if too_long {
            Response::from_result(Err(VmmError::Control(format!(
                "request longer than {LINE_MAX} bytes"
            ))))
        } else {
            match serde_json::from_slice::<Request>(&line) {
                Ok(r) => handle(c, &r),
                Err(e) => {
                    Response::from_result(Err(VmmError::Control(format!("bad request: {e}"))))
                }
            }
        };
        let Ok(mut text) = serde_json::to_vec(&resp) else {
            return;
        };
        text.push(b'\n');
        if out.write_all(&text).is_err() || too_long {
            return;
        }
    }
}

/// Send one request to the monitor at `path` and wait for its answer.
pub fn request(path: &Path, r: &Request) -> Result<Response> {
    let shown = path.display().to_string();
    let mut s = UnixStream::connect(path)
        .map_err(|e| VmmError::config("--control", format!("{shown}: {e}")))?;
    s.set_read_timeout(Some(CLIENT_TIMEOUT))
        .map_err(|e| VmmError::io("timeout", shown.clone(), &e))?;
    let mut line = serde_json::to_vec(r).map_err(|e| VmmError::Control(e.to_string()))?;
    line.push(b'\n');
    s.write_all(&line)
        .map_err(|e| VmmError::io("write", shown.clone(), &e))?;
    let mut answer = String::new();
    BufReader::new(s)
        .read_line(&mut answer)
        .map_err(|e| VmmError::io("read", shown.clone(), &e))?;
    serde_json::from_str(&answer)
        .map_err(|e| VmmError::Control(format!("bad answer from {shown}: {e}")))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::devices::StopReason;
    use crate::testing::scratch_dir;
    use std::sync::Mutex;

    pub(crate) struct Fake {
        pub s: Mutex<Status>,
        pub stop: Arc<StopSignal>,
    }

    impl Controller for Fake {
        fn resize_memory(&self, bytes: u64) -> Result<Status> {
            let mut s = self.s.lock().unwrap();
            if bytes > s.memory_max_bytes {
                return Err(VmmError::Control("above --memory-max".into()));
            }
            s.memory_bytes = bytes;
            Ok(s.clone())
        }
        fn resize_cpus(&self, cpus: u32) -> Result<Status> {
            let mut s = self.s.lock().unwrap();
            s.cpus = cpus;
            Ok(s.clone())
        }
        fn status(&self) -> Status {
            let mut s = self.s.lock().unwrap().clone();
            s.stopped = self.stop.is_stopped();
            s
        }
        fn stop(&self) {
            self.stop.request(StopReason::Killed);
        }
    }

    pub(crate) fn fake() -> Arc<Fake> {
        Arc::new(Fake {
            s: Mutex::new(Status {
                memory_bytes: 1 << 30,
                memory_plugged_bytes: 1 << 30,
                memory_max_bytes: 4 << 30,
                cpus: 1,
                cpus_max: 4,
                stopped: false,
            }),
            stop: Arc::new(StopSignal::default()),
        })
    }

    #[test]
    fn vm_t18_requests_round_trip_as_json() {
        for (r, text) in [
            (
                Request::ResizeMemory { bytes: 5 },
                r#"{"op":"resize_memory","bytes":5}"#,
            ),
            (
                Request::ResizeCpus { cpus: 2 },
                r#"{"op":"resize_cpus","cpus":2}"#,
            ),
            (Request::Status, r#"{"op":"status"}"#),
            (Request::Stop, r#"{"op":"stop"}"#),
        ] {
            assert_eq!(serde_json::to_string(&r).unwrap(), text);
            assert_eq!(serde_json::from_str::<Request>(text).unwrap(), r);
        }
        // No request can add a device.
        for bad in [
            r#"{"op":"add_device","kind":"net"}"#,
            r#"{"op":"resize_memory","bytes":1,"net":true}"#,
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn vm_t18_socket_serves_requests_and_refuses_garbage() {
        let dir = scratch_dir("ctl");
        let path = dir.join("c.sock");
        let f = fake();
        let server = ControlServer::bind(&path).unwrap();
        let (f2, stop) = (f.clone(), f.stop.clone());
        let t = std::thread::spawn(move || server.serve(&*f2, &stop));

        let r = request(&path, &Request::ResizeMemory { bytes: 2 << 30 }).unwrap();
        assert!(r.ok);
        assert_eq!(r.status.unwrap().memory_bytes, 2 << 30);
        let r = request(&path, &Request::ResizeMemory { bytes: 8 << 30 }).unwrap();
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("--memory-max"));
        let r = request(&path, &Request::ResizeCpus { cpus: 3 }).unwrap();
        assert_eq!(r.status.unwrap().cpus, 3);
        assert!(
            !request(&path, &Request::Status)
                .unwrap()
                .status
                .unwrap()
                .stopped
        );

        // Garbage and an over-long line get an error answer.
        let mut s = UnixStream::connect(&path).unwrap();
        s.write_all(b"{nonsense}\n").unwrap();
        let mut answer = String::new();
        let mut rd = BufReader::new(s.try_clone().unwrap());
        rd.read_line(&mut answer).unwrap();
        let resp: Response = serde_json::from_str(&answer).unwrap();
        assert!(!resp.ok && resp.error.unwrap().contains("bad request"));
        s.write_all(&vec![b'x'; LINE_MAX + 10]).unwrap();
        s.write_all(b"\n").unwrap();
        answer.clear();
        rd.read_line(&mut answer).unwrap();
        assert!(answer.contains("longer than"));

        // A second monitor cannot take the socket while this one lives.
        assert!(matches!(
            ControlServer::bind(&path),
            Err(VmmError::Config {
                field: "--control",
                ..
            })
        ));

        let r = request(&path, &Request::Stop).unwrap();
        assert!(r.status.unwrap().stopped);
        t.join().unwrap();
    }

    #[test]
    fn vm_t18_bind_replaces_stale_sockets_only() {
        let dir = scratch_dir("ctl-bind");
        let path = dir.join("c.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let s = ControlServer::bind(&path).unwrap();
        drop(s);
        assert!(!path.exists(), "the socket is removed with the server");
        assert!(request(&path, &Request::Status).is_err());
        std::fs::write(&path, b"x").unwrap();
        assert!(ControlServer::bind(&path).is_err());
        assert!(ControlServer::bind(&dir.join("no/such/dir/c.sock")).is_err());
    }
}
