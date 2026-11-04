//! Starting one program in a jail: `clone3` into new namespaces, the jail's root, the
//! credentials, `execve`. Between `clone3` and `execve` the child runs only system calls on
//! memory prepared before the clone (no allocation, no lock): the launching process may have
//! other threads.

use super::{Access, Entry, JAIL_ID, Jailed};
use crate::error::{Result, VmmError};
use std::{
    ffi::{CString, OsStr},
    fs::File,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
};

/// One program to start in a jail.
pub(crate) struct Spec<'a> {
    /// Canonical path of the program; argument zero.
    pub program: &'a Path,
    /// Arguments after argument zero.
    pub args: Vec<std::ffi::OsString>,
    /// Everything the jail shows (the program's own files included).
    pub entries: Vec<Entry>,
    /// An existing empty directory the jail's root is mounted on, inside its mount namespace
    /// only.
    pub root: &'a Path,
    /// Standard output; standard input is `/dev/null`, opened before the jail exists.
    pub stdout: File,
    /// Standard error.
    pub stderr: File,
    /// The cgroup leaf the process is born in, as a directory descriptor.
    pub cgroup: Option<RawFd>,
}

/// `struct clone_args` (linux/sched.h), version 2 (with `cgroup`).
#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;
const NAMESPACES: u64 = (libc::CLONE_NEWUSER
    | libc::CLONE_NEWNS
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWCGROUP) as u64;

const SECBIT_NOROOT: libc::c_ulong = 1 << 0;
const SECBIT_NOROOT_LOCKED: libc::c_ulong = 1 << 1;
const SECBIT_NO_SETUID_FIXUP: libc::c_ulong = 1 << 2;
const SECBIT_NO_SETUID_FIXUP_LOCKED: libc::c_ulong = 1 << 3;
const SECBIT_KEEP_CAPS_LOCKED: libc::c_ulong = 1 << 5;
const SECBIT_NO_CAP_AMBIENT_RAISE: libc::c_ulong = 1 << 6;
const SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED: libc::c_ulong = 1 << 7;
const SECUREBITS: libc::c_ulong = SECBIT_NOROOT
    | SECBIT_NOROOT_LOCKED
    | SECBIT_NO_SETUID_FIXUP
    | SECBIT_NO_SETUID_FIXUP_LOCKED
    | SECBIT_KEEP_CAPS_LOCKED
    | SECBIT_NO_CAP_AMBIENT_RAISE
    | SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED;

/// The steps the child takes, named in the error the launcher reports when one fails.
const STEPS: &[&str] = &[
    "deny setgroups",
    "write uid_map",
    "write gid_map",
    "install the namespace filter",
    "make mounts private",
    "mount the jail's root",
    "make a mount point directory",
    "make a mount point file",
    "bind an entry",
    "restrict a bound entry",
    "enter the jail's root",
    "pivot_root",
    "detach the node's file system",
    "make the jail's root read-only",
    "set up standard streams",
    "mark descriptors close-on-exec",
    "set no_new_privs",
    "drop the bounding set",
    "clear ambient capabilities",
    "lock securebits",
    "drop capabilities",
    "execve",
];
const STEP_SETGROUPS: u32 = 0;
const STEP_UID_MAP: u32 = 1;
const STEP_GID_MAP: u32 = 2;
const STEP_FILTER: u32 = 3;
const STEP_PRIVATE: u32 = 4;
const STEP_ROOT: u32 = 5;
const STEP_MKDIR: u32 = 6;
const STEP_TOUCH: u32 = 7;
const STEP_BIND: u32 = 8;
const STEP_REMOUNT: u32 = 9;
const STEP_CHDIR: u32 = 10;
const STEP_PIVOT: u32 = 11;
const STEP_DETACH: u32 = 12;
const STEP_ROOT_RO: u32 = 13;
const STEP_STDIO: u32 = 14;
const STEP_CLOEXEC: u32 = 15;
const STEP_NNP: u32 = 16;
const STEP_BOUNDING: u32 = 17;
const STEP_AMBIENT: u32 = 18;
const STEP_SECUREBITS: u32 = 19;
const STEP_CAPSET: u32 = 20;
const STEP_EXEC: u32 = 21;

enum Op {
    Mkdir(CString),
    Touch(CString),
    Bind {
        source: CString,
        target: CString,
        flags: libc::c_ulong,
    },
}

/// Everything the child needs, built before the clone.
struct Prepared {
    setgroups: CString,
    uid_map: CString,
    gid_map: CString,
    uid_line: Vec<u8>,
    gid_line: Vec<u8>,
    root: CString,
    tmpfs: CString,
    tmpfs_options: CString,
    slash: CString,
    dot: CString,
    ops: Vec<Op>,
    program: CString,
    _argv: Vec<CString>,
    argv: Vec<*const libc::c_char>,
    envp: [*const libc::c_char; 1],
    stdin: RawFd,
    stdout: RawFd,
    stderr: RawFd,
    report: RawFd,
    last_cap: libc::c_ulong,
    _filter: Vec<libc::sock_filter>,
    filter: libc::sock_fprog,
}

