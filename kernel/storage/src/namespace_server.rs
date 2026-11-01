//! Connected namespace actors on the real owner dispatcher. Actual driver-channel
//! signals repoll retained futures; no blocking task is called a parallel async IO.
use crate::{
    admission::Permit,
    authority::Authority,
    memory::Loan,
    namespace::{CancelChannel, Cancellation, Namespace, Registry},
    protocol::namespace as protocol,
    volume::Volume,
};
use alloc::{boxed::Box, rc::Rc, vec::Vec};
use core::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    task::{Context as TaskContext, Poll, Waker},
};
use moruna_storage_format::{
    Scope,
    auth::{Action, Admission, Authorization, SessionRole, Verifier},
    namespace::{self as format, Io},
    wire::{self, Refusal},
};
use moruna_svc::{
    Channel, Completions, Context, Decoder, Encoder, Handle, Handler, Memory, ObjectType, Outcome,
    Pending, PrivateMemory, ProtocolError, ProtocolId, Rights, Signals, Watch, Wire,
};
struct Bound {
    scope: Scope,
    account: Rc<moruna_sys::Account>,
    authority: Rc<Authority>,
    role: SessionRole,
    limits: format::Limits,
    closed: Rc<Cell<bool>>,
    _permit: Rc<Permit>,
}
struct Finished {
    namespace: Option<Namespace>,
    reply: Option<(Pending, Outcome)>,
    fault: Option<Refusal>,
}
struct Task {
    future: Pin<Box<dyn Future<Output = Finished>>>,
    started: bool,
}
enum Job {
    Session {
        pending: Pending,
        reply: Loan,
    },
    Execute {
        pending: Pending,
        request: format::Request,
        payload: Option<Loan>,
        cancel: CancelChannel,
        reply: Loan,
    },
}
struct Actor {
    bound: Rc<Bound>,
    namespace: Option<Namespace>,
    task: Option<Task>,
    queued: Option<Job>,
    fault: Option<Refusal>,
}
/// One bounded actor scheduler for ONE actual mounted Volume/Registry. Its watch is
/// registered exactly once; namespace handlers retain it through actual original IO.
pub struct Owner {
    volume: Rc<RefCell<Volume>>,
    registry: Rc<RefCell<Registry>>,
    actors: RefCell<Vec<Rc<RefCell<Actor>>>>,
    limit: usize,
    role: SessionRole,
    dead: Cell<bool>,
    kick: Channel,
    maintenance: RefCell<Option<MaintenanceTask>>,
}
struct OwnerWatch {
    owner: Rc<Owner>,
    driver: Handle,
    kick: Channel,
    driver_selected: Cell<bool>,
}
impl Owner {
    /// Construct after actual synchronous mounts and explicit configure_async admission.
    /// Returns one real WAIT watch; the caller accounts/registers that dispatcher slot.
    pub fn new(
        volume: Rc<RefCell<Volume>>,
        registry: Rc<RefCell<Registry>>,
        actor_limit: usize,
    ) -> Result<(Rc<Self>, Box<dyn Watch>), Refusal> {
        if actor_limit == 0 || actor_limit >= moruna_svc::limits::WAIT_MAX_ITEMS {
            return Err(Refusal::Invalid);
        }
        let (role, handle) = {
            let raw = volume.try_borrow().map_err(|_| Refusal::Unavailable)?;
            let role = match raw.identity().2 {
                moruna_storage_format::Role::SingleNodeHome => SessionRole::SingleNodeHome,
                moruna_storage_format::Role::CheckpointMetadata => SessionRole::CheckpointMetadata,
                _ => return Err(Refusal::Unsupported),
            };
            (role, raw.async_wait_handle()?)
        };
        let (kick, receiver) = Channel::create().map_err(|_| Refusal::Capacity)?;
        let owner = Rc::new(Self {
            volume,
            registry,
            actors: RefCell::new(Vec::new()),
            limit: actor_limit,
            role,
            dead: Cell::new(false),
            kick,
            maintenance: RefCell::new(None),
        });
        Ok((
            owner.clone(),
            Box::new(OwnerWatch {
                owner,
                driver: handle,
                kick: receiver,
                driver_selected: Cell::new(false),
            }),
        ))
    }
    /// Create an actual child handler with its OWN dispatcher Permit. Scope/Account and
    /// live Authority are retained actual objects from the private parent admission.
    pub(crate) fn bind(
        self: &Rc<Self>,
        open: format::OpenRequest,
        scope: Scope,
        account: Rc<moruna_sys::Account>,
        authority: Rc<Authority>,
        permit: Rc<Permit>,
    ) -> Result<Box<dyn Handler>, Refusal> {
        if self.dead.get() {
            return Err(Refusal::Unavailable);
        }
        open.limits.validate().map_err(|_| Refusal::Invalid)?;
        format::root_object(&open.root).map_err(|_| Refusal::Invalid)?;
        let mut actors = self
            .actors
            .try_borrow_mut()
            .map_err(|_| Refusal::Unavailable)?;
        actors.retain(|actor| {
            let actor = actor.borrow();
            !actor.bound.closed.get() || actor.task.is_some()
        });
        if actors.len() >= self.limit {
            return Err(Refusal::Capacity);
        }
        actors.try_reserve(1).map_err(|_| Refusal::Capacity)?;
        let bound = Rc::new(Bound {
            scope,
            account,
            authority,
            role: self.role,
            limits: open.limits,
            closed: Rc::new(Cell::new(false)),
            _permit: permit,
        });
        let context = bound.clone();
        let volume = self.volume.clone();
        let registry = self.registry.clone();
        let task = Task {
            started: false,
            future: Box::pin(async move {
                match Namespace::open(
                    volume,
                    registry,
                    open.root,
                    context.scope.clone(),
                    context.account.clone(),
                    open.limits,
                    open.create,
                    context.authority.as_ref(),
                )
                .await
                {
                    Ok(namespace) => Finished {
                        namespace: Some(namespace),
                        reply: None,
                        fault: None,
                    },
                    Err(error) => Finished {
                        namespace: None,
                        reply: None,
                        fault: Some(error),
                    },
                }
            }),
        };
        let actor = Rc::new(RefCell::new(Actor {
            bound,
            namespace: None,
            task: Some(task),
            queued: None,
            fault: None,
        }));
        actors.push(actor.clone());
        Ok(Box::new(Server {
            owner: self.clone(),
            actor,
        }))
    }
    fn advance(&self) -> Vec<(Pending, Outcome)> {
        let mut replies = Vec::new();
        let actors = self.actors.borrow().clone();
        // Only ready work is rechecked. A completed original can unblock another actor's
        // initial Volume borrow in this SAME signal turn; no busy self-wake is used.
        for _ in 0..self.limit.saturating_mul(3).saturating_add(1) {
            let mut progress = false;
            let maintenance = self.maintenance.borrow().is_some();
            for actor in &actors {
                let mut state = actor.borrow_mut();
                if state.bound.closed.get() && state.task.is_none() {
                    state.queued = None;
                    state.namespace = None;
                    continue;
                }
                if !maintenance && state.task.is_none() && state.queued.is_some() {
                    if let Some(error) = state.fault {
                        if let Some(job) = state.queued.take() {
                            replies.push((job.pending(), failure(error)));
                        }
                        progress = true;
                        continue;
                    }
                    if let Some(namespace) = state.namespace.take() {
                        let job = state.queued.take().expect("owned queued job");
                        state.task = Some(job.task(namespace, state.bound.clone()));
                        progress = true;
                    }
                }
                let Some(task) = state.task.as_mut() else {
                    continue;
                };
                if !task.started && (maintenance || self.volume.try_borrow_mut().is_err()) {
                    continue;
                }
                task.started = true;
                let mut context = TaskContext::from_waker(Waker::noop());
                if let Poll::Ready(finished) = task.future.as_mut().poll(&mut context) {
                    state.task = None;
                    state.namespace = finished.namespace;
                    state.fault = finished.fault;
                    if let Some(reply) = finished.reply {
                        replies.push(reply);
                    }
                    progress = true;
                }
            }
            let originals = actors.iter().any(|actor| {
                actor
                    .borrow()
                    .task
                    .as_ref()
                    .is_some_and(|task| task.started)
            });
            if !originals {
                let mut maintenance = self.maintenance.borrow_mut();
                if let Some(task) = maintenance.as_mut() {
                    task.started = true;
                    let mut context = TaskContext::from_waker(Waker::noop());
                    if let Poll::Ready(result) = task.future.as_mut().poll(&mut context) {
                        *task.result.borrow_mut() = Some(result);
                        let _ = task.signal.write(&[1], []);
                        *maintenance = None;
                        progress = true;
                    }
                }
            }
            if !progress {
                break;
            }
        }
        self.actors.borrow_mut().retain(|actor| {
            let state = actor.borrow();
            !state.bound.closed.get() || state.task.is_some()
        });
        replies
    }
}
impl Watch for OwnerWatch {
    fn handle(&self) -> &Handle {
        let driving = self.owner.actors.borrow().iter().any(|actor| {
            actor
                .borrow()
                .task
                .as_ref()
                .is_some_and(|task| task.started)
        }) || self
            .owner
            .maintenance
            .borrow()
            .as_ref()
            .is_some_and(|task| task.started);
        self.driver_selected.set(driving);
        if driving { &self.driver } else { &self.kick.0 }
    }
    fn signals(&self) -> Signals {
        Signals::READABLE | Signals::PEER_CLOSED
    }
    fn signaled(&mut self, seen: Signals, done: &mut Completions) -> bool {
        let driver = self.driver_selected.get();
        if driver && seen.contains(Signals::PEER_CLOSED) {
            self.owner.dead.set(true);
        }
        if !driver {
            let mut bytes = [0; 1];
            let mut handles = [];
            while self.kick.read(&mut bytes, &mut handles).is_ok() {}
        }
        for (pending, outcome) in self.owner.advance() {
            done.complete(pending, outcome);
        }
        !(driver && seen.contains(Signals::PEER_CLOSED))
    }
}

