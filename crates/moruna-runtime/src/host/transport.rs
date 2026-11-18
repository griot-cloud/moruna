//! Addresses and sockets for the host protocol (MH 4.3): a Unix socket anywhere, a vsock socket
//! on Linux, on the Moruna kernel a stream of its vsock driver (SDD 30 f.2), which the engine
//! reaches through the connection it was granted, and on a Moruna node the channel
//! `node-admission` started it with (system design 3A). One peer, one connection, so there is
//! no multiplexing and no framing beyond the newline: over a channel, each message is the next
//! piece of the stream ([`Messages`]).

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
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
    /// `channel:SLOT`: a channel of the Moruna kernel the engine was started with, granted under
    /// `SLOT` (system design 3A). Served on, it puts `moruna serve` in node mode
    /// ([`super::node`]).
    Channel(String),
}

/// Whether `slot` is a slot's name as the kernel takes one: 1 to 32 of lowercase letters,
/// digits, `.`, `_` and `-`.
fn is_slot(slot: &str) -> bool {
    (1..=32).contains(&slot.len())
        && slot
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

/// `VMADDR_CID_ANY`.
pub const VSOCK_CID_ANY: u32 = u32::MAX;

impl Address {
    /// Parse an address (MH 4.3). The error says what was expected.
    pub fn parse(text: &str) -> Result<Address, String> {
        if let Some(slot) = text.strip_prefix("channel:") {
            if !is_slot(slot) {
                return Err(format!(
                    "`{text}`: a channel is channel:SLOT, its slot 1 to 32 of a-z, 0-9, . _ -"
                ));
            }
            return Ok(Address::Channel(slot.to_string()));
        }
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
                "`{text}`: the protocol runs over unix:// or vsock:// sockets, or a channel"
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
            Address::Channel(slot) => write!(f, "channel:{slot}"),
        }
    }
}

/// The largest message a channel of the Moruna kernel carries
/// (`moruna_abi::limits::CHANNEL_MAX_BYTES`).
pub const CHANNEL_MESSAGE_BYTES: usize = 64 * 1024;

/// One end of a channel, as the host protocol's byte stream rides it (system design 3A): each
/// message is the next piece of the stream, at most [`CHANNEL_MESSAGE_BYTES`], with no handle,
/// and the stream ends when the peer closes its end.
pub trait Messages: Send + Sync {
    /// Sends `bytes`, at most [`CHANNEL_MESSAGE_BYTES`] and never empty, as one message,
    /// waiting while the peer's queue is full, at most [`WRITE_TIMEOUT`].
    fn send(&self, bytes: &[u8]) -> io::Result<()>;
    /// Receives the next message into `buf`, [`CHANNEL_MESSAGE_BYTES`] long, waiting for one;
    /// `Ok(0)` once the stream has ended.
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize>;
    /// Closes this end: a `recv` waiting on it returns the end of the stream.
    fn close(&self);
}

/// The byte stream over `messages`, as a [`Conn`].
pub fn over_messages(messages: Arc<dyn Messages>) -> Conn {
    let closer = messages.clone();
    Conn {
        reader: Box::new(MessageReader {
            messages: messages.clone(),
            buf: vec![0; CHANNEL_MESSAGE_BYTES],
            at: 0,
            len: 0,
            ended: false,
        }),
        writer: Box::new(MessageWriter { messages }),
        closer: Box::new(move || closer.close()),
    }
}

/// The stream's inbound half: one message at a time, read out as asked.
struct MessageReader {
    messages: Arc<dyn Messages>,
    buf: Vec<u8>,
    at: usize,
    len: usize,
    ended: bool,
}

impl Read for MessageReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.at == self.len {
            if self.ended {
                return Ok(0);
            }
            self.len = self.messages.recv(&mut self.buf)?;
            self.at = 0;
            if self.len == 0 {
                self.ended = true;
                return Ok(0);
            }
        }
        let count = out.len().min(self.len - self.at);
        out[..count].copy_from_slice(&self.buf[self.at..self.at + count]);
        self.at += count;
        Ok(count)
    }
}

/// The stream's outbound half: each write is one message, of as much as one carries.
struct MessageWriter {
    messages: Arc<dyn Messages>,
}

impl Write for MessageWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = bytes.len().min(CHANNEL_MESSAGE_BYTES);
        self.messages.send(&bytes[..count])?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A channel exists only on the Moruna kernel.
fn no_channel(slot: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("channel:{slot} is a channel of the Moruna kernel, and this process is not on it"),
    )
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
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Address::Vsock { cid, port } if moruna_micro::on_kernel() => {
            micro::conn(micro::driver()?.connect(*cid, *port)?)
        }
        Address::Vsock { cid, port } => vsock::connect(*cid, *port),
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Address::Channel(slot) if moruna_micro::on_kernel() => micro::channel(slot),
        Address::Channel(slot) => Err(no_channel(slot)),
    }
}

