//! `moruna.net` for a program's threads (system design 3D, item 3.4): the sockets the engine's
//! object-store connector (`moruna-storage`'s `moruna-net`) opens through the job's network on a
//! node.
//!
//! The connector moves each connection's bytes on two threads at once, one waiting to receive
//! while the other sends, and closes the connection from either. A `moruna.net` client's calls
//! each wait for their reply, and a socket is its session's alone. So each [`Socket`] has a
//! session of its own, a connection to the job's channel, in which:
//!
//! - any thread sends its request, under the session's lock only while it sends;
//! - one reader thread waits on the session's channel, holding no lock while it waits, files
//!   each reply as it arrives (`ClientCore::file`), and wakes the threads waiting, which take
//!   their own (`ClientCore::take_reply`): a condition variable, never a poll;
//! - [`Socket::close`] closes the socket: every request still waiting on it is answered
//!   `Closed`. Dropping the socket closes it and ends its reader and its session.
//!
//! Each socket lends the network server memory of the job's account, one loan each way, as
//! `moruna.net` requires. A refusal of the job's policy is
//! [`io::ErrorKind::PermissionDenied`], and a failed TLS handshake
//! [`io::ErrorKind::InvalidData`], as the connector expects.

use std::{
    io,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use moruna_svc::{
    Account, CallError, Channel, DEADLINE_INFINITE, Encoder, Handle, Memory, Rights, Signals,
    WaitItem, Wire, frame::Frame, map, now, wait,
};
use netstack_protocol::{Target, net};

/// The most bytes one send or receive moves: the memory lent to the network server each way.
pub const LOAN_BYTES: usize = 64 * 1024;

/// How long opening a connection may take: the name resolved and the TCP handshake done.
pub const OPEN_NS: u64 = 30_000_000_000;

// The `moruna.net` requests a socket sends through its reader (netstack-protocol's ordinals).
const SEND_FROM: u16 = 5;
const RECEIVE_INTO: u16 = 6;
const CLOSE: u16 = 8;
const START_TLS: u16 = 16;

/// A failure of the network server's, as the connector reads it.
fn failure(error: CallError<net::Error>) -> io::Error {
    use io::ErrorKind::*;
    let (kind, what) = match error {
        CallError::Remote(net::Error::NotPermitted) => {
            (PermissionDenied, "the job's network does not permit it")
        }
        CallError::Remote(net::Error::TlsFailed) => (
            InvalidData,
            "TLS failed: the peer's certificate is not verified for its name",
        ),
        CallError::Remote(net::Error::TlsUnavailable) => (
            InvalidData,
            "TLS is unavailable: the network server has no roots or the wall clock is unset",
        ),
        CallError::Remote(net::Error::Refused) => (ConnectionRefused, "the peer refused it"),
        CallError::Remote(net::Error::TimedOut) => (TimedOut, "the connection timed out"),
        CallError::Remote(net::Error::Reset) => (ConnectionReset, "the peer reset it"),
        CallError::Remote(net::Error::NoRoute) => (HostUnreachable, "no route to the host"),
        CallError::Remote(net::Error::NotFound) => (NotFound, "the name was not found"),
        CallError::Remote(net::Error::Closed) => (NotConnected, "the socket is closed"),
        CallError::TimedOut => (TimedOut, "the network server did not answer in time"),
        other => return io::Error::other(format!("the network server answered {other:?}")),
    };
    io::Error::new(kind, what)
}

/// A kernel refusal, in words.
fn kernel(error: moruna_svc::Error) -> io::Error {
    io::Error::other(format!("the kernel refused: {}", error.name()))
}

/// The error of a session whose channel has ended.
fn ended() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "the network server's session has ended",
    )
}

/// The job's network: the channel node-admission minted for the job, and the account its
/// sockets' memory is charged to.
#[derive(Debug)]
pub struct Network {
    listener: Channel,
    account: Account,
}

impl Network {
    /// The job's network on `listener`, its sockets charged to `account`.
    pub fn new(listener: Channel, account: Account) -> Network {
        Network { listener, account }
    }

