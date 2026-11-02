//! KA2: the port's conformance suite against `LinuxPlane` for real. Every task is a process of
//! its own (this binary run again as `plane_task`), confined by a seccomp filter that allows the
//! task library's calls ([`moruna_plane_linux::task::SYSCALLS`]) and the few the script itself
//! makes, and nothing else: a call outside it kills the task with `SIGSYS`, which fails the case.
//! The kernel adapter runs the same suite in `model.rs`.
#![cfg(target_os = "linux")]

mod support;

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;

use moruna_plane::TaskId;
use moruna_plane::conformance::{
    self, Ending, Harness, Outcome, Seen, Step, decode_script, decode_seen, encode_script,
    encode_seen, sum,
};
use moruna_plane_linux::task::{Plane, SYSCALLS};
use moruna_plane_linux::{LinuxPlane, sys};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};
use support::process::{self, Task};

/// The calls the script runner makes beside the library's: its report, its allocations, its
/// crash, its exit.
const RUNNER: &[&str] = &[
    "write",
    "brk",
    "madvise",
    "mremap",
    "mprotect",
    "rt_sigprocmask",
    "rt_sigreturn",
    "sigaltstack",
    "getpid",
    "gettid",
    "kill",
    "tgkill",
    "exit",
    "exit_group",
];

fn number(name: &str) -> i64 {
    match name {
        "sendmsg" => libc::SYS_sendmsg,
        "recvmsg" => libc::SYS_recvmsg,
        "mmap" => libc::SYS_mmap,
        "munmap" => libc::SYS_munmap,
        "close" => libc::SYS_close,
        "ppoll" => libc::SYS_ppoll,
        "getsockopt" => libc::SYS_getsockopt,
        "fcntl" => libc::SYS_fcntl,
        "futex" => libc::SYS_futex,
        "write" => libc::SYS_write,
        "brk" => libc::SYS_brk,
        "madvise" => libc::SYS_madvise,
        "mremap" => libc::SYS_mremap,
        "mprotect" => libc::SYS_mprotect,
        "rt_sigprocmask" => libc::SYS_rt_sigprocmask,
        "rt_sigreturn" => libc::SYS_rt_sigreturn,
        "sigaltstack" => libc::SYS_sigaltstack,
        "getpid" => libc::SYS_getpid,
        "gettid" => libc::SYS_gettid,
        "kill" => libc::SYS_kill,
        "tgkill" => libc::SYS_tgkill,
        "exit" => libc::SYS_exit,
        "exit_group" => libc::SYS_exit_group,
        other => panic!("no number for `{other}`"),
    }
}

/// The task's filter: the library's calls and the runner's; anything else kills the process.
fn filter() -> BpfProgram {
    let rules = SYSCALLS
        .iter()
        .chain(RUNNER)
        .map(|n| (number(n), Vec::new()))
        .collect::<BTreeMap<_, _>>();
    let arch = if cfg!(target_arch = "x86_64") {
        TargetArch::x86_64
    } else {
        TargetArch::aarch64
    };
    SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        arch,
    )
    .expect("the filter")
    .try_into()
    .expect("the program")
}

/// The plane's tasks as processes.
#[derive(Default)]
struct LinuxHarness {
    tasks: BTreeMap<u32, Task>,
}

impl Harness for LinuxHarness {
    type Plane = LinuxPlane;

    fn plane(&mut self, budget: u64) -> LinuxPlane {
        for (_, mut t) in std::mem::take(&mut self.tasks) {
            let _ = t.child.kill();
            let _ = t.child.wait();
        }
        LinuxPlane::new(budget)
    }

    fn start(
        &mut self,
        plane: &mut LinuxPlane,
        task: TaskId,
        end: std::os::fd::OwnedFd,
        script: Vec<Step>,
    ) -> Result<(), String> {
        let started = process::start("plane_task", end, &encode_script(&script));
        let pidfd = sys::pidfd(started.child.id()).map_err(|e| format!("pidfd: {e}"))?;
        plane
            .attach(task, pidfd)
            .map_err(|e| format!("attach: {e:?}"))?;
        self.tasks.insert(task.0, started);
        Ok(())
    }

    fn run(&mut self, _plane: &mut LinuxPlane) {}

    fn deadline(&self, ms: u64) -> u64 {
        sys::now() + ms * 1_000_000
    }

    fn kill(&mut self, _plane: &mut LinuxPlane, task: TaskId) -> Result<(), String> {
        let t = self
            .tasks
            .get_mut(&task.0)
            .ok_or_else(|| format!("no task {task:?}"))?;
        t.child.kill().map_err(|e| format!("kill: {e}"))
    }

    fn outcome(&mut self, task: TaskId) -> Result<Outcome, String> {
        let t = self
            .tasks
            .get_mut(&task.0)
            .ok_or_else(|| format!("no task {task:?}"))?;
        let (status, report) = t.finish();
        let seen = decode_seen(&report)?;
        let ending = match (status.code(), status.signal()) {
            (Some(0), _) => Ending::Exited,
            (_, Some(libc::SIGSEGV | libc::SIGBUS)) => Ending::Faulted,
            (_, Some(libc::SIGKILL)) => Ending::Killed,
            (_, Some(libc::SIGSYS)) => {
                return Err(format!(
                    "task {task:?} made a system call outside its filter (SIGSYS), after {seen:?}"
                ));
            }
            _ => return Err(format!("task {task:?} ended with {status}, after {seen:?}")),
        };
        Ok(Outcome { seen, ending })
    }
}

