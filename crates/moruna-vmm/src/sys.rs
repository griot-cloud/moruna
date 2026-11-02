//! The operating-system call the portable part of the monitor makes that `std` does not
//! wrap: `poll(2)`, for the vsock backend. `unsafe` is confined to this module, the virtio-mem
//! discard, and the Linux-only KVM modules.

use std::os::fd::RawFd;
use std::time::Duration;

/// One descriptor to poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollFd {
    /// The descriptor.
    pub fd: RawFd,
    /// Wait until it is readable.
    pub read: bool,
    /// Wait until it is writable.
    pub write: bool,
}

/// What `poll` reported for one descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ready {
    /// Readable, or at end of file.
    pub read: bool,
    /// Writable.
    pub write: bool,
    /// Error or invalid descriptor.
    pub error: bool,
}

/// Wait up to `timeout` for any of `fds`; returns one [`Ready`] per descriptor, in order.
/// An interrupted wait returns all-not-ready rather than an error.
pub fn poll(fds: &[PollFd], timeout: Duration) -> std::io::Result<Vec<Ready>> {
    let mut raw: Vec<libc::pollfd> = fds
        .iter()
        .map(|p| libc::pollfd {
            fd: p.fd,
            events: (if p.read { libc::POLLIN } else { 0 })
                | (if p.write { libc::POLLOUT } else { 0 }),
            revents: 0,
        })
        .collect();
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: `raw` is a live, exclusively borrowed array of `raw.len()` pollfd structs for
    // the duration of the call; poll writes only their `revents` fields.
    let rc = unsafe { libc::poll(raw.as_mut_ptr(), raw.len() as libc::nfds_t, ms) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            return Ok(vec![Ready::default(); fds.len()]);
        }
        return Err(e);
    }
    Ok(raw
        .iter()
        .map(|p| Ready {
            read: p.revents & (libc::POLLIN | libc::POLLHUP) != 0,
            write: p.revents & libc::POLLOUT != 0,
            error: p.revents & (libc::POLLERR | libc::POLLNVAL) != 0,
        })
        .collect())
}

static STOP_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static SIGNAL_OWNER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn stop_signal(signal: libc::c_int) {
    STOP_SIGNAL.store(signal, std::sync::atomic::Ordering::Relaxed);
}
/// CLI-only ownership of its termination handlers; embedding APIs install none.
pub(crate) struct Termination {
    previous: [libc::sigaction; 2],
}
impl Termination {
    pub(crate) fn install() -> std::io::Result<Self> {
        use std::sync::atomic::Ordering;
        if SIGNAL_OWNER
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "termination handlers already owned",
            ));
        }
        STOP_SIGNAL.store(0, Ordering::Relaxed);
        // SAFETY: sigaction is a C POD, initialized before use; the handler only
        // stores a lock-free atomic integer and never allocates, locks or calls IO.
        let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
        action.sa_sigaction = stop_signal as *const () as usize;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        let mut previous: [libc::sigaction; 2] = unsafe { core::mem::zeroed() };
        for (index, signal) in [libc::SIGINT, libc::SIGTERM].iter().enumerate() {
            if unsafe { libc::sigaction(*signal, &action, &mut previous[index]) } != 0 {
                let error = std::io::Error::last_os_error();
                if index == 1 {
                    unsafe { libc::sigaction(libc::SIGINT, &previous[0], core::ptr::null_mut()) };
                }
                SIGNAL_OWNER.store(false, Ordering::Release);
                return Err(error);
            }
        }
        Ok(Self { previous })
    }
    pub(crate) fn signal(&self) -> Option<i32> {
        let signal = STOP_SIGNAL.load(std::sync::atomic::Ordering::Relaxed);
        (signal != 0).then_some(signal)
    }
}
impl Drop for Termination {
    fn drop(&mut self) {
        // SAFETY: this CLI owns both installations and restores the saved POD actions.
        for (index, signal) in [libc::SIGINT, libc::SIGTERM].iter().enumerate() {
            unsafe { libc::sigaction(*signal, &self.previous[index], core::ptr::null_mut()) };
        }
        SIGNAL_OWNER.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn vm_t11_poll_reports_readiness() {
        let (mut a, b) = UnixStream::pair().unwrap();
        let fds = [PollFd {
            fd: b.as_raw_fd(),
            read: true,
            write: false,
        }];
        let r = poll(&fds, Duration::from_millis(1)).unwrap();
        assert_eq!(r, vec![Ready::default()]);
        a.write_all(b"x").unwrap();
        let r = poll(&fds, Duration::from_millis(100)).unwrap();
        assert!(r[0].read);
        let w = poll(
            &[PollFd {
                fd: a.as_raw_fd(),
                read: false,
                write: true,
            }],
            Duration::from_millis(100),
        )
        .unwrap();
        assert!(w[0].write);
        let bad = poll(
            &[PollFd {
                fd: 1_000_000,
                read: true,
                write: false,
            }],
            Duration::ZERO,
        )
        .unwrap();
        assert!(bad[0].error);
    }
}
