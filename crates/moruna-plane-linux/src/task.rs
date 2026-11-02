//! A task's side of the data plane on Linux: what a sandboxed program calls `batch_get`,
//! `batch_put` and `port_call` through (SDD 29 d.1, on Linux).
//!
//! The program's plane is descriptor 3, a `SOCK_SEQPACKET` socket the engine made
//! ([`Plane::take`]). The API is `moruna-sys`'s on the kernel: [`Plane`] gives each slot once,
//! as an [`Input`] whose [`Batch`] is the next record mapped read only (the previous one
//! released), or an [`Output`] whose [`Window`] is mapped writable until it is put; [`Port`]
//! calls the engine. A batch is the engine's sealed memfd mapped in place, never a copy; a
//! window's put unmaps it before the engine is told, and the engine seals it before it reads a
//! byte. The calls take the kernel's arguments and give the kernel's errors
//! (`moruna_abi::Error`), so a program and the engine see one plane whichever runs them.
//!
//! The plane may be used from many threads: each slot and the port is one value, and the
//! answers on the one socket are handed to whichever thread waits for them.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use moruna_abi::{Error, plane::request};

use crate::engine::code;
use crate::sys::{self, Access, Map, PAGE};
use crate::wire::{self, Header};

/// The descriptor a task's plane is on.
pub const DESCRIPTOR: i32 = 3;

/// The system calls the plane's three calls make, and nothing else: what a seccomp filter must
/// allow a task that uses this library. An image's filter (`moruna-sandbox`, SDD 19 e.4)
/// allows every one of them already.
pub const SYSCALLS: &[&str] = &[
    "sendmsg",
    "recvmsg",
    "mmap",
    "munmap",
    "close",
    "ppoll",
    "getsockopt",
    "fcntl",
    "futex",
];

/// Who an answer is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Slot(u32),
    Token(u64),
}

struct Message {
    header: Header,
    fd: Option<OwnedFd>,
}

/// What a slot has mapped now.
struct Mapped {
    map: Map,
    /// A window's memfd, kept until its put is answered.
    window: Option<OwnedFd>,
}

#[derive(Default)]
struct State {
    /// A thread is reading the socket.
    reading: bool,
    /// The engine's end is gone.
    closed: bool,
    mailbox: BTreeMap<Key, Message>,
    /// Port calls waiting for their answers.
    calls: BTreeSet<u64>,
    token: u64,
    mapped: BTreeMap<u32, Mapped>,
}

struct Shared {
    socket: OwnedFd,
    state: Mutex<State>,
    wake: Condvar,
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    /// Sends `header` and `body` as one message.
    fn send(&self, header: Header, body: &[u8]) -> Result<(), Error> {
        let mut message = Vec::with_capacity(wire::HEADER + body.len());
        message.extend_from_slice(&header.bytes());
        message.extend_from_slice(body);
        sys::send(self.socket.as_fd(), &message, None, true).map_err(|e| code(&e))
    }

    /// The answer for `key`, reading the socket for every thread while none else does.
    fn answer(&self, key: Key) -> Result<Message, Error> {
        let mut st = lock(&self.state);
        loop {
            if let Some(m) = st.mailbox.remove(&key) {
                if let Key::Token(t) = key {
                    st.calls.remove(&t);
                }
                return Ok(m);
            }
            if st.closed {
                return Err(Error::PeerClosed);
            }
            if st.reading {
                st = self
                    .wake
                    .wait(st)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            }
            st.reading = true;
            drop(st);
            let got = self.receive();
            st = lock(&self.state);
            st.reading = false;
            match got {
                Ok(Some(m)) => {
                    let to = match m.header.kind {
                        wire::REPLY => Key::Token(m.header.a),
                        _ => Key::Slot(m.header.slot),
                    };
                    // An answer for a call no thread waits on is dropped with its memfd.
                    if matches!(to, Key::Slot(_))
                        || matches!(to, Key::Token(t) if st.calls.contains(&t))
                    {
                        st.mailbox.insert(to, m);
                    }
                }
                Ok(None) => {}
                Err(_) => st.closed = true,
            }
            self.wake.notify_all();
        }
    }

    /// One message from the engine; `None` for one that is no answer.
    fn receive(&self) -> Result<Option<Message>, Error> {
        let mut buf = [0u8; wire::HEADER];
        let got = sys::recv(self.socket.as_fd(), &mut buf, true).map_err(|e| code(&e))?;
        if got.len == 0 {
            return match sys::hung_up(self.socket.as_fd()) {
                Ok(false) => Ok(None),
                _ => Err(Error::PeerClosed),
            };
        }
        let Some(header) = Header::read(&buf[..got.len]) else {
            return Ok(None);
        };
        Ok(Some(Message {
            header,
            fd: got.fds.into_iter().next(),
        }))
    }

