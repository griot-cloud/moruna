//! What the port's tests share: the fixtures both adapters move, and, on Linux, tasks as
//! processes of their own.
//!
//! A task is this test binary run again as the ignored test that plays it, with its plane on
//! descriptor 3, what it is to do on descriptor 4 (a memfd) and a memfd on descriptor 5 for
//! what it reports. Started any other way (`cargo test -- --ignored`), descriptor 3 is no plane
//! socket and descriptor 4 no script, and the test returns at once.

#![allow(dead_code)]

use moruna_plane::conformance::{Fixtures, Refusal};

/// The fixtures of the suite: a record of eight rows and every refusal of DP-T12.
pub fn fixtures() -> Fixtures {
    let good = moruna_testkit::plane::record(&moruna_testkit::plane::mixed(8).expect("rows"))
        .expect("a record");
    let refusals = moruna_testkit::plane::refusals()
        .expect("the refusals")
        .into_iter()
        .map(|r| Refusal {
            label: r.label,
            bytes: r.bytes,
            put: r.put,
            says: r.says.to_string(),
        })
        .collect();
    Fixtures {
        good,
        rows: 8,
        refusals,
    }
}

#[cfg(target_os = "linux")]
pub mod process {
    use std::io::{Read, Write};
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, ExitStatus, Stdio};

    use moruna_plane_linux::sys;

    /// The name the scripts' memfds carry, so a task knows it was started as one.
    const SCRIPT: &str = "/memfd:moruna-plane";

    /// A task started: its process, and the memfd it reports into.
    pub struct Task {
        pub child: Child,
        pub results: OwnedFd,
    }

    impl Task {
        /// Waits for the process and reads what it reported.
        pub fn finish(&mut self) -> (ExitStatus, Vec<u8>) {
            let status = self.child.wait().expect("the task is waited for");
            let (size, _, _) = sys::stat(self.results.as_fd()).expect("the results' size");
            let mut bytes = vec![0u8; size as usize];
            sys::pread_exact(self.results.as_fd(), 0, &mut bytes).expect("the results");
            (status, bytes)
        }
    }

    /// Starts this binary as the ignored test `entry`, with `plane` as its descriptor 3 and
    /// `input` on descriptor 4.
    pub fn start(entry: &str, plane: OwnedFd, input: &[u8]) -> Task {
        let script = sys::memfd(0).expect("a memfd");
        sys::pwrite_all(script.as_fd(), 0, input).expect("the script");
        let results = sys::memfd(0).expect("a memfd");
        let ends = [plane.as_raw_fd(), script.as_raw_fd(), results.as_raw_fd()];
        let mut command = Command::new(std::env::current_exe().expect("this binary"));
        command
            .args([
                "--exact",
                entry,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // SAFETY: between fork and exec the closure calls only fcntl and dup2, which are
        // async-signal-safe, on descriptors the parent holds open across the spawn.
        unsafe {
            command.pre_exec(move || {
                let mut high = [0; 3];
                for (h, fd) in high.iter_mut().zip(ends) {
                    *h = libc::fcntl(fd, libc::F_DUPFD, 100);
                    if *h < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for (target, fd) in high.into_iter().enumerate() {
                    if libc::dup2(fd, 3 + target as i32) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("the task starts");
        drop((plane, script));
        Task { child, results }
    }

    /// In a task: its plane socket, its input and its report memfd, when it was started by
    /// [`start`]; `None` otherwise.
    pub fn task() -> Option<(OwnedFd, Vec<u8>, std::fs::File)> {
        let link = std::fs::read_link("/proc/self/fd/4").ok()?;
        if !link.to_string_lossy().starts_with(SCRIPT) {
            return None;
        }
        let plane = sys::claim(3).ok()?;
        if sys::socket_type(plane.as_fd()).ok()? != libc::SOCK_SEQPACKET {
            return None;
        }
        let mut input = Vec::new();
        std::fs::File::from(sys::claim(4).ok()?)
            .read_to_end(&mut input)
            .ok()?;
        let results = std::fs::File::from(sys::claim(5).ok()?);
        Some((plane, input, results))
    }

    /// Writes `bytes` to the report.
    pub fn report(results: &mut std::fs::File, bytes: &[u8]) {
        results.write_all(bytes).expect("the report");
    }
}
