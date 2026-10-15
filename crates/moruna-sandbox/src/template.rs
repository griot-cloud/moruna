//! The warm template (17 f.9): one standard-build interpreter per Moruna process and sandbox
//! interpreter, started with the data libraries imported and one thread, which forks a child per
//! `.pex` stage on request.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::fdpass::send_with_fds;
use crate::protocol::{FromTemplate, ToTemplate, read_frame, write_frame};

/// How long a template has to import its libraries and say hello.
pub const START_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a template has to answer a request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The descriptor the template's socket is at in the template.
const TEMPLATE_FD: RawFd = 3;

/// What the template said when it was ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// Its pid.
    pub pid: u32,
    /// Its interpreter's version.
    pub python: String,
    /// The libraries it imported, by distribution name, with their versions.
    pub libraries: BTreeMap<String, String>,
    /// Its C library's version.
    pub glibc: Option<(u32, u32)>,
    /// The machine.
    pub machine: String,
}

impl Hello {
    /// The `.pex` target this template offers (16 f.7).
    pub fn target(&self) -> Result<String, String> {
        let minor = self
            .python
            .split('.')
            .nth(1)
            .and_then(|m| m.parse::<u32>().ok())
            .ok_or_else(|| format!("the template's Python is `{}`", self.python))?;
        moruna_package::host_target(minor, self.glibc, &self.machine).map_err(|e| e.to_string())
    }
}

/// A running template.
pub struct Template {
    process: Mutex<Child>,
    socket: Mutex<UnixStream>,
    /// What it said when it started.
    pub hello: Hello,
}

impl std::fmt::Debug for Template {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Template")
            .field("hello", &self.hello)
            .finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Template {
    /// Start `python -I -m moruna._sandbox` with its socket at descriptor 3 and wait for its
    /// hello (f.9).
    pub fn spawn(python: &Path) -> Result<Template, String> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
        let theirs_fd = theirs.as_raw_fd();
        let mut command = Command::new(python);
        command
            .args(["-I", "-m", "moruna._sandbox"])
            .env_clear()
            .env("OPENBLAS_NUM_THREADS", "1")
            .env("OMP_NUM_THREADS", "1")
            .env("MKL_NUM_THREADS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        // SAFETY: the closure runs in the forked child before exec and calls only dup2 and fcntl,
        // which are async-signal-safe; it touches no allocation of the parent.
        unsafe {
            command.pre_exec(move || {
                // What the kernel prints goes to Moruna's stderr, never to its stdout.
                if libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if theirs_fd == TEMPLATE_FD {
                    let flags = libc::fcntl(theirs_fd, libc::F_GETFD);
                    if flags < 0
                        || libc::fcntl(theirs_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::dup2(theirs_fd, TEMPLATE_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let process = command.spawn().map_err(|e| {
            format!(
                "{}: cannot start the sandbox template: {e}",
                python.display()
            )
        })?;
        drop(theirs);
        ours.set_read_timeout(Some(START_TIMEOUT))
            .map_err(|e| e.to_string())?;
        let mut reader = &ours;
        let hello = match read_frame::<FromTemplate>(&mut reader) {
            Ok(FromTemplate::Hello {
                pid,
                python,
                libraries,
                glibc,
                machine,
            }) => Hello {
                pid,
                python,
                libraries,
                glibc: glibc.map(|[a, b]| (a, b)),
                machine,
            },
            Ok(FromTemplate::Refused { reason }) => {
                let mut process = process;
                let _ = process.wait();
                return Err(format!("the sandbox template refused to start: {reason}"));
            }
            Ok(other) => return Err(format!("the sandbox template said {other:?} first")),
            Err(e) => {
                let mut process = process;
                let _ = process.kill();
                let status = process.wait().map(|s| s.to_string()).unwrap_or_default();
                return Err(format!(
                    "the sandbox template ({}) did not say hello: {e} ({status})",
                    python.display()
                ));
            }
        };
        ours.set_read_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|e| e.to_string())?;
        tracing::info!(target: "sandbox", pid = hello.pid, python = %hello.python, "template ready");
        Ok(Template {
            process: Mutex::new(process),
            socket: Mutex::new(ours),
            hello,
        })
    }

    /// Whether the template is still running.
    pub fn alive(&self) -> bool {
        matches!(lock(&self.process).try_wait(), Ok(None))
    }

    /// Fork a child for a stage: `control` is the child's end of its control socket, `memfd`
    /// the mapping. Returns the child's pid.
    pub fn start(&self, request: &ToTemplate, control: RawFd, memfd: RawFd) -> Result<u32, String> {
        let socket = lock(&self.socket);
        let mut frame = Vec::new();
        write_frame(&mut frame, request).map_err(|e| e.to_string())?;
        send_with_fds(&socket, &frame, &[control, memfd]).map_err(|e| format!("start: {e}"))?;
        let mut reader = &*socket;
        match read_frame::<FromTemplate>(&mut reader).map_err(|e| format!("start: {e}"))? {
            FromTemplate::Started { pid } => Ok(pid),
            FromTemplate::Refused { reason } => Err(reason),
            other => Err(format!("start: the template answered {other:?}")),
        }
    }

    /// How child `pid` ended, from the template that reaped it (SB-I11).
    pub fn status(&self, pid: u32) -> Result<String, String> {
        let socket = lock(&self.socket);
        let mut writer = &*socket;
        write_frame(&mut writer, &ToTemplate::Status { pid }).map_err(|e| e.to_string())?;
        let mut reader = &*socket;
        match read_frame::<FromTemplate>(&mut reader).map_err(|e| e.to_string())? {
            FromTemplate::Exited { status, .. } => Ok(status),
            FromTemplate::Refused { reason } => Err(reason),
            other => Err(format!("status: the template answered {other:?}")),
        }
    }
}

impl Drop for Template {
    fn drop(&mut self) {
        let mut process = lock(&self.process);
        let _ = process.kill();
        let _ = process.wait();
    }
}

static TEMPLATES: OnceLock<Mutex<HashMap<PathBuf, Arc<Template>>>> = OnceLock::new();

/// The template for `python`: the running one, or a new one when there is none or it died.
pub fn template(python: &Path) -> Result<Arc<Template>, String> {
    let mut templates = lock(TEMPLATES.get_or_init(|| Mutex::new(HashMap::new())));
    if let Some(t) = templates.get(python)
        && t.alive()
    {
        return Ok(Arc::clone(t));
    }
    let t = Arc::new(Template::spawn(python)?);
    templates.insert(python.to_path_buf(), Arc::clone(&t));
    Ok(t)
}
