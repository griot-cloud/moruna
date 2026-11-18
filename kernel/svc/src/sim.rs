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
//! Channel handles carry the channel's full rights. Memory objects additionally
//! model shared backing, subset rights and mapping bounds. Numeric handle values,
//! the kernel's enforcement of an account's limit and DMA are not modeled. Explicit
//! Account objects model charged-owner identity and rights, and count what is charged
//! to them: a memory object created in an account is charged its whole pages until its
//! last handle closes, which `Account::usage` reports as the kernel's `account_info`
//! does. That is never native proof; inert handles exist only for wrong-object tests.

extern crate std;

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
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

/// An account: its limit, and what memory objects created in it hold now and held at most.
struct AccountIdentity {
    limit: u64,
    used: AtomicU64,
    peak: AtomicU64,
}

impl AccountIdentity {
    fn charge(&self, bytes: u64) {
        let used = self.used.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(used, Ordering::Relaxed);
    }
}

struct MemoryState {
    bytes: Mutex<Vec<u8>>,
    owner: Option<Arc<AccountIdentity>>,
}

/// A memory object's last handle closed: its account is charged its pages no longer.
impl Drop for MemoryState {
    fn drop(&mut self) {
        if let Some(owner) = &self.owner {
            let size = self.bytes.get_mut().map_or(0, |b| b.len()) as u64;
            owner.used.fetch_sub(size, Ordering::Relaxed);
        }
    }
}
enum Object {
    Endpoint(Arc<Endpoint>),
    Inert,
    Memory(Arc<MemoryState>, Rights),
    Account(Arc<AccountIdentity>, Rights),
}

/// An owned handle; dropping it closes it.
pub struct Handle(Object);

impl core::fmt::Debug for Handle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.0 {
            Object::Endpoint(e) => write!(f, "Handle(endpoint {})", e.side),
            Object::Inert => f.write_str("Handle(inert)"),
            Object::Memory(_, _) => f.write_str("Handle(memory)"),
            Object::Account(_, _) => f.write_str("Handle(account-model)"),
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
            Object::Memory(_, rights) => (ObjectType::Memory, *rights),
            Object::Account(_, rights) => (ObjectType::Account, *rights),
        })
    }

    /// Duplicate only the requested subset of an actual memory handle's rights.
    pub fn duplicate(&self, rights: Rights) -> Result<Handle> {
        match &self.0 {
            Object::Memory(bytes, have)
                if have.contains(Rights::DUPLICATE) && have.contains(rights) =>
            {
                Ok(Handle(Object::Memory(bytes.clone(), rights)))
            }
            Object::Memory(_, _) => Err(Error::AccessDenied),
            Object::Account(account, have)
                if have.contains(Rights::DUPLICATE) && have.contains(rights) =>
            {
                Ok(Handle(Object::Account(account.clone(), rights)))
            }
            Object::Account(_, _) => Err(Error::AccessDenied),
            // A channel end's duplicate that may only be waited on: it shares the end,
            // which closes once every handle to it has closed, as the kernel's does.
            // The model keeps no rights on a channel's handles, so it makes no other.
            Object::Endpoint(e) if rights == Rights::WAIT => {
                Ok(Handle(Object::Endpoint(e.clone())))
            }
            _ => Err(Error::WrongType),
        }
    }

    fn endpoint(&self) -> Result<&Arc<Endpoint>> {
        match &self.0 {
            Object::Endpoint(e) => Ok(e),
            Object::Inert | Object::Memory(_, _) | Object::Account(_, _) => Err(Error::WrongType),
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
            if !handle.info()?.1.contains(Rights::TRANSFER) {
                return Err(Error::AccessDenied);
            }
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

/// Explicit host identity/rights model only. No native quota, DMA, process or
/// physical allocation proof is provided by this type.
#[derive(Debug)]
pub struct Account(pub Handle);
impl Account {
    pub fn carve(limit: u64, name: &str) -> Result<Self> {
        if limit == 0
            || !limit.is_multiple_of(4096)
            || name.is_empty()
            || name.len() > 32
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        {
            return Err(Error::InvalidArgs);
        }
        Ok(Self(Handle(Object::Account(
            Arc::new(AccountIdentity {
                limit,
                used: AtomicU64::new(0),
                peak: AtomicU64::new(0),
            }),
            Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
        ))))
    }

    /// Its figures, as `moruna_sys::Account::usage` reads them: its limit, what the memory
    /// objects created in it hold now and held at most at once. Its handle needs `READ`.
    pub fn usage(&self) -> Result<Usage> {
        let Object::Account(account, rights) = &self.0.0 else {
            return Err(Error::WrongType);
        };
        if !rights.contains(Rights::READ) {
            return Err(Error::AccessDenied);
        }
        Ok(Usage {
            limit: account.limit,
            used: account.used.load(Ordering::Relaxed),
            peak: account.peak.load(Ordering::Relaxed),
            reserved: 0,
            refused: 0,
            refused_max: 0,
        })
    }
    pub fn matches(&self, other: &Handle) -> Result<bool> {
        let Object::Account(account, rights) = &self.0.0 else {
            return Err(Error::WrongType);
        };
        if !rights.contains(Rights::READ) || !other.info()?.1.contains(Rights::READ) {
            return Err(Error::AccessDenied);
        }
        match &other.0 {
            Object::Account(other, _) => Ok(Arc::ptr_eq(account, other)),
            Object::Memory(memory, _) => Ok(memory
                .owner
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(account, owner))),
            _ => Err(Error::WrongType),
        }
    }
}

/// An account's figures (host model of `moruna_sys::Usage`). No account is carved out of
/// another here, so none is reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// Its limit.
    pub limit: u64,
    /// Charged to it now.
    pub used: u64,
    /// The most ever charged to it at once.
    pub peak: u64,
    /// Held for the accounts carved out of it: none here.
    pub reserved: u64,
    /// The charges it refused (ABI 2.10): none here, where no charge is refused.
    pub refused: u64,
    /// The largest of them, in bytes: none here.
    pub refused_max: u64,
}

