//! The host protocol's socket on the Moruna kernel (SDD 30 f.2): streams to the embedder through
//! the vsock driver (`moruna.driver.vsock`, SDD 25 d.5), as the reader and writer the host
//! protocol already speaks over a Unix or a vsock socket on Linux.
//!
//! The engine holds one connection to the driver, granted under the slot `vsock`; the driver
//! ties streams and listeners to the connection that opened them. One thread owns it and keeps
//! every request in flight at once: a `recv` the driver answers only when bytes arrive does not
//! hold up a `send` from the writer's thread. Requests reach that thread through a queue and a
//! doorbell channel it waits on beside the driver's, so it sleeps in one kernel `wait` until
//! there is something to send or something answered.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use moruna_svc::{
    Channel, ClientCore, DEADLINE_INFINITE, Decoder, Encoder, Error, ProtocolError, Signals,
    Status, WaitItem, Wire,
    frame::{self, Kind},
};
use vsock_protocol::{CHUNK_MAX, vsock};

/// How long the driver's connection handshake may take.
const HELLO_SECONDS: u64 = 120;
/// How long a write may wait for the peer's credit before the peer is treated as gone, as on
/// Linux (MH 4.3).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a request's reply decodes to.
#[derive(Clone, Copy, Debug)]
enum Shape {
    Unit,
    U32,
    Bytes,
    Accepted,
}

/// A request's answer.
#[derive(Debug)]
enum Answer {
    Unit,
    U32(u32),
    Bytes(Vec<u8>),
    Accepted(u32, u64, u32),
}

/// Why a request failed.
#[derive(Clone, Debug)]
enum Failure {
    /// The driver's own refusal.
    Driver(vsock::Error),
    /// The SDK's: the driver does not take the request as sent.
    Sdk(Status),
    /// The connection to the driver is gone, or never answered.
    Gone(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Driver(e) => write!(f, "the driver refused: {e:?}"),
            Failure::Sdk(s) => write!(f, "the driver does not take the request: {s:?}"),
            Failure::Gone(why) => write!(f, "{why}"),
        }
    }
}

impl Failure {
    fn io(self, what: &str) -> io::Error {
        let kind = match &self {
            Failure::Driver(vsock::Error::Refused) => io::ErrorKind::ConnectionRefused,
            Failure::Driver(vsock::Error::Reset) => io::ErrorKind::ConnectionReset,
            Failure::Driver(vsock::Error::InUse) => io::ErrorKind::AddrInUse,
            Failure::Driver(_) | Failure::Sdk(_) => io::ErrorKind::InvalidInput,
            Failure::Gone(_) => io::ErrorKind::BrokenPipe,
        };
        io::Error::new(kind, format!("{what}: the vsock driver: {self}"))
    }
}

struct Request {
    ordinal: u16,
    body: Encoder,
    shape: Shape,
    reply: Answers,
}

/// The engine's connection to the vsock driver.
pub struct Driver {
    requests: Mutex<mpsc::Sender<Request>>,
    bell: Channel,
}

impl Driver {
    /// The driver on the connection granted under `slot` (SDD 30 d.3): the handshake, then the
    /// thread that owns the connection.
    pub fn open(slot: &str) -> io::Result<Arc<Driver>> {
        let handle = crate::start::take(slot).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "the engine was granted no `{slot}` connection to the vsock driver (it holds \
                     {:?})",
                    crate::start::slots()
                ),
            )
        })?;
        let core = ClientCore::connect(
            Channel(handle),
            vsock::ID,
            moruna_svc::deadline_in(HELLO_SECONDS * 1_000_000_000),
        )
        .map_err(|e| io::Error::other(format!("hello to the vsock driver: {e:?}")))?;
        let channel = core.into_channel();
        let (bell, rung) = Channel::create()
            .map_err(|e| io::Error::other(format!("channel_create: {}", e.name())))?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("moruna-vsock".into())
            .spawn(move || pump(channel, rung, rx))?;
        Ok(Arc::new(Driver {
            requests: Mutex::new(tx),
            bell,
        }))
    }

    /// Sends request `ordinal` and returns where its answer will come.
    fn begin(
        &self,
        ordinal: u16,
        body: Encoder,
        shape: Shape,
    ) -> Result<mpsc::Receiver<Result<Answer, Failure>>, Failure> {
        let (reply, answer) = mpsc::channel();
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(Request {
                ordinal,
                body,
                shape,
                reply,
            })
            .map_err(|_| Failure::Gone("the thread owning the connection has ended".into()))?;
        // A full doorbell already wakes the thread; nothing else can refuse a write to it
        // while it is open.
        match self.bell.write_with(&[1], &mut []) {
            Ok(()) | Err(Error::ShouldWait) => Ok(answer),
            Err(e) => Err(Failure::Gone(format!("the doorbell: {}", e.name()))),
        }
    }

    fn call(&self, ordinal: u16, body: Encoder, shape: Shape) -> Result<Answer, Failure> {
        self.begin(ordinal, body, shape)?
            .recv()
            .map_err(|_| Failure::Gone("the request was dropped unanswered".into()))?
    }

    fn encode(args: impl FnOnce(&mut Encoder) -> Result<(), moruna_svc::WireError>) -> Encoder {
        let mut e = Encoder::new();
        // Every argument here is a number or a chunk within `CHUNK_MAX`: nothing to refuse.
        let _ = args(&mut e);
        e
    }

    /// Accepts streams to `port` on this connection.
    pub fn listen(self: &Arc<Self>, port: u32) -> io::Result<Listener> {
        match self.call(3, Driver::encode(|e| port.encode(e)), Shape::Unit) {
            Ok(_) => Ok(Listener {
                driver: Arc::clone(self),
                port,
            }),
            Err(f) => Err(f.io(&format!("listening on port {port}"))),
        }
    }

    /// A stream to `port` at `cid`.
    pub fn connect(self: &Arc<Self>, cid: u32, port: u32) -> io::Result<Stream> {
        let body = Driver::encode(|e| {
            (cid as u64).encode(e)?;
            port.encode(e)
        });
        match self.call(2, body, Shape::U32) {
            Ok(Answer::U32(stream)) => Ok(Stream::new(Arc::clone(self), stream)),
            Ok(other) => Err(io::Error::other(format!("connect_to answered {other:?}"))),
            Err(f) => Err(f.io(&format!("connecting to cid {cid} port {port}"))),
        }
    }
}

