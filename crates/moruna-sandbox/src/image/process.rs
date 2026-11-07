//! An image's command as a source or a sink (SDD 19 f.6): one confined process for the task,
//! started from an unpacked image in a cgroup of its own, its batches an Arrow IPC stream on its
//! standard output (a source) or its standard input (a sink), and its needs on its port socket.
//! What it writes on its standard error is the task's log. A sink's standard output is read as it
//! comes, so a process that writes there never blocks, and is what it hands back at its end: its
//! commit, or nothing.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use moruna_image::{Image, Launch};
use moruna_kernel::{MorunaError, Result, StagePorts};

use super::spawn::{Streams, spawn};
use super::{Child, EXIT_WAIT, config, plan_of, port_socket};
use crate::cgroup::StageCgroup;
use crate::output::{Tail, follow_stderr};

/// The most a sink's process may write back on its standard output.
pub const WRITTEN_BACK_MAX: usize = 1 << 20;

/// A started task process: the end of its stream Moruna holds, and how it ended.
pub struct Started {
    /// A sink's standard input, which Moruna writes its batches to.
    pub stdin: Option<File>,
    /// A source's standard output, which Moruna reads its batches from.
    pub stdout: Option<File>,
    /// How the process ended.
    pub end: ProcessEnd,
}

/// How long a process whose stream failed has to end by itself before it is killed: one that ended
/// (an empty stream) is reaped at once, one still writing what is not Arrow is usually alive.
const FAULT_WAIT: Duration = Duration::from_secs(1);

/// How a task's process ended, asked once its stream has ended.
pub struct ProcessEnd {
    child: Child,
    written: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
}

impl ProcessEnd {
    /// Whether the process ended well: exit status 0, and nothing gone wrong with its stream
    /// (`fault`); else why, with the end of its standard error. For a sink, what it wrote back.
    pub fn wait(mut self, fault: Option<String>) -> std::result::Result<Vec<u8>, String> {
        if let Some(fault) = fault {
            // The stream's fault is the cause, and how the process ended comes first when it
            // ended by itself: a process the filter killed (SIGSYS) leaves an empty stream, and
            // 0.6.3 said only that the stream was empty, killing the process before looking.
            let by_itself = self.child.reap(FAULT_WAIT);
            if by_itself.is_none() {
                self.child.kill();
            }
            let lines = self.child.tail.lines(super::STDERR_WAIT);
            let oom = self.child.cgroup.oom_kills() > 0;
            return Err(super::with_stderr(
                super::failure_head(&self.child.name, &fault, by_itself, oom),
                &lines,
            ));
        }
        self.child.ended_well(EXIT_WAIT)?;
        match self.written.take() {
            None => Ok(Vec::new()),
            Some(reader) => match reader.join() {
                Ok(Ok(bytes)) if bytes.len() > WRITTEN_BACK_MAX => Err(format!(
                    "image `{}` wrote back more than {WRITTEN_BACK_MAX} bytes on its standard \
                     output; a sink writes back one JSON object, its commit",
                    self.child.name
                )),
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(e)) => Err(format!(
                    "image `{}`: its standard output could not be read: {e}",
                    self.child.name
                )),
                Err(_) => Err(format!(
                    "image `{}`: the reader of its standard output failed",
                    self.child.name
                )),
            },
        }
    }
}

/// Start the process of a source (`sink` false) or a sink of `image` (SDD 19 f.5): its command
/// and environment as `launch` says, in a cgroup under `cgroup_parent` limited to `limits`
/// (`(bytes, cores)`), its port socket served by `ports` when the task calls any.
pub fn start_process(
    name: &str,
    image: &Image,
    launch: &Launch,
    cgroup_parent: &Path,
    limits: (u64, f64),
    ports: Option<Arc<StagePorts>>,
    sink: bool,
) -> Result<Started> {
    let rootfs = image
        .rootfs
        .canonicalize()
        .map_err(|e| config(format!("{}: {e}", image.rootfs.display())))?;
    let filters = crate::seccomp::image_filters().map_err(config)?;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let cgroup = Arc::new(
        StageCgroup::create(
            cgroup_parent,
            &format!("image-task-{}-{at}", std::process::id()),
            limits.0,
            limits.1,
        )
        .map_err(|e| config(format!("{name}: the task's cgroup: {e}")))?,
    );
    let ports = ports.filter(|p| !p.is_empty());
    let socket = port_socket(name, ports.is_some(), false)?;
    let plan = plan_of(
        launch,
        &rootfs,
        &filters,
        cgroup.dir(),
        socket.as_ref().map(|(_, _, env)| env.as_str()),
    )?;
    let (task_end, runner_end) = match socket {
        Some((task, runner, _)) => (Some(task), Some(runner)),
        None => (None, None),
    };
    let started = spawn(&plan, Streams::Stdio, task_end)
        .map_err(|e| config(format!("{name}: the process could not be started: {e}")))?;
    let tail = Arc::new(Tail::default());
    follow_stderr(
        started.stderr,
        moruna_kernel::output::current(),
        name,
        Arc::clone(&tail),
    );
    if let (Some(runner), Some(ports)) = (runner_end, ports) {
        crate::ports::serve_on_thread(runner, ports, name.to_string())
            .map_err(|e| config(format!("{name}: the port socket's thread: {e}")))?;
    }
    let child = Child {
        pid: started.pid,
        tail,
        cgroup,
        exit: None,
        name: name.to_string(),
    };
    let (stdin, stdout, written) = if sink {
        let mut out = started
            .stdout
            .ok_or_else(|| config(format!("{name}: the process has no standard output")))?;
        let reader = std::thread::Builder::new()
            .name(format!("image-written-{name}"))
            .spawn(move || {
                let mut bytes = Vec::new();
                (&mut out)
                    .take(WRITTEN_BACK_MAX as u64 + 1)
                    .read_to_end(&mut bytes)?;
                // Anything past the limit is drained, so the process never blocks on it.
                std::io::copy(&mut out, &mut std::io::sink())?;
                Ok(bytes)
            })
            .map_err(|e| MorunaError::Config {
                name: "image",
                msg: format!("{name}: the standard output's thread: {e}"),
            })?;
        (started.stdin, None, Some(reader))
    } else {
        // A source reads nothing: its standard input ends at once.
        drop(started.stdin);
        (None, started.stdout, None)
    };
    tracing::info!(target: "sandbox", pid = child.pid, image = %name, "image task process started");
    Ok(Started {
        stdin,
        stdout,
        end: ProcessEnd { child, written },
    })
}
