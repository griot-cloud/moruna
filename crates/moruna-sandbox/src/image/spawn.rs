//! An image's process: forked from Moruna, confined in the order of SDD 19 f.5, then the image's
//! command is executed. Everything the child needs is made before the fork, so what runs after
//! it is system calls and a few allocations, never a lock another thread of Moruna may hold.
//!
//! Two processes stand between Moruna and the command. The first (the middle) joins the stage's
//! cgroup, makes the namespaces and maps the identity; a pid namespace takes effect for the
//! middle's children, so it forks the second, which is process 1 of the new namespace, builds the
//! root and becomes the command. The middle waits for it and ends the way it ended, so Moruna
//! reaps one process and sees one status.

use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus,
};
use seccompiler::BpfProgram;

use crate::confine::{self, DEVICES, check, drop_privileges, join_cgroup, mount};

/// The status a child exits with when it could not be confined or started, whatever the step.
pub const SETUP_FAILED: i32 = 125;
/// The status of a command that is not there (the shell's convention).
pub const NOT_FOUND: i32 = 127;
/// The status of a command that is there and cannot be executed.
pub const NOT_EXECUTABLE: i32 = 126;

/// Everything the child needs, made before the fork.
pub struct Plan {
    /// The stage's cgroup.
    pub cgroup: PathBuf,
    /// The unpacked root filesystem.
    pub rootfs: PathBuf,
    /// The user and group the command runs as inside its user namespace.
    pub uid: u32,
    /// See `uid`.
    pub gid: u32,
    /// Where the command starts, an absolute path in the image.
    pub working_dir: CString,
    /// The paths the command is tried at, in order.
    pub candidates: Vec<CString>,
    /// The command's arguments, the first being the command as named.
    pub argv: Vec<CString>,
    /// The command's whole environment.
    pub envp: Vec<CString>,
    /// The size of the `tmpfs` at `/tmp`.
    pub tmp_bytes: u64,
    /// The system-call filters, built (SDD 19 e.4).
    pub filters: Vec<BpfProgram>,
}

/// A started image: its process and the three pipes.
pub struct Spawned {
    /// The process Moruna reaps.
    pub pid: libc::pid_t,
    /// What Moruna writes the command's input to.
    pub stdin: File,
    /// What Moruna reads the command's output from.
    pub stdout: File,
    /// The command's standard error.
    pub stderr: OwnedFd,
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0 as RawFd; 2];
    // SAFETY: `ends` is two descriptors' room; pipe2 writes both or neither.
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just made and nobody else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) })
}

/// Start the image's process (SDD 19 f.5): fork, and in the child confine and execute.
pub fn spawn(plan: &Plan) -> io::Result<Spawned> {
    use std::os::fd::AsRawFd;
    let (in_r, in_w) = pipe()?;
    let (out_r, out_w) = pipe()?;
    let (err_r, err_w) = pipe()?;
    // What the child needs to ask the system about, asked here.
    // SAFETY: getuid and getgid take nothing and cannot fail.
    let (outer_uid, outer_gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let rootfs_flags = inherited_flags(&plan.rootfs)?;
    let ends = [in_r.as_raw_fd(), out_w.as_raw_fd(), err_w.as_raw_fd()];
    // SAFETY: the child (below) touches only memory this thread owns and calls the system; it
    // never returns into the parent's code, and ends with `_exit`.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => Err(io::Error::last_os_error()),
        0 => middle(plan, ends, (outer_uid, outer_gid), rootfs_flags),
        pid => {
            drop((in_r, out_w, err_w));
            Ok(Spawned {
                pid,
                stdin: File::from(in_w),
                stdout: File::from(out_r),
                stderr: err_r,
            })
        }
    }
}

/// A failed step, written to the child's standard error for the failure to quote, and the
/// process ended.
fn fail(step: &str, error: impl std::fmt::Display, code: i32) -> ! {
    let line = format!("moruna-image: {step}: {error}\n");
    // SAFETY: a write of a buffer this function owns to descriptor 2, whose result is ignored
    // because there is nowhere left to report it; then the process ends without running the
    // parent's exit handlers.
    unsafe {
        libc::write(2, line.as_ptr().cast(), line.len());
        libc::_exit(code)
    }
}

fn must<T>(step: &str, r: io::Result<T>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => fail(step, e, SETUP_FAILED),
    }
}

