//! A host model of the kernel's channel ABI (SDD 23, f.7), so the SDK, its dispatch
//! loop and the conformance harness run as ordinary tests on a development machine and
//! in CI's host job, with threads standing in for processes.
//!
//! It models what the SDK uses, with the kernel's rules (SDD 20, f.4): a channel is two
//! endpoints, each with a queue bounded at `limits::CHANNEL_DEPTH` messages of at most
//! `limits::CHANNEL_MAX_BYTES` bytes and `limits::CHANNEL_MAX_HANDLES` handles; a write
//! never blocks (`ShouldWait` on a full queue, `PeerClosed` once the peer has closed);
//! a read takes the whole message or nothing (`BufferTooSmall`); handles move; a
//! channel's own endpoints never travel through it, and the cycle rule holds; closing
//! an endpoint drops what is queued to it. Signals are `READABLE`, `WRITABLE` and
//! `PEER_CLOSED`; `wait` sleeps until one is raised or the deadline passes.
//!
//! What it does not model: rights (every handle carries all of a channel's), handle
//! values (a handle is an owned Rust value), and objects other than channels, except an
//! inert object for tests that need a handle that is not a channel.

extern crate std;

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use std::{
    sync::{Condvar, Mutex, MutexGuard, OnceLock},
    time::Instant,
};

pub use moruna_abi::{DEADLINE_INFINITE, Error, ObjectType, Rights, Signals, limits};

/// The rights every simulated channel handle carries.
const CHANNEL_RIGHTS: Rights = Rights::READ
    .union(Rights::WRITE)
    .union(Rights::DUPLICATE)
    .union(Rights::TRANSFER)
    .union(Rights::WAIT);

pub type Result<T> = core::result::Result<T, Error>;

struct Message {
    bytes: Vec<u8>,
    handles: Vec<Handle>,
    endpoints: usize,
}

struct Inner {
    queues: [VecDeque<Message>; 2],
    open: [bool; 2],
    carried: [usize; 2],
}

struct Pair {
    inner: Mutex<Inner>,
}

/// The world's clock and its change counter: every change of any channel bumps the
/// counter and wakes every waiter, which then looks again.
struct World {
    origin: Instant,
    generation: Mutex<u64>,
    changed: Condvar,
    /// The cycle rule's lock, as the kernel's `TRANSFER`.
    transfer: Mutex<()>,
}

fn world() -> &'static World {
    static WORLD: OnceLock<World> = OnceLock::new();
    WORLD.get_or_init(|| World {
        origin: Instant::now(),
        generation: Mutex::new(0),
        changed: Condvar::new(),
        transfer: Mutex::new(()),
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A test thread that panicked while holding a lock has already failed its test;
    // the state it guarded is still consistent, since every critical section here
    // completes its change before any call that could panic.
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn changed() {
    let w = world();
    *lock(&w.generation) += 1;
    w.changed.notify_all();
}

struct Endpoint {
    pair: Arc<Pair>,
    side: usize,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let dropped = {
            let mut inner = lock(&self.pair.inner);
            inner.open[self.side] = false;
            inner.carried[self.side] = 0;
            core::mem::take(&mut inner.queues[self.side])
        };
        changed();
        drop(dropped);
    }
}

enum Object {
    Endpoint(Arc<Endpoint>),
    Inert,
}

/// An owned handle; dropping it closes it.
pub struct Handle(Object);

impl core::fmt::Debug for Handle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.0 {
            Object::Endpoint(e) => write!(f, "Handle(endpoint {})", e.side),
            Object::Inert => f.write_str("Handle(inert)"),
        }
    }
}

impl Handle {
    /// An object that is not a channel, raising no signal: for tests of a peer that
    /// sends the wrong kind of handle.
    pub fn inert() -> Handle {
        Handle(Object::Inert)
    }

    /// The type of the object and the rights this handle carries.
    pub fn info(&self) -> Result<(ObjectType, Rights)> {
        Ok(match &self.0 {
            Object::Endpoint(_) => (ObjectType::Channel, CHANNEL_RIGHTS),
            Object::Inert => (ObjectType::Memory, Rights::READ | Rights::TRANSFER),
        })
    }

    fn endpoint(&self) -> Result<&Arc<Endpoint>> {
        match &self.0 {
            Object::Endpoint(e) => Ok(e),
            Object::Inert => Err(Error::WrongType),
        }
    }

    fn signals(&self) -> Signals {
        let Object::Endpoint(e) = &self.0 else {
            return Signals::NONE;
        };
        let inner = lock(&e.pair.inner);
        let (side, peer) = (e.side, 1 - e.side);
        let mut set = Signals::NONE;
        if !inner.queues[side].is_empty() {
            set = set | Signals::READABLE;
        }
        if inner.open[peer] && inner.queues[peer].len() < limits::CHANNEL_DEPTH {
            set = set | Signals::WRITABLE;
        }
        if !inner.open[peer] {
            set = set | Signals::PEER_CLOSED;
        }
        set
    }
}

/// One end of a channel.
#[derive(Debug)]
pub struct Channel(pub Handle);

impl Channel {
    pub fn create() -> Result<(Channel, Channel)> {
        let pair = Arc::new(Pair {
            inner: Mutex::new(Inner {
                queues: [VecDeque::new(), VecDeque::new()],
                open: [true, true],
                carried: [0, 0],
            }),
        });
        let end = |side| {
            Channel(Handle(Object::Endpoint(Arc::new(Endpoint {
                pair: pair.clone(),
                side,
            }))))
        };
        Ok((end(0), end(1)))
    }

