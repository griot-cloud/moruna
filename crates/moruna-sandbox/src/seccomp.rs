//! The child's system-call allow-list (17 e.5, SB-I5): what the interpreter and the data
//! libraries call, and nothing that opens a connection, executes a program, enters or makes a
//! namespace, mounts, traces or signals another process. Every other call fails with `EPERM`;
//! `clone3` fails with `ENOSYS`, so the C library falls back to `clone`, whose flags the filter
//! reads.

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// The calls allowed unconditionally, by name, on every architecture (17 e.5).
pub const ALLOWED: &[&str] = &[
    "read",
    "write",
    "readv",
    "writev",
    "pread64",
    "pwrite64",
    "preadv",
    "pwritev",
    "lseek",
    "close",
    "close_range",
    "openat",
    "openat2",
    "newfstatat",
    "fstat",
    "statx",
    "statfs",
    "fstatfs",
    "getdents64",
    "readlinkat",
    "faccessat",
    "faccessat2",
    "getcwd",
    "chdir",
    "fchdir",
    "fcntl",
    "ioctl",
    "dup",
    "dup3",
    "pipe2",
    "mkdirat",
    "unlinkat",
    "renameat",
    "renameat2",
    "ftruncate",
    "fallocate",
    "fadvise64",
    "fsync",
    "fdatasync",
    "flock",
    "fchmod",
    "fchmodat",
    "utimensat",
    "umask",
    "copy_file_range",
    "mmap",
    "munmap",
    "mremap",
    "mprotect",
    "madvise",
    "mincore",
    "brk",
    "membarrier",
    "futex",
    "futex_waitv",
    "set_robust_list",
    "get_robust_list",
    "rseq",
    "set_tid_address",
    "clone3",
    "exit",
    "exit_group",
    "wait4",
    "restart_syscall",
    "rt_sigaction",
    "rt_sigprocmask",
    "rt_sigreturn",
    "rt_sigtimedwait",
    "sigaltstack",
    "getpid",
    "gettid",
    "getppid",
    "getuid",
    "geteuid",
    "getgid",
    "getegid",
    "getresuid",
    "getresgid",
    "getgroups",
    "getrandom",
    "getcpu",
    "clock_gettime",
    "clock_getres",
    "clock_nanosleep",
    "nanosleep",
    "gettimeofday",
    "sched_yield",
    "sched_getaffinity",
    "sched_getparam",
    "sched_getscheduler",
    "getpriority",
    "prlimit64",
    "getrusage",
    "sysinfo",
    "uname",
    "prctl",
    "ppoll",
    "pselect6",
    "epoll_create1",
    "epoll_ctl",
    "epoll_pwait",
    "epoll_pwait2",
    "eventfd2",
    "sendmsg",
    "recvmsg",
    "sendto",
    "recvfrom",
    "shutdown",
    "getsockopt",
];

/// The calls allowed unconditionally on x86_64 only: the older forms its C library still uses.
#[cfg(target_arch = "x86_64")]
pub const ALLOWED_ARCH: &[&str] = &[
    "open",
    "stat",
    "lstat",
    "access",
    "readlink",
    "poll",
    "select",
    "pipe",
    "dup2",
    "mkdir",
    "unlink",
    "rename",
    "arch_prctl",
    "epoll_wait",
    "getdents",
    "time",
    "getrlimit",
];
/// The calls allowed unconditionally on aarch64 only.
#[cfg(not(target_arch = "x86_64"))]
pub const ALLOWED_ARCH: &[&str] = &[];

/// `clone` flags that make or enter a namespace: a `clone` with any of them is refused.
const NAMESPACE_FLAGS: u64 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWCGROUP
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET) as u64;