/// A port this connection listens on.
pub struct Listener {
    driver: Arc<Driver>,
    port: u32,
}

impl Listener {
    /// The next stream to the port, and its peer's cid and port.
    pub fn accept(&self) -> io::Result<(Stream, u64, u32)> {
        match self
            .driver
            .call(4, Driver::encode(|e| self.port.encode(e)), Shape::Accepted)
        {
            Ok(Answer::Accepted(stream, cid, port)) => {
                Ok((Stream::new(Arc::clone(&self.driver), stream), cid, port))
            }
            Ok(other) => Err(io::Error::other(format!("accept answered {other:?}"))),
            Err(f) => Err(f.io(&format!("accepting on port {}", self.port))),
        }
    }
}

/// One stream: what the driver has given that a read has not taken yet.
pub struct Stream {
    driver: Arc<Driver>,
    id: u32,
}

impl Stream {
    fn new(driver: Arc<Driver>, id: u32) -> Stream {
        Stream { driver, id }
    }

    /// The stream's three parts, for the threads that each own one: the reader, the writer,
    /// and what closes it, so a thread blocked reading returns.
    pub fn into_parts(self) -> (StreamReader, StreamWriter, Box<dyn Fn() + Send + Sync>) {
        let closer = {
            let driver = Arc::clone(&self.driver);
            let id = self.id;
            Box::new(move || {
                // The answer is not waited for: a close is final either way, and the driver
                // answers a waiting recv of the stream with its end.
                let _ = driver.begin(7, Driver::encode(|e| id.encode(e)), Shape::Unit);
            }) as Box<dyn Fn() + Send + Sync>
        };
        (
            StreamReader {
                driver: Arc::clone(&self.driver),
                id: self.id,
                held: Vec::new(),
                at: 0,
                ended: false,
            },
            StreamWriter {
                driver: self.driver,
                id: self.id,
            },
            closer,
        )
    }
}

