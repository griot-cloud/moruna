//! The Linux calls the plane is made of, each wrapped once: memfds and their seals, mappings,
//! the plane socket with descriptors passed beside a message, waits on many sockets, the
//! monotonic clock, and pidfds. Every `unsafe` block of the crate is here but the two that
//! make a task's mapped batch and window slices (`task`).

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;

/// The page, as the plane's layout and Linux's mappings both have it.
pub const PAGE: u64 = 4096;

/// The seals a batch carries: no write, no shrink, no growth, and no more seals.
pub const SEALS: libc::c_int =
    libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;

/// The seals without which an object is not read only for good.
const SEALED: libc::c_int = libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;

fn check(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn check_size(r: isize) -> io::Result<usize> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as usize)
    }
}

/// A memfd of `bytes` bytes, zero, that can be sealed.
pub fn memfd(bytes: u64) -> io::Result<OwnedFd> {
    let name = c"moruna-plane";
    // SAFETY: a NUL-terminated name and flags; the result is a new descriptor or -1.
    let fd = check(unsafe {
        libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
    })?;
    // SAFETY: a descriptor the kernel just gave this process, owned by nothing else.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let len = libc::off_t::try_from(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a memfd past off_t"))?;
    // SAFETY: ftruncate of a descriptor this process owns.
    check(unsafe { libc::ftruncate(fd.as_raw_fd(), len) })?;
    Ok(fd)
}

/// Why an object could not be sealed read only.
#[derive(Debug)]
pub enum SealError {
    /// A writable shared mapping of it exists (`EBUSY`): a task kept its window mapped.
    Mapped,
    /// Its seals were sealed without these (`EPERM`): a task sealed its window first.
    Forbidden,
    /// Anything else the system said.
    Io(io::Error),
}

/// Seals `fd` read only for good ([`SEALS`]); an object already sealed that way is sealed.
pub fn seal(fd: BorrowedFd<'_>) -> Result<(), SealError> {
    // SAFETY: fcntl with an integer argument on a descriptor the caller holds.
    let added = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, SEALS) };
    if added < 0 {
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EBUSY) => return Err(SealError::Mapped),
            Some(libc::EPERM) => {}
            _ => return Err(SealError::Io(e)),
        }
    }
    let have = seals(fd).map_err(SealError::Io)?;
    if have & SEALED == SEALED {
        Ok(())
    } else {
        Err(SealError::Forbidden)
    }
}

/// The seals `fd` carries.
pub fn seals(fd: BorrowedFd<'_>) -> io::Result<libc::c_int> {
    // SAFETY: fcntl with no argument on a descriptor the caller holds.
    check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) })
}

/// The size of the file `fd`, and its inode and device: which object it is.
pub fn stat(fd: BorrowedFd<'_>) -> io::Result<(u64, u64, u64)> {
    // SAFETY: an all-zero `stat` is a valid value of the plain C struct fstat fills.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat into a struct on this stack.
    check(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    Ok((st.st_size as u64, st.st_ino, st.st_dev))
}

/// `buf.len()` bytes of `fd` at `at`, every one of them.
pub fn pread_exact(fd: BorrowedFd<'_>, at: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut got = 0;
    while got < buf.len() {
        let off = libc::off_t::try_from(at + got as u64)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "an offset past off_t"))?;
        // SAFETY: pread into the rest of `buf`, which is valid for that many bytes.
        let r = unsafe {
            libc::pread(
                fd.as_raw_fd(),
                buf[got..].as_mut_ptr().cast(),
                buf.len() - got,
                off,
            )
        };
        match check_size(r) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the object ends before the bytes asked for",
                ));
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `data` written into `fd` at `at`, every byte.
pub fn pwrite_all(fd: BorrowedFd<'_>, at: u64, data: &[u8]) -> io::Result<()> {
    let mut put = 0;
    while put < data.len() {
        let off = libc::off_t::try_from(at + put as u64)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "an offset past off_t"))?;
        // SAFETY: pwrite from the rest of `data`, which is valid for that many bytes.
        let r = unsafe {
            libc::pwrite(
                fd.as_raw_fd(),
                data[put..].as_ptr().cast(),
                data.len() - put,
                off,
            )
        };
        match check_size(r) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "pwrite wrote 0")),
            Ok(n) => put += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// How a mapping is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Shared and writable: a window, which its writer's stores reach.
    Write,
    /// Private and read only: a batch or a reply. The pages are the object's own (a read
    /// never copies one); a write faults, and could reach only a private copy if the mapping
    /// were made writable, never the object.
    Read,
}

