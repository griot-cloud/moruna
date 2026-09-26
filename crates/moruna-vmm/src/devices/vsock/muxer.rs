//! The host side of the vsock device: connections between guest ports and host Unix sockets,
//! Firecracker's convention.
//!
//! - **Host to guest.** A host process connects to the Unix socket at `uds_path` and writes
//!   `CONNECT <port>\n`; the muxer sends the guest a `REQUEST` from a fresh host port, and when
//!   the guest accepts, writes `OK <host port>\n` back. From then on the stream is the
//!   connection.
//! - **Guest to host.** A guest connecting to host port `P` reaches the Unix socket
//!   `<uds_path>_P`, or an in-process endpoint the monitor registered for `P` (the status
//!   port). No listener there is a refusal (`RST`).
//!
//! The muxer never interprets the bytes it carries (MH 4.8.4, "the monitor never reads the
//! traffic"); the status port is the monitor's own endpoint, not a peek into anyone else's.
//!
//! Flow control is virtio's credit scheme: the guest never has more than [`BUF_ALLOC`] bytes in
//! flight towards a host stream, the muxer never sends more than the guest's advertised buffer,
//! and a guest that exceeds its credit is reset rather than buffered without bound.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use virtio_queue::{Queue, QueueOwnedT, QueueT};

use super::packet::{HDR_BYTES, Header, SHUTDOWN_RCV, SHUTDOWN_SEND, TYPE_STREAM, op};
use crate::config::HOST_CID;
use crate::devices::Mem;
use crate::error::{Result, VmmError};
use crate::sys::{PollFd, Ready};

/// Receive buffer the host side advertises per connection: 256 KiB, Firecracker's figure;
/// enough to keep a stream at link speed with the backend's poll latency.
pub const BUF_ALLOC: u32 = 256 << 10;
/// Unreported consumption that triggers a `CREDIT_UPDATE`: a quarter of the buffer, so the
/// guest never stalls waiting for credit the host already has.
pub const CREDIT_UPDATE_THRESHOLD: u32 = BUF_ALLOC / 4;
/// Largest payload in one packet: Linux's `VIRTIO_VSOCK_MAX_PKT_BUF_SIZE`.
pub const MAX_PKT_PAYLOAD: usize = 64 << 10;
/// Longest `CONNECT <port>\n` line accepted.
pub const CONNECT_LINE_MAX: usize = 32;
/// Most connections at once; a guest opening more is refused (`RST`).
pub const MAX_CONNECTIONS: usize = 256;
/// Most host connections waiting to send their `CONNECT` line.
pub const MAX_ACCEPTING: usize = 64;
/// Most control packets queued for a guest that posts no receive buffers; beyond this the
/// oldest are dropped (they are best-effort resets and credit updates).
pub const MAX_PENDING_CONTROL: usize = 4096;
/// The first host port handed to a host-initiated connection; high, so it never collides
/// with a well-known port a guest might connect to.
pub const FIRST_HOST_PORT: u32 = 1 << 30;

/// Opens the host end of a guest-initiated connection to one host port.
pub type Connector = Box<dyn Fn() -> std::io::Result<UnixStream> + Send>;

/// A connection's key: the host port and the guest port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    /// The host side's port.
    pub host_port: u32,
    /// The guest side's port.
    pub guest_port: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Host-initiated, `REQUEST` sent, waiting for the guest.
    Connecting,
    /// Data flows.
    Established,
}

struct Conn {
    stream: UnixStream,
    state: State,
    /// Bytes for the host stream: the `OK` line, then guest data.
    out: VecDeque<u8>,
    /// Of `out`, how many leading bytes are the `OK` line (not guest data, not credited).
    reply_len: usize,
    fwd_cnt: u32,
    fwd_reported: u32,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    rx_cnt: u32,
    readable: bool,
    host_eof: bool,
    guest_shut_send: bool,
    guest_shut_rcv: bool,
    write_shut: bool,
}

impl Conn {
    fn new(stream: UnixStream, state: State) -> Self {
        Conn {
            stream,
            state,
            out: VecDeque::new(),
            reply_len: 0,
            fwd_cnt: 0,
            fwd_reported: 0,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            rx_cnt: 0,
            readable: state == State::Established,
            host_eof: false,
            guest_shut_send: false,
            guest_shut_rcv: false,
            write_shut: false,
        }
    }

    /// Bytes the guest can still take from us.
    fn peer_credit(&self) -> u32 {
        self.peer_buf_alloc
            .saturating_sub(self.rx_cnt.wrapping_sub(self.peer_fwd_cnt))
    }