fn number(name: &str) -> Option<i64> {
    let n = match name {
        "read" => libc::SYS_read,
        "write" => libc::SYS_write,
        "readv" => libc::SYS_readv,
        "writev" => libc::SYS_writev,
        "pread64" => libc::SYS_pread64,
        "pwrite64" => libc::SYS_pwrite64,
        "preadv" => libc::SYS_preadv,
        "pwritev" => libc::SYS_pwritev,
        "lseek" => libc::SYS_lseek,
        "close" => libc::SYS_close,
        "close_range" => libc::SYS_close_range,
        "openat" => libc::SYS_openat,
        "openat2" => libc::SYS_openat2,
        "newfstatat" => libc::SYS_newfstatat,
        "fstat" => libc::SYS_fstat,
        "statx" => libc::SYS_statx,
        "statfs" => libc::SYS_statfs,
        "fstatfs" => libc::SYS_fstatfs,
        "getdents64" => libc::SYS_getdents64,
        "readlinkat" => libc::SYS_readlinkat,
        "faccessat" => libc::SYS_faccessat,
        "faccessat2" => libc::SYS_faccessat2,
        "getcwd" => libc::SYS_getcwd,
        "chdir" => libc::SYS_chdir,
        "fchdir" => libc::SYS_fchdir,
        "fcntl" => libc::SYS_fcntl,
        "ioctl" => libc::SYS_ioctl,
        "dup" => libc::SYS_dup,
        "dup3" => libc::SYS_dup3,
        "pipe2" => libc::SYS_pipe2,
        "mkdirat" => libc::SYS_mkdirat,
        "unlinkat" => libc::SYS_unlinkat,
        "renameat" => libc::SYS_renameat,
        "renameat2" => libc::SYS_renameat2,
        "ftruncate" => libc::SYS_ftruncate,
        "fallocate" => libc::SYS_fallocate,
        "fadvise64" => libc::SYS_fadvise64,
        "fsync" => libc::SYS_fsync,
        "fdatasync" => libc::SYS_fdatasync,
        "flock" => libc::SYS_flock,
        "fchmod" => libc::SYS_fchmod,
        "fchmodat" => libc::SYS_fchmodat,
        "utimensat" => libc::SYS_utimensat,
        "umask" => libc::SYS_umask,
        "copy_file_range" => libc::SYS_copy_file_range,
        "mmap" => libc::SYS_mmap,
        "munmap" => libc::SYS_munmap,
        "mremap" => libc::SYS_mremap,
        "mprotect" => libc::SYS_mprotect,
        "madvise" => libc::SYS_madvise,
        "mincore" => libc::SYS_mincore,
        "brk" => libc::SYS_brk,
        "membarrier" => libc::SYS_membarrier,
        "futex" => libc::SYS_futex,
        "futex_waitv" => libc::SYS_futex_waitv,
        "set_robust_list" => libc::SYS_set_robust_list,
        "get_robust_list" => libc::SYS_get_robust_list,
        "rseq" => libc::SYS_rseq,
        "set_tid_address" => libc::SYS_set_tid_address,
        "clone3" => libc::SYS_clone3,
        "clone" => libc::SYS_clone,
        "exit" => libc::SYS_exit,
        "exit_group" => libc::SYS_exit_group,
        "wait4" => libc::SYS_wait4,
        "restart_syscall" => libc::SYS_restart_syscall,
        "rt_sigaction" => libc::SYS_rt_sigaction,
        "rt_sigprocmask" => libc::SYS_rt_sigprocmask,
        "rt_sigreturn" => libc::SYS_rt_sigreturn,
        "rt_sigtimedwait" => libc::SYS_rt_sigtimedwait,
        "sigaltstack" => libc::SYS_sigaltstack,
        "getpid" => libc::SYS_getpid,
        "gettid" => libc::SYS_gettid,
        "getppid" => libc::SYS_getppid,
        "getuid" => libc::SYS_getuid,
        "geteuid" => libc::SYS_geteuid,
        "getgid" => libc::SYS_getgid,
        "getegid" => libc::SYS_getegid,
        "getresuid" => libc::SYS_getresuid,
        "getresgid" => libc::SYS_getresgid,
        "getgroups" => libc::SYS_getgroups,
        "getrandom" => libc::SYS_getrandom,
        "getcpu" => libc::SYS_getcpu,
        "clock_gettime" => libc::SYS_clock_gettime,
        "clock_getres" => libc::SYS_clock_getres,
        "clock_nanosleep" => libc::SYS_clock_nanosleep,
        "nanosleep" => libc::SYS_nanosleep,
        "gettimeofday" => libc::SYS_gettimeofday,
        "sched_yield" => libc::SYS_sched_yield,
        "sched_getaffinity" => libc::SYS_sched_getaffinity,
        "sched_getparam" => libc::SYS_sched_getparam,
        "sched_getscheduler" => libc::SYS_sched_getscheduler,
        "getpriority" => libc::SYS_getpriority,
        "prlimit64" => libc::SYS_prlimit64,
        "getrusage" => libc::SYS_getrusage,
        "sysinfo" => libc::SYS_sysinfo,
        "uname" => libc::SYS_uname,
        "prctl" => libc::SYS_prctl,
        "ppoll" => libc::SYS_ppoll,
        "pselect6" => libc::SYS_pselect6,
        "epoll_create1" => libc::SYS_epoll_create1,
        "epoll_ctl" => libc::SYS_epoll_ctl,
        "epoll_pwait" => libc::SYS_epoll_pwait,
        "epoll_pwait2" => libc::SYS_epoll_pwait2,
        "eventfd2" => libc::SYS_eventfd2,
        "sendmsg" => libc::SYS_sendmsg,
        "recvmsg" => libc::SYS_recvmsg,
        "sendto" => libc::SYS_sendto,
        "recvfrom" => libc::SYS_recvfrom,
        "shutdown" => libc::SYS_shutdown,
        "getsockopt" => libc::SYS_getsockopt,
        "kill" => libc::SYS_kill,
        "tgkill" => libc::SYS_tgkill,
        #[cfg(target_arch = "x86_64")]
        other => return x86_64(other),
        #[cfg(not(target_arch = "x86_64"))]
        _ => return None,
    };
    Some(n)
}