/// A mapping of `len` bytes of an object, unmapped when dropped.
#[derive(Debug)]
pub struct Map {
    at: NonNull<u8>,
    len: usize,
}

// SAFETY: a mapping is memory of the whole process; nothing about it belongs to a thread.
unsafe impl Send for Map {}
// SAFETY: as above; shared references read it, and a writer holds `&mut`.
unsafe impl Sync for Map {}

impl Map {
    /// `len` bytes of `fd` from `offset`, a page multiple.
    pub fn new(fd: BorrowedFd<'_>, offset: u64, len: usize, access: Access) -> io::Result<Map> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a mapping of no bytes",
            ));
        }
        let off = libc::off_t::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "an offset past off_t"))?;
        let (prot, flags) = match access {
            Access::Write => (libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED),
            Access::Read => (libc::PROT_READ, libc::MAP_PRIVATE),
        };
        // SAFETY: a new mapping at an address the kernel chooses; nothing of this process is
        // replaced. The result is checked against MAP_FAILED.
        let at = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd.as_raw_fd(), off) };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let at = NonNull::new(at.cast::<u8>()).ok_or_else(|| io::Error::other("mmap gave 0"))?;
        Ok(Map { at, len })
    }

    /// Where the mapping starts.
    pub fn addr(&self) -> u64 {
        self.at.as_ptr() as u64
    }

    /// Its length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it has no bytes (never: a mapping has at least one).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `len` bytes at `at` are mapped readable for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }

    /// The bytes, to write. Only a [`Access::Write`] mapping may be written; a write to any
    /// other faults, as the plane means it to.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`, and `&mut self` is the only reference to them in this process.
        unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: the mapping this value made, unmapped once.
        unsafe { libc::munmap(self.at.as_ptr().cast(), self.len) };
    }
}

/// A connected `SOCK_SEQPACKET` pair, close-on-exec: the engine's end and the task's.
pub fn seqpacket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: socketpair writes two descriptors into `fds` or none.
    check(unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    })?;
    // SAFETY: both descriptors were just made and nobody else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The type of the socket `fd` (`SOCK_SEQPACKET`, ...), or the error of a descriptor that is
/// none.
pub fn socket_type(fd: BorrowedFd<'_>) -> io::Result<libc::c_int> {
    let mut kind: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt into an int on this stack, with its length.
    check(unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut len,
        )
    })?;
    Ok(kind)
}

/// The room for this many descriptors beside a message.
fn control_space(fds: usize) -> usize {
    // SAFETY: CMSG_SPACE computes a size from an integer.
    unsafe { libc::CMSG_SPACE((fds * std::mem::size_of::<RawFd>()) as u32) as usize }
}