impl Usage {
    /// What may still be charged to it.
    pub fn room(&self) -> u64 {
        self.limit
            .saturating_sub(self.used)
            .saturating_sub(self.reserved)
    }
}

/// Host model of the native owned memory object. It models bounds and handle
/// rights, not kernel quota charging or DMA; kernel acceptance proves those.
#[derive(Debug)]
pub struct Memory(pub Handle);
pub struct Mapping<'a> {
    memory: &'a Memory,
    offset: usize,
    len: usize,
    writable: bool,
}
impl Memory {
    pub fn create(size: u64) -> Result<Self> {
        Self::allocate(size, None)
    }
    pub fn create_in(size: u64, handle: &Handle) -> Result<Self> {
        let Object::Account(account, rights) = &handle.0 else {
            return Err(Error::WrongType);
        };
        if !rights.contains(Rights::WRITE) {
            return Err(Error::AccessDenied);
        }
        Self::allocate(size, Some(account.clone()))
    }
    fn allocate(size: u64, owner: Option<Arc<AccountIdentity>>) -> Result<Self> {
        if size == 0 {
            return Err(Error::InvalidArgs);
        }
        if size > limits::MEMORY_MAX_BYTES {
            return Err(Error::Limit);
        }
        let size = usize::try_from(size.checked_add(4095).ok_or(Error::Limit)? / 4096 * 4096)
            .map_err(|_| Error::Limit)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).map_err(|_| Error::NoMemory)?;
        bytes.resize(size, 0);
        if let Some(owner) = &owner {
            owner.charge(size as u64);
        }
        Ok(Self(Handle(Object::Memory(
            Arc::new(MemoryState {
                bytes: Mutex::new(bytes),
                owner,
            }),
            Rights::ALL,
        ))))
    }
    pub fn map(&self, offset: u64, len: u64, flags: u64) -> Result<Mapping<'_>> {
        let Object::Memory(bytes, rights) = &self.0.0 else {
            return Err(Error::WrongType);
        };
        let offset = usize::try_from(offset).map_err(|_| Error::InvalidArgs)?;
        let len = usize::try_from(len).map_err(|_| Error::InvalidArgs)?;
        if !rights.contains(Rights::MAP | Rights::READ)
            || (flags & moruna_abi::map::WRITE != 0 && !rights.contains(Rights::WRITE))
        {
            return Err(Error::AccessDenied);
        }
        if flags & !moruna_abi::map::ALL != 0
            || flags & (moruna_abi::map::WRITE | moruna_abi::map::EXECUTE)
                == (moruna_abi::map::WRITE | moruna_abi::map::EXECUTE)
            || len == 0
            || offset % 4096 != 0
            || len % 4096 != 0
        {
            return Err(Error::InvalidArgs);
        }
        if offset
            .checked_add(len)
            .is_none_or(|end| end > lock(&bytes.bytes).len())
        {
            return Err(Error::OutOfRange);
        }
        Ok(Mapping {
            memory: self,
            offset,
            len,
            writable: flags & moruna_abi::map::WRITE != 0,
        })
    }
}
impl Mapping<'_> {
    pub fn read(&self, offset: usize, out: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(out.len())
            .filter(|end| *end <= self.len)
            .ok_or(Error::InvalidArgs)?;
        let Object::Memory(bytes, _) = &self.memory.0.0 else {
            return Err(Error::WrongType);
        };
        out.copy_from_slice(&lock(&bytes.bytes)[self.offset + offset..self.offset + end]);
        Ok(())
    }
    pub fn write(&self, offset: usize, data: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(Error::AccessDenied);
        }
        let end = offset
            .checked_add(data.len())
            .filter(|end| *end <= self.len)
            .ok_or(Error::InvalidArgs)?;
        let Object::Memory(bytes, _) = &self.memory.0.0 else {
            return Err(Error::WrongType);
        };
        lock(&bytes.bytes)[self.offset + offset..self.offset + end].copy_from_slice(data);
        Ok(())
    }
}