#[cfg(target_arch = "x86_64")]
fn x86_64(name: &str) -> Option<i64> {
    let n = match name {
        "open" => libc::SYS_open,
        "stat" => libc::SYS_stat,
        "lstat" => libc::SYS_lstat,
        "access" => libc::SYS_access,
        "readlink" => libc::SYS_readlink,
        "poll" => libc::SYS_poll,
        "select" => libc::SYS_select,
        "pipe" => libc::SYS_pipe,
        "dup2" => libc::SYS_dup2,
        "mkdir" => libc::SYS_mkdir,
        "unlink" => libc::SYS_unlink,
        "rename" => libc::SYS_rename,
        "arch_prctl" => libc::SYS_arch_prctl,
        "epoll_wait" => libc::SYS_epoll_wait,
        "getdents" => libc::SYS_getdents,
        "time" => libc::SYS_time,
        "getrlimit" => libc::SYS_getrlimit,
        _ => return None,
    };
    Some(n)
}

fn arch() -> Result<TargetArch, String> {
    if cfg!(target_arch = "x86_64") {
        Ok(TargetArch::x86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(TargetArch::aarch64)
    } else {
        Err("the sandbox's seccomp filter is built for x86_64 and aarch64".into())
    }
}

fn condition(arg: u8, op: SeccompCmpOp, value: u64) -> Result<SeccompCondition, String> {
    SeccompCondition::new(arg, SeccompCmpArgLen::Qword, op, value).map_err(|e| e.to_string())
}

/// The two filters, in the order they are installed: `clone3` as `ENOSYS`, then the allow-list
/// (everything else `EPERM`), which does not allow `seccomp` itself and so goes last. `pid` is the child's own, the only target `kill` and `tgkill` may
/// name.
pub fn filters(pid: u32) -> Result<Vec<BpfProgram>, String> {
    let arch = arch()?;
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for name in ALLOWED.iter().chain(ALLOWED_ARCH) {
        let n = number(name).ok_or_else(|| format!("no system call number for `{name}`"))?;
        rules.insert(n, Vec::new());
    }
    let clone = number("clone").ok_or("no system call number for `clone`")?;
    rules.insert(
        clone,
        vec![
            SeccompRule::new(vec![condition(
                0,
                SeccompCmpOp::MaskedEq(NAMESPACE_FLAGS),
                0,
            )?])
            .map_err(|e| e.to_string())?,
        ],
    );
    let own = u64::from(pid);
    for (name, arg) in [("kill", 0u8), ("tgkill", 0u8)] {
        let n = number(name).ok_or_else(|| format!("no system call number for `{name}`"))?;
        rules.insert(
            n,
            vec![
                SeccompRule::new(vec![condition(arg, SeccompCmpOp::Eq, own)?])
                    .map_err(|e| e.to_string())?,
            ],
        );
    }
    let allow = SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::EPERM as u32),
        SeccompAction::Allow,
        arch,
    )
    .map_err(|e| e.to_string())?;
    let clone3 = number("clone3").ok_or("no system call number for `clone3`")?;
    let enosys = SeccompFilter::new(
        [(clone3, Vec::new())].into_iter().collect(),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )
    .map_err(|e| e.to_string())?;
    Ok(vec![
        enosys
            .try_into()
            .map_err(|e: seccompiler::BackendError| e.to_string())?,
        allow
            .try_into()
            .map_err(|e: seccompiler::BackendError| e.to_string())?,
    ])
}