/// Sends one message, with `fd` beside it when given. `wait` false makes a full socket
/// `WouldBlock` rather than a wait. A closed peer is `BrokenPipe`, never a signal.
pub fn send(
    socket: BorrowedFd<'_>,
    bytes: &[u8],
    fd: Option<BorrowedFd<'_>>,
    wait: bool,
) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let mut control = vec![0u8; control_space(1)];
    // SAFETY: an all-zero msghdr is a valid value of the plain C struct.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len() as _;
        // SAFETY: the control buffer has room for one header and one descriptor
        // (CMSG_SPACE(4)), so the first header and its data are inside it.
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd.as_raw_fd());
        }
    }
    let flags = libc::MSG_NOSIGNAL | if wait { 0 } else { libc::MSG_DONTWAIT };
    loop {
        // SAFETY: sendmsg reads the message, its one iovec and its control buffer, all of
        // which outlive the call.
        let r = unsafe { libc::sendmsg(socket.as_raw_fd(), &msg, flags) };
        match check_size(r) {
            Ok(n) if n == bytes.len() => return Ok(()),
            Ok(n) => {
                return Err(io::Error::other(format!(
                    "a message of {} bytes went as {n}",
                    bytes.len()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// One message received: its length, the descriptors beside it, and whether some were cut.
pub struct Received {
    /// The message's length.
    pub len: usize,
    /// The descriptors passed beside it, now this process's.
    pub fds: Vec<OwnedFd>,
    /// The message was longer than the buffer, or carried more descriptors than room.
    pub truncated: bool,
}

/// Receives one message into `buf` (at most four descriptors beside it are kept, close on
/// exec). `wait` false makes an empty socket `WouldBlock`. A length of zero is an empty
/// message or the peer's end: [`hung_up`] tells which.
pub fn recv(socket: BorrowedFd<'_>, buf: &mut [u8], wait: bool) -> io::Result<Received> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut control = vec![0u8; control_space(4)];
    // SAFETY: an all-zero msghdr is a valid value of the plain C struct.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;
    let flags = libc::MSG_CMSG_CLOEXEC | if wait { 0 } else { libc::MSG_DONTWAIT };
    let len = loop {
        // SAFETY: recvmsg writes at most `buf.len()` bytes into `buf` and at most the control
        // buffer's length into it, both of which outlive the call.
        let r = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, flags) };
        match check_size(r) {
            Ok(n) => break n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    };
    let mut fds = Vec::new();
    // SAFETY: the headers walked are those recvmsg wrote inside the control buffer, which
    // CMSG_FIRSTHDR and CMSG_NXTHDR bound by `msg_controllen`; each SCM_RIGHTS header's data
    // holds whole descriptors that this process now owns.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header);
                let bytes = (*header).cmsg_len as usize - (data as usize - header as usize);
                for i in 0..bytes / std::mem::size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.cast::<RawFd>().add(i));
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    let truncated = msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0;
    Ok(Received {
        len,
        fds,
        truncated,
    })
}

/// Whether the peer of `socket` has closed its end: every process holding it has.
pub fn hung_up(socket: BorrowedFd<'_>) -> io::Result<bool> {
    let mut item = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ppoll of one item on this stack, not waiting, with no signal mask.
    check(unsafe { libc::ppoll(&mut item, 1, &now, std::ptr::null()) })?;
    Ok(item.revents & (libc::POLLHUP | libc::POLLERR) != 0)
}

/// Waits until one of `sockets` can be read or has hung up, or until `deadline` on the
/// monotonic clock; whether one is ready.
pub fn wait(sockets: &[BorrowedFd<'_>], deadline: u64) -> io::Result<bool> {
    let mut items: Vec<libc::pollfd> = sockets
        .iter()
        .map(|s| libc::pollfd {
            fd: s.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    loop {
        let left = deadline.saturating_sub(now());
        let timeout = libc::timespec {
            tv_sec: (left / 1_000_000_000) as libc::time_t,
            tv_nsec: (left % 1_000_000_000) as libc::c_long,
        };
        // SAFETY: ppoll of the items in this vector, with a timeout on this stack and no
        // signal mask.
        let r = unsafe {
            libc::ppoll(
                items.as_mut_ptr(),
                items.len() as libc::nfds_t,
                &timeout,
                std::ptr::null(),
            )
        };
        match check(r) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                if now() >= deadline {
                    return Ok(false);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Nanoseconds on the monotonic clock: the clock a plane's deadlines are on.
pub fn now() -> u64 {
    // SAFETY: an all-zero timespec is a valid value of the plain C struct.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: clock_gettime of a clock every Linux has, into a struct on this stack.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// A pidfd for the process `pid`.
pub fn pidfd(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor the kernel just gave this process.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// `SIGKILL` to the process of `pidfd`; one that has already exited is not an error.
pub fn kill(pidfd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: pidfd_send_signal with a pidfd this process holds, a signal and no siginfo.
    let r = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(e);
        }
    }
    Ok(())
}

/// Shuts `socket` down both ways: its peer reads the end, and so does this side.
pub fn shutdown(socket: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: shutdown of a socket this process holds.
    check(unsafe { libc::shutdown(socket.as_raw_fd(), libc::SHUT_RDWR) }).map(|_| ())
}

/// Descriptor `fd`, which this process was started with, as an owned one; an error when it is
/// not open.
pub fn claim(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: fcntl F_GETFD on an integer: it reads the descriptor's flags or fails with EBADF.
    check(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
    // SAFETY: the descriptor is open, and by the plane's contract with the process that started
    // this one it is the plane's alone: nothing else of this process owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