    /// As `moruna_sys::Channel::write_with`: sends `bytes` and moves every handle in
    /// `handles`; on failure every handle stays where it was.
    pub fn write_with(&self, bytes: &[u8], handles: &mut [Option<Handle>]) -> Result<()> {
        let me = self.0.endpoint()?;
        if bytes.len() > limits::CHANNEL_MAX_BYTES || handles.len() > limits::CHANNEL_MAX_HANDLES {
            return Err(Error::Limit);
        }
        let mut carried = Vec::new();
        for slot in handles.iter() {
            let handle = slot.as_ref().ok_or(Error::InvalidArgs)?;
            if let Object::Endpoint(e) = &handle.0 {
                if Arc::ptr_eq(&e.pair, &me.pair) {
                    return Err(Error::InvalidArgs);
                }
                carried.push(e.clone());
            }
        }
        let w = world();
        let _transfer = (!carried.is_empty()).then(|| lock(&w.transfer));
        // The carried endpoints' queues are read before this channel's lock is taken,
        // so no two channel locks are ever held at once; under `transfer` no endpoint
        // count can grow meanwhile.
        if carried
            .iter()
            .any(|e| lock(&e.pair.inner).carried[e.side] != 0)
        {
            return Err(Error::BadState);
        }
        {
            let mut inner = lock(&me.pair.inner);
            let peer = 1 - me.side;
            if !inner.open[peer] {
                return Err(Error::PeerClosed);
            }
            if inner.queues[peer].len() >= limits::CHANNEL_DEPTH {
                return Err(Error::ShouldWait);
            }
            let moved: Vec<Handle> = handles.iter_mut().filter_map(Option::take).collect();
            inner.carried[peer] += carried.len();
            inner.queues[peer].push_back(Message {
                bytes: bytes.to_vec(),
                handles: moved,
                endpoints: carried.len(),
            });
        }
        changed();
        Ok(())
    }

    /// As `moruna_sys::Channel::read`.
    pub fn read(&self, bytes: &mut [u8], handles: &mut [Option<Handle>]) -> Result<(usize, usize)> {
        let me = self.0.endpoint()?;
        let message = {
            let mut inner = lock(&me.pair.inner);
            let side = me.side;
            let Some(front) = inner.queues[side].front() else {
                return Err(if inner.open[1 - side] {
                    Error::ShouldWait
                } else {
                    Error::PeerClosed
                });
            };
            if front.bytes.len() > bytes.len() || front.handles.len() > handles.len() {
                return Err(Error::BufferTooSmall);
            }
            let message = inner.queues[side].pop_front().ok_or(Error::BadState)?;
            inner.carried[side] -= message.endpoints;
            message
        };
        changed();
        let (nb, nh) = (message.bytes.len(), message.handles.len());
        bytes[..nb].copy_from_slice(&message.bytes);
        for (slot, h) in handles.iter_mut().zip(message.handles) {
            *slot = Some(h);
        }
        Ok((nb, nh))
    }

    /// As `moruna_sys::Channel::needed`: the sizes of the next message, `None` if
    /// nothing is queued. An empty message is consumed and reported as `(0, 0)`.
    pub fn needed(&self) -> Result<Option<(usize, usize)>> {
        let me = self.0.endpoint()?;
        let mut inner = lock(&me.pair.inner);
        let side = me.side;
        match inner.queues[side].front() {
            Some(m) if m.bytes.is_empty() && m.handles.is_empty() => {
                inner.queues[side].pop_front();
                drop(inner);
                changed();
                Ok(Some((0, 0)))
            }
            Some(m) => Ok(Some((m.bytes.len(), m.handles.len()))),
            None if inner.open[1 - side] => Ok(None),
            None => Err(Error::PeerClosed),
        }
    }
}

/// One entry of a [`wait`].
pub struct WaitItem<'a> {
    handle: &'a Handle,
    awaited: Signals,
    observed: Signals,
}

impl<'a> WaitItem<'a> {
    pub fn new(handle: &'a Handle, awaited: Signals) -> WaitItem<'a> {
        WaitItem {
            handle,
            awaited,
            observed: Signals::NONE,
        }
    }

    pub fn observed(&self) -> Signals {
        self.observed
    }
}

/// Nanoseconds since the simulated machine started.
pub fn now() -> u64 {
    u64::try_from(world().origin.elapsed().as_nanos()).unwrap_or(u64::MAX - 1)
}

/// As the kernel's `wait`: returns how many items are satisfied, `TimedOut` once the
/// deadline passes; every item's observed signals are set.
pub fn wait(items: &mut [WaitItem<'_>], deadline: u64) -> Result<usize> {
    if items.is_empty() || items.len() > limits::WAIT_MAX_ITEMS {
        return Err(Error::InvalidArgs);
    }
    let w = world();
    loop {
        let seen = *lock(&w.generation);
        let mut hits = 0;
        for item in items.iter_mut() {
            item.observed = item.handle.signals();
            if item.observed.intersects(item.awaited) {
                hits += 1;
            }
        }
        if hits > 0 {
            return Ok(hits);
        }
        let now = now();
        if deadline != DEADLINE_INFINITE && now >= deadline {
            return Err(Error::TimedOut);
        }
        let generation = lock(&w.generation);
        if *generation != seen {
            continue;
        }
        let left = if deadline == DEADLINE_INFINITE {
            std::time::Duration::from_millis(100)
        } else {
            std::time::Duration::from_nanos(deadline - now)
        };
        drop(w.changed.wait_timeout(generation, left));
    }
}

/// Waits for `signals` on one handle; returns what it observed.
pub fn wait_one(handle: &Handle, signals: Signals, deadline: u64) -> Result<Signals> {
    let mut items = [WaitItem::new(handle, signals)];
    wait(&mut items, deadline)?;
    Ok(items[0].observed())
}
