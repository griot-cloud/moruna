//! System-call filters.
//!
//! [`namespaces`] is installed on every jailed process just before it executes its program:
//! it refuses every call that makes or enters a namespace (`unshare`, `setns`, `clone` with a
//! namespace flag; `clone3`, whose flags a filter cannot read, answers `ENOSYS` so the C
//! library falls back to `clone`), and kills any call of another architecture's ABI.
//!
//! [`program`] is the one `moruna-vhost-vsock` installs on itself before it binds its socket:
//! only the calls the vhost-user server, the muxer and the Rust runtime under them make. A
//! call outside the list kills the process (`SECCOMP_RET_KILL_PROCESS`), so the launcher sees
//! the back end die by `SIGSYS`. No file is opened after the filter is in place, no process or
//! thread is made, no program executed, no memory made executable, and no socket other than a
//! Unix one is created. QEMU's own filter is QEMU's (`-sandbox`).

use crate::error::{Result, VmmError};

const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const fn offset_arg_low(index: u32) -> u32 {
    16 + 8 * index
}

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_JSET_K: u16 = 0x45;
const BPF_RET_K: u16 = 0x06;

fn insn(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// Calls allowed whatever their arguments.
fn plain() -> Vec<libc::c_long> {
    let mut calls = vec![
        libc::SYS_read,
        libc::SYS_readv,
        libc::SYS_write,
        libc::SYS_writev,
        libc::SYS_close,
        libc::SYS_fcntl,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_brk,
        libc::SYS_ppoll,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept4,
        libc::SYS_connect,
        libc::SYS_recvfrom,
        libc::SYS_recvmsg,
        libc::SYS_sendto,
        libc::SYS_sendmsg,
        libc::SYS_shutdown,
        libc::SYS_getsockopt,
        libc::SYS_unlinkat,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_fstat,
        libc::SYS_futex,
        libc::SYS_sched_yield,
        libc::SYS_nanosleep,
        libc::SYS_clock_nanosleep,
        libc::SYS_clock_gettime,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigprocmask,
        libc::SYS_sigaltstack,
        libc::SYS_getrandom,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_tgkill,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ];
    calls.extend_from_slice(ARCH_ONLY);
    calls
}

/// The calls only this architecture has (its older forms of `ppoll`, `accept4`, `unlinkat`
/// and `newfstatat`).
#[cfg(target_arch = "x86_64")]
const ARCH_ONLY: &[libc::c_long] = &[
    libc::SYS_poll,
    libc::SYS_accept,
    libc::SYS_unlink,
    libc::SYS_lstat,
    libc::SYS_stat,
];
#[cfg(target_arch = "aarch64")]
const ARCH_ONLY: &[libc::c_long] = &[];

/// The filter program for this architecture.
pub fn program() -> Vec<libc::sock_filter> {
    let mut p = vec![
        insn(BPF_LD_W_ABS, 0, 0, OFFSET_ARCH),
        insn(BPF_JEQ_K, 1, 0, AUDIT_ARCH),
        insn(BPF_RET_K, 0, 0, RET_KILL_PROCESS),
        insn(BPF_LD_W_ABS, 0, 0, OFFSET_NR),
    ];
    for nr in plain() {
        p.push(insn(BPF_JEQ_K, 0, 1, nr as u32));
        p.push(insn(BPF_RET_K, 0, 0, RET_ALLOW));
    }
    // An argument that must equal a value: socket(2) and socketpair(2) of AF_UNIX only;
    // ioctl(2) only to set non-blocking mode.
    for (nr, arg, value) in [
        (libc::SYS_socket, 0, libc::AF_UNIX as u32),
        (libc::SYS_socketpair, 0, libc::AF_UNIX as u32),
        (libc::SYS_ioctl, 1, libc::FIONBIO as u32),
    ] {
        p.push(insn(BPF_JEQ_K, 0, 5, nr as u32));
        p.push(insn(BPF_LD_W_ABS, 0, 0, offset_arg_low(arg)));
        p.push(insn(BPF_JEQ_K, 0, 1, value));
        p.push(insn(BPF_RET_K, 0, 0, RET_ALLOW));
        p.push(insn(BPF_RET_K, 0, 0, RET_KILL_PROCESS));
        p.push(insn(BPF_LD_W_ABS, 0, 0, OFFSET_NR));
    }
    // Memory may be mapped and protected, never made executable.
    for nr in [libc::SYS_mmap, libc::SYS_mprotect] {
        p.push(insn(BPF_JEQ_K, 0, 5, nr as u32));
        p.push(insn(BPF_LD_W_ABS, 0, 0, offset_arg_low(2)));
        p.push(insn(BPF_JSET_K, 1, 0, libc::PROT_EXEC as u32));
        p.push(insn(BPF_RET_K, 0, 0, RET_ALLOW));
        p.push(insn(BPF_RET_K, 0, 0, RET_KILL_PROCESS));
        p.push(insn(BPF_LD_W_ABS, 0, 0, OFFSET_NR));
    }
    p.push(insn(BPF_RET_K, 0, 0, RET_KILL_PROCESS));
    p
}

/// Every namespace flag `clone` takes.
const NAMESPACE_FLAGS: u32 = (libc::CLONE_NEWUSER
    | libc::CLONE_NEWNS
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWCGROUP
    | 0x80) as u32;

/// The filter every jailed process runs under: no new or other namespace.
pub fn namespaces() -> Vec<libc::sock_filter> {
    let errno = |e: i32| RET_ERRNO | e as u32;
    vec![
        insn(BPF_LD_W_ABS, 0, 0, OFFSET_ARCH),
        insn(BPF_JEQ_K, 1, 0, AUDIT_ARCH),
        insn(BPF_RET_K, 0, 0, RET_KILL_PROCESS),
        insn(BPF_LD_W_ABS, 0, 0, OFFSET_NR),
        insn(BPF_JEQ_K, 0, 1, libc::SYS_clone3 as u32),
        insn(BPF_RET_K, 0, 0, errno(libc::ENOSYS)),
        insn(BPF_JEQ_K, 0, 1, libc::SYS_unshare as u32),
        insn(BPF_RET_K, 0, 0, errno(libc::EPERM)),
        insn(BPF_JEQ_K, 0, 1, libc::SYS_setns as u32),
        insn(BPF_RET_K, 0, 0, errno(libc::EPERM)),
        insn(BPF_JEQ_K, 0, 3, libc::SYS_clone as u32),
        insn(BPF_LD_W_ABS, 0, 0, offset_arg_low(0)),
        insn(BPF_JSET_K, 0, 1, NAMESPACE_FLAGS),
        insn(BPF_RET_K, 0, 0, errno(libc::EPERM)),
        insn(BPF_RET_K, 0, 0, RET_ALLOW),
    ]
}

/// Install `filter` on the calling thread (and every thread it later makes); `no_new_privs`
/// is set first, as an unprivileged filter requires.
pub fn install(filter: &[libc::sock_filter]) -> Result<()> {
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len())
            .map_err(|_| VmmError::config("seccomp", "filter longer than 65535 instructions"))?,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: prctl with integer arguments, then with a pointer to a live sock_fprog whose
    // filter outlives the call (the kernel copies it).
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(VmmError::io(
                "prctl",
                "no_new_privs",
                &std::io::Error::last_os_error(),
            ));
        }
        if libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &program as *const libc::sock_fprog,
        ) != 0
        {
            return Err(VmmError::io(
                "prctl",
                "seccomp filter",
                &std::io::Error::last_os_error(),
            ));
        }
    }
    Ok(())
}

/// Confine the vsock back end: install [`program`].
pub fn confine_vsock_backend() -> Result<()> {
    install(&program())
}