/// The vsock of the Moruna kernel (SDD 30 f.2): the engine's one connection to its driver,
/// granted under `vsock`, and the streams it opens as a [`Conn`].
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod micro {
    use std::io;
    use std::sync::{Arc, OnceLock};

    use moruna_micro::vsock::{Driver, Stream};

    use super::Conn;

    /// The slot the engine's connection to the vsock driver is granted under.
    const SLOT: &str = "vsock";

    pub(super) fn driver() -> io::Result<Arc<Driver>> {
        static DRIVER: OnceLock<Result<Arc<Driver>, String>> = OnceLock::new();
        DRIVER
            .get_or_init(|| Driver::open(SLOT).map_err(|e| e.to_string()))
            .clone()
            .map_err(io::Error::other)
    }

    /// The channel granted under `slot`, as the host protocol's stream.
    pub(super) fn channel(slot: &str) -> io::Result<Conn> {
        Ok(super::over_messages(Arc::new(Channel(
            moruna_micro::channel::Endpoint::open(slot)?,
        ))))
    }

    /// The kernel's channel endpoint as [`super::Messages`].
    struct Channel(moruna_micro::channel::Endpoint);

    impl super::Messages for Channel {
        fn send(&self, bytes: &[u8]) -> io::Result<()> {
            self.0.send(bytes)
        }

        fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.recv(buf)
        }

        fn close(&self) {
            self.0.close();
        }
    }

    pub(super) fn conn(stream: Stream) -> io::Result<Conn> {
        let (reader, writer, closer) = stream.into_parts();
        Ok(Conn {
            reader: Box::new(reader),
            writer: Box::new(writer),
            closer,
        })
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
    /// A port of the Moruna kernel's vsock driver (SDD 30 f.2).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Kernel(moruna_micro::vsock::Listener),
    /// A channel already connected to its one peer: accepting it hands it over, once.
    Channel(std::sync::Mutex<Option<Conn>>),
}

impl Listener {
    /// Wait for the peer.
    pub fn accept(&self) -> io::Result<Conn> {
        match &self.inner {
            ListenerKind::Unix(listener, _) => Conn::unix(listener.accept()?.0),
            #[cfg(target_os = "linux")]
            ListenerKind::Vsock(fd) => vsock::accept(fd),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            ListenerKind::Kernel(listener) => micro::conn(listener.accept()?.0),
            ListenerKind::Channel(conn) => conn
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "a channel has one peer, and it was accepted",
                    )
                }),
        }
    }

    /// A listener whose one peer is already connected on `conn`: what a channel is.
    pub fn connected(conn: Conn) -> Listener {
        Listener {
            inner: ListenerKind::Channel(std::sync::Mutex::new(Some(conn))),
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
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            ListenerKind::Kernel(_) => {}
            ListenerKind::Channel(_) => {}
        }
    }
}