    /// A TCP connection to `host` at `port`, by name: the network server resolves it and checks
    /// it against the job's policy. On a session of its own, with its own reader.
    pub fn open(&self, host: &str, port: u16) -> io::Result<Socket> {
        let deadline = now().saturating_add(OPEN_NS);
        let channel = moruna_svc::connect(&self.listener).map_err(|e| failure(e.typed()))?;
        let account = self
            .account
            .0
            .info()
            .and_then(|(_, rights)| self.account.0.duplicate(rights))
            .map(Account)
            .map_err(kernel)?;
        let mut client = net::Client::connect(channel, deadline, account).map_err(failure)?;
        client.core().set_timeout(OPEN_NS);
        let opened = client
            .open(Target::Name(host.into(), port), None)
            .map_err(failure)?;
        let loan = |client: &net::Client| {
            Memory::create_in(LOAN_BYTES as u64, &client.account().0).map_err(kernel)
        };
        let (send_loan, receive_loan) = (loan(&client)?, loan(&client)?);
        let readable = client
            .core()
            .channel()
            .0
            .duplicate(Rights::WAIT)
            .map_err(kernel)?;
        let (wake, woken) = Channel::create().map_err(kernel)?;
        let session = Arc::new(Session {
            state: Mutex::new(State {
                client: Some(client),
                ended: false,
            }),
            filed: Condvar::new(),
        });
        let reader = {
            let session = Arc::clone(&session);
            thread::Builder::new()
                .name(format!("moruna-net {host}:{port}"))
                .spawn(move || session.read(readable, woken))
                .map_err(|e| io::Error::other(format!("no reader thread: {e}")))?
        };
        Ok(Socket {
            session,
            socket: opened.socket,
            send_loan: Mutex::new(send_loan),
            receive_loan: Mutex::new(receive_loan),
            closed: AtomicBool::new(false),
            wake: Some(wake),
            reader: Some(reader),
        })
    }
}

/// One socket's session: its client, behind the lock its threads send under, and the condition
/// its reader wakes them with.
struct Session {
    state: Mutex<State>,
    filed: Condvar,
}

struct State {
    /// The session's client; `None` once the socket is gone.
    client: Option<net::Client>,
    /// The session's channel has ended: no reply is still to come.
    ended: bool,
}

impl Session {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The reader: waits on the session's channel, holding no lock while it waits, files every
    /// frame that has arrived, and wakes the threads waiting for theirs, until the channel ends
    /// or the socket is dropped (`woken`'s other end closes).
    fn read(&self, readable: Handle, woken: Channel) {
        loop {
            let watched = Signals::READABLE | Signals::PEER_CLOSED;
            let mut items = [
                WaitItem::new(&readable, watched),
                WaitItem::new(&woken.0, watched),
            ];
            let waited = wait(&mut items, DEADLINE_INFINITE);
            if items[1].observed().intersects(watched) {
                return;
            }
            let mut state = self.state();
            let ended = waited.is_err()
                || match state.client.as_mut() {
                    None => true,
                    Some(client) => loop {
                        match client.core().file(now()) {
                            Ok(()) => continue,
                            Err(CallError::TimedOut) => break false,
                            Err(_) => break true,
                        }
                    },
                };
            state.ended |= ended;
            drop(state);
            self.filed.notify_all();
            if ended {
                return;
            }
        }
    }

    /// Sends request `ordinal` with `body` and waits for its reply, which the reader files.
    fn call(&self, ordinal: u16, body: Encoder) -> io::Result<Frame> {
        let mut state = self.state();
        let txid = state
            .client
            .as_mut()
            .ok_or_else(ended)?
            .core()
            .send(ordinal, body)
            .map_err(|e| failure(e.typed()))?;
        loop {
            if let Some(answer) = state
                .client
                .as_mut()
                .and_then(|client| client.core().take_reply(txid))
            {
                return answer.map_err(|e| failure(e.typed()));
            }
            if state.ended || state.client.is_none() {
                return Err(ended());
            }
            state = self
                .filed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The `u32` a reply holds.
    fn count(mut frame: Frame) -> io::Result<u32> {
        let mut d = frame.decoder();
        let count = u32::decode(&mut d).map_err(|_| malformed())?;
        d.finish().map_err(|_| malformed())?;
        Ok(count)
    }
}

fn malformed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the network server's reply is malformed",
    )
}

/// One TCP connection through the job's network. Its methods may be called from several
/// threads at once; dropping it closes it.
pub struct Socket {
    session: Arc<Session>,
    socket: u32,
    send_loan: Mutex<Memory>,
    receive_loan: Mutex<Memory>,
    closed: AtomicBool,
    /// Dropped to end the reader.
    wake: Option<Channel>,
    reader: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Socket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Socket")
            .field("socket", &self.socket)
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}