/// One task: the script on descriptor 4 run against the plane on descriptor 3, each step's
/// result reported on descriptor 5 as it happens.
fn run_script(plane: &Plane, script: Vec<Step>, results: &mut std::fs::File) {
    let mut mapped: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
    let mut put_at: BTreeMap<u32, u64> = BTreeMap::new();
    let mut port = plane.port().expect("the port, once");
    for step in script {
        let seen = match step {
            Step::Get(n) => match plane.batch_get(n) {
                Ok(Some(got)) => {
                    mapped.insert(n, (got.addr, got.len));
                    if got.window {
                        Seen::Window { len: got.len }
                    } else {
                        // SAFETY: the plane mapped `len` bytes at `addr` read only until this
                        // slot's next call, which this loop makes after reading them.
                        let bytes = unsafe {
                            std::slice::from_raw_parts(got.addr as *const u8, got.len as usize)
                        };
                        Seen::Batch {
                            len: got.len,
                            sum: sum(bytes),
                        }
                    }
                }
                Ok(None) => {
                    mapped.remove(&n);
                    Seen::End
                }
                Err(e) => Seen::Error(e),
            },
            Step::Write { slot, at, bytes } => {
                let (addr, len) = mapped.get(&slot).copied().unwrap_or((0, 0));
                assert!(
                    at + bytes.len() as u64 <= len,
                    "a write past what slot {slot} maps"
                );
                // SAFETY: none, deliberately, when the mapping is a batch: it is read only and
                // the store faults, ending the task with SIGSEGV, which is what the step
                // tests. A window is mapped writable for `len` bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        (addr + at) as *mut u8,
                        bytes.len(),
                    );
                }
                Seen::Wrote
            }
            Step::Put { slot, offset, len } => {
                let base = mapped.get(&slot).map_or(0, |m| m.0);
                match plane.batch_put(slot, base + offset, len) {
                    Ok(()) => {
                        mapped.remove(&slot);
                        put_at.insert(slot, base);
                        Seen::Put
                    }
                    Err(e) => Seen::Error(e),
                }
            }
            Step::WriteAfterPut(slot) => {
                let addr = put_at.get(&slot).copied().expect("a window put");
                // SAFETY: none, deliberately: the window left this address space at its put,
                // so the store faults, which is what the step tests.
                unsafe { std::ptr::write_volatile(addr as *mut u8, 0xee) };
                Seen::Wrote
            }
            Step::Call { port: to, request } => match port.call(to, &request) {
                Ok(Some(reply)) => Seen::Reply(reply.bytes().to_vec()),
                Ok(None) => Seen::Reply(Vec::new()),
                Err(e) => Seen::Error(e),
            },
            Step::Crash => {
                // SAFETY: a signal to this process.
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                unreachable!("SIGKILL returned");
            }
        };
        results.write_all(&encode_seen(&seen)).expect("the report");
    }
}

/// A task of the suite, started by [`LinuxHarness::start`]; nothing when run any other way.
#[test]
#[ignore = "a task of the conformance suite, which starts it as a process of its own"]
fn plane_task() {
    let Some((socket, input, mut results)) = process::task() else {
        return;
    };
    let script = decode_script(&input).expect("the script");
    let plane = Plane::new(socket).expect("the plane");
    // This child deliberately faults on writes after ownership transfer. Restore
    // the OS default before confinement: Rust's stack-overflow signal handler
    // otherwise tries to reinstall SIG_DFL from inside the seccomp filter, so
    // the handler's forbidden rt_sigaction becomes SIGSYS instead of the real
    // memory fault. The plane's syscall allowlist remains unchanged.
    for signal in [libc::SIGSEGV, libc::SIGBUS] {
        // SAFETY: this isolated child installs no handler or borrowed pointer;
        // its intended invalid writes must terminate with the kernel's signal.
        assert_ne!(
            unsafe { libc::signal(signal, libc::SIG_DFL) },
            libc::SIG_ERR
        );
    }
    seccompiler::apply_filter(&filter()).expect("the filter");
    run_script(&plane, script, &mut results);
    // SAFETY: the task's end, without unwinding through the test harness.
    unsafe { libc::_exit(0) };
}

#[test]
fn ka2_the_suite_passes_on_the_linux_adapter() {
    let fixtures = support::fixtures();
    let mut harness = LinuxHarness::default();
    let results = conformance::run_all(&mut harness, &fixtures);
    let failed: Vec<String> = results
        .iter()
        .filter_map(|(name, r)| r.as_ref().err().map(|e| format!("{name}: {e}")))
        .collect();
    for (name, r) in &results {
        println!(
            "ka2 conformance, linux adapter: {name}: {}",
            if r.is_ok() { "ok" } else { "FAILED" }
        );
    }
    assert!(
        failed.is_empty(),
        "the Linux adapter:\n{}",
        failed.join("\n")
    );
    println!(
        "ka2 conformance, linux adapter: {} cases passed, {} records refused",
        results.len(),
        fixtures.refusals.len()
    );
}
