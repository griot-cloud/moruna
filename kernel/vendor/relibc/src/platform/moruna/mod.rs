//! Moruna's platform (system design 5B, item 5.2b): relibc's `Pal` over `relibc-moruna`
//! (`kernel/relibc-moruna`), which holds the logic and the kernel's calls, through `moruna-sys`.
//! This file is relibc's types in and out.
//!
//! A native function on Moruna's core has memory, threads, futexes, clocks, randomness, its
//! exit, and its three standard streams (0 empty, 1 and 2 the task's log). Paths answer
//! `ENOENT`, as a system with no files does; processes, sockets, timers and the like answer
//! `ENOSYS` (5A0). Signals are kept within the process: a handler installed runs when the process
//! raises its signal, and a signal left to its default ends the process.

use core::{
    cell::Cell,
    num::NonZeroU64,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

use super::{Pal, types::*};
use crate::{
    c_str::CStr,
    error::{Errno, Result},
    header::{
        bits_sigset_t::sigset_t,
        errno::{EBADF, ECHILD, EINVAL, ENOENT, ENOMEM, ENOSYS, EPERM, ERANGE, ESRCH},
        fcntl::{AT_EMPTY_PATH, F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_SETFD, F_SETFL},
        poll::{POLLHUP, POLLIN, POLLNVAL, POLLOUT, pollfd},
        signal::sigevent,
        sys_resource::{RLIM_INFINITY, RLIMIT_NOFILE, rlimit, rusage},
        sys_select::timeval,
        sys_stat::stat,
        sys_statvfs::statvfs,
        sys_time::timezone,
        sys_uio::iovec,
        sys_utsname::{UTSLENGTH, utsname},
        time::{itimerspec, timespec},
    },
    ld_so::tcb::Tcb,
    out::Out,
    pthread,
};
use relibc_moruna as moruna;

mod epoll;
mod ptrace;
mod signal;
mod socket;

// relibc's headers and the platform crate speak the same ABI, Linux's.
const _: () = {
    use crate::header::errno as e;
    assert!(e::EPERM == moruna::errno::EPERM);
    assert!(e::ENOENT == moruna::errno::ENOENT);
    assert!(e::EINTR == moruna::errno::EINTR);
    assert!(e::EBADF == moruna::errno::EBADF);
    assert!(e::EAGAIN == moruna::errno::EAGAIN);
    assert!(e::ENOMEM == moruna::errno::ENOMEM);
    assert!(e::EINVAL == moruna::errno::EINVAL);
    assert!(e::ENOTTY == moruna::errno::ENOTTY);
    assert!(e::ESPIPE == moruna::errno::ESPIPE);
    assert!(e::ENOSYS == moruna::errno::ENOSYS);
    assert!(e::ENODEV == moruna::errno::ENODEV);
    assert!(e::ETIMEDOUT == moruna::errno::ETIMEDOUT);
    use crate::header::sys_mman as m;
    assert!(m::PROT_READ == moruna::memory::PROT_READ);
    assert!(m::PROT_WRITE == moruna::memory::PROT_WRITE);
    assert!(m::PROT_EXEC == moruna::memory::PROT_EXEC);
    assert!(m::MAP_SHARED == moruna::memory::MAP_SHARED);
    assert!(m::MAP_PRIVATE == moruna::memory::MAP_PRIVATE);
    assert!(m::MAP_FIXED == moruna::memory::MAP_FIXED);
    assert!(m::MAP_ANONYMOUS == moruna::memory::MAP_ANONYMOUS);
    use crate::header::time as t;
    assert!(t::CLOCK_REALTIME == moruna::time::CLOCK_REALTIME);
    assert!(t::CLOCK_MONOTONIC == moruna::time::CLOCK_MONOTONIC);
    assert!(t::CLOCK_PROCESS_CPUTIME_ID == moruna::time::CLOCK_PROCESS_CPUTIME_ID);
    use crate::header::sys_stat as s;
    assert!(s::S_IFCHR == moruna::fd::S_IFCHR);
};

fn errno(e: moruna::Errno) -> Errno {
    Errno(e.0)
}

fn nosys<T>() -> Result<T> {
    Err(Errno(ENOSYS))
}

/// What a path names: nothing, on a system with no files.
fn no_path<T>() -> Result<T> {
    Err(Errno(ENOENT))
}

fn timespec_of(t: moruna::time::Timespec) -> timespec {
    timespec {
        tv_sec: t.sec,
        tv_nsec: t.nsec,
    }
}

fn of_timespec(t: &timespec) -> moruna::time::Timespec {
    moruna::time::Timespec {
        sec: t.tv_sec,
        nsec: t.tv_nsec,
    }
}

/// A thread's id, given by its creator, or on first asking for the first thread: what
/// relibc's control block keeps for a Moruna thread (`OsSpecific`).
#[derive(Debug, Default)]
pub struct ThreadId(AtomicUsize);

/// The next thread's id; the first thread's is 1.
static NEXT_THREAD: AtomicUsize = AtomicUsize::new(1);

impl ThreadId {
    fn get(&self) -> usize {
        match self.0.load(Ordering::Acquire) {
            0 => {
                let id = NEXT_THREAD.fetch_add(1, Ordering::Relaxed);
                match self
                    .0
                    .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => id,
                    Err(other) => other,
                }
            }
            id => id,
        }
    }
}

