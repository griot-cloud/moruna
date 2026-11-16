// SPDX-License-Identifier: MPL-2.0

//! The gVisor system-call tests' runner, as a booted personality's first program
//! (`init=/gvisor/runner -- /gvisor`). The data OS has no shell (`architecture/data-os.md`
//! §3a), so this takes the place of Asterinas's `run_gvisor_test.sh`:
//!
//! - it runs each test binary `ROOT/applies` names, from `ROOT/tests`, with the cases its
//!   `ROOT/blocklists/NAME` lists left out by the gtest filter;
//! - each test gets an empty scratch (`TEST_TMPDIR`, `/tmp`) and a large descriptor limit,
//!   as gVisor's runner gives it;
//! - as the first program it reaps what each test leaves behind;
//! - it prints `gvisor: ok: NAME` or `gvisor: FAIL: NAME ...` for each, and a count, and
//!   exits 0 only when every test passed.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, ExitStatus},
    time::{Duration, Instant},
};

/// Where the tests are when no root is given.
const DEFAULT_ROOT: &str = "/gvisor";

/// The scratch every test writes in.
const SCRATCH: &str = "/tmp";

/// The descriptor limit gVisor's runner gives a test: `PipeTest.PipeFdCount` needs more than
/// 66,000 (gVisor `runsc/boot/limits.go`).
const NOFILE: u64 = 1_048_576;

/// How long one test binary may run before it is killed and failed.
const TEST_LIMIT: Duration = Duration::from_secs(600);

/// The test binaries `applies` names, one a line, `#` starting a comment.
fn names(applies: &str) -> Vec<String> {
    applies
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|name| !name.is_empty())
        .map(String::from)
        .collect()
}

/// The gtest filter that leaves out every case `blocklist` lists (same format).
fn filter(blocklist: &str) -> String {
    let blocked = names(blocklist);
    if blocked.is_empty() {
        String::from("*")
    } else {
        format!("-{}", blocked.join(":"))
    }
}

/// Waits for `child` until `limit` has passed; kills it then, and answers `None`.
fn wait_for(mut child: Child, limit: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How a test ended, as a line ends; `None` for one killed at its time limit.
fn verdict(name: &str, status: io::Result<Option<ExitStatus>>) -> (bool, String) {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Ok(None) => (
            false,
            format!("gvisor: FAIL: {name} ran past its time limit"),
        ),
        Ok(Some(s)) if s.success() => (true, format!("gvisor: ok: {name}")),
        Ok(Some(s)) => match (s.code(), s.signal()) {
            (Some(code), _) => (false, format!("gvisor: FAIL: {name} exited with {code}")),
            (None, Some(signal)) => (
                false,
                format!("gvisor: FAIL: {name} was killed by signal {signal}"),
            ),
            (None, None) => (false, format!("gvisor: FAIL: {name} ended as {s}")),
        },
        Err(e) => (false, format!("gvisor: FAIL: {name} could not run: {e}")),
    }
}

/// Empties `dir`, leaving it.
fn empty(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let _ = if path.is_dir() && !path.is_symlink() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
        }
    }
}

/// Reaps every child that has ended: tests leave orphans, which come to the first program.
fn reap() {
    loop {
        // SAFETY: `waitpid` with no status pointer touches no memory of ours.
        let pid = unsafe { libc::waitpid(-1, core::ptr::null_mut(), libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
    }
}

/// Raises the descriptor limit for the tests.
fn raise_nofile() -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: NOFILE,
        rlim_max: NOFILE,
    };
    // SAFETY: `limit` is a valid `rlimit` for the call to read.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Runs every test `root/applies` names, each for at most `limit`; how many passed, of how
/// many.
fn run(root: &Path, scratch: &Path, limit: Duration, out: &mut impl FnMut(&str)) -> (usize, usize) {
    let applies = fs::read_to_string(root.join("applies")).unwrap_or_default();
    let tests = root.join("tests");
    let (mut passed, mut ran) = (0, 0);
    for name in names(&applies) {
        let blocklist = fs::read_to_string(root.join("blocklists").join(&name)).unwrap_or_default();
        out(&format!("gvisor: running {name}"));
        let status = Command::new(tests.join(&name))
            .arg(format!("--gtest_filter={}", filter(&blocklist)))
            .current_dir(&tests)
            .env("TEST_TMPDIR", scratch)
            .spawn()
            .and_then(|child| wait_for(child, limit));
        reap();
        empty(scratch);
        let (ok, line) = verdict(&name, status);
        out(&line);
        ran += 1;
        passed += usize::from(ok);
    }
    out(&format!("gvisor: {passed} of {ran} test binaries passed"));
    (passed, ran)
}

/// Runs the tests under `root` (`DEFAULT_ROOT` when not given); whether every one passed.
fn start(root: Option<String>, scratch: &Path, out: &mut impl FnMut(&str)) -> bool {
    let root = PathBuf::from(root.unwrap_or_else(|| DEFAULT_ROOT.into()));
    if let Err(e) = raise_nofile() {
        out(&format!("gvisor: the descriptor limit stays: {e}"));
    }
    let (passed, ran) = run(&root, scratch, TEST_LIMIT, out);
    ran > 0 && passed == ran
}

fn main() -> ExitCode {
    let root = std::env::args().nth(1);
    if start(root, Path::new(SCRATCH), &mut |line| println!("{line}")) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod test;
