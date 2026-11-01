//! Private native session admission and real connected-channel registration. The
//! designated authority, not a wire scope/name check, decides delegation to the actual
//! child Account. No public object listener exposes this privileged protocol.
use crate::{
    delegation::Delegator,
    memory::Loan,
    protocol::{admission, object},
    session::{Backend, Bound},
};
use alloc::{boxed::Box, collections::VecDeque, rc::Rc};
use core::cell::{Cell, RefCell};
use moruna_storage_format::{
    auth::Admission,
    wire::{self, Refusal},
};
use moruna_svc::{Channel, Context, Dispatcher, Handle, Memory, ObjectType, PrivateMemory, Rights};

/// Actual configured backend provider. It must open/retain real device/network/key
/// capabilities; role mismatch refuses, and no default or empty backend is supplied.
pub trait Factory {
    /// Obtain this explicitly requested configured role. Missing providers refuse.
    fn open(
        &mut self,
        admission: &Admission,
        account: &Handle,
    ) -> Result<Box<dyn Backend>, Refusal>;
}
pub(crate) struct Permit(Rc<Cell<usize>>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}
/// Bounded owned session queue and lifetime count shared with the process dispatch loop.
/// A bind result is not usable until its real object handshake/session check succeeds.
pub struct Pending {
    sessions: VecDeque<(Channel, Box<dyn moruna_svc::Handler>)>,
    active: Rc<Cell<usize>>,
    limit: usize,
    namespace_owners: alloc::vec::Vec<NamespaceEntry>,
    watches: VecDeque<Box<dyn moruna_svc::Watch>>,
}
struct NamespaceEntry {
    volume: Rc<RefCell<crate::volume::Volume>>,
    registry: Rc<RefCell<crate::namespace::Registry>>,
    owner: Option<Rc<crate::namespace_server::Owner>>,
}
struct OwnedWatch {
    inner: Box<dyn moruna_svc::Watch>,
    _permit: Rc<Permit>,
}
impl moruna_svc::Watch for OwnedWatch {
    fn handle(&self) -> &Handle {
        self.inner.handle()
    }
    fn signals(&self) -> moruna_svc::Signals {
        self.inner.signals()
    }
    fn signaled(&mut self, seen: moruna_svc::Signals, done: &mut moruna_svc::Completions) -> bool {
        self.inner.signaled(seen, done)
    }
}
impl Pending {
    /// Reserve dispatcher capacity for the private admission endpoint and process watches.
    /// Caller supplies the exact remaining connection budget, never an unbounded queue.
    pub fn new(limit: usize) -> Result<Rc<RefCell<Self>>, Refusal> {
        if limit == 0 || limit >= moruna_svc::limits::WAIT_MAX_ITEMS {
            return Err(Refusal::Invalid);
        }
        Ok(Rc::new(RefCell::new(Self {
            sessions: VecDeque::new(),
            active: Rc::new(Cell::new(0)),
            limit,
            namespace_owners: alloc::vec::Vec::new(),
            watches: VecDeque::new(),
        })))
    }
    pub(crate) fn reserve(&self) -> Result<Rc<Permit>, Refusal> {
        if self.active.get() >= self.limit {
            return Err(Refusal::Capacity);
        }
        self.active.set(self.active.get() + 1);
        Ok(Rc::new(Permit(self.active.clone())))
    }
    /// Obtain the sole namespace lease/liveness registry for this exact mounted owner.
    /// Rc identity is internal; no resource name or caller wire value selects ownership.
    pub(crate) fn namespace_registry(
        &mut self,
        volume: Rc<RefCell<crate::volume::Volume>>,
    ) -> Result<Rc<RefCell<crate::namespace::Registry>>, Refusal> {
        if let Some(entry) = self
            .namespace_owners
            .iter()
            .find(|entry| Rc::ptr_eq(&entry.volume, &volume))
        {
            return Ok(entry.registry.clone());
        }
        if self.namespace_owners.len() >= self.limit {
            return Err(Refusal::Capacity);
        }
        self.namespace_owners
            .try_reserve(1)
            .map_err(|_| Refusal::Capacity)?;
        let registry = crate::namespace::Registry::new(volume.clone(), self.limit)?;
        self.namespace_owners.push(NamespaceEntry {
            volume,
            registry: registry.clone(),
            owner: None,
        });
        Ok(registry)
    }
    /// One real driver-watch actor per mounted owner, shared by all namespace channels.
    pub fn namespace_owner(
        &mut self,
        volume: Rc<RefCell<crate::volume::Volume>>,
    ) -> Result<Rc<crate::namespace_server::Owner>, Refusal> {
        let registry = self.namespace_registry(volume.clone())?;
        if let Some(owner) = self
            .namespace_owners
            .iter()
            .find(|entry| Rc::ptr_eq(&entry.volume, &volume))
            .and_then(|entry| entry.owner.clone())
        {
            return Ok(owner);
        }
        let permit = self.reserve()?;
        self.watches.try_reserve(1).map_err(|_| Refusal::Capacity)?;
        let (owner, watch) =
            crate::namespace_server::Owner::new(volume.clone(), registry, self.limit)?;
        let entry = self
            .namespace_owners
            .iter_mut()
            .find(|entry| Rc::ptr_eq(&entry.volume, &volume))
            .ok_or(Refusal::Unavailable)?;
        entry.owner = Some(owner.clone());
        self.watches.push_back(Box::new(OwnedWatch {
            inner: watch,
            _permit: permit,
        }));
        Ok(owner)
    }
    /// Reserve actual handler queue storage before constructing a live asynchronous task.
    pub(crate) fn prepare_enqueue(&mut self) -> Result<(), Refusal> {
        if self.sessions.len() >= self.limit {
            return Err(Refusal::Capacity);
        }
        self.sessions.try_reserve(1).map_err(|_| Refusal::Capacity)
    }
    /// Reserve queue backing before starting a real asynchronous owner operation.
    pub(crate) fn prepare_watch(&mut self) -> Result<(), Refusal> {
        self.watches.try_reserve(1).map_err(|_| Refusal::Capacity)
    }
    /// Register an already resource-admitted real watch; permit lives through actual
    /// completion, independent of the requesting private RPC channel's lifetime.
    pub(crate) fn enqueue_reserved_watch(
        &mut self,
        watch: Box<dyn moruna_svc::Watch>,
        permit: Rc<Permit>,
    ) -> Result<(), Refusal> {
        if self.watches.len() == self.watches.capacity() {
            return Err(Refusal::Capacity);
        }
        self.watches.push_back(Box::new(OwnedWatch {
            inner: watch,
            _permit: permit,
        }));
        Ok(())
    }
    /// Retain an actual child handler before returning its already-connected channel.
    /// Caller owns a real reserve Permit through handler lifetime; no unbounded listeners.
    pub(crate) fn enqueue(
        &mut self,
        channel: Channel,
        handler: Box<dyn moruna_svc::Handler>,
    ) -> Result<(), Refusal> {
        if self.sessions.len() >= self.limit {
            return Err(Refusal::Capacity);
        }
        self.sessions
            .try_reserve(1)
            .map_err(|_| Refusal::Capacity)?;
        self.sessions.push_back((channel, handler));
        Ok(())
    }
    /// Register every already-owned channel before the next dispatch turn. Registration
    /// failure closes that unhandshaken session, releases its actual owners and reports
    /// the real error; remaining queued sessions stay retained until handled/dropped.
    pub fn register(&mut self, dispatcher: &mut Dispatcher) -> Result<(), moruna_svc::Error> {
        while let Some(watch) = self.watches.pop_front() {
            dispatcher.watch(watch)?;
        }
        while let Some((channel, session)) = self.sessions.pop_front() {
            dispatcher.serve(channel, session)?;
        }
        Ok(())
    }
}
/// Private connected admission handler, assembled only from boot/application grants.
pub struct Binder {
    delegator: Delegator,
    factory: Box<dyn Factory>,
    pending: Rc<RefCell<Pending>>,
    body_limit: u64,
}
impl Binder {
    /// Use explicit configured providers and bounded resource admission; no ambient grant.
    pub fn new(
        delegator: Delegator,
        factory: Box<dyn Factory>,
        pending: Rc<RefCell<Pending>>,
        body_limit: u64,
    ) -> Result<Self, Refusal> {
        if body_limit == 0 {
            return Err(Refusal::Invalid);
        }
        Ok(Self {
            delegator,
            factory,
            pending,
            body_limit,
        })
    }
}
fn refusal(code: Refusal) -> admission::Error {
    match code {
        Refusal::Denied => admission::Error::Denied,
        Refusal::Invalid | Refusal::Unsupported => admission::Error::Invalid,
        _ => admission::Error::Unavailable,
    }
}
impl admission::Server for Binder {
    fn bind(
        &mut self,
        _: &mut Context<'_>,
        control: Memory,
        bytes: u32,
        account: Handle,
    ) -> Result<Channel, admission::Error> {
        let (kind, rights) = account.info().map_err(|_| admission::Error::Unavailable)?;
        let required = Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER;
        if kind != ObjectType::Account
            || !rights.contains(required)
            || !(wire::PREFIX_BYTES..=wire::MAX_HEADER).contains(&(bytes as usize))
        {
            return Err(admission::Error::Invalid);
        }
        let permit = self
            .pending
            .try_borrow()
            .map_err(|_| admission::Error::Unavailable)?
            .reserve()
            .map_err(refusal)?;
        // Launcher admission control is copied into fresh Memory under the REAL
        // carved child account. Check immutable charge identity before mapping/parsing.
        let account = moruna_sys::Account(account);
        if !account
            .matches(&control.0)
            .map_err(|_| admission::Error::Unavailable)?
        {
            return Err(admission::Error::Denied);
        }
        let control =
            Loan::new(control, 0, bytes as u64, false).map_err(|_| admission::Error::Invalid)?;
        let mut private = PrivateMemory::create_in(wire::MAX_HEADER as u64, &account.0)
            .map_err(|_| admission::Error::Unavailable)?;
        let admission = private
            .with_bytes(|destination| {
                let result = (|| {
                    control
                        .read(0, &mut destination[..wire::PREFIX_BYTES])
                        .map_err(|_| admission::Error::Invalid)?;
                    if wire::header_bytes(&destination[..wire::PREFIX_BYTES])
                        .map_err(|_| admission::Error::Invalid)?
                        != bytes as usize
                    {
                        return Err(admission::Error::Invalid);
                    }
                    control
                        .read(0, &mut destination[..bytes as usize])
                        .map_err(|_| admission::Error::Invalid)?;
                    Admission::decode(&destination[..bytes as usize])
                        .map_err(|_| admission::Error::Invalid)
                })();
                destination.fill(0);
                result
            })
            .map_err(|_| admission::Error::Unavailable)??;
        let authority = self
            .delegator
            .bind(&admission, &account.0)
            .map_err(refusal)?;
        let backend = self.factory.open(&admission, &account.0).map_err(refusal)?;
        let session = Bound::new(
            account.0,
            admission,
            authority,
            backend,
            self.body_limit,
            permit,
            self.pending.clone(),
        )
        .map_err(refusal)?;
        let (server, client) = Channel::create().map_err(|_| admission::Error::Unavailable)?;
        self.pending
            .try_borrow_mut()
            .map_err(|_| admission::Error::Unavailable)?
            .sessions
            .push_back((server, Box::new(object::Serve(session))));
        Ok(client)
    }
}