/// A stream's inbound half.
pub struct StreamReader {
    driver: Arc<Driver>,
    id: u32,
    held: Vec<u8>,
    at: usize,
    ended: bool,
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.at == self.held.len() {
            if self.ended {
                return Ok(0);
            }
            let body = Driver::encode(|e| {
                self.id.encode(e)?;
                CHUNK_MAX.encode(e)
            });
            match self.driver.call(6, body, Shape::Bytes) {
                Ok(Answer::Bytes(bytes)) if bytes.is_empty() => self.ended = true,
                Ok(Answer::Bytes(bytes)) => {
                    self.held = bytes;
                    self.at = 0;
                }
                Ok(other) => return Err(io::Error::other(format!("recv answered {other:?}"))),
                Err(f) => return Err(f.io("reading the stream")),
            }
        }
        let n = buf.len().min(self.held.len() - self.at);
        buf[..n].copy_from_slice(&self.held[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// A stream's outbound half.
pub struct StreamWriter {
    driver: Arc<Driver>,
    id: u32,
}

impl Write for StreamWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let chunk = &buf[..buf.len().min(CHUNK_MAX as usize)];
        let body = Driver::encode(|e| {
            self.id.encode(e)?;
            chunk.to_vec().encode(e)
        });
        let answer = self
            .driver
            .begin(5, body, Shape::U32)
            .map_err(|f| f.io("writing the stream"))?;
        match answer.recv_timeout(WRITE_TIMEOUT) {
            Ok(Ok(Answer::U32(n))) => Ok(n as usize),
            Ok(Ok(other)) => Err(io::Error::other(format!("send answered {other:?}"))),
            Ok(Err(f)) => Err(f.io("writing the stream")),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the peer took no bytes for {} s (MH 4.3)",
                    WRITE_TIMEOUT.as_secs()
                ),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the connection to the vsock driver is gone",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn decode(shape: Shape, d: &mut Decoder<'_>) -> Result<Answer, moruna_svc::WireError> {
    let answer = match shape {
        Shape::Unit => {
            <()>::decode(d)?;
            Answer::Unit
        }
        Shape::U32 => Answer::U32(u32::decode(d)?),
        Shape::Bytes => Answer::Bytes(Vec::<u8>::decode(d)?),
        Shape::Accepted => {
            let (stream, cid, port) = <(u32, u64, u32)>::decode(d)?;
            Answer::Accepted(stream, cid, port)
        }
    };
    d.finish()?;
    Ok(answer)
}

/// Where a request's answer goes.
type Answers = mpsc::Sender<Result<Answer, Failure>>;

/// The requests sent and not answered, by request id.
type Pending = HashMap<u32, (Shape, Answers)>;

/// The thread that owns the driver's connection.
fn pump(driver: Channel, bell: Channel, requests: mpsc::Receiver<Request>) {
    let mut pending: Pending = HashMap::new();
    let mut next = 1u32;
    // Once every sender has gone the doorbell is closed for good, and only the driver's
    // answers to what is still in flight are waited for.
    let mut bell_open = true;
    let fail_all = |pending: &mut Pending, why: String| {
        for (_, (_, reply)) in pending.drain() {
            let _ = reply.send(Err(Failure::Gone(why.clone())));
        }
    };
    loop {
        let mut senders_gone = false;
        loop {
            match requests.try_recv() {
                Ok(req) => {
                    let txid = next;
                    next = next.checked_add(1).unwrap_or(1);
                    match frame::send(&driver, Kind::Request, req.ordinal, txid, req.body) {
                        Ok(()) => {
                            pending.insert(txid, (req.shape, req.reply));
                        }
                        Err(t) => {
                            let _ = req
                                .reply
                                .send(Err(Failure::Gone(format!("sending the request: {t:?}"))));
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    senders_gone = true;
                    break;
                }
            }
        }
        loop {
            match frame::recv(&driver) {
                Ok(Some(mut f)) => {
                    let Some((shape, reply)) = pending.remove(&f.txid) else {
                        tracing::error!(target: "vsock", txid = f.txid, "the vsock driver answered a request nobody sent");
                        continue;
                    };
                    let answer = match f.kind {
                        Kind::Reply => decode(shape, &mut f.decoder()).map_err(|e| {
                            Failure::Gone(format!("an answer that does not decode: {e:?}"))
                        }),
                        Kind::Failure => {
                            let mut d = f.decoder();
                            match u32::decode(&mut d) {
                                Ok(code) => Err(match Status::from_code(code) {
                                    Some(s) => Failure::Sdk(s),
                                    None => match vsock::Error::from_code(code) {
                                        Some(e) => Failure::Driver(e),
                                        None => {
                                            Failure::Gone(format!("an unknown error code {code}"))
                                        }
                                    },
                                }),
                                Err(e) => Err(Failure::Gone(format!(
                                    "a failure that does not decode: {e:?}"
                                ))),
                            }
                        }
                        other => Err(Failure::Gone(format!("a {other:?} frame for a request"))),
                    };
                    let _ = reply.send(answer);
                }
                Ok(None) => break,
                Err(t) => {
                    fail_all(
                        &mut pending,
                        format!("the vsock driver's connection: {t:?}"),
                    );
                    return;
                }
            }
        }
        if senders_gone && pending.is_empty() {
            return;
        }
        let awaited = Signals::READABLE | Signals::PEER_CLOSED;
        let mut items = [
            WaitItem::new(&driver.0, awaited),
            WaitItem::new(&bell.0, awaited),
        ];
        let waited = if bell_open {
            &mut items[..]
        } else {
            &mut items[..1]
        };
        if let Err(e) = moruna_svc::wait(waited, DEADLINE_INFINITE) {
            fail_all(
                &mut pending,
                format!("waiting on the vsock driver: {}", e.name()),
            );
            return;
        }
        let driver_seen = items[0].observed();
        if driver_seen.contains(Signals::PEER_CLOSED) && !driver_seen.contains(Signals::READABLE) {
            fail_all(
                &mut pending,
                "the vsock driver closed the connection".into(),
            );
            return;
        }
        if bell_open {
            let mut ring = [0u8; 8];
            loop {
                match bell.read(&mut ring, &mut []) {
                    Ok(_) => {}
                    Err(Error::PeerClosed) => {
                        bell_open = false;
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
    }
}