struct Server {
    owner: Rc<Owner>,
    actor: Rc<RefCell<Actor>>,
}
fn failure(code: Refusal) -> Outcome {
    let error = match code {
        Refusal::Busy => protocol::Error::Busy,
        Refusal::Denied => protocol::Error::Denied,
        Refusal::Missing => protocol::Error::Missing,
        Refusal::Conflict => protocol::Error::Conflict,
        Refusal::Unavailable => protocol::Error::Unavailable,
        Refusal::Unsupported => protocol::Error::Unsupported,
        Refusal::Capacity => protocol::Error::Capacity,
        Refusal::Integrity | Refusal::Invalid => protocol::Error::BadControl,
    };
    Outcome::Failure(error.code())
}
fn bad_memory() -> Outcome {
    Outcome::Failure(protocol::Error::BadMemory.code())
}
fn origin(bound: &Bound, memory: &Memory) -> Result<(), Refusal> {
    let (kind, _) = memory.0.info().map_err(|_| Refusal::Invalid)?;
    if kind != ObjectType::Memory
        || !bound
            .account
            .matches(&memory.0)
            .map_err(|_| Refusal::Unavailable)?
    {
        return Err(Refusal::Denied);
    }
    Ok(())
}
fn reply_loan(bound: &Bound, memory: Memory, capacity: u32) -> Result<Loan, Refusal> {
    if capacity as usize != wire::MAX_HEADER {
        return Err(Refusal::Invalid);
    }
    origin(bound, &memory)?;
    Loan::new(memory, 0, u64::from(capacity), true).map_err(|_| Refusal::Invalid)
}
impl Handler for Server {
    fn protocol(&self) -> ProtocolId {
        protocol::ID
    }
    fn closed(&mut self) {
        let mut state = self.actor.borrow_mut();
        state.bound.closed.set(true);
        if state.task.as_ref().is_some_and(|task| !task.started) {
            state.task = None;
        }
        if state.task.is_none() {
            state.queued = None;
            state.namespace = None;
        }
        // Other actors' completions belong to the owner's Watch/Context, never drop
        // them here. A started task stays owned until its actual original replies drain.
    }
    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, body: &mut Decoder<'_>) -> Outcome {
        if self.owner.dead.get() {
            return failure(Refusal::Unavailable);
        }
        if self.owner.maintenance.borrow().is_some() {
            return failure(Refusal::Busy);
        }
        {
            let state = self.actor.borrow();
            if state.queued.is_some() {
                return failure(Refusal::Capacity);
            }
            if let Some(error) = state.fault {
                return failure(error);
            }
        }
        let bound = self.actor.borrow().bound.clone();
        let job = match ordinal {
            2 => {
                let parsed = (|| {
                    let reply = Memory::decode(body).map_err(|_| Refusal::Invalid)?;
                    let capacity = u32::decode(body).map_err(|_| Refusal::Invalid)?;
                    body.finish().map_err(|_| Refusal::Invalid)?;
                    reply_loan(&bound, reply, capacity)
                })();
                let reply = match parsed {
                    Ok(reply) => reply,
                    Err(_) => return bad_memory(),
                };
                Job::Session {
                    pending: cx.defer(),
                    reply,
                }
            }
            1 => {
                let parsed = (|| {
                    let control = Memory::decode(body).map_err(|_| Refusal::Invalid)?;
                    let bytes = u32::decode(body).map_err(|_| Refusal::Invalid)? as usize;
                    let payload = Option::<Memory>::decode(body).map_err(|_| Refusal::Invalid)?;
                    let payload_offset = u64::decode(body).map_err(|_| Refusal::Invalid)?;
                    let cancel = Channel::decode(body).map_err(|_| Refusal::Invalid)?;
                    let reply = Memory::decode(body).map_err(|_| Refusal::Invalid)?;
                    let capacity = u32::decode(body).map_err(|_| Refusal::Invalid)?;
                    body.finish().map_err(|_| Refusal::Invalid)?;
                    if !(wire::PREFIX_BYTES..=wire::MAX_HEADER).contains(&bytes) {
                        return Err(Refusal::Invalid);
                    }
                    origin(&bound, &control)?;
                    let input =
                        Loan::new(control, 0, bytes as u64, false).map_err(|_| Refusal::Invalid)?;
                    let mut private =
                        PrivateMemory::create_in(wire::MAX_HEADER as u64, &bound.account.0)
                            .map_err(|_| Refusal::Capacity)?;
                    let request = private
                        .with_bytes(|destination| {
                            input
                                .read(0, &mut destination[..bytes])
                                .map_err(|_| Refusal::Invalid)?;
                            format::Request::decode(&destination[..bytes])
                                .map_err(|_| Refusal::Invalid)
                        })
                        .map_err(|_| Refusal::Unavailable)??;
                    let (length, writable) = request.payload().map_err(|_| Refusal::Invalid)?;
                    if length > bound.limits.transfer_bytes {
                        return Err(Refusal::Capacity);
                    }
                    let payload = match payload {
                        Some(memory) => {
                            origin(&bound, &memory)?;
                            let (_, rights) = memory.0.info().map_err(|_| Refusal::Invalid)?;
                            if !rights.contains(Rights::DUPLICATE) {
                                return Err(Refusal::Invalid);
                            }
                            Some(
                                Loan::new(memory, payload_offset, length, writable)
                                    .map_err(|_| Refusal::Invalid)?,
                            )
                        }
                        None if length == 0 => None,
                        None => return Err(Refusal::Invalid),
                    };
                    let cancel = CancelChannel::new(cancel)?;
                    let reply = reply_loan(&bound, reply, capacity)?;
                    Ok((request, payload, cancel, reply))
                })();
                let (request, payload, cancel, reply) = match parsed {
                    Ok(value) => value,
                    Err(error) => return failure(error),
                };
                Job::Execute {
                    pending: cx.defer(),
                    request,
                    payload,
                    cancel,
                    reply,
                }
            }
            _ => return Outcome::Status(moruna_svc::Status::UnknownMethod),
        };
        self.actor.borrow_mut().queued = Some(job);
        for (pending, outcome) in self.owner.advance() {
            cx.complete(pending, outcome);
        }
        Outcome::Deferred
    }
}
struct Cancel {
    client: CancelChannel,
    closed: Rc<Cell<bool>>,
}
impl Cancellation for Cancel {
    fn cancelled(&self) -> Result<bool, Refusal> {
        if self.closed.get() {
            Ok(true)
        } else {
            self.client.cancelled()
        }
    }
}
impl Job {
    fn pending(self) -> Pending {
        match self {
            Self::Session { pending, .. } | Self::Execute { pending, .. } => pending,
        }
    }
    fn task(self, mut namespace: Namespace, bound: Rc<Bound>) -> Task {
        Task {
            started: false,
            future: Box::pin(async move {
                let (pending, outcome) = match self {
                    Job::Session { pending, reply } => {
                        let result = (|| {
                            bound.authority.verify(
                                &Authorization::native(
                                    &bound.scope,
                                    &namespace.receipt().object,
                                    Action::Read,
                                )?
                                .with_version(&namespace.receipt().version)?,
                            )?;
                            let session = format::Session {
                                admission: Admission {
                                    scope: bound.scope.clone(),
                                    role: bound.role,
                                },
                                receipt: namespace.receipt().clone(),
                                limits: bound.limits,
                            };
                            let mut private =
                                PrivateMemory::create_in(wire::MAX_HEADER as u64, &bound.account.0)
                                    .map_err(|_| Refusal::Capacity)?;
                            let length = private
                                .with_bytes(|bytes| {
                                    let length = session
                                        .encode_into(bytes)
                                        .map_err(|_| Refusal::Capacity)?;
                                    reply
                                        .write(0, &bytes[..length])
                                        .map_err(|_| Refusal::Unavailable)?;
                                    Ok::<_, Refusal>(length)
                                })
                                .map_err(|_| Refusal::Unavailable)??;
                            let mut body = Encoder::new();
                            (length as u32)
                                .encode(&mut body)
                                .map_err(|_| Refusal::Capacity)?;
                            bound
                                .account
                                .0
                                .duplicate(Rights::READ | Rights::TRANSFER)
                                .map_err(|_| Refusal::Unavailable)?
                                .encode(&mut body)
                                .map_err(|_| Refusal::Capacity)?;
                            Ok::<_, Refusal>(Outcome::Reply(body))
                        })();
                        (pending, result.unwrap_or_else(failure))
                    }
                    Job::Execute {
                        pending,
                        request,
                        payload,
                        cancel,
                        reply,
                    } => {
                        let copy = request.clone();
                        let cancel = Cancel {
                            client: cancel,
                            closed: bound.closed.clone(),
                        };
                        let result = namespace
                            .execute(request, payload, &cancel, bound.authority.as_ref())
                            .await;
                        let result = result.and_then(|response| {
                            let mut private =
                                PrivateMemory::create_in(wire::MAX_HEADER as u64, &bound.account.0)
                                    .map_err(|_| Refusal::Capacity)?;
                            let length = private
                                .with_bytes(|bytes| {
                                    let length = response
                                        .encode_into(bytes)
                                        .map_err(|_| Refusal::Capacity)?;
                                    reply
                                        .write(0, &bytes[..length])
                                        .map_err(|_| Refusal::Unavailable)?;
                                    Ok::<_, Refusal>(length)
                                })
                                .map_err(|_| Refusal::Unavailable)??;
                            let mut span = 0;
                            for (step, outcome) in copy.steps.iter().zip(&response.outcomes) {
                                if let format::Command::ReadAt { payload_offset, .. } = step.command
                                {
                                    let count = match outcome {
                                        format::Outcome::Io(
                                            Io::Done(n) | Io::Cancelled(n) | Io::Failed(n, _),
                                        ) => *n,
                                        _ => 0,
                                    };
                                    if count > 0 {
                                        span = span.max(
                                            payload_offset
                                                .checked_add(count)
                                                .ok_or(Refusal::Integrity)?,
                                        );
                                    }
                                }
                            }
                            let mut body = Encoder::new();
                            (length as u32)
                                .encode(&mut body)
                                .map_err(|_| Refusal::Capacity)?;
                            span.encode(&mut body).map_err(|_| Refusal::Capacity)?;
                            Ok(Outcome::Reply(body))
                        });
                        (pending, result.unwrap_or_else(failure))
                    }
                };
                Finished {
                    namespace: Some(namespace),
                    reply: Some((pending, outcome)),
                    fault: None,
                }
            }),
        }
    }
}
struct MaintenanceTask {
    future: Pin<Box<dyn Future<Output = Result<(), Refusal>>>>,
    started: bool,
    result: Rc<RefCell<Option<Result<(), Refusal>>>>,
    signal: Channel,
}
/// Actual maintenance receiver. Dropping it does not cancel/release issued physical IO;
/// OwnerWatch retains the task until original replies and root durability are known.
pub struct Maintenance {
    result: Rc<RefCell<Option<Result<(), Refusal>>>>,
    wake: Channel,
}
impl Maintenance {
    /// Real result-channel wake for the private admin aggregate watcher.
    pub fn wait_handle(&self) -> &Handle {
        &self.wake.0
    }
}
impl Future for Maintenance {
    type Output = Result<(), Refusal>;
    fn poll(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.result.borrow_mut().take() {
            return Poll::Ready(result);
        }
        let mut bytes = [0; 1];
        let mut handles = [];
        match self.wake.read(&mut bytes, &mut handles) {
            Err(moruna_svc::Error::PeerClosed) => Poll::Ready(Err(Refusal::Unavailable)),
            _ => Poll::Pending,
        }
    }
}
impl Owner {
    /// Queue privileged maintenance with the ACTUAL service Account and configured
    /// ceilings. No tenant account fallback, role selector or client GC root list exists.
    /// A real kick channel starts idle work; existing original tasks drain first.
    pub(crate) fn request_maintenance(
        self: &Rc<Self>,
        account: Rc<moruna_sys::Account>,
        limits: format::Limits,
        max_objects: usize,
        max_roots: usize,
    ) -> Result<Maintenance, Refusal> {
        limits.validate().map_err(|_| Refusal::Invalid)?;
        if self.dead.get() {
            return Err(Refusal::Unavailable);
        }
        if self.maintenance.borrow().is_some() {
            return Err(Refusal::Busy);
        }
        let (signal, wake) = Channel::create().map_err(|_| Refusal::Capacity)?;
        let result = Rc::new(RefCell::new(None));
        let volume = self.volume.clone();
        let registry = self.registry.clone();
        let future = Box::pin(async move {
            // Actual copy/receipt/index/DMA and set/traversal capacity BEFORE freezing.
            let resources = volume
                .try_borrow()
                .map_err(|_| Refusal::Busy)?
                .prepare_compaction(&account.0)?;
            let mut live = crate::namespace::LiveSet::prepare(
                volume.clone(),
                account,
                limits,
                max_objects,
                max_roots,
            )?;
            let frozen = Registry::freeze(&registry)?;
            live.collect(frozen).await?;
            volume
                .try_borrow_mut()
                .map_err(|_| Refusal::Busy)?
                .compact_namespaces_async(&live, resources)
                .await
        });
        *self.maintenance.borrow_mut() = Some(MaintenanceTask {
            future,
            started: false,
            result: result.clone(),
            signal,
        });
        if let Err((error, _)) = self.kick.write(&[1], []) {
            if error != moruna_svc::Error::ShouldWait {
                self.maintenance.borrow_mut().take();
                return Err(Refusal::Unavailable);
            }
        }
        Ok(Maintenance { result, wake })
    }
}