    fn wants_read(&self) -> bool {
        self.state == State::Established
            && !self.host_eof
            && !self.guest_shut_rcv
            && self.peer_credit() > 0
    }

    /// Write what can be written without blocking; returns false when the host stream failed.
    fn flush(&mut self) -> bool {
        while !self.out.is_empty() {
            let (a, _) = self.out.as_slices();
            match self.stream.write(a) {
                Ok(0) => return false,
                Ok(n) => {
                    self.out.drain(..n);
                    let reply = n.min(self.reply_len);
                    self.reply_len -= reply;
                    self.fwd_cnt = self.fwd_cnt.wrapping_add((n - reply) as u32);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        if self.guest_shut_send && !self.write_shut {
            let _ = self.stream.shutdown(std::net::Shutdown::Write);
            self.write_shut = true;
        }
        true
    }
}

struct Accepting {
    stream: UnixStream,
    line: Vec<u8>,
}

/// What a poll entry refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// The wake pipe.
    Wake,
    /// The host listener.
    Listener,
    /// A host connection still sending its `CONNECT` line.
    Accepting(usize),
    /// A connection.
    Conn(Key),
}

/// Wakes the backend thread so it recomputes what to poll.
#[derive(Debug)]
pub struct Waker(UnixStream);

impl Waker {
    /// Wake the backend; a full pipe already means it will wake.
    pub fn wake(&self) {
        let _ = (&self.0).write(&[1]);
    }
}

/// The muxer.
pub struct Muxer {
    guest_cid: u64,
    uds_path: PathBuf,
    listener: UnixListener,
    accepting: Vec<Accepting>,
    conns: BTreeMap<Key, Conn>,
    control: VecDeque<Header>,
    next_port: u32,
    connectors: HashMap<u32, Connector>,
    wake_rx: UnixStream,
    wake_tx: UnixStream,
    last_served: Option<Key>,
    buf: Vec<u8>,
}

fn unix_path_for_port(uds: &Path, port: u32) -> PathBuf {
    let mut s = uds.as_os_str().to_owned();
    s.push(format!("_{port}"));
    PathBuf::from(s)
}

impl Muxer {
    /// A muxer for the guest `guest_cid`, listening for host connections at `uds_path` (a
    /// stale socket file there is replaced).
    pub fn new(guest_cid: u32, uds_path: &Path) -> Result<Self> {
        if let Ok(meta) = std::fs::symlink_metadata(uds_path) {
            use std::os::unix::fs::FileTypeExt;
            if !meta.file_type().is_socket() {
                return Err(VmmError::config(
                    "--vsock-uds",
                    format!("{} exists and is not a socket", uds_path.display()),
                ));
            }
            std::fs::remove_file(uds_path)
                .map_err(|e| VmmError::io("remove", uds_path.display().to_string(), &e))?;
        }
        let listener = UnixListener::bind(uds_path)
            .map_err(|e| VmmError::config("--vsock-uds", format!("{}: {e}", uds_path.display())))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| VmmError::io("nonblocking", uds_path.display().to_string(), &e))?;
        let (wake_rx, wake_tx) =
            UnixStream::pair().map_err(|e| VmmError::io("socketpair", "vsock wake", &e))?;
        for s in [&wake_rx, &wake_tx] {
            s.set_nonblocking(true)
                .map_err(|e| VmmError::io("nonblocking", "vsock wake", &e))?;
        }
        Ok(Muxer {
            guest_cid: guest_cid as u64,
            uds_path: uds_path.to_path_buf(),
            listener,
            accepting: Vec::new(),
            conns: BTreeMap::new(),
            control: VecDeque::new(),
            next_port: FIRST_HOST_PORT,
            connectors: HashMap::new(),
            wake_rx,
            wake_tx,
            last_served: None,
            buf: vec![0; MAX_PKT_PAYLOAD],
        })
    }

    /// A handle that wakes the backend thread.
    pub fn waker(&self) -> Result<Waker> {
        self.wake_tx
            .try_clone()
            .map(Waker)
            .map_err(|e| VmmError::io("dup", "vsock wake", &e))
    }

    /// Serve guest connections to host port `port` in-process with `connector`.
    pub fn add_connector(&mut self, port: u32, connector: Connector) {
        self.connectors.insert(port, connector);
    }

    /// Open a host-initiated connection to guest port `guest_port` over `stream`, as if a
    /// host process had connected and sent `CONNECT <guest_port>`.
    pub fn connect_to_guest(&mut self, stream: UnixStream, guest_port: u32) -> Result<()> {
        stream
            .set_nonblocking(true)
            .map_err(|e| VmmError::io("nonblocking", "vsock connection", &e))?;
        if self.conns.len() >= MAX_CONNECTIONS {
            return Err(VmmError::Control("too many vsock connections".into()));
        }
        let host_port = self.alloc_port(guest_port);
        let key = Key {
            host_port,
            guest_port,
        };
        self.conns.insert(key, Conn::new(stream, State::Connecting));
        self.push_control(key, op::REQUEST, 0);
        Ok(())
    }

    /// Live connections.
    pub fn connections(&self) -> usize {
        self.conns.len()
    }

    /// Control packets waiting for guest receive buffers.
    pub fn pending_control(&self) -> usize {
        self.control.len()
    }

    fn alloc_port(&mut self, guest_port: u32) -> u32 {
        loop {
            let p = self.next_port;
            self.next_port = self.next_port.checked_add(1).unwrap_or(FIRST_HOST_PORT);
            if !self.conns.contains_key(&Key {
                host_port: p,
                guest_port,
            }) {
                return p;
            }
        }
    }

    fn header(&self, key: Key, op_: u16, flags: u32, len: u32, fwd_cnt: u32) -> Header {
        Header {
            src_cid: HOST_CID as u64,
            dst_cid: self.guest_cid,
            src_port: key.host_port,
            dst_port: key.guest_port,
            len,
            type_: TYPE_STREAM,
            op: op_,
            flags,
            buf_alloc: BUF_ALLOC,
            fwd_cnt,
        }
    }

    fn push_control(&mut self, key: Key, op_: u16, flags: u32) {
        let fwd = self.conns.get(&key).map_or(0, |c| c.fwd_cnt);
        if let Some(c) = self.conns.get_mut(&key) {
            c.fwd_reported = c.fwd_cnt;
        }
        let h = self.header(key, op_, flags, 0, fwd);
        if self.control.len() >= MAX_PENDING_CONTROL {
            self.control.pop_front();
        }
        self.control.push_back(h);
    }

    fn reset(&mut self, key: Key) {
        self.conns.remove(&key);
        self.push_control(key, op::RST, 0);
    }

    /// A packet the guest sent (from the transmit queue).
    pub fn recv(&mut self, h: &Header, payload: &[u8]) {
        if h.dst_cid != HOST_CID as u64 || h.src_cid != self.guest_cid {
            return; // not ours to answer: a spoofed or misrouted packet is dropped
        }
        let key = Key {
            host_port: h.dst_port,
            guest_port: h.src_port,
        };
        if h.type_ != TYPE_STREAM {
            if h.op != op::RST {
                self.push_control(key, op::RST, 0);
            }
            return;
        }
        if h.op == op::REQUEST {
            self.guest_connect(key, h);
            return;
        }
        let Some(c) = self.conns.get_mut(&key) else {
            if h.op != op::RST {
                self.push_control(key, op::RST, 0);
            }
            return;
        };
        c.peer_buf_alloc = h.buf_alloc;
        c.peer_fwd_cnt = h.fwd_cnt;
        match h.op {
            op::RESPONSE if c.state == State::Connecting => {
                c.state = State::Established;
                c.readable = true;
                let line = format!("OK {}\n", key.host_port);
                c.reply_len = line.len();
                c.out.extend(line.as_bytes());
                if !c.flush() {
                    self.reset(key);
                }
            }
            op::RW if c.state == State::Established => {
                let guest_data = c.out.len() - c.reply_len;
                if guest_data + payload.len() > BUF_ALLOC as usize {
                    // The guest sent beyond the credit it was given.
                    self.reset(key);
                    return;
                }
                c.out.extend(payload);
                if !c.flush() {
                    self.reset(key);
                    return;
                }
                self.maybe_credit_update(key);
            }
            op::CREDIT_UPDATE => {}
            op::CREDIT_REQUEST => self.push_control(key, op::CREDIT_UPDATE, 0),
            op::SHUTDOWN => {
                c.guest_shut_send |= h.flags & SHUTDOWN_SEND != 0;
                c.guest_shut_rcv |= h.flags & SHUTDOWN_RCV != 0;
                if !c.flush() {
                    self.reset(key);
                    return;
                }
                self.finish_if_closed(key);
            }
            op::RST => {
                self.conns.remove(&key);
            }
            // A RESPONSE to an established connection, data before the guest accepted, or an
            // unknown operation: the connection is torn down.
            _ => self.reset(key),
        }
    }

    /// The guest shut both directions and every byte reached the host: close with `RST`.
    fn finish_if_closed(&mut self, key: Key) {
        if let Some(c) = self.conns.get(&key)
            && c.guest_shut_send
            && c.guest_shut_rcv
            && c.out.is_empty()
        {
            self.reset(key);
        }
    }

    fn maybe_credit_update(&mut self, key: Key) {
        if let Some(c) = self.conns.get(&key)
            && c.fwd_cnt.wrapping_sub(c.fwd_reported) >= CREDIT_UPDATE_THRESHOLD
        {
            self.push_control(key, op::CREDIT_UPDATE, 0);
        }
    }

    fn guest_connect(&mut self, key: Key, h: &Header) {
        if self.conns.contains_key(&key) || self.conns.len() >= MAX_CONNECTIONS {
            self.push_control(key, op::RST, 0);
            return;
        }
        let stream = match self.connectors.get(&key.host_port) {
            Some(connect) => connect(),
            None => UnixStream::connect(unix_path_for_port(&self.uds_path, key.host_port)),
        };
        let Ok(stream) = stream else {
            self.push_control(key, op::RST, 0);
            return;
        };
        if stream.set_nonblocking(true).is_err() {
            self.push_control(key, op::RST, 0);
            return;
        }
        let mut c = Conn::new(stream, State::Established);
        c.peer_buf_alloc = h.buf_alloc;
        c.peer_fwd_cnt = h.fwd_cnt;
        self.conns.insert(key, c);
        self.push_control(key, op::RESPONSE, 0);
    }

    /// What the backend thread should poll.
    pub fn poll_set(&self) -> Vec<(PollFd, Token)> {
        let mut v = vec![
            (
                PollFd {
                    fd: self.wake_rx.as_raw_fd(),
                    read: true,
                    write: false,
                },
                Token::Wake,
            ),
            (
                PollFd {
                    fd: self.listener.as_raw_fd(),
                    read: true,
                    write: false,
                },
                Token::Listener,
            ),
        ];
        for (i, a) in self.accepting.iter().enumerate() {
            v.push((
                PollFd {
                    fd: a.stream.as_raw_fd(),
                    read: true,
                    write: false,
                },
                Token::Accepting(i),
            ));
        }
        for (k, c) in &self.conns {
            let read = c.wants_read() && !c.readable;
            let write = !c.out.is_empty();
            if read || write {
                v.push((
                    PollFd {
                        fd: c.stream.as_raw_fd(),
                        read,
                        write,
                    },
                    Token::Conn(*k),
                ));
            }
        }
        v
    }

    /// Handle what `poll` reported. Returns true when something may now be sent to the guest.
    pub fn on_ready(&mut self, ready: &[(Token, Ready)]) -> bool {
        let mut accepting_done = Vec::new();
        for (token, r) in ready {
            match token {
                Token::Wake if r.read => {
                    let mut sink = [0u8; 64];
                    while matches!((&self.wake_rx).read(&mut sink), Ok(n) if n > 0) {}
                }
                Token::Listener if r.read => self.accept(),
                Token::Accepting(i) if r.read || r.error => {
                    if self.read_connect_line(*i) {
                        accepting_done.push(*i);
                    }
                }
                Token::Conn(key) => {
                    if let Some(c) = self.conns.get_mut(key) {
                        if r.read || r.error {
                            c.readable = true;
                        }
                        if r.write || r.error {
                            if !c.flush() {
                                self.reset(*key);
                                continue;
                            }
                            self.maybe_credit_update(*key);
                            self.finish_if_closed(*key);
                        }
                    }
                }
                _ => {}
            }
        }
        accepting_done.sort_unstable();
        for i in accepting_done.into_iter().rev() {
            self.accepting.swap_remove(i);
        }
        !self.control.is_empty() || self.conns.values().any(|c| c.readable && c.wants_read())
    }

    fn accept(&mut self) {
        while let Ok((stream, _)) = self.listener.accept() {
            if self.accepting.len() >= MAX_ACCEPTING || stream.set_nonblocking(true).is_err() {
                continue; // dropped: the host sees the stream close
            }
            self.accepting.push(Accepting {
                stream,
                line: Vec::new(),
            });
        }
    }

    /// Read the `CONNECT` line one byte at a time (so no byte of the connection is consumed);
    /// true when this accepting entry is finished, either way.
    fn read_connect_line(&mut self, i: usize) -> bool {
        let Some(a) = self.accepting.get_mut(i) else {
            return true;
        };
        let mut b = [0u8];
        loop {
            match a.stream.read(&mut b) {
                Ok(0) => return true,
                Ok(_) if b[0] == b'\n' => break,
                Ok(_) if a.line.len() >= CONNECT_LINE_MAX => return true,
                Ok(_) => a.line.push(b[0]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return true,
            }
        }
        let line = String::from_utf8_lossy(&a.line)
            .trim_end_matches('\r')
            .to_string();
        let port = line
            .strip_prefix("CONNECT ")
            .and_then(|p| p.trim().parse::<u32>().ok());
        let Some(port) = port else {
            return true;
        };
        let Ok(stream) = a.stream.try_clone() else {
            return true;
        };
        // A refusal here (too many connections) closes the stream, which the host sees.
        let _ = self.connect_to_guest(stream, port);
        true
    }

    /// Write pending bytes to every host stream that can take them.
    pub fn flush_all(&mut self) {
        let keys: Vec<Key> = self.conns.keys().copied().collect();
        for k in keys {
            if let Some(c) = self.conns.get_mut(&k)
                && !c.flush()
            {
                self.reset(k);
                continue;
            }
            self.maybe_credit_update(k);
            self.finish_if_closed(k);
        }
    }

    /// Fill the guest's receive queue: control packets first, then data from host streams,
    /// round-robin. Returns true when any buffer was used.
    pub fn fill_rx(&mut self, q: &mut Queue, mem: &Mem) -> Result<bool> {
        let mut any = false;
        loop {
            if let Some(h) = self.control.front().copied() {
                let Some(chain) = q.pop_descriptor_chain(mem) else {
                    break;
                };
                let head = chain.head_index();
                let used = match chain.writer(mem) {
                    Ok(mut w) if w.available_bytes() >= HDR_BYTES => {
                        w.write_all(&h.encode()).map_or(0, |_| HDR_BYTES as u32)
                    }
                    _ => 0,
                };
                add_used(q, mem, head, used)?;
                if used > 0 {
                    self.control.pop_front();
                }
                any = true;
                continue;
            }
            let Some(key) = self.next_readable() else {
                break;
            };
            let Some(chain) = q.pop_descriptor_chain(mem) else {
                break;
            };
            let head = chain.head_index();
            let Ok(mut w) = chain.writer(mem) else {
                add_used(q, mem, head, 0)?;
                any = true;
                continue;
            };
            if w.available_bytes() <= HDR_BYTES {
                add_used(q, mem, head, 0)?;
                any = true;
                continue;
            }
            let Some(c) = self.conns.get_mut(&key) else {
                q.go_to_previous_position();
                continue;
            };
            let n = (w.available_bytes() - HDR_BYTES)
                .min(c.peer_credit() as usize)
                .min(MAX_PKT_PAYLOAD);
            self.last_served = Some(key);
            match c.stream.read(&mut self.buf[..n]) {
                Ok(0) => {
                    q.go_to_previous_position();
                    c.host_eof = true;
                    c.readable = false;
                    self.push_control(key, op::SHUTDOWN, SHUTDOWN_SEND);
                }
                Ok(k) => {
                    c.rx_cnt = c.rx_cnt.wrapping_add(k as u32);
                    c.fwd_reported = c.fwd_cnt;
                    let fwd = c.fwd_cnt;
                    let h = self.header(key, op::RW, 0, k as u32, fwd);
                    let ok =
                        w.write_all(&h.encode()).is_ok() && w.write_all(&self.buf[..k]).is_ok();
                    add_used(q, mem, head, if ok { (HDR_BYTES + k) as u32 } else { 0 })?;
                    any = true;
                }
                Err(e)
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted =>
                {
                    q.go_to_previous_position();
                    c.readable = false;
                }
                Err(_) => {
                    q.go_to_previous_position();
                    self.reset(key);
                }
            }
        }
        Ok(any)
    }

    /// The next connection with data to read, after the last one served.
    fn next_readable(&self) -> Option<Key> {
        let ok = |c: &Conn| c.readable && c.wants_read();
        let after = self.last_served.and_then(|k| {
            self.conns
                .range((std::ops::Bound::Excluded(k), std::ops::Bound::Unbounded))
                .find(|(_, c)| ok(c))
                .map(|(k, _)| *k)
        });
        after.or_else(|| self.conns.iter().find(|(_, c)| ok(c)).map(|(k, _)| *k))
    }
}

fn add_used(q: &mut Queue, mem: &Mem, head: u16, len: u32) -> Result<()> {
    q.add_used(mem, head, len)
        .map_err(|e| VmmError::device("virtio-vsock", format!("used ring: {e}")))
}

impl Drop for Muxer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.uds_path);
    }
}