/// The file creation mask, which nothing creates files under.
static UMASK: AtomicU32 = AtomicU32::new(0o022);

/// Moruna's implementation of [`Pal`].
pub struct Sys;

impl Sys {
    /// `ioctl`: no device answers one; a terminal's request on 0 to 2 is `ENOTTY`.
    pub unsafe fn ioctl(fd: c_int, _request: c_ulong, _out: *mut c_void) -> Result<c_int> {
        moruna::fd::terminal(fd).map(|()| 0).map_err(errno)
    }
}

/// Each of `iovs` gathered into one buffer.
unsafe fn gather(iovs: &[iovec]) -> alloc::vec::Vec<u8> {
    let mut all = alloc::vec::Vec::new();
    for iov in iovs {
        if iov.iov_len > 0 {
            all.extend_from_slice(unsafe {
                core::slice::from_raw_parts(iov.iov_base.cast::<u8>(), iov.iov_len)
            });
        }
    }
    all
}

fn iovecs<'a>(iov: *const iovec, iovcnt: c_int) -> Result<&'a [iovec]> {
    let n = usize::try_from(iovcnt).map_err(|_| Errno(EINVAL))?;
    if n == 0 {
        return Ok(&[]);
    }
    Ok(unsafe { core::slice::from_raw_parts(iov, n) })
}

/// Polls 0 to 2: input at its end, output always ready, any other descriptor invalid.
fn ready(fds: &mut [pollfd]) -> c_int {
    let mut n = 0;
    for p in fds.iter_mut() {
        p.revents = match p.fd {
            fd if fd < 0 => 0,
            moruna::fd::STDIN => (POLLIN & p.events) | POLLHUP,
            moruna::fd::STDOUT | moruna::fd::STDERR => POLLOUT & p.events,
            _ => POLLNVAL,
        };
        if p.revents != 0 {
            n += 1;
        }
    }
    n
}

fn field(text: &[u8]) -> [c_char; UTSLENGTH] {
    let mut f = [0 as c_char; UTSLENGTH];
    for (d, s) in f.iter_mut().zip(text.iter().take(UTSLENGTH - 1)) {
        *d = *s as c_char;
    }
    f
}

impl Pal for Sys {
    fn faccessat(_fd: c_int, _path: CStr, _amode: c_int, _flags: c_int) -> Result<()> {
        no_path()
    }

    unsafe fn brk(_addr: *mut c_void) -> Result<*mut c_void> {
        Err(Errno(ENOMEM))
    }

    fn chdir(_path: CStr) -> Result<()> {
        no_path()
    }

    fn clock_getres(clk_id: clockid_t, tp: Option<Out<timespec>>) -> Result<()> {
        let r = moruna::time::resolution(clk_id).map_err(errno)?;
        if let Some(mut tp) = tp {
            tp.write(timespec_of(r));
        }
        Ok(())
    }

    fn clock_gettime(clk_id: clockid_t, mut tp: Out<timespec>) -> Result<()> {
        tp.write(timespec_of(moruna::time::now(clk_id).map_err(errno)?));
        Ok(())
    }

    unsafe fn clock_settime(_clk_id: clockid_t, _tp: *const timespec) -> Result<()> {
        Err(Errno(EPERM))
    }

    fn close(fildes: c_int) -> Result<()> {
        moruna::fd::close(fildes).map_err(errno)
    }

    fn dup2(fildes: c_int, fildes2: c_int) -> Result<c_int> {
        // The three streams are one log: 1 and 2 already go to the same place.
        if moruna::fd::is_open(fildes) && moruna::fd::is_open(fildes2) {
            return Ok(fildes2);
        }
        if !moruna::fd::is_open(fildes) {
            return Err(Errno(EBADF));
        }
        nosys()
    }