/// The first child: descriptors, signals, the cgroup, the namespaces, the identity, then the
/// second child; and it waits for that one.
fn middle(plan: &Plan, ends: [RawFd; 3], outer: (u32, u32), rootfs_flags: libc::c_ulong) -> ! {
    must("descriptors", stdio(ends));
    // SAFETY: setrlimit with a pointer to a struct on this stack, and signal calls with
    // integers; nothing but this process is touched.
    unsafe {
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &none);
        // Moruna ignores SIGPIPE and may block signals on its threads; a command starts with the
        // defaults it expects, since both are inherited across an exec.
        for signal in [libc::SIGPIPE, libc::SIGXFSZ] {
            libc::signal(signal, libc::SIG_DFL);
        }
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, core::ptr::null_mut());
    }
    if let Err(e) = join_cgroup(&plan.cgroup) {
        fail("cgroup", e.error, SETUP_FAILED);
    }
    let flags = libc::CLONE_NEWUSER
        | libc::CLONE_NEWNS
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWNET
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWCGROUP;
    // SAFETY: unshare takes flags only.
    must(
        "namespaces",
        check(unsafe { libc::unshare(flags) }, "unshare"),
    );
    must("identity", map_identity(plan, outer));
    // SAFETY: fork in a process with one thread.
    match unsafe { libc::fork() } {
        -1 => fail("fork", io::Error::last_os_error(), SETUP_FAILED),
        0 => {
            let e = inner(plan, rootfs_flags);
            fail(e.0, e.1, e.2)
        }
        child => reflect(child),
    }
}

/// Wait for `child` and end the way it ended: its status, or the signal that killed it.
fn reflect(child: libc::pid_t) -> ! {
    let mut status = 0;
    loop {
        // SAFETY: waiting for the child this process made, into a local.
        let r = unsafe { libc::waitpid(child, &mut status, 0) };
        if r == child {
            break;
        }
        if r == -1 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            fail("wait", io::Error::last_os_error(), SETUP_FAILED);
        }
    }
    // SAFETY: the status macros read an integer; the signal calls take integers; `_exit` ends
    // this process.
    unsafe {
        if libc::WIFSIGNALED(status) {
            let signal = libc::WTERMSIG(status);
            libc::signal(signal, libc::SIG_DFL);
            libc::kill(libc::getpid(), signal);
            libc::_exit(128 + signal);
        }
        libc::_exit(libc::WEXITSTATUS(status))
    }
}

/// Descriptors 0, 1 and 2 are the three pipes; every other descriptor is closed.
fn stdio(ends: [RawFd; 3]) -> io::Result<()> {
    for (target, fd) in ends.iter().enumerate() {
        let target = target as RawFd;
        // SAFETY: dup2 and fcntl on descriptors this process holds.
        unsafe {
            if *fd == target {
                libc::fcntl(*fd, libc::F_SETFD, 0);
            } else if libc::dup2(*fd, target) == -1 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    close_from(3)
}

/// `close_range(first, ~0, 0)`.
fn close_from(first: u32) -> io::Result<()> {
    // SAFETY: close_range takes integers.
    let r = unsafe { libc::syscall(libc::SYS_close_range, first, u32::MAX, 0) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The user and group the command runs as, mapped to the process's own: one id each, which is
/// all an unprivileged process may map (SDD 19 IM-I4).
fn map_identity(plan: &Plan, (uid, gid): (u32, u32)) -> io::Result<()> {
    let write = |file: &str, text: String| {
        fs::write(Path::new("/proc/self").join(file), text)
            .map_err(|e| io::Error::new(e.kind(), format!("/proc/self/{file}: {e}")))
    };
    write("setgroups", "deny".into())?;
    write("uid_map", format!("{} {uid} 1\n", plan.uid))?;
    write("gid_map", format!("{} {gid} 1\n", plan.gid))
}

/// The mount flags the filesystem under `path` has, in the terms `mount(2)` takes them: a bind
/// mount made inside a user namespace may not clear what its source's mount has set.
fn inherited_flags(path: &Path) -> io::Result<libc::c_ulong> {
    let path = confine::c(path)?;
    // SAFETY: statvfs of a NUL-terminated path into a zeroed struct on this stack.
    let flags = unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut stat) != 0 {
            return Err(io::Error::last_os_error());
        }
        stat.f_flag
    };
    const ST_NOSUID: libc::c_ulong = 2;
    const ST_NODEV: libc::c_ulong = 4;
    const ST_NOEXEC: libc::c_ulong = 8;
    const ST_NOATIME: libc::c_ulong = 1024;
    const ST_NODIRATIME: libc::c_ulong = 2048;
    const ST_RELATIME: libc::c_ulong = 4096;
    let mut out = 0;
    for (st, ms) in [
        (ST_NOSUID, libc::MS_NOSUID),
        (ST_NODEV, libc::MS_NODEV),
        (ST_NOEXEC, libc::MS_NOEXEC),
        (ST_NOATIME, libc::MS_NOATIME),
        (ST_NODIRATIME, libc::MS_NODIRATIME),
        (ST_RELATIME, libc::MS_RELATIME),
    ] {
        if flags & st != 0 {
            out |= ms;
        }
    }
    Ok(out)
}

/// What the second child does, in the order of SDD 19 f.5; it returns only a failure: the step,
/// what the system said, and the status to end with.
fn inner(plan: &Plan, rootfs_flags: libc::c_ulong) -> (&'static str, String, i32) {
    macro_rules! step {
        ($name:literal, $e:expr) => {
            if let Err(e) = $e {
                return ($name, e.to_string(), SETUP_FAILED);
            }
        };
    }
    // The namespace dies with its first process; the command dies with the process that waits
    // for it, whatever kills that one.
    // SAFETY: prctl with integers.
    step!(
        "parent death",
        check(
            unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) },
            "prctl"
        )
    );
    step!("root", build_root(plan, rootfs_flags));
    step!("capabilities", drop_privileges());
    step!(
        "working directory",
        std::env::set_current_dir(Path::new(
            std::str::from_utf8(plan.working_dir.to_bytes()).unwrap_or("/")
        ))
        .map_err(|e| io::Error::new(
            e.kind(),
            format!(
                "{} (the image's working directory is not in the image): {e}",
                plan.working_dir.to_string_lossy()
            )
        ))
    );
    if let Err(e) = landlock() {
        return ("landlock", e, SETUP_FAILED);
    }
    if let Err(e) = crate::seccomp::apply_programs(&plan.filters) {
        return ("seccomp", e, SETUP_FAILED);
    }
    let mut argv: Vec<*const libc::c_char> = plan.argv.iter().map(|a| a.as_ptr()).collect();
    argv.push(core::ptr::null());
    let mut envp: Vec<*const libc::c_char> = plan.envp.iter().map(|a| a.as_ptr()).collect();
    envp.push(core::ptr::null());
    let mut code = NOT_FOUND;
    let mut last = io::Error::from_raw_os_error(libc::ENOENT);
    let mut tried = String::new();
    for path in &plan.candidates {
        // SAFETY: NUL-terminated strings and NULL-terminated pointer arrays that outlive the
        // call; execve returns only on failure.
        unsafe { libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOENT) {
            code = NOT_EXECUTABLE;
        }
        if !tried.is_empty() {
            tried.push_str(", ");
        }
        tried.push_str(&path.to_string_lossy());
        last = e;
    }
    ("exec", format!("{tried}: {last}"), code)
}

