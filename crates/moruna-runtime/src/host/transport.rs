//! Addresses and sockets for the host protocol (MH 4.3): a Unix socket anywhere, a vsock socket
//! on Linux. One peer, one connection, so there is no multiplexing and no framing beyond the
//! newline.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

/// How long a write to the peer may block before the peer is treated as gone (MH 4.3). A peer
/// that stops reading without closing would otherwise stall the run's last message for ever.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the peer is (MH 4.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    /// `unix:///path/to/socket`, `unix:/path` or an absolute path.
    Unix(PathBuf),
    /// `vsock://CID:PORT`; `-1` as the CID is `VMADDR_CID_ANY`, for listening.
    Vsock {
        /// The context id.
        cid: u32,
        /// The port.
        port: u32,
    },
}

/// `VMADDR_CID_ANY`.
pub const VSOCK_CID_ANY: u32 = u32::MAX;

impl Address {
    /// Parse an address (MH 4.3). The error says what was expected.
    pub fn parse(text: &str) -> Result<Address, String> {
        if let Some(rest) = text.strip_prefix("vsock://") {
            let (cid, port) = rest
                .split_once(':')
                .ok_or_else(|| format!("`{text}`: a vsock address is vsock://CID:PORT"))?;
            let cid = if cid == "-1" {
                VSOCK_CID_ANY
            } else {
                cid.parse::<u32>()
                    .map_err(|_| format!("`{text}`: the CID `{cid}` is not a number or -1"))?
            };
            let port = port
                .parse::<u32>()
                .map_err(|_| format!("`{text}`: the port `{port}` is not a number"))?;
            return Ok(Address::Vsock { cid, port });
        }
        let path = if let Some(rest) = text.strip_prefix("unix://") {
            rest
        } else if let Some(rest) = text.strip_prefix("unix:") {
            rest
        } else if text.contains("://") {
            return Err(format!(
                "`{text}`: the protocol runs over unix:// or vsock:// sockets"
            ));
        } else {
            text
        };
        if path.is_empty() || !path.starts_with('/') {
            return Err(format!(
                "`{text}`: a Unix socket address is an absolute path"
            ));
        }
        Ok(Address::Unix(PathBuf::from(path)))
    }
}

impl core::fmt::Display for Address {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Address::Unix(path) => write!(f, "unix://{}", path.display()),
            Address::Vsock { cid, port } if *cid == VSOCK_CID_ANY => write!(f, "vsock://-1:{port}"),
            Address::Vsock { cid, port } => write!(f, "vsock://{cid}:{port}"),
        }
    }
}

/// A connection's three parts: the inbound reader, the outbound writer, and what closes both.
pub type Parts = (
    Box<dyn Read + Send>,
    Box<dyn Write + Send>,
    Box<dyn Fn() + Send + Sync>,
);

/// One connection, split: a reader for the inbound messages and a writer for the outbound.
pub struct Conn {
    /// Inbound.
    pub reader: Box<dyn Read + Send>,
    /// Outbound.
    pub writer: Box<dyn Write + Send>,
    closer: Box<dyn Fn() + Send + Sync>,
}

impl Conn {
    /// Close both directions, so a thread blocked reading the peer returns.
    pub fn close(&self) {
        (self.closer)();
    }

    /// Split into its three parts, for threads that each own one.
    pub fn into_parts(self) -> Parts {
        (self.reader, self.writer, self.closer)
    }

    fn unix(stream: std::os::unix::net::UnixStream) -> io::Result<Conn> {
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let reader = stream.try_clone()?;
        let closer = stream.try_clone()?;
        Ok(Conn {
            reader: Box::new(reader),
            writer: Box::new(stream),
            closer: Box::new(move || {
                let _ = closer.shutdown(std::net::Shutdown::Both);
            }),
        })
    }
}

/// Connect to a peer that is listening (`moruna run` with `report.socket`).
pub fn connect(address: &Address) -> io::Result<Conn> {
    match address {
        Address::Unix(path) => Conn::unix(std::os::unix::net::UnixStream::connect(path)?),
        Address::Vsock { cid, port } => vsock::connect(*cid, *port),
    }
}

/// A socket waiting for its one peer (`moruna serve --listen`).
pub struct Listener {
    inner: ListenerKind,
}

enum ListenerKind {
    Unix(std::os::unix::net::UnixListener, PathBuf),
    #[cfg(target_os = "linux")]
    Vsock(std::os::fd::OwnedFd),
}

impl Listener {
    /// Wait for the peer.
    pub fn accept(&self) -> io::Result<Conn> {
        match &self.inner {
            ListenerKind::Unix(listener, _) => Conn::unix(listener.accept()?.0),
            #[cfg(target_os = "linux")]
            ListenerKind::Vsock(fd) => vsock::accept(fd),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        match &self.inner {
            ListenerKind::Unix(_, path) => {
                let _ = std::fs::remove_file(path);
            }
            #[cfg(target_os = "linux")]
            ListenerKind::Vsock(_) => {}
        }
    }
}

/// Listen at `address` for one peer.
pub fn listen(address: &Address) -> io::Result<Listener> {
    match address {
        Address::Unix(path) => Ok(Listener {
            inner: ListenerKind::Unix(std::os::unix::net::UnixListener::bind(path)?, path.clone()),
        }),
        #[cfg(target_os = "linux")]
        Address::Vsock { cid, port } => Ok(Listener {
            inner: ListenerKind::Vsock(vsock::listen(*cid, *port)?),
        }),
        #[cfg(not(target_os = "linux"))]
        Address::Vsock { .. } => Err(vsock::unsupported()),
    }
}

#[cfg(not(target_os = "linux"))]
mod vsock {
    use std::io;