    fn request(&self, header: Header, body: &[u8], key: Key) -> Result<Message, Error> {
        self.send(header, body)?;
        self.answer(key)
    }

    /// `batch_get` on `slot`.
    fn get(&self, slot: u32) -> Result<Option<Got>, Error> {
        {
            let mut st = lock(&self.state);
            match st.mapped.get(&slot) {
                // An output's window, mapped now, is its answer.
                Some(m) if m.window.is_some() => {
                    return Ok(Some(Got {
                        addr: m.map.addr(),
                        len: m.map.len() as u64,
                        window: true,
                    }));
                }
                // An input's batch leaves the task as the call starts.
                Some(_) => drop(st.mapped.remove(&slot)),
                None => {}
            }
        }
        let m = self.request(Header::new(wire::GET, slot), &[], Key::Slot(slot))?;
        let len = usize::try_from(m.header.b).map_err(|_| Error::OutOfRange)?;
        let (map, window) = match m.header.kind {
            wire::BATCH => {
                let fd = m.fd.ok_or(Error::BadState)?;
                let map =
                    Map::new(fd.as_fd(), m.header.a, len, Access::Read).map_err(|e| code(&e))?;
                (map, None)
            }
            wire::WINDOW => {
                let fd = m.fd.ok_or(Error::BadState)?;
                let map = Map::new(fd.as_fd(), 0, len, Access::Write).map_err(|e| code(&e))?;
                (map, Some(fd))
            }
            wire::END => return Ok(None),
            _ => return Err(m.header.status().unwrap_or(Error::BadState)),
        };
        let got = Got {
            addr: map.addr(),
            len: map.len() as u64,
            window: window.is_some(),
        };
        lock(&self.state)
            .mapped
            .insert(slot, Mapped { map, window });
        Ok(Some(got))
    }

    /// `batch_put` of `len` bytes at `addr` in `slot`'s window.
    fn put(&self, slot: u32, addr: u64, len: u64) -> Result<(), Error> {
        let mut st = lock(&self.state);
        let Some(m) = st.mapped.get(&slot) else {
            drop(st);
            // Nothing mapped: the engine says what is wrong (no such slot, an input, no window).
            let mut header = Header::new(wire::PUT, slot);
            header.a = addr;
            header.b = len;
            let m = self.request(header, &[], Key::Slot(slot));
            return match m {
                Ok(m) if m.header.kind == wire::PUT_DONE => Ok(()),
                Ok(m) => Err(m.header.status().unwrap_or(Error::BadState)),
                Err(e) => Err(e),
            };
        };
        if m.window.is_none() {
            return Err(Error::InvalidArgs);
        }
        if len == 0 || !addr.is_multiple_of(PAGE) {
            return Err(Error::InvalidArgs);
        }
        let base = m.map.addr();
        let size = m.map.len() as u64;
        let offset = addr.checked_sub(base).ok_or(Error::OutOfRange)?;
        if offset.checked_add(len).is_none_or(|end| end > size) {
            return Err(Error::OutOfRange);
        }
        // The window leaves the address space before the engine is told of the record.
        let Some(Mapped { map, window }) = st.mapped.remove(&slot) else {
            return Err(Error::BadState);
        };
        drop(map);
        drop(st);
        let mut header = Header::new(wire::PUT, slot);
        header.a = offset;
        header.b = len;
        let answer = self.request(header, &[], Key::Slot(slot))?;
        if answer.header.kind == wire::PUT_DONE {
            return Ok(());
        }
        let error = answer.header.status().unwrap_or(Error::BadState);
        // Refused before the engine took it: the window is the task's again.
        if let Some(fd) = window {
            let size = usize::try_from(size).map_err(|_| Error::OutOfRange)?;
            let map = Map::new(fd.as_fd(), 0, size, Access::Write).map_err(|e| code(&e))?;
            lock(&self.state).mapped.insert(
                slot,
                Mapped {
                    map,
                    window: Some(fd),
                },
            );
        }
        Err(error)
    }