/// The new root (SDD 19 f.5 steps 3 to 6): the unpacked root bind-mounted read only, a `tmpfs`
/// on `/tmp`, a new procfs on `/proc`, a `tmpfs` on `/dev` holding the three devices; then
/// `pivot_root` into it and the old root gone.
fn build_root(plan: &Plan, rootfs_flags: libc::c_ulong) -> io::Result<()> {
    let root = &plan.rootfs;
    // Nothing mounted from here is seen outside.
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )?;
    mount(Some(root), root, None, libc::MS_BIND | libc::MS_REC, None)?;
    mount(
        None,
        root,
        None,
        libc::MS_BIND
            | libc::MS_REMOUNT
            | libc::MS_RDONLY
            | libc::MS_NOSUID
            | libc::MS_NODEV
            | rootfs_flags,
        None,
    )?;
    mount(
        Some(Path::new("tmpfs")),
        &root.join("tmp"),
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some(&format!("mode=1777,size={}", plan.tmp_bytes)),
    )?;
    mount(
        Some(Path::new("proc")),
        &root.join("proc"),
        Some("proc"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )?;
    let dev = root.join("dev");
    mount(
        Some(Path::new("tmpfs")),
        &dev,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NOEXEC,
        Some("mode=0755,size=64k"),
    )?;
    for device in DEVICES {
        let at = dev.join(device);
        File::create(&at)?;
        let from = Path::new("/dev").join(device);
        let inherited = inherited_flags(&from)?;
        mount(Some(&from), &at, None, libc::MS_BIND, None)?;
        mount(
            None,
            &at,
            None,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID | inherited,
            None,
        )?;
    }
    // The old root is put on the new one and unmounted from there: no directory to name for it.
    std::env::set_current_dir(root)?;
    let here = CString::new(".").map_err(io::Error::other)?;
    // SAFETY: two NUL-terminated paths; pivot_root has no libc wrapper.
    check(
        unsafe { libc::syscall(libc::SYS_pivot_root, here.as_ptr(), here.as_ptr()) } as i32,
        "pivot_root",
    )?;
    // SAFETY: a NUL-terminated path.
    check(
        unsafe { libc::umount2(here.as_ptr(), libc::MNT_DETACH) },
        "umount the old root",
    )?;
    std::env::set_current_dir("/")
}

/// Landlock (SDD 19 f.5 step 8): read and execute everywhere under `/`, read and write on the
/// three devices, everything under `/tmp`, and nothing else. A kernel without it refuses the
/// child.
fn landlock() -> Result<(), String> {
    let abi = ABI::V1;
    let read = AccessFs::from_read(abi);
    let all = AccessFs::from_all(abi);
    let mut ruleset = Ruleset::default()
        .handle_access(all)
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?;
    let rules = [(PathBuf::from("/"), read), (PathBuf::from("/tmp"), all)]
        .into_iter()
        .chain(DEVICES.iter().map(|d| {
            (
                Path::new("/dev").join(d),
                AccessFs::ReadFile | AccessFs::WriteFile,
            )
        }));
    for (path, access) in rules {
        let fd = PathFd::new(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access))
            .map_err(|e| e.to_string())?;
    }
    let status = ruleset.restrict_self().map_err(|e| e.to_string())?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(format!(
            "Landlock is not available ({:?}); an image's process is not started without it",
            status.ruleset
        ));
    }
    Ok(())
}