    unsafe fn execve(
        _path: CStr,
        _argv: *const *mut c_char,
        _envp: *const *mut c_char,
    ) -> Result<()> {
        nosys()
    }

    unsafe fn fexecve(
        _fildes: c_int,
        _argv: *const *mut c_char,
        _envp: *const *mut c_char,
    ) -> Result<()> {
        nosys()
    }

    fn exit(status: c_int) -> ! {
        moruna::process::exit(status)
    }

    unsafe fn exit_thread(stack_base: *mut (), stack_size: usize) -> ! {
        unsafe { moruna::thread::exit(stack_base as usize, stack_size) }
    }

    fn fchdir(_fildes: c_int) -> Result<()> {
        nosys()
    }

    fn fchmodat(_dirfd: c_int, _path: Option<CStr>, _mode: mode_t, _flags: c_int) -> Result<()> {
        no_path()
    }

    fn fchownat(
        _fildes: c_int,
        _path: CStr,
        _owner: uid_t,
        _group: gid_t,
        _flags: c_int,
    ) -> Result<()> {
        no_path()
    }

    fn fdatasync(fildes: c_int) -> Result<()> {
        Self::fsync(fildes)
    }

    fn flock(_fd: c_int, _operation: c_int) -> Result<()> {
        nosys()
    }

    fn fstatat(fildes: c_int, path: Option<CStr>, mut buf: Out<stat>, flags: c_int) -> Result<()> {
        let this_fd = path.is_none_or(|p| p.to_bytes().is_empty()) && flags & AT_EMPTY_PATH != 0;
        if !this_fd {
            return no_path();
        }
        let mode = moruna::fd::mode(fildes).map_err(errno)?;
        buf.write(stat {
            st_mode: mode as mode_t,
            st_nlink: 1,
            st_blksize: 4096,
            ..stat::default()
        });
        Ok(())
    }

    fn fstatvfs(_fildes: c_int, _buf: Out<statvfs>) -> Result<()> {
        nosys()
    }

    fn fcntl(fildes: c_int, cmd: c_int, _arg: c_ulonglong) -> Result<c_int> {
        let flags = moruna::fd::status_flags(fildes).map_err(errno)?;
        match cmd {
            F_GETFL => Ok(flags),
            F_GETFD | F_SETFD | F_SETFL => Ok(0),
            F_DUPFD | F_DUPFD_CLOEXEC => nosys(),
            _ => Err(Errno(EINVAL)),
        }
    }

    unsafe fn fork() -> Result<pid_t> {
        nosys()
    }

    fn fpath(_fildes: c_int, _out: &mut [u8]) -> Result<usize> {
        nosys()
    }

    /// Nothing is held back from the log: there is nothing to flush.
    fn fsync(fildes: c_int) -> Result<()> {
        moruna::fd::status_flags(fildes).map(|_| ()).map_err(errno)
    }

    fn ftruncate(fildes: c_int, _length: off_t) -> Result<()> {
        moruna::fd::status_flags(fildes).map_err(errno)?;
        Err(Errno(EINVAL))
    }

    unsafe fn futex_wait(addr: *mut u32, val: u32, deadline: Option<&timespec>) -> Result<()> {
        let word = unsafe { AtomicU32::from_ptr(addr) };
        moruna::thread::futex_wait(word, val, deadline.map(of_timespec)).map_err(errno)
    }

    unsafe fn futex_wake(addr: *mut u32, num: u32) -> Result<u32> {
        let word = unsafe { AtomicU32::from_ptr(addr) };
        moruna::thread::futex_wake(word, num).map_err(errno)
    }

    fn utimensat(
        _dirfd: c_int,
        _path: CStr,
        _times: Option<&[timespec; 2]>,
        _flag: c_int,
    ) -> Result<()> {
        no_path()
    }

    fn getcwd(mut buf: Out<[u8]>) -> Result<()> {
        if buf.len() < 2 {
            return Err(Errno(ERANGE));
        }
        buf.subslice(0, 2).copy_from_slice(b"/\0");
        Ok(())
    }

    fn getdents(_fd: c_int, _buf: &mut [u8], _opaque_offset: u64) -> Result<usize> {
        Err(Errno(EBADF))
    }