    /// `port_call` of `bytes` to `port`.
    fn call(&self, port: u64, bytes: &[u8]) -> Result<Option<Map>, Error> {
        if bytes.len() > request::MAX {
            return Err(Error::Limit);
        }
        let token = {
            let mut st = lock(&self.state);
            st.token += 1;
            let t = st.token;
            st.calls.insert(t);
            t
        };
        let mut header = Header::new(wire::CALL, 0);
        header.a = token;
        header.b = port;
        header.c = bytes.len() as u64;
        let answered = self.request(header, bytes, Key::Token(token));
        let m = match answered {
            Ok(m) => m,
            Err(e) => {
                lock(&self.state).calls.remove(&token);
                return Err(e);
            }
        };
        if let Some(e) = m.header.status() {
            return Err(e);
        }
        if m.header.c == 0 {
            return Ok(None);
        }
        let len = usize::try_from(m.header.c).map_err(|_| Error::OutOfRange)?;
        let fd = m.fd.ok_or(Error::BadState)?;
        Map::new(fd.as_fd(), 0, len, Access::Read)
            .map(Some)
            .map_err(|e| code(&e))
    }
}

/// What `batch_get` mapped: where, how long, and whether it is a window (writable) or a
/// batch (read only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Got {
    /// Where it is mapped.
    pub addr: u64,
    /// Its length.
    pub len: u64,
    /// An output's window, rather than an input's batch.
    pub window: bool,
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// A task's plane: its slots by number, each given once, and its port.
pub struct Plane {
    shared: Arc<Shared>,
    slots: AtomicU64,
    port: AtomicBool,
}

impl Plane {
    /// The plane on descriptor 3, the first time it is asked for; `None` after. A process
    /// whose descriptor 3 is not a plane socket has no plane (`BadHandle`).
    pub fn take() -> Result<Option<Plane>, Error> {
        if TAKEN.swap(true, Ordering::AcqRel) {
            return Ok(None);
        }
        let socket = sys::claim(DESCRIPTOR).map_err(|_| Error::BadHandle)?;
        Ok(Some(Plane::new(socket)?))
    }

    /// The plane on `socket`, the task's end of what [`crate::LinuxPlane`] opened: how a
    /// program hands a plane to the code that uses it, and how the engine's own adapters
    /// (the image kind's standard streams) are a task.
    pub fn new(socket: OwnedFd) -> Result<Plane, Error> {
        match sys::socket_type(socket.as_fd()) {
            Ok(libc::SOCK_SEQPACKET) => {}
            _ => return Err(Error::BadHandle),
        }
        Ok(Plane {
            shared: Arc::new(Shared {
                socket,
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
            }),
            slots: AtomicU64::new(0),
            port: AtomicBool::new(false),
        })
    }

    /// Ends the plane from the task's side: every call waiting on it, in any thread, returns
    /// `PeerClosed`, and the engine reads the task's end once every value of it is dropped.
    pub fn close(&self) {
        let mut st = lock(&self.shared.state);
        st.closed = true;
        // A socket already shut down, or whose peer is gone, is closed all the same.
        let _ = sys::shutdown(self.shared.socket.as_fd());
        drop(st);
        self.shared.wake.notify_all();
    }

    fn slot(&self, n: u32) -> bool {
        n < 64 && self.slots.fetch_or(1 << n, Ordering::AcqRel) & (1 << n) == 0
    }

    fn taken(&self, n: u32) -> bool {
        n < 64 && self.slots.load(Ordering::Acquire) & (1 << n) != 0
    }

    /// Input slot `n`, once; `None` when it was taken (the engine says whether the plane has
    /// it).
    pub fn input(&self, n: u32) -> Option<Input<'_>> {
        self.slot(n).then_some(Input {
            plane: self,
            slot: n,
        })
    }

    /// Output slot `n`, once.
    pub fn output(&self, n: u32) -> Option<Output<'_>> {
        self.slot(n).then_some(Output {
            plane: self,
            slot: n,
        })
    }

    /// The port, once.
    pub fn port(&self) -> Option<Port<'_>> {
        (!self.port.swap(true, Ordering::AcqRel)).then_some(Port { plane: self })
    }

    /// `batch_get` on a slot no [`Input`] or [`Output`] holds (`BadState` for one that is
    /// held): the next batch's address and length, or the window's; `None` at the end. The
    /// memory stays mapped until the slot's next `batch_get` or `batch_put`; reading it after
    /// that is the caller's fault.
    pub fn batch_get(&self, slot: u32) -> Result<Option<Got>, Error> {
        if self.taken(slot) {
            return Err(Error::BadState);
        }
        self.shared.get(slot)
    }

    /// `batch_put` of the `len` bytes at `addr` of the window a slot no [`Output`] holds has
    /// mapped.
    pub fn batch_put(&self, slot: u32, addr: u64, len: u64) -> Result<(), Error> {
        if self.taken(slot) {
            return Err(Error::BadState);
        }
        self.shared.put(slot, addr, len)
    }
}

