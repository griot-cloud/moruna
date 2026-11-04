//! A jailed process, owned and reaped by the process that started it.

use std::{os::unix::process::ExitStatusExt, process::ExitStatus};

/// A jailed process this process started and alone reaps. Its pid cannot be reused before it
/// is reaped here, and it is never signalled after that.
#[derive(Debug)]
pub(crate) struct Jailed {
    pid: libc::pid_t,
    status: Option<ExitStatus>,
}

impl Jailed {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(pid: libc::pid_t) -> Self {
        Jailed { pid, status: None }
    }

    /// Its pid in the launching process's PID namespace.
    pub(crate) fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Its status if it has exited, reaping it.
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let mut raw = 0;
        // SAFETY: waitpid on our own unreaped child with a valid out pointer.
        let rc = unsafe { libc::waitpid(self.pid, &mut raw, libc::WNOHANG) };
        match rc {
            0 => Ok(None),
            rc if rc == self.pid => {
                let status = ExitStatus::from_raw(raw);
                self.status = Some(status);
                Ok(Some(status))
            }
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    /// Wait until it exits, reaping it.
    pub(crate) fn wait(&mut self) -> std::io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.status {
                return Ok(status);
            }
            let mut raw = 0;
            // SAFETY: as in try_wait, blocking.
            let rc = unsafe { libc::waitpid(self.pid, &mut raw, 0) };
            if rc == self.pid {
                self.status = Some(ExitStatus::from_raw(raw));
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }

    /// Kill it unless it has already been reaped.
    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        // SAFETY: the pid is our unreaped child (a zombie at worst), never a reused one.
        if unsafe { libc::kill(self.pid, libc::SIGKILL) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Jailed {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.kill();
            let _ = self.wait();
        }
    }
}