fn bad(message: impl Into<String>) -> VmmError {
    VmmError::config("jail", message)
}

fn cstring(bytes: &[u8]) -> Result<CString> {
    CString::new(bytes).map_err(|_| bad("path contains a NUL byte"))
}

fn inside(root: &Path, path: &Path) -> Result<CString> {
    let mut bytes = root.as_os_str().as_bytes().to_vec();
    bytes.extend_from_slice(path.as_os_str().as_bytes());
    cstring(&bytes)
}

/// The mount flags the kernel locks on a bind of `path` made in a user namespace: they must be
/// restated when the bind is restricted.
fn locked_flags(path: &Path) -> Result<libc::c_ulong> {
    let name = cstring(path.as_os_str().as_bytes())?;
    // SAFETY: statvfs fills a plain C struct for a NUL-terminated path.
    let mut info: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(name.as_ptr(), &mut info) } != 0 {
        return Err(VmmError::io(
            "statvfs",
            path.display().to_string(),
            &std::io::Error::last_os_error(),
        ));
    }
    let f = info.f_flag;
    let mut flags = 0;
    for (st, ms) in [
        (libc::ST_RDONLY, libc::MS_RDONLY),
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (libc::ST_RELATIME, libc::MS_RELATIME),
    ] {
        if f & st != 0 {
            flags |= ms;
        }
    }
    if f & (libc::ST_NOATIME | libc::ST_RELATIME) == 0 {
        flags |= libc::MS_STRICTATIME;
    }
    Ok(flags)
}

fn last_cap() -> Result<libc::c_ulong> {
    let text = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .map_err(|e| VmmError::io("read", "/proc/sys/kernel/cap_last_cap", &e))?;
    text.trim()
        .parse()
        .map_err(|_| bad("unreadable /proc/sys/kernel/cap_last_cap"))
}

fn prepare(spec: &Spec<'_>, stdin: RawFd, report: RawFd) -> Result<Prepared> {
    let root = spec.root;
    let mut ops = Vec::new();
    let mut made = std::collections::BTreeSet::new();
    for entry in &spec.entries {
        let path = entry.path();
        let parent = path
            .parent()
            .ok_or_else(|| bad(format!("{} has no parent", path.display())))?;
        let mut ancestors: Vec<&Path> = parent
            .ancestors()
            .filter(|a| *a != Path::new("/"))
            .collect();
        ancestors.reverse();
        for dir in ancestors {
            if made.insert(dir.to_path_buf()) {
                ops.push(Op::Mkdir(inside(root, dir)?));
            }
        }
        let source = cstring(path.as_os_str().as_bytes())?;
        let target = inside(root, path)?;
        let metadata = std::fs::metadata(path)
            .map_err(|e| VmmError::io("stat jail entry", path.display().to_string(), &e))?;
        let (access, exec, device) = match entry {
            Entry::File { access, exec, .. } => {
                if !metadata.is_file() {
                    return Err(bad(format!("{} is not a regular file", path.display())));
                }
                ops.push(Op::Touch(target.clone()));
                (*access, *exec, false)
            }
            Entry::Dir { access, .. } => {
                if !metadata.is_dir() {
                    return Err(bad(format!("{} is not a directory", path.display())));
                }
                if made.insert(path.to_path_buf()) {
                    ops.push(Op::Mkdir(target.clone()));
                }
                (*access, false, false)
            }
            Entry::Device { .. } => {
                use std::os::unix::fs::FileTypeExt;
                if !metadata.file_type().is_char_device() {
                    return Err(bad(format!("{} is not a character device", path.display())));
                }
                ops.push(Op::Touch(target.clone()));
                (Access::Write, false, true)
            }
        };
        let mut flags = locked_flags(path)? | libc::MS_NOSUID;
        if access == Access::Read {
            flags |= libc::MS_RDONLY;
        } else if flags & libc::MS_RDONLY != 0 {
            return Err(bad(format!(
                "{} must be writable but its file system is read-only",
                path.display()
            )));
        }
        if !exec {
            flags |= libc::MS_NOEXEC;
        }
        if !device {
            flags |= libc::MS_NODEV;
        } else if flags & libc::MS_NODEV != 0 {
            return Err(bad(format!(
                "{} lies on a nodev file system",
                path.display()
            )));
        }
        ops.push(Op::Bind {
            source,
            target,
            flags,
        });
    }
    let program = cstring(spec.program.as_os_str().as_bytes())?;
    let mut argv_owned = vec![program.clone()];
    for arg in &spec.args {
        argv_owned.push(cstring(OsStr::as_bytes(arg))?);
    }
    let mut argv: Vec<*const libc::c_char> = argv_owned.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    // SAFETY: plain getters.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let filter = super::seccomp::namespaces();
    let fprog = libc::sock_fprog {
        len: u16::try_from(filter.len()).map_err(|_| bad("namespace filter too long"))?,
        filter: filter.as_ptr().cast_mut(),
    };
    Ok(Prepared {
        setgroups: cstring(b"/proc/self/setgroups")?,
        uid_map: cstring(b"/proc/self/uid_map")?,
        gid_map: cstring(b"/proc/self/gid_map")?,
        uid_line: format!("{JAIL_ID} {uid} 1\n").into_bytes(),
        gid_line: format!("{JAIL_ID} {gid} 1\n").into_bytes(),
        root: cstring(root.as_os_str().as_bytes())?,
        tmpfs: cstring(b"tmpfs")?,
        tmpfs_options: cstring(b"mode=0755,size=1m,nr_inodes=4096")?,
        slash: cstring(b"/")?,
        dot: cstring(b".")?,
        ops,
        program,
        _argv: argv_owned,
        argv,
        envp: [std::ptr::null()],
        stdin,
        stdout: spec.stdout.as_raw_fd(),
        stderr: spec.stderr.as_raw_fd(),
        report,
        last_cap: last_cap()?,
        _filter: filter,
        filter: fprog,
    })
}