/// An input slot.
pub struct Input<'p> {
    plane: &'p Plane,
    slot: u32,
}

/// A batch mapped read only; it leaves the task at the slot's next `get`.
pub struct Batch<'s> {
    addr: u64,
    len: u64,
    _slot: PhantomData<&'s mut ()>,
}

impl Input<'_> {
    /// The next batch, the previous one released; `None` at the end. Waits for the engine
    /// while there is none.
    pub fn get(&mut self) -> Result<Option<Batch<'_>>, Error> {
        Ok(self.plane.shared.get(self.slot)?.map(|g| Batch {
            addr: g.addr,
            len: g.len,
            _slot: PhantomData,
        }))
    }
}

impl Batch<'_> {
    /// Where it is mapped.
    pub fn addr(&self) -> u64 {
        self.addr
    }

    /// Its length.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether it has no bytes (never: the plane maps at least a page).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The batch's bytes, in place.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the plane mapped `len` bytes at `addr` read only and keeps them mapped until
        // this slot's next `get`, which needs the `&mut Input` this batch borrows (the raw
        // calls refuse a slot an `Input` holds). The memfd is sealed: no one writes them.
        unsafe { std::slice::from_raw_parts(self.addr as *const u8, self.len as usize) }
    }
}

/// An output slot.
pub struct Output<'p> {
    plane: &'p Plane,
    slot: u32,
}

/// A window mapped writable; it leaves the task when it is put.
pub struct Window<'s> {
    plane: &'s Plane,
    slot: u32,
    addr: u64,
    len: u64,
    _slot: PhantomData<&'s mut ()>,
}

impl Output<'_> {
    /// The slot's window: the one mapped now, or the next the engine gives (waiting for it);
    /// `None` once the engine gives no more.
    pub fn window(&mut self) -> Result<Option<Window<'_>>, Error> {
        Ok(self.plane.shared.get(self.slot)?.map(|g| Window {
            plane: self.plane,
            slot: self.slot,
            addr: g.addr,
            len: g.len,
            _slot: PhantomData,
        }))
    }
}

impl Window<'_> {
    /// Where it is mapped.
    pub fn addr(&self) -> u64 {
        self.addr
    }

    /// Its length.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether it has no bytes (never: the plane maps at least a page).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The window's bytes, to write a record into.
    pub fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the plane mapped `len` bytes at `addr` shared and writable and keeps them
        // mapped until `put` consumes this window or the slot's next call, which needs the
        // `&mut Output` it borrows; no other mapping of the memfd is writable.
        unsafe { std::slice::from_raw_parts_mut(self.addr as *mut u8, self.len as usize) }
    }

    /// Hands the engine the record of `len` bytes at `offset` (a page multiple) of the
    /// window. On a refusal the window is still the task's and comes back with the error.
    pub fn put(self, offset: u64, len: u64) -> Result<(), (Error, Self)> {
        let addr = self.addr.saturating_add(offset);
        match self.plane.shared.put(self.slot, addr, len) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Mapped again if the engine refused it after it left: its place may differ.
                let at = lock(&self.plane.shared.state)
                    .mapped
                    .get(&self.slot)
                    .map(|m| m.map.addr());
                let addr = at.unwrap_or(self.addr);
                Err((e, Window { addr, ..self }))
            }
        }
    }
}

/// The task's port.
pub struct Port<'p> {
    plane: &'p Plane,
}

/// A port call's reply, mapped read only while it lives.
pub struct Reply<'c> {
    map: Map,
    _port: PhantomData<&'c mut ()>,
}

impl Port<'_> {
    /// Sends `request` to the engine for `port` and waits for the reply; `None` for an empty
    /// one. At most `moruna_abi::plane::request::MAX` bytes (`Limit` past it).
    pub fn call(&mut self, port: u64, request: &[u8]) -> Result<Option<Reply<'_>>, Error> {
        Ok(self.plane.shared.call(port, request)?.map(|map| Reply {
            map,
            _port: PhantomData,
        }))
    }
}

impl Reply<'_> {
    /// Its length.
    pub fn len(&self) -> u64 {
        self.map.len() as u64
    }

    /// Whether it has no bytes (never: an empty reply is `None`).
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The reply's bytes, in place.
    pub fn bytes(&self) -> &[u8] {
        self.map.bytes()
    }
}