    fn dir_seek(_fd: c_int, _opaque_offset: u64) -> Result<()> {
        Err(Errno(EBADF))
    }

    unsafe fn dent_reclen_offset(_this_dent: &[u8], _offset: usize) -> Option<(u16, u64)> {
        None
    }

    fn getegid() -> gid_t {
        0
    }

    fn geteuid() -> uid_t {
        0
    }

    fn getgid() -> gid_t {
        0
    }

    fn getgroups(_list: Out<[gid_t]>) -> Result<c_int> {
        Ok(0)
    }

    fn getpagesize() -> usize {
        moruna::memory::PAGE
    }

    fn getpgid(pid: pid_t) -> Result<pid_t> {
        Self::getsid(pid)
    }

    fn getpid() -> pid_t {
        moruna::process::PID
    }

    fn getppid() -> pid_t {
        0
    }

    fn getpriority(_which: c_int, _who: id_t) -> Result<c_int> {
        Ok(0)
    }

    fn getrandom(buf: &mut [u8], flags: c_uint) -> Result<usize> {
        moruna::random::fill(buf, flags).map_err(errno)
    }

    fn getresgid(
        rgid: Option<Out<gid_t>>,
        egid: Option<Out<gid_t>>,
        sgid: Option<Out<gid_t>>,
    ) -> Result<()> {
        for mut id in [rgid, egid, sgid].into_iter().flatten() {
            id.write(0);
        }
        Ok(())
    }

    fn getresuid(
        ruid: Option<Out<uid_t>>,
        euid: Option<Out<uid_t>>,
        suid: Option<Out<uid_t>>,
    ) -> Result<()> {
        for mut id in [ruid, euid, suid].into_iter().flatten() {
            id.write(0);
        }
        Ok(())
    }