    pub(super) fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "vsock is a Linux socket family; this build is not for Linux",
        )
    }

    pub(super) fn connect(_cid: u32, _port: u32) -> io::Result<super::Conn> {
        Err(unsupported())
    }
}

/// AF_VSOCK through `libc`, the only `unsafe` in this crate . Each call is a plain
/// system call on a descriptor this module owns; the descriptors are wrapped in `OwnedFd` at
/// once, so none is leaked or closed twice.
#[cfg(target_os = "linux")]
mod vsock {
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::{Conn, WRITE_TIMEOUT};

    fn address(cid: u32, port: u32) -> libc::sockaddr_vm {
        // SAFETY: `sockaddr_vm` is a plain C struct of integers, for which all zeroes is a
        // valid value; the fields that matter are set below.
        let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = cid;
        addr
    }

    fn socket() -> io::Result<OwnedFd> {
        // SAFETY: `socket` takes no pointers; a negative return is an error and nothing is
        // owned, a non-negative one is a new descriptor this function takes ownership of.
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by `socket` and is owned by nobody else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn conn(fd: OwnedFd) -> io::Result<Conn> {
        let timeout = libc::timeval {
            tv_sec: WRITE_TIMEOUT.as_secs() as libc::time_t,
            tv_usec: 0,
        };
        // SAFETY: the pointer and length describe `timeout`, which outlives the call.
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDTIMEO,
                (&raw const timeout).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let writer = File::from(fd);
        let reader = writer.try_clone()?;
        let closer = writer.try_clone()?;
        Ok(Conn {
            reader: Box::new(reader),
            writer: Box::new(writer),
            closer: Box::new(move || {
                // SAFETY: `shutdown` takes a descriptor this closure owns (through `closer`)
                // and no pointers.
                unsafe {
                    libc::shutdown(closer.as_raw_fd(), libc::SHUT_RDWR);
                }
            }),
        })
    }

    pub(super) fn connect(cid: u32, port: u32) -> io::Result<Conn> {
        let fd = socket()?;
        let addr = address(cid, port);
        // SAFETY: the pointer and length describe `addr`, which outlives the call.
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&raw const addr).cast(),
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        conn(fd)
    }

    pub(super) fn listen(cid: u32, port: u32) -> io::Result<OwnedFd> {
        let fd = socket()?;
        let addr = address(cid, port);
        // SAFETY: the pointer and length describe `addr`, which outlives the call.
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const addr).cast(),
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `listen` takes a descriptor this function owns and no pointers.
        if unsafe { libc::listen(fd.as_raw_fd(), 1) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }

    pub(super) fn accept(listener: &OwnedFd) -> io::Result<Conn> {
        // SAFETY: null address pointers ask the kernel not to report the peer's address; the
        // return is a new descriptor or an error.
        let fd = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by `accept4` and is owned by nobody else.
        conn(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse() {
        assert_eq!(
            Address::parse("unix:///run/m.sock"),
            Ok(Address::Unix(PathBuf::from("/run/m.sock")))
        );
        assert_eq!(
            Address::parse("unix:/run/m.sock"),
            Ok(Address::Unix(PathBuf::from("/run/m.sock")))
        );
        assert_eq!(
            Address::parse("/run/m.sock"),
            Ok(Address::Unix(PathBuf::from("/run/m.sock")))
        );
        assert_eq!(
            Address::parse("vsock://-1:5000"),
            Ok(Address::Vsock {
                cid: VSOCK_CID_ANY,
                port: 5000
            })
        );
        assert_eq!(
            Address::parse("vsock://2:5000"),
            Ok(Address::Vsock { cid: 2, port: 5000 })
        );
        assert!(Address::parse("vsock://2").is_err());
        assert!(Address::parse("vsock://x:1").is_err());
        assert!(Address::parse("vsock://2:y").is_err());
        assert!(Address::parse("tcp://1.2.3.4:5").is_err());
        assert!(Address::parse("relative.sock").is_err());
        assert!(Address::parse("unix://").is_err());
        for text in ["unix:///run/m.sock", "vsock://-1:5000", "vsock://3:7"] {
            let parsed = Address::parse(text).expect("parses");
            assert_eq!(parsed.to_string(), text);
        }
    }

    /// A Unix socket carries a line each way, and the listener removes its file.
    #[test]
    fn a_unix_socket_carries_lines() {
        let path = std::env::temp_dir().join(format!("moruna-t-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let address = Address::Unix(path.clone());
        let listener = listen(&address).expect("listen");
        let client = std::thread::spawn({
            let address = address.clone();
            move || {
                let mut conn = connect(&address).expect("connect");
                conn.writer.write_all(b"ping\n").expect("write");
                let mut buf = [0u8; 5];
                conn.reader.read_exact(&mut buf).expect("read");
                buf
            }
        });
        let mut server = listener.accept().expect("accept");
        let mut buf = [0u8; 5];
        server.reader.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"ping\n");
        server.writer.write_all(b"pong\n").expect("write");
        assert_eq!(&client.join().expect("client"), b"pong\n");
        server.close();
        drop(listener);
        assert!(!path.exists(), "the listener removes its socket file");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn vsock_is_linux_only_here() {
        let address = Address::Vsock { cid: 2, port: 1 };
        assert_eq!(
            connect(&address).err().map(|e| e.kind()),
            Some(io::ErrorKind::Unsupported)
        );
        assert!(listen(&address).is_err());
    }
}
