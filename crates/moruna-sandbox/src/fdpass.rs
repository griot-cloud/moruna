//! Descriptors over a Unix socket (`SCM_RIGHTS`): the child's control socket and the memfd go to
//! the template with the `start` request (17 e.3).

use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

/// Send `bytes` on `stream` with `fds` attached to the first of them; the rest of the bytes, if
/// the first send was short, follow without descriptors.
pub fn send_with_fds(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
    if bytes.is_empty() {
        return Err(io::Error::other(
            "descriptors travel with at least one byte",
        ));
    }
    let payload = core::mem::size_of_val(fds);
    // SAFETY: CMSG_SPACE is arithmetic on its argument.
    let space = unsafe { libc::CMSG_SPACE(payload as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // SAFETY: an all-zero msghdr is a valid empty one; its pointers are set below to buffers
    // that outlive the call.
    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg_control` has room for one header and `payload` bytes (CMSG_SPACE), so the first
    // header exists and its data area holds the descriptors.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(payload as u32) as _;
        core::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(cmsg), payload);
    }
    let sent = loop {
        // SAFETY: a connected socket we hold and a message whose buffers are valid.
        let n = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    let mut rest = stream;
    rest.write_all(&bytes[sent..])
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};

    use super::*;

    fn recv_with_fds(stream: &UnixStream, buf: &mut [u8]) -> (usize, Vec<OwnedFd>) {
        let mut control = vec![0u8; 256];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        // SAFETY: a zeroed msghdr pointed at live buffers.
        let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len() as _;
        // SAFETY: as above. The descriptors arrive close-on-exec, as every one the test makes.
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        assert!(n >= 0);
        let mut fds = Vec::new();
        // SAFETY: walking the headers the kernel wrote into `control`.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                let count = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(*data.add(i)));
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        (n as usize, fds)
    }

    #[test]
    fn descriptors_cross_with_their_bytes() {
        let (a, b) = UnixStream::pair().unwrap();
        let (x, mut y) = UnixStream::pair().unwrap();
        send_with_fds(&a, b"hello", &[x.as_raw_fd()]).unwrap();
        let mut buf = [0u8; 16];
        let (n, fds) = recv_with_fds(&b, &mut buf);
        assert_eq!(&buf[..n], b"hello");
        assert_eq!(fds.len(), 1);
        let mut passed = UnixStream::from(fds.into_iter().next().unwrap());
        passed.write_all(b"ok").unwrap();
        let mut got = [0u8; 2];
        y.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ok");
        drop(x);
        assert!(send_with_fds(&a, b"", &[]).is_err());
        peer_gone(&a, b);
    }

    /// A peer that is gone: the send fails with its cause, no signal. The peer ends with
    /// `shutdown`, which acts on the socket, not with dropping `b`, which closes only this
    /// process's descriptor of it. Other tests of this binary fork children that never exec
    /// (seccomp, confine), and a child forked while `b` is open holds a copy of it until it
    /// exits; close-on-exec does not close it, because there is no exec. Dropping `b` then
    /// left the socket open and the send succeeded.
    fn peer_gone(a: &UnixStream, b: UnixStream) {
        b.shutdown(std::net::Shutdown::Both).unwrap();
        drop(b);
        let e = send_with_fds(a, b"late", &[]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::BrokenPipe, "{e}");
    }

    /// The interleaving that failed, made certain: a forked child holds `b` (as a child of
    /// another test does when it forks between `pair` and the end of the test above) for as
    /// long as the send takes, and the peer is still gone.
    #[test]
    fn a_peer_held_by_a_forked_child_is_still_gone() {
        let (a, b) = UnixStream::pair().unwrap();
        let (hold, release) = UnixStream::pair().unwrap();
        // SAFETY: the child closes its copy of `release`, blocks in `read` on `hold` and
        // `_exit`s; every other descriptor it inherited, `b` among them, stays open until then.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let mut byte = [0u8; 1];
            // SAFETY: closing a descriptor this child owns, a read into a live one-byte buffer,
            // then the end of the child. With its own `release` closed, the read ends when the
            // parent writes or when the parent's `release` closes, a failed assertion included.
            unsafe {
                libc::close(release.as_raw_fd());
                libc::read(hold.as_raw_fd(), byte.as_mut_ptr().cast(), 1);
                libc::_exit(0);
            }
        }
        drop(hold);
        peer_gone(&a, b);
        (&release).write_all(b"x").unwrap();
        let mut status = 0;
        // SAFETY: waiting for the child this test forked.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "{status:#x}"
        );
    }
}