    fn getrlimit(resource: c_int, mut rlim: Out<rlimit>) -> Result<()> {
        // Three descriptors; no other limit but the process's account, which says no in time.
        let limit = if resource == RLIMIT_NOFILE {
            3
        } else {
            RLIM_INFINITY
        };
        rlim.write(rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        });
        Ok(())
    }

    unsafe fn setrlimit(_resource: c_int, _rlim: *const rlimit) -> Result<()> {
        Err(Errno(EPERM))
    }

    fn getrusage(_who: c_int, _r_usage: Out<rusage>) -> Result<()> {
        nosys()
    }

    fn getsid(pid: pid_t) -> Result<pid_t> {
        if pid == 0 || pid == moruna::process::PID {
            Ok(moruna::process::PID)
        } else {
            Err(Errno(ESRCH))
        }
    }

    fn gettid() -> pid_t {
        Self::current_os_tid().thread_id as pid_t
    }

    fn gettimeofday(mut tp: Out<timeval>, tzp: Option<Out<timezone>>) -> Result<()> {
        let now = moruna::time::now(moruna::time::CLOCK_REALTIME).map_err(errno)?;
        tp.write(timeval {
            tv_sec: now.sec,
            tv_usec: (now.nsec / 1000) as suseconds_t,
        });
        if let Some(mut tz) = tzp {
            tz.write(timezone {
                tz_minuteswest: 0,
                tz_dsttime: 0,
            });
        }
        Ok(())
    }

    fn getuid() -> uid_t {
        0
    }

    fn linkat(
        _fd1: c_int,
        _oldpath: CStr,
        _fd2: c_int,
        _newpath: CStr,
        _flags: c_int,
    ) -> Result<()> {
        no_path()
    }

    fn lseek(fildes: c_int, _offset: off_t, _whence: c_int) -> Result<off_t> {
        moruna::fd::seek(fildes).map_err(errno)
    }

    fn mkdirat(_fildes: c_int, _path: CStr, _mode: mode_t) -> Result<()> {
        no_path()
    }

    fn mkfifoat(_dir_fd: c_int, _path: CStr, _mode: mode_t) -> Result<()> {
        no_path()
    }

    fn mknodat(_fildes: c_int, _path: CStr, _mode: mode_t, _dev: dev_t) -> Result<()> {
        no_path()
    }

    // Memory on the core is never paged out: locking it is already so.
    unsafe fn mlock(_addr: *const c_void, _len: usize) -> Result<()> {
        Ok(())
    }

    unsafe fn mlockall(_flags: c_int) -> Result<()> {
        Ok(())
    }

    unsafe fn mmap(
        _addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        _fildes: c_int,
        off: off_t,
    ) -> Result<*mut c_void> {
        moruna::memory::map(len, prot, flags, off)
            .map(|a| a as *mut c_void)
            .map_err(errno)
    }

    unsafe fn mremap(
        _addr: *mut c_void,
        _len: usize,
        _new_len: usize,
        _flags: c_int,
        _args: *mut c_void,
    ) -> Result<*mut c_void> {
        nosys()
    }

    unsafe fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> Result<()> {
        unsafe { moruna::memory::protect(addr as usize, len, prot) }.map_err(errno)
    }

    unsafe fn msync(_addr: *mut c_void, _len: usize, _flags: c_int) -> Result<()> {
        Ok(())
    }

    unsafe fn munlock(_addr: *const c_void, _len: usize) -> Result<()> {
        Ok(())
    }

    unsafe fn madvise(_addr: *mut c_void, _len: usize, _flags: c_int) -> Result<()> {
        Ok(())
    }

    unsafe fn munlockall() -> Result<()> {
        Ok(())
    }

    unsafe fn munmap(addr: *mut c_void, len: usize) -> Result<()> {
        unsafe { moruna::memory::unmap(addr as usize, len) }.map_err(errno)
    }

    fn nanosleep(rqtp: &timespec, rmtp: Option<Out<timespec>>) -> Result<()> {
        moruna::time::sleep(of_timespec(rqtp)).map_err(errno)?;
        if let Some(mut rest) = rmtp {
            rest.write(timespec::default());
        }
        Ok(())
    }

    fn openat(_dirfd: c_int, _path: CStr, _oflag: c_int, _mode: mode_t) -> Result<c_int> {
        no_path()
    }

    fn pipe2(_fildes: Out<[c_int; 2]>, _flags: c_int) -> Result<()> {
        nosys()
    }

    fn poll(fds: &mut [pollfd], timeout: c_int) -> Result<c_int> {
        let timeout = (timeout >= 0).then(|| timespec {
            tv_sec: time_t::from(timeout / 1000),
            tv_nsec: c_long::from((timeout % 1000) * 1_000_000),
        });
        Self::ppoll(fds, timeout.as_ref(), None)
    }

    fn ppoll(
        fds: &mut [pollfd],
        timeout: Option<&timespec>,
        _sigmask: Option<&sigset_t>,
    ) -> Result<c_int> {
        let n = ready(fds);
        if n > 0 {
            return Ok(n);
        }
        // Nothing will ever be ready: wait out the timeout, or refuse to wait for ever.
        match timeout {
            Some(t) => moruna::time::sleep(of_timespec(t))
                .map(|()| 0)
                .map_err(errno),
            None => nosys(),
        }
    }

    fn posix_fallocate(_fd: c_int, _offset: u64, _length: NonZeroU64) -> Result<()> {
        nosys()
    }

    fn posix_getdents(_fildes: c_int, _buf: &mut [u8]) -> Result<usize> {
        Err(Errno(EBADF))
    }

    unsafe fn rlct_clone(
        stack: *mut usize,
        os_specific: &mut ThreadId,
    ) -> Result<pthread::OsTid, Errno> {
        let id = os_specific.get();
        unsafe { moruna::thread::spawn(stack) }.map_err(errno)?;
        Ok(pthread::OsTid { thread_id: id })
    }

    unsafe fn rlct_kill(_os_tid: pthread::OsTid, _signal: usize) -> Result<()> {
        nosys()
    }

    fn current_os_tid() -> pthread::OsTid {
        let thread_id = match unsafe { Tcb::current() } {
            Some(tcb) => tcb.os_specific.get(),
            None => MAIN_ID.with_main(),
        };
        pthread::OsTid { thread_id }
    }

    fn read(fildes: c_int, buf: &mut [u8]) -> Result<usize> {
        moruna::fd::read(fildes, buf).map_err(errno)
    }

    fn pread(fildes: c_int, _buf: &mut [u8], _offset: off_t) -> Result<usize> {
        moruna::fd::seek(fildes).map(|_| 0).map_err(errno)
    }

    unsafe fn readv(fildes: c_int, iov: *const iovec, iovcnt: c_int) -> Result<usize> {
        let iovs = iovecs(iov, iovcnt)?;
        match iovs.iter().find(|v| v.iov_len > 0) {
            Some(v) => {
                let buf = unsafe { core::slice::from_raw_parts_mut(v.iov_base.cast(), v.iov_len) };
                Self::read(fildes, buf)
            }
            None => Self::read(fildes, &mut []),
        }
    }

    fn readlinkat(_dirfd: c_int, _pathname: CStr, _out: &mut [u8]) -> Result<usize> {
        no_path()
    }

    fn renameat2(
        _old_dir: c_int,
        _old_path: CStr,
        _new_dir: c_int,
        _new_path: CStr,
        _flags: c_uint,
    ) -> Result<()> {
        no_path()
    }

    fn sched_yield() -> Result<()> {
        moruna::thread::yield_now().map_err(errno)
    }

    unsafe fn setgroups(_size: size_t, _list: *const gid_t) -> Result<()> {
        Err(Errno(EPERM))
    }

    fn setpgid(_pid: pid_t, _pgid: pid_t) -> Result<()> {
        Err(Errno(EPERM))
    }

    fn setpriority(_which: c_int, _who: id_t, _prio: c_int) -> Result<()> {
        Err(Errno(EPERM))
    }

    fn setresgid(_rgid: gid_t, _egid: gid_t, _sgid: gid_t) -> Result<()> {
        Err(Errno(EPERM))
    }

    fn setresuid(_ruid: uid_t, _euid: uid_t, _suid: uid_t) -> Result<()> {
        Err(Errno(EPERM))
    }

    fn setsid() -> Result<c_int> {
        Err(Errno(EPERM))
    }

    unsafe fn spawn(
        _program: CStr,
        _fac: Option<&crate::header::spawn::posix_spawn_file_actions_t>,
        _fat: Option<&crate::header::spawn::posix_spawnattr_t>,
        _argv: crate::iter::NulTerminated<*mut c_char>,
        _envp: Option<crate::iter::NulTerminated<*mut c_char>>,
    ) -> Result<pid_t> {
        nosys()
    }

    fn symlinkat(_path1: CStr, _fd: c_int, _path2: CStr) -> Result<()> {
        no_path()
    }

    fn sync() -> Result<()> {
        Ok(())
    }

    fn timer_create(_clock_id: clockid_t, _evp: &sigevent) -> Result<timer_t> {
        nosys()
    }

    fn timer_delete(_timerid: timer_t) -> Result<()> {
        nosys()
    }

    fn timer_gettime(_timerid: timer_t) -> Result<itimerspec> {
        nosys()
    }

    fn timer_settime(
        _timerid: timer_t,
        _flags: c_int,
        _value: &itimerspec,
        _ovalue: Option<Out<itimerspec>>,
    ) -> Result<()> {
        nosys()
    }

    fn umask(mask: mode_t) -> mode_t {
        UMASK.swap(mask & 0o777, Ordering::Relaxed)
    }

    fn uname(mut utsname: Out<utsname>) -> Result<()> {
        let [sysname, nodename, release, version, machine] = moruna::process::uname();
        utsname.write(utsname {
            sysname: field(sysname.as_bytes()),
            nodename: field(nodename.as_bytes()),
            release: field(release.as_bytes()),
            version: field(version.as_bytes()),
            machine: field(machine.as_bytes()),
            domainname: field(b""),
        });
        Ok(())
    }

    fn unlinkat(_fd: c_int, _path: CStr, _flags: c_int) -> Result<()> {
        no_path()
    }

    fn waitpid(_pid: pid_t, _stat_loc: Option<Out<c_int>>, _options: c_int) -> Result<pid_t> {
        Err(Errno(ECHILD))
    }

    fn write(fildes: c_int, buf: &[u8]) -> Result<usize> {
        moruna::fd::write(fildes, buf).map_err(errno)
    }

    fn pwrite(fildes: c_int, _buf: &[u8], _offset: off_t) -> Result<usize> {
        moruna::fd::seek(fildes).map(|_| 0).map_err(errno)
    }

    unsafe fn writev(fildes: c_int, iov: *const iovec, iovcnt: c_int) -> Result<usize> {
        let iovs = iovecs(iov, iovcnt)?;
        let all = unsafe { gather(iovs) };
        Self::write(fildes, &all)
    }

    fn verify() -> bool {
        true
    }
}

/// The first thread's id before it has a control block to keep it in.
struct MainId(Cell<usize>);

// SAFETY: read only by the first thread, before any other runs.
unsafe impl Sync for MainId {}

static MAIN_ID: MainId = MainId(Cell::new(0));

impl MainId {
    fn with_main(&self) -> usize {
        if self.0.get() == 0 {
            self.0.set(NEXT_THREAD.fetch_add(1, Ordering::Relaxed));
        }
        self.0.get()
    }
}