#[cfg(test)]
mod account_memory_model_tests {
    use super::*;
    #[test]
    fn explicit_model_account_identity_survives_loans_and_refuses_foreign_or_unowned_memory() {
        let one = Account::carve(8 << 20, "same").unwrap();
        let two = Account::carve(8 << 20, "same").unwrap();
        assert!(!one.matches(&two.0).unwrap());
        let memory = Memory::create_in(4096, &one.0).unwrap();
        let loan = memory
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .unwrap();
        assert!(one.matches(&loan).unwrap());
        assert!(!two.matches(&loan).unwrap());
        assert!(!one.matches(&Memory::create(4096).unwrap().0).unwrap());
        let no_read = memory
            .0
            .duplicate(Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .unwrap();
        assert_eq!(one.matches(&no_read), Err(Error::AccessDenied));
        let readonly = one.0.duplicate(Rights::READ | Rights::TRANSFER).unwrap();
        assert!(matches!(
            Memory::create_in(4096, &readonly),
            Err(Error::AccessDenied)
        ));
    }

    #[test]
    fn an_account_is_charged_its_memory_objects_pages_until_their_last_handle_closes() {
        let account = Account::carve(8 << 20, "charged").unwrap();
        let fresh = account.usage().unwrap();
        assert_eq!(
            (fresh.limit, fresh.used, fresh.peak, fresh.reserved),
            (8 << 20, 0, 0, 0)
        );
        assert_eq!(fresh.room(), 8 << 20);
        // Whole pages, however few bytes were asked for.
        let one = Memory::create_in(10, &account.0).unwrap();
        let two = Memory::create_in(3 * 4096, &account.0).unwrap();
        assert_eq!(account.usage().unwrap().used, 4 * 4096);
        // Another account's objects, and unowned ones, are not this one's.
        let other = Account::carve(4096, "other").unwrap();
        let theirs = Memory::create_in(4096, &other.0).unwrap();
        let unowned = Memory::create(4096).unwrap();
        assert_eq!(account.usage().unwrap().used, 4 * 4096);
        assert_eq!(other.usage().unwrap().used, 4096);
        // A duplicate keeps the object, and its charge, until it closes too.
        let loan = two.0.duplicate(Rights::READ | Rights::MAP).unwrap();
        drop(two);
        assert_eq!(account.usage().unwrap().used, 4 * 4096);
        drop(loan);
        let after = account.usage().unwrap();
        assert_eq!((after.used, after.peak), (4096, 4 * 4096));
        assert_eq!(after.room(), (8 << 20) - 4096);
        drop((one, theirs, unowned));
        assert_eq!(account.usage().unwrap().used, 0);
        // Reading the figures needs READ; only an account has them.
        let write_only = Account(account.0.duplicate(Rights::WRITE).unwrap());
        assert_eq!(write_only.usage(), Err(Error::AccessDenied));
        let memory = Account(Memory::create(4096).unwrap().0);
        assert_eq!(memory.usage(), Err(Error::WrongType));
    }
}