/// Start `spec.program` in a new jail. Returns once the program has been executed inside it,
/// or with the step that failed and its errno.
pub(crate) fn spawn(spec: Spec<'_>) -> Result<Jailed> {
    let stdin = File::open("/dev/null").map_err(|e| VmmError::io("open", "/dev/null", &e))?;
    let mut pipe = [0; 2];
    // SAFETY: pipe2 fills two descriptors we then own.
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(VmmError::io(
            "pipe",
            "jail report",
            &std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: freshly created, owned here.
    let (reader, writer) =
        unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
    let prepared = prepare(&spec, stdin.as_raw_fd(), writer.as_raw_fd())?;
    let mut args = CloneArgs {
        flags: NAMESPACES,
        exit_signal: libc::SIGCHLD as u64,
        ..CloneArgs::default()
    };
    if let Some(fd) = spec.cgroup {
        args.flags |= CLONE_INTO_CGROUP;
        args.cgroup = fd as u64;
    }
    // SAFETY: clone3 without CLONE_VM gives the child a copy of this address space, like fork;
    // the child calls only `child`, which makes system calls on `prepared` and never returns.
    let pid = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &mut args as *mut CloneArgs,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    if pid == 0 {
        // SAFETY: in the child, see `child`.
        unsafe { child(&prepared) }
    }
    if pid < 0 {
        return Err(VmmError::io(
            "clone3",
            format!("jail for {}", spec.program.display()),
            &std::io::Error::last_os_error(),
        ));
    }
    drop(writer);
    let mut jailed = Jailed::new(pid as libc::pid_t);
    let mut report = [0u8; 8];
    let mut file = File::from(reader);
    let mut read = 0;
    while read < report.len() {
        match file.read(&mut report[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(VmmError::io("read", "jail report", &e)),
        }
    }
    drop(prepared);
    if read == 0 {
        return Ok(jailed);
    }
    let status = jailed
        .wait()
        .map_err(|e| VmmError::io("reap", "failed jail", &e))?;
    if read != report.len() {
        return Err(bad(format!(
            "jail setup ended with a torn report ({status})"
        )));
    }
    let step = u32::from_le_bytes(report[..4].try_into().unwrap_or([0; 4]));
    let errno = i32::from_le_bytes(report[4..].try_into().unwrap_or([0; 4]));
    Err(VmmError::Io {
        op: "jail",
        target: format!(
            "{}: {}",
            spec.program.display(),
            STEPS.get(step as usize).copied().unwrap_or("unknown step")
        ),
        msg: std::io::Error::from_raw_os_error(errno).to_string(),
    })
}

/// Report the failed step and errno on the report pipe and exit.
unsafe fn fail(p: &Prepared, step: u32) -> ! {
    // SAFETY: errno read, a write of a stack buffer, _exit: all async-signal-safe.
    unsafe {
        let errno = *libc::__errno_location();
        let mut report = [0u8; 8];
        report[..4].copy_from_slice(&step.to_le_bytes());
        report[4..].copy_from_slice(&errno.to_le_bytes());
        libc::write(p.report, report.as_ptr().cast(), report.len());
        libc::_exit(127)
    }
}

unsafe fn write_file(p: &Prepared, path: &CString, bytes: &[u8], step: u32) {
    // SAFETY: open/write/close of prepared, NUL-terminated paths and buffers.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            fail(p, step);
        }
        if libc::write(fd, bytes.as_ptr().cast(), bytes.len()) != bytes.len() as isize {
            fail(p, step);
        }
        libc::close(fd);
    }
}

/// The child between `clone3` and `execve`. Only system calls on prepared memory.
unsafe fn child(p: &Prepared) -> ! {
    // SAFETY: every call below is a system call (or its thin libc wrapper) on memory `p`
    // owns, which this copy of the address space keeps alive; none allocates or locks.
    unsafe {
        let null = std::ptr::null::<libc::c_char>();
        write_file(p, &p.setgroups, b"deny", STEP_SETGROUPS);
        write_file(p, &p.uid_map, &p.uid_line, STEP_UID_MAP);
        write_file(p, &p.gid_map, &p.gid_line, STEP_GID_MAP);
        if libc::mount(
            null,
            p.slash.as_ptr(),
            null,
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ) != 0
        {
            fail(p, STEP_PRIVATE);
        }
        if libc::mount(
            p.tmpfs.as_ptr(),
            p.root.as_ptr(),
            p.tmpfs.as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            p.tmpfs_options.as_ptr().cast(),
        ) != 0
        {
            fail(p, STEP_ROOT);
        }
        for op in &p.ops {
            match op {
                Op::Mkdir(path) => {
                    if libc::mkdir(path.as_ptr(), 0o755) != 0
                        && *libc::__errno_location() != libc::EEXIST
                    {
                        fail(p, STEP_MKDIR);
                    }
                }
                Op::Touch(path) => {
                    let fd = libc::open(
                        path.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                        0o600,
                    );
                    if fd < 0 {
                        fail(p, STEP_TOUCH);
                    }
                    libc::close(fd);
                }
                Op::Bind {
                    source,
                    target,
                    flags,
                } => {
                    if libc::mount(
                        source.as_ptr(),
                        target.as_ptr(),
                        null,
                        libc::MS_BIND,
                        std::ptr::null(),
                    ) != 0
                    {
                        fail(p, STEP_BIND);
                    }
                    if libc::mount(
                        null,
                        target.as_ptr(),
                        null,
                        libc::MS_REMOUNT | libc::MS_BIND | flags,
                        std::ptr::null(),
                    ) != 0
                    {
                        fail(p, STEP_REMOUNT);
                    }
                }
            }
        }
        if libc::chdir(p.root.as_ptr()) != 0 {
            fail(p, STEP_CHDIR);
        }
        if libc::syscall(libc::SYS_pivot_root, p.dot.as_ptr(), p.dot.as_ptr()) != 0 {
            fail(p, STEP_PIVOT);
        }
        if libc::umount2(p.dot.as_ptr(), libc::MNT_DETACH) != 0 {
            fail(p, STEP_DETACH);
        }
        if libc::chdir(p.slash.as_ptr()) != 0 {
            fail(p, STEP_CHDIR);
        }
        if libc::mount(
            null,
            p.slash.as_ptr(),
            null,
            libc::MS_REMOUNT
                | libc::MS_BIND
                | libc::MS_RDONLY
                | libc::MS_NOSUID
                | libc::MS_NODEV
                | libc::MS_NOEXEC,
            std::ptr::null(),
        ) != 0
        {
            fail(p, STEP_ROOT_RO);
        }
        if libc::dup2(p.stdin, 0) < 0 || libc::dup2(p.stdout, 1) < 0 || libc::dup2(p.stderr, 2) < 0
        {
            fail(p, STEP_STDIO);
        }
        if libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        ) != 0
        {
            fail(p, STEP_CLOEXEC);
        }
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            fail(p, STEP_NNP);
        }
        let mut cap: libc::c_ulong = 0;
        while cap <= p.last_cap {
            if libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) != 0 {
                fail(p, STEP_BOUNDING);
            }
            cap += 1;
        }
        if libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        ) != 0
        {
            fail(p, STEP_AMBIENT);
        }
        if libc::prctl(libc::PR_SET_SECUREBITS, SECUREBITS, 0, 0, 0) != 0 {
            fail(p, STEP_SECUREBITS);
        }
        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        struct Data {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        let header = Header {
            version: 0x2008_0522,
            pid: 0,
        };
        let data = [
            Data {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
            Data {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
        ];
        if libc::syscall(libc::SYS_capset, &header as *const Header, data.as_ptr()) != 0 {
            fail(p, STEP_CAPSET);
        }
        // Last: no namespace of its own, nested user namespaces included (the one kind an
        // unprivileged process could make, with the capabilities that come with it).
        if libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &p.filter as *const libc::sock_fprog,
        ) != 0
        {
            fail(p, STEP_FILTER);
        }
        libc::execve(p.program.as_ptr(), p.argv.as_ptr(), p.envp.as_ptr());
        fail(p, STEP_EXEC)
    }
}