/// Listen at `address` for one peer.
pub fn listen(address: &Address) -> io::Result<Listener> {
    match address {
        Address::Unix(path) => Ok(Listener {
            inner: ListenerKind::Unix(std::os::unix::net::UnixListener::bind(path)?, path.clone()),
        }),
        // On the Moruna kernel the driver listens for every cid: the address's is the host's
        // business (`-1`, `VMADDR_CID_ANY`, in every document that listens).
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Address::Vsock { port, .. } if moruna_micro::on_kernel() => Ok(Listener {
            inner: ListenerKind::Kernel(micro::driver()?.listen(*port)?),
        }),
        #[cfg(target_os = "linux")]
        Address::Vsock { cid, port } => Ok(Listener {
            inner: ListenerKind::Vsock(vsock::listen(*cid, *port)?),
        }),
        #[cfg(not(target_os = "linux"))]
        Address::Vsock { .. } => Err(vsock::unsupported()),
        Address::Channel(_) => connect(address).map(Listener::connected),
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

    #[test]
    fn a_channels_address_is_its_slot() {
        assert_eq!(
            Address::parse("channel:engine"),
            Ok(Address::Channel("engine".into()))
        );
        assert_eq!(
            Address::parse("channel:a.b_c-1").map(|a| a.to_string()),
            Ok("channel:a.b_c-1".to_string())
        );
        let longest = format!("channel:{}", "s".repeat(32));
        assert!(Address::parse(&longest).is_ok());
        for text in [
            "channel:",
            "channel:Engine",
            "channel:a b",
            "channel:a/b",
            &format!("channel:{}", "s".repeat(33)),
        ] {
            assert!(Address::parse(text).is_err(), "{text}");
        }
    }

    /// Messages in memory: what one end sent, for the other to receive.
    #[derive(Default)]
    struct Memory {
        queue: std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>,
        closed: std::sync::atomic::AtomicBool,
    }

    impl Messages for Memory {
        fn send(&self, bytes: &[u8]) -> io::Result<()> {
            assert!(!bytes.is_empty() && bytes.len() <= CHANNEL_MESSAGE_BYTES);
            self.queue.lock().unwrap().push_back(bytes.to_vec());
            Ok(())
        }

        fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            assert_eq!(buf.len(), CHANNEL_MESSAGE_BYTES, "room for any message");
            match self.queue.lock().unwrap().pop_front() {
                Some(message) => {
                    buf[..message.len()].copy_from_slice(&message);
                    Ok(message.len())
                }
                None => Ok(0),
            }
        }

        fn close(&self) {
            self.closed
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The stream rides the messages: a write is one message of at most 64 KiB, a read takes
    /// what it asks for across them, and the end of the messages is the end of the stream.
    #[test]
    fn a_stream_rides_a_channels_messages() {
        let memory = Arc::new(Memory::default());
        let conn = over_messages(memory.clone());
        let (mut reader, mut writer, closer) = conn.into_parts();
        let long: Vec<u8> = (0..150 * 1024).map(|i| (i % 251) as u8).collect();
        writer.write_all(&long).expect("written");
        writer.write_all(b"hello\n").expect("written");
        assert_eq!(writer.write(b"").expect("nothing"), 0);
        writer.flush().expect("flushed");
        let sizes: Vec<usize> = memory.queue.lock().unwrap().iter().map(Vec::len).collect();
        assert_eq!(
            sizes,
            [
                CHANNEL_MESSAGE_BYTES,
                CHANNEL_MESSAGE_BYTES,
                150 * 1024 - 2 * CHANNEL_MESSAGE_BYTES,
                6
            ]
        );
        let mut back = vec![0u8; long.len() + 6];
        let mut small = [0u8; 7];
        // A small read takes part of a message, and the rest is read after it.
        assert_eq!(reader.read(&mut small).expect("read"), 7);
        back[..7].copy_from_slice(&small);
        reader.read_exact(&mut back[7..]).expect("the rest");
        assert_eq!(&back[..long.len()], &long[..]);
        assert_eq!(&back[long.len()..], b"hello\n");
        assert_eq!(reader.read(&mut []).expect("an empty read"), 0);
        assert_eq!(reader.read(&mut small).expect("the end"), 0);
        assert_eq!(reader.read(&mut small).expect("still the end"), 0);
        closer();
        assert!(memory.closed.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// A channel is connected to its one peer already: listening on it hands that peer over
    /// once.
    #[test]
    fn a_channel_is_accepted_once() {
        let listener = Listener::connected(over_messages(Arc::new(Memory::default())));
        assert!(listener.accept().is_ok());
        assert_eq!(
            listener.accept().err().map(|e| e.kind()),
            Some(io::ErrorKind::NotConnected)
        );
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

/// The vsock transport over the host's loopback (CID 1), where the host has one.
#[cfg(all(test, target_os = "linux"))]
mod vsock_loopback {
    use std::io::{Read, Write};

    #[test]
    fn a_vsock_listener_accepts_its_peer_and_carries_bytes_both_ways() {
        let port = 40_000 + (std::process::id() % 20_000);
        let listener = match super::vsock::listen(libc::VMADDR_CID_LOCAL, port) {
            Ok(l) => l,
            // No vsock (or no loopback transport) on this host: nothing to exercise.
            Err(e) => {
                eprintln!("no vsock loopback here: {e}");
                return;
            }
        };
        let server = std::thread::spawn(move || {
            let mut conn = super::vsock::accept(&listener).expect("accept");
            let mut buf = [0u8; 4];
            conn.reader.read_exact(&mut buf).expect("read");
            conn.writer.write_all(&buf).expect("echo");
            conn.close();
        });
        let mut client = super::vsock::connect(libc::VMADDR_CID_LOCAL, port).expect("connect");
        client.writer.write_all(b"ping").unwrap();
        let mut back = [0u8; 4];
        client.reader.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"ping");
        server.join().unwrap();
        // The peer closed: reads end.
        let mut rest = Vec::new();
        assert_eq!(client.reader.read_to_end(&mut rest).unwrap_or(0), 0);
        client.close();
        // Nothing listens on another port: connecting is refused.
        assert!(super::vsock::connect(libc::VMADDR_CID_LOCAL, port + 1).is_err());
        // The same port cannot be bound twice while held... after release it can again.
    }
}