/// `memory` lent for one call, with `rights`.
fn lend(memory: &Memory, rights: Rights) -> io::Result<Memory> {
    memory.0.duplicate(rights).map(Memory).map_err(kernel)
}

impl Socket {
    /// Upgrades the connection, before any byte has moved, to TLS verified for `server_name`
    /// under the network server's roots and the node's wall clock. Never a plaintext fallback.
    pub fn start_tls(&self, server_name: &str) -> io::Result<()> {
        let mut e = Encoder::new();
        self.socket.encode(&mut e).map_err(|_| malformed())?;
        String::from(server_name)
            .encode(&mut e)
            .map_err(|_| malformed())?;
        let mut reply = self.session.call(START_TLS, e)?;
        let mut d = reply.decoder();
        <()>::decode(&mut d).map_err(|_| malformed())?;
        d.finish().map_err(|_| malformed())
    }

    /// Queues bytes to send, waiting for room: how many were taken, at least one, at most
    /// [`LOAN_BYTES`].
    pub fn send(&self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(LOAN_BYTES);
        let loan = self
            .send_loan
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loan.map(0, LOAN_BYTES as u64, map::WRITE)
            .and_then(|mapping| mapping.write(0, &bytes[..count]))
            .map_err(kernel)?;
        let mut e = Encoder::new();
        self.socket.encode(&mut e).map_err(|_| malformed())?;
        lend(&loan, Rights::READ | Rights::MAP | Rights::TRANSFER)?
            .encode(&mut e)
            .map_err(|_| malformed())?;
        0u64.encode(&mut e).map_err(|_| malformed())?;
        (count as u32).encode(&mut e).map_err(|_| malformed())?;
        let taken = Session::count(self.session.call(SEND_FROM, e)?)? as usize;
        if taken > count {
            return Err(malformed());
        }
        Ok(taken)
    }

    /// Bytes received into `buffer`, waiting for them: how many, zero only at the end of the
    /// stream, at most [`LOAN_BYTES`].
    pub fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let most = buffer.len().min(LOAN_BYTES);
        let loan = self
            .receive_loan
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut e = Encoder::new();
        self.socket.encode(&mut e).map_err(|_| malformed())?;
        lend(
            &loan,
            Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER,
        )?
        .encode(&mut e)
        .map_err(|_| malformed())?;
        0u64.encode(&mut e).map_err(|_| malformed())?;
        (most as u32).encode(&mut e).map_err(|_| malformed())?;
        let count = Session::count(self.session.call(RECEIVE_INTO, e)?)? as usize;
        if count > most {
            return Err(malformed());
        }
        loan.map(0, LOAN_BYTES as u64, map::READ)
            .and_then(|mapping| mapping.read(0, &mut buffer[..count]))
            .map_err(kernel)?;
        Ok(count)
    }

    /// Closes the connection both ways, now: a send or receive waiting on another thread is
    /// answered `Closed`, and nothing more moves. Closing again does nothing.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut e = Encoder::new();
        if self.socket.encode(&mut e).is_ok() {
            let _ = self.session.call(CLOSE, e);
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.close();
        // The reader ends when its wake channel's other end closes, and the session with it.
        drop(self.wake.take());
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.session.state().client = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_network_server_s_answers_are_the_errors_the_connector_reads() {
        use io::ErrorKind::*;
        for (error, kind) in [
            (net::Error::NotPermitted, PermissionDenied),
            (net::Error::TlsFailed, InvalidData),
            (net::Error::TlsUnavailable, InvalidData),
            (net::Error::Refused, ConnectionRefused),
            (net::Error::TimedOut, TimedOut),
            (net::Error::Reset, ConnectionReset),
            (net::Error::NoRoute, HostUnreachable),
            (net::Error::NotFound, NotFound),
            (net::Error::Closed, NotConnected),
            (net::Error::TooMany, Other),
        ] {
            assert_eq!(failure(CallError::Remote(error)).kind(), kind, "{error:?}");
        }
        assert_eq!(failure(CallError::TimedOut).kind(), TimedOut);
        assert_eq!(ended().kind(), NotConnected);
        assert_eq!(malformed().kind(), InvalidData);
        let refused = kernel(moruna_svc::Error::InvalidArgs);
        assert_eq!(refused.kind(), Other);
        assert!(refused.to_string().starts_with("the kernel refused: "));
    }
}