/// Install the filters on the calling thread (and, with `no_new_privs` set, every thread it
/// starts).
pub fn apply(pid: u32) -> Result<(), String> {
    for program in filters(pid)? {
        seccompiler::apply_filter(&program).map_err(|e| format!("seccomp: {e}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The allow-list of an image's process (SDD 19 e.4, IM-I5): what any program in any language
// needs to start and run, and nothing that reaches outside its own namespaces. A call not on it
// kills the process (`SECCOMP_RET_KILL_PROCESS`, so the parent sees `SIGSYS`); `clone3` fails
// with `ENOSYS` so the C library falls back to `clone`, whose flags the filter reads.
// ---------------------------------------------------------------------------------------------

/// The calls an image's process may make, by name, on every architecture (SDD 19 e.4).
pub const IMAGE_ALLOWED: &[&str] = &[
    "read",
    "write",
    "readv",
    "writev",
    "pread64",
    "pwrite64",
    "preadv",
    "pwritev",
    "preadv2",
    "pwritev2",
    "lseek",
    "close",
    "close_range",
    "openat",
    "openat2",
    "newfstatat",
    "fstat",
    "statx",
    "statfs",
    "fstatfs",
    "getdents64",
    "readlinkat",
    "faccessat",
    "faccessat2",
    "getcwd",
    "chdir",
    "fchdir",
    "fcntl",
    "ioctl",
    "dup",
    "dup3",
    "pipe2",
    "mkdirat",
    "unlinkat",
    "renameat",
    "renameat2",
    "linkat",
    "symlinkat",
    "ftruncate",
    "truncate",
    "fallocate",
    "fadvise64",
    "fsync",
    "fdatasync",
    "sync_file_range",
    "syncfs",
    "sync",
    "flock",
    "fchmod",
    "fchmodat",
    "fchown",
    "fchownat",
    "utimensat",
    "umask",
    "copy_file_range",
    "sendfile",
    "splice",
    "tee",
    "vmsplice",
    "readahead",
    "inotify_init1",
    "inotify_add_watch",
    "inotify_rm_watch",
    "getxattr",
    "lgetxattr",
    "fgetxattr",
    "listxattr",
    "llistxattr",
    "flistxattr",
    "setxattr",
    "lsetxattr",
    "fsetxattr",
    "removexattr",
    "lremovexattr",
    "fremovexattr",
    "mmap",
    "munmap",
    "mremap",
    "mprotect",
    "madvise",
    "mincore",
    "brk",
    "mlock",
    "mlock2",
    "munlock",
    "mlockall",
    "munlockall",
    "msync",
    "membarrier",
    "memfd_create",
    "get_mempolicy",
    "set_mempolicy",
    "mbind",
    "pkey_alloc",
    "pkey_free",
    "pkey_mprotect",
    "shmget",
    "shmat",
    "shmctl",
    "shmdt",
    "semget",
    "semop",
    "semctl",
    "semtimedop",
    "msgget",
    "msgsnd",
    "msgrcv",
    "msgctl",
    "futex",
    "futex_waitv",
    "set_robust_list",
    "get_robust_list",
    "rseq",
    "set_tid_address",
    "clone3",
    "exit",
    "exit_group",
    "wait4",
    "waitid",
    "execve",
    "execveat",
    "restart_syscall",
    "prctl",
    "capget",
    "capset",
    "getpid",
    "gettid",
    "getppid",
    "getpgid",
    "setpgid",
    "getsid",
    "setsid",
    "getuid",
    "geteuid",
    "getgid",
    "getegid",
    "getresuid",
    "getresgid",
    "getgroups",
    "setuid",
    "setgid",
    "setreuid",
    "setregid",
    "setresuid",
    "setresgid",
    "setgroups",
    "setfsuid",
    "setfsgid",
    "getpriority",
    "setpriority",
    "sched_yield",
    "sched_getaffinity",
    "sched_setaffinity",
    "sched_getparam",
    "sched_setparam",
    "sched_getscheduler",
    "sched_setscheduler",
    "sched_get_priority_max",
    "sched_get_priority_min",
    "sched_rr_get_interval",
    "sched_getattr",
    "sched_setattr",
    "getcpu",
    "prlimit64",
    "getrusage",
    "sysinfo",
    "uname",
    "times",
    "personality",
    "ioprio_get",
    "ioprio_set",
    "rt_sigaction",
    "rt_sigprocmask",
    "rt_sigreturn",
    "rt_sigtimedwait",
    "rt_sigsuspend",
    "rt_sigpending",
    "rt_sigqueueinfo",
    "rt_tgsigqueueinfo",
    "sigaltstack",
    "kill",
    "tkill",
    "tgkill",
    "signalfd4",
    "setitimer",
    "getitimer",
    "pidfd_send_signal",
    "pidfd_open",
    "clock_gettime",
    "clock_getres",
    "clock_nanosleep",
    "nanosleep",
    "gettimeofday",
    "timerfd_create",
    "timerfd_settime",
    "timerfd_gettime",
    "timer_create",
    "timer_settime",
    "timer_gettime",
    "timer_getoverrun",
    "timer_delete",
    "ppoll",
    "pselect6",
    "epoll_create1",
    "epoll_ctl",
    "epoll_pwait",
    "epoll_pwait2",
    "eventfd2",
    "socketpair",
    "sendmsg",
    "recvmsg",
    "sendto",
    "recvfrom",
    "sendmmsg",
    "recvmmsg",
    "shutdown",
    "getsockopt",
    "setsockopt",
    "getsockname",
    "getpeername",
    "getrandom",
];

/// The calls an image's process may make on x86_64 only: the older forms its C library still
/// uses, which aarch64 never had.
#[cfg(target_arch = "x86_64")]
pub const IMAGE_ALLOWED_ARCH: &[&str] = &[
    "open",
    "stat",
    "lstat",
    "access",
    "readlink",
    "poll",
    "select",
    "pipe",
    "dup2",
    "mkdir",
    "rmdir",
    "unlink",
    "rename",
    "link",
    "symlink",
    "chmod",
    "chown",
    "lchown",
    "creat",
    "getdents",
    "utime",
    "utimes",
    "futimesat",
    "arch_prctl",
    "epoll_wait",
    "epoll_create",
    "eventfd",
    "signalfd",
    "inotify_init",
    "time",
    "getrlimit",
    "setrlimit",
    "getpgrp",
    "fork",
    "vfork",
    "pause",
    "alarm",
];
/// The calls an image's process may make on aarch64 only.
#[cfg(not(target_arch = "x86_64"))]
pub const IMAGE_ALLOWED_ARCH: &[&str] = &[];

/// The calls that exist in an image's list and not in the `.pex`'s, by name.
fn image_number(name: &str) -> Option<i64> {
    if let Some(n) = number(name) {
        return Some(n);
    }
    let n = match name {
        "preadv2" => libc::SYS_preadv2,
        "pwritev2" => libc::SYS_pwritev2,
        "linkat" => libc::SYS_linkat,
        "symlinkat" => libc::SYS_symlinkat,
        "truncate" => libc::SYS_truncate,
        "sync_file_range" => libc::SYS_sync_file_range,
        "syncfs" => libc::SYS_syncfs,
        "sync" => libc::SYS_sync,
        "fchown" => libc::SYS_fchown,
        "fchownat" => libc::SYS_fchownat,
        "sendfile" => libc::SYS_sendfile,
        "splice" => libc::SYS_splice,
        "tee" => libc::SYS_tee,
        "vmsplice" => libc::SYS_vmsplice,
        "readahead" => libc::SYS_readahead,
        "inotify_init1" => libc::SYS_inotify_init1,
        "inotify_add_watch" => libc::SYS_inotify_add_watch,
        "inotify_rm_watch" => libc::SYS_inotify_rm_watch,
        "getxattr" => libc::SYS_getxattr,
        "lgetxattr" => libc::SYS_lgetxattr,
        "fgetxattr" => libc::SYS_fgetxattr,
        "listxattr" => libc::SYS_listxattr,
        "llistxattr" => libc::SYS_llistxattr,
        "flistxattr" => libc::SYS_flistxattr,
        "setxattr" => libc::SYS_setxattr,
        "lsetxattr" => libc::SYS_lsetxattr,
        "fsetxattr" => libc::SYS_fsetxattr,
        "removexattr" => libc::SYS_removexattr,
        "lremovexattr" => libc::SYS_lremovexattr,
        "fremovexattr" => libc::SYS_fremovexattr,
        "mlock" => libc::SYS_mlock,
        "mlock2" => libc::SYS_mlock2,
        "munlock" => libc::SYS_munlock,
        "mlockall" => libc::SYS_mlockall,
        "munlockall" => libc::SYS_munlockall,
        "msync" => libc::SYS_msync,
        "memfd_create" => libc::SYS_memfd_create,
        "get_mempolicy" => libc::SYS_get_mempolicy,
        "set_mempolicy" => libc::SYS_set_mempolicy,
        "mbind" => libc::SYS_mbind,
        "pkey_alloc" => libc::SYS_pkey_alloc,
        "pkey_free" => libc::SYS_pkey_free,
        "pkey_mprotect" => libc::SYS_pkey_mprotect,
        "shmget" => libc::SYS_shmget,
        "shmat" => libc::SYS_shmat,
        "shmctl" => libc::SYS_shmctl,
        "shmdt" => libc::SYS_shmdt,
        "semget" => libc::SYS_semget,
        "semop" => libc::SYS_semop,
        "semctl" => libc::SYS_semctl,
        "semtimedop" => libc::SYS_semtimedop,
        "msgget" => libc::SYS_msgget,
        "msgsnd" => libc::SYS_msgsnd,
        "msgrcv" => libc::SYS_msgrcv,
        "msgctl" => libc::SYS_msgctl,
        "waitid" => libc::SYS_waitid,
        "execve" => libc::SYS_execve,
        "execveat" => libc::SYS_execveat,
        "capget" => libc::SYS_capget,
        "capset" => libc::SYS_capset,
        "getpgid" => libc::SYS_getpgid,
        "setpgid" => libc::SYS_setpgid,
        "getsid" => libc::SYS_getsid,
        "setsid" => libc::SYS_setsid,
        "setuid" => libc::SYS_setuid,
        "setgid" => libc::SYS_setgid,
        "setreuid" => libc::SYS_setreuid,
        "setregid" => libc::SYS_setregid,
        "setresuid" => libc::SYS_setresuid,
        "setresgid" => libc::SYS_setresgid,
        "setgroups" => libc::SYS_setgroups,
        "setfsuid" => libc::SYS_setfsuid,
        "setfsgid" => libc::SYS_setfsgid,
        "setpriority" => libc::SYS_setpriority,
        "sched_setaffinity" => libc::SYS_sched_setaffinity,
        "sched_setparam" => libc::SYS_sched_setparam,
        "sched_setscheduler" => libc::SYS_sched_setscheduler,
        "sched_get_priority_max" => libc::SYS_sched_get_priority_max,
        "sched_get_priority_min" => libc::SYS_sched_get_priority_min,
        "sched_rr_get_interval" => libc::SYS_sched_rr_get_interval,
        "sched_getattr" => libc::SYS_sched_getattr,
        "sched_setattr" => libc::SYS_sched_setattr,
        "times" => libc::SYS_times,
        "personality" => libc::SYS_personality,
        "ioprio_get" => libc::SYS_ioprio_get,
        "ioprio_set" => libc::SYS_ioprio_set,
        "rt_sigsuspend" => libc::SYS_rt_sigsuspend,
        "rt_sigpending" => libc::SYS_rt_sigpending,
        "rt_sigqueueinfo" => libc::SYS_rt_sigqueueinfo,
        "rt_tgsigqueueinfo" => libc::SYS_rt_tgsigqueueinfo,
        "tkill" => libc::SYS_tkill,
        "signalfd4" => libc::SYS_signalfd4,
        "setitimer" => libc::SYS_setitimer,
        "getitimer" => libc::SYS_getitimer,
        "pidfd_send_signal" => libc::SYS_pidfd_send_signal,
        "pidfd_open" => libc::SYS_pidfd_open,
        "timerfd_create" => libc::SYS_timerfd_create,
        "timerfd_settime" => libc::SYS_timerfd_settime,
        "timerfd_gettime" => libc::SYS_timerfd_gettime,
        "timer_create" => libc::SYS_timer_create,
        "timer_settime" => libc::SYS_timer_settime,
        "timer_gettime" => libc::SYS_timer_gettime,
        "timer_getoverrun" => libc::SYS_timer_getoverrun,
        "timer_delete" => libc::SYS_timer_delete,
        "socketpair" => libc::SYS_socketpair,
        "sendmmsg" => libc::SYS_sendmmsg,
        "recvmmsg" => libc::SYS_recvmmsg,
        "setsockopt" => libc::SYS_setsockopt,
        "getsockname" => libc::SYS_getsockname,
        "getpeername" => libc::SYS_getpeername,
        #[cfg(target_arch = "x86_64")]
        other => return image_x86_64(other),
        #[cfg(not(target_arch = "x86_64"))]
        _ => return None,
    };
    Some(n)
}

#[cfg(target_arch = "x86_64")]
fn image_x86_64(name: &str) -> Option<i64> {
    if let Some(n) = x86_64(name) {
        return Some(n);
    }
    let n = match name {
        "rmdir" => libc::SYS_rmdir,
        "link" => libc::SYS_link,
        "symlink" => libc::SYS_symlink,
        "chmod" => libc::SYS_chmod,
        "chown" => libc::SYS_chown,
        "lchown" => libc::SYS_lchown,
        "creat" => libc::SYS_creat,
        "utime" => libc::SYS_utime,
        "utimes" => libc::SYS_utimes,
        "futimesat" => libc::SYS_futimesat,
        "epoll_create" => libc::SYS_epoll_create,
        "eventfd" => libc::SYS_eventfd,
        "signalfd" => libc::SYS_signalfd,
        "inotify_init" => libc::SYS_inotify_init,
        "setrlimit" => libc::SYS_setrlimit,
        "getpgrp" => libc::SYS_getpgrp,
        "fork" => libc::SYS_fork,
        "vfork" => libc::SYS_vfork,
        "pause" => libc::SYS_pause,
        "alarm" => libc::SYS_alarm,
        _ => return None,
    };
    Some(n)
}

/// The two filters of an image's process, in the order they are installed: `clone3` as `ENOSYS`,
/// then the allow-list, every call off it killing the process. Built before the fork, so the
/// child only installs them (SDD 19 f.5).
pub fn image_filters() -> Result<Vec<BpfProgram>, String> {
    let arch = arch()?;
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for name in IMAGE_ALLOWED.iter().chain(IMAGE_ALLOWED_ARCH) {
        let n = image_number(name).ok_or_else(|| format!("no system call number for `{name}`"))?;
        rules.entry(n).or_default();
    }
    // `socketpair` only for `AF_UNIX`; `socket`, `connect`, `bind`, `listen` and `accept` are not
    // on the list at all.
    let pair = image_number("socketpair").ok_or("no system call number for `socketpair`")?;
    rules.insert(
        pair,
        vec![
            SeccompRule::new(vec![
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Eq,
                    libc::AF_UNIX as u64,
                )
                .map_err(|e| e.to_string())?,
            ])
            .map_err(|e| e.to_string())?,
        ],
    );
    // `clone` makes threads and processes, never a namespace.
    let clone = number("clone").ok_or("no system call number for `clone`")?;
    rules.insert(
        clone,
        vec![
            SeccompRule::new(vec![condition(
                0,
                SeccompCmpOp::MaskedEq(NAMESPACE_FLAGS),
                0,
            )?])
            .map_err(|e| e.to_string())?,
        ],
    );
    let allow = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        arch,
    )
    .map_err(|e| e.to_string())?;
    let clone3 = number("clone3").ok_or("no system call number for `clone3`")?;
    let enosys = SeccompFilter::new(
        [(clone3, Vec::new())].into_iter().collect(),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )
    .map_err(|e| e.to_string())?;
    Ok(vec![
        enosys
            .try_into()
            .map_err(|e: seccompiler::BackendError| e.to_string())?,
        allow
            .try_into()
            .map_err(|e: seccompiler::BackendError| e.to_string())?,
    ])
}

/// Install a program built by [`image_filters`] on the calling thread.
pub fn apply_programs(programs: &[BpfProgram]) -> Result<(), String> {
    for program in programs {
        seccompiler::apply_filter(program).map_err(|e| format!("seccomp: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SB-T10: the lists in code are the lists in 17 e.5, name for name.
    #[test]
    fn sb_t10_seccomp_list_is_the_documents() {
        let sdd = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../architecture/sdd/17-sandbox.md"),
        )
        .unwrap();
        let start = sdd.find("### e.5").unwrap();
        let blocks: Vec<Vec<&str>> = sdd[start..]
            .split("```text")
            .skip(1)
            .take(2)
            .map(|b| b.split("```").next().unwrap().split_whitespace().collect())
            .collect();
        assert_eq!(blocks[0], ALLOWED, "17 e.5's first list");
        let x86 = [
            "open",
            "stat",
            "lstat",
            "access",
            "readlink",
            "poll",
            "select",
            "pipe",
            "dup2",
            "mkdir",
            "unlink",
            "rename",
            "arch_prctl",
            "epoll_wait",
            "getdents",
            "time",
            "getrlimit",
        ];
        assert_eq!(blocks[1], x86, "17 e.5's x86_64 list");
        if cfg!(target_arch = "x86_64") {
            assert_eq!(ALLOWED_ARCH, x86);
        } else {
            assert!(ALLOWED_ARCH.is_empty());
        }
        for name in ALLOWED.iter().chain(ALLOWED_ARCH) {
            assert!(number(name).is_some(), "{name}");
        }
        for never in ["connect", "socket", "execve", "unshare", "mount", "ptrace"] {
            assert!(
                number(never).is_none(),
                "{never} has no number here: it is never allowed"
            );
        }
        assert_eq!(filters(std::process::id()).unwrap().len(), 2);
    }

    /// The filters install, in a forked child of the test, and refuse what they should.
    #[test]
    fn filters_install_and_refuse() {
        // SAFETY: the child calls only async-signal-safe functions and the filter installation,
        // then `_exit`s with a code that says what happened.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            crate::coverage::open();
            // SAFETY: as above.
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    libc::_exit(10);
                }
                let me = libc::getpid() as u32;
                let programs = match filters(me) {
                    Ok(p) => p,
                    Err(_) => libc::_exit(11),
                };
                for (i, program) in programs.iter().enumerate() {
                    if let Err(e) = seccompiler::apply_filter(program) {
                        let code = match e {
                            seccompiler::Error::Seccomp(io) => io.raw_os_error().unwrap_or(99),
                            _ => 98,
                        };
                        libc::_exit(20 + i as i32 * 100 + code);
                    }
                }
                let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
                if fd >= 0 || *libc::__errno_location() != libc::EPERM {
                    libc::_exit(30);
                }
                if libc::syscall(libc::SYS_clone3, core::ptr::null::<u8>(), 0) != -1
                    || *libc::__errno_location() != libc::ENOSYS
                {
                    libc::_exit(31);
                }
                if libc::kill(1, 0) == 0 || *libc::__errno_location() != libc::EPERM {
                    libc::_exit(32);
                }
                if libc::kill(me as i32, 0) != 0 {
                    libc::_exit(33);
                }
                crate::coverage::write();
                libc::_exit(0);
            }
        }
        let mut status = 0;
        // SAFETY: waiting for the child this test forked.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(libc::WIFEXITED(status));
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "the child's code says which step failed"
        );
    }
}
