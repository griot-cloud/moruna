//! Actual nonblocking submit/poll on the SAME SDK connection and writer lease. Pending
//! owns original DMA Memory + actual Account until ORIGINAL reply. Cancel intent and
//! soft timeout never release pins, return memory, or stand in for device completion.
#[path = "device_reserved.rs"]
pub mod reserved;
use super::{Controller, Device, Error};
use alloc::{collections::BTreeMap, rc::Rc};
use block_protocol::block;
use moruna_svc::{CallError, Encoder, Memory, ObjectType, Rights, Wire};
/// Hard service admission bound, independent of driver queue capacity.
pub const MAX_IN_FLIGHT: usize = 64;
/// One actual pending operation; private fields prevent forging another owner's ticket.
#[derive(Clone)]
pub struct Ticket {
    owner: Rc<core::cell::RefCell<Controller>>,
    id: u64,
    base: u64,
    sectors: u64,
}
/// Cancellation acknowledgement is intent only, NEVER original completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelState {
    /// Original operation still belongs to the controller. Poll its ORIGINAL ticket.
    Pending,
    /// No active original exists (collected or awaiting mailbox collection); no
    /// pending operation was cancelled.
    Collected,
}
/// Exact completed original operation and retained caller ownership.
pub struct Completion {
    /// The original DMA Memory, not a replacement adopted from a peer reply.
    pub memory: Option<Memory>,
    /// Actual completed bytes on success, typed original driver error otherwise.
    pub result: Result<u64, Error>,
    /// Actual flush completed after the write (or this ticket itself was a flush).
    pub stable: bool,
    /// Caller requested cancellation; successful original completion remains truthful.
    pub cancel_requested: bool,
}
/// Actual ticket observation. Quarantined owners stay held, not returned as error memory.
pub enum Poll {
    /// No original completion yet. Timeout is diagnostic, not cancellation/quiescence.
    Pending {
        /// Actual configured soft deadline elapsed.
        soft_timeout: bool,
        /// Actual caller intent.
        cancel_requested: bool,
    },
    /// Original reply decoded and validated; memory may now be returned.
    Complete(Completion),
    /// Ambiguous transport/protocol loss. Memory/account remain quarantined until an
    /// independently proven original reset/drain can retire them; never infer from close.
    Quarantined(Error),
}
#[derive(Clone, Copy)]
enum Phase {
    Read,
    Write,
    WriteThenSync,
    FlushAfterWrite,
    Flush,
}
struct Pending {
    txid: u32,
    phase: Phase,
    original: Option<Memory>,
    account: Option<Rc<moruna_sys::Account>>,
    _budget: Option<Memory>,
    sector: u64,
    count: u32,
    region_base: u64,
    region_sectors: u64,
    bytes: u64,
    cancelled: bool,
    deadline: u64,
    expired: bool,
    quarantine: Option<Error>,
}
/// Controller-owned original-operation map. The parent initializes it once; no second
/// driver connection or lease can be created here. Explicit configure precedes submit.
struct Ready {
    completion: Completion,
    base: u64,
    sectors: u64,
    sector: u64,
    count: u32,
    flush: bool,
}
pub(super) struct State {
    ready: BTreeMap<u64, Ready>,
    next: u64,
    limit: usize,
    pending: BTreeMap<u64, Pending>,
}
impl State {
    pub(super) fn new() -> Self {
        Self {
            ready: BTreeMap::new(),
            next: 1,
            limit: 0,
            pending: BTreeMap::new(),
        }
    }
    /// Parent sync admission/compaction checks THIS state, not a receiver count.
    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty() || !self.ready.is_empty()
    }
}
impl Device {
    /// Explicit bounded admission on the shared physical owner. Reconfiguration while
    /// ANY original or quarantined operation exists refuses; no implicit queue default.
    pub fn configure_async(&self, limit: usize) -> Result<(), Error> {
        if limit == 0 || limit > MAX_IN_FLIGHT {
            return Err(Error::Geometry);
        }
        let mut owner = self.controller.try_borrow_mut().map_err(|_| Error::Busy)?;
        if owner.async_state.has_pending() {
            return Err(Error::Busy);
        }
        owner.async_state.limit = limit;
        Ok(())
    }
    /// Submit a real read immediately via ClientCore.send; no wait/spawn/blocking adapter.
    /// The original Memory must be duplicable DMA backing from the actual supplied Account.
    pub fn submit_read(
        &self,
        sector: u64,
        count: u32,
        memory: Memory,
        account: Rc<moruna_sys::Account>,
    ) -> Result<Ticket, Error> {
        self.submit_data(sector, count, memory, account, Phase::Read)
    }
    /// Submit a real write under the SAME original writer lease.
    pub fn submit_write(
        &self,
        sector: u64,
        count: u32,
        memory: Memory,
        account: Rc<moruna_sys::Account>,
    ) -> Result<Ticket, Error> {
        self.submit_data(sector, count, memory, account, Phase::Write)
    }
    /// Linked write then actual flush. Flush is sent ONLY after original successful write
    /// reply; a failed/malformed write never submits it. Original memory survives both.
    pub fn submit_write_sync(
        &self,
        sector: u64,
        count: u32,
        memory: Memory,
        account: Rc<moruna_sys::Account>,
    ) -> Result<Ticket, Error> {
        self.submit_data(sector, count, memory, account, Phase::WriteThenSync)
    }
    fn submit_data(
        &self,
        sector: u64,
        count: u32,
        memory: Memory,
        account: Rc<moruna_sys::Account>,
        phase: Phase,
    ) -> Result<Ticket, Error> {
        let sector = self.sector(sector, count)?;
        let bytes = u64::from(count)
            .checked_mul(u64::from(self.info.sector_size))
            .ok_or(Error::Geometry)?;
        let (kind, rights) = memory.0.info().map_err(Error::Memory)?;
        let needed = Rights::READ
            | Rights::MAP
            | Rights::TRANSFER
            | Rights::DUPLICATE
            | if matches!(phase, Phase::Read) {
                Rights::WRITE
            } else {
                Rights::READ
            };
        if kind != ObjectType::Memory
            || !rights.contains(needed)
            || !account.matches(&memory.0).map_err(Error::Memory)?
        {
            return Err(Error::Geometry);
        }
        // Validate actual backing before any request; Mapping owns its real Memory view.
        let _mapping = memory
            .map(
                0,
                bytes.checked_add(4095).ok_or(Error::Geometry)? / 4096 * 4096,
                0,
            )
            .map_err(Error::Memory)?;
        drop(_mapping);
        let budget = Memory::create_in(4096, &account.0).map_err(Error::Memory)?;
        let loan_rights = Rights::READ
            | Rights::MAP
            | Rights::TRANSFER
            | if matches!(phase, Phase::Read) {
                Rights::WRITE
            } else {
                Rights::READ
            };
        let loan = Memory(memory.0.duplicate(loan_rights).map_err(Error::Memory)?);
        let mut owner = self.controller.try_borrow_mut().map_err(|_| Error::Busy)?;
        let id = owner.admit_async()?;
        let mut body = Encoder::new();
        let ordinal = if matches!(phase, Phase::Read) {
            2
        } else {
            owner.lease.encode(&mut body).map_err(|_| Error::Geometry)?;
            3
        };
        sector.encode(&mut body).map_err(|_| Error::Geometry)?;
        count.encode(&mut body).map_err(|_| Error::Geometry)?;
        loan.encode(&mut body).map_err(|_| Error::Geometry)?;
        let txid = owner
            .client
            .as_mut()
            .ok_or(Error::Closed)?
            .core()
            .send(ordinal, body)
            .map_err(|error| Error::Driver(error.typed()))?;
        let deadline = moruna_svc::deadline_in(owner.timeout_ns);
        owner.async_state.pending.insert(
            id,
            Pending {
                txid,
                phase,
                original: Some(memory),
                account: Some(account),
                _budget: Some(budget),
                sector,
                count,
                region_base: self.base_sector,
                region_sectors: self.info.sectors,
                bytes,
                cancelled: false,
                deadline,
                expired: false,
                quarantine: None,
            },
        );
        Ok(Ticket {
            owner: self.controller.clone(),
            id,
            base: self.base_sector,
            sectors: self.info.sectors,
        })
    }
    /// Submit the original device flush asynchronously. A real reply is required for
    /// stable=true; enqueueing it does not acknowledge durability.
    pub fn submit_flush(&self) -> Result<Ticket, Error> {
        let mut owner = self.controller.try_borrow_mut().map_err(|_| Error::Busy)?;
        let id = owner.admit_async()?;
        let mut body = Encoder::new();
        owner.lease.encode(&mut body).map_err(|_| Error::Geometry)?;
        let txid = owner
            .client
            .as_mut()
            .ok_or(Error::Closed)?
            .core()
            .send(4, body)
            .map_err(|error| Error::Driver(error.typed()))?;
        let deadline = moruna_svc::deadline_in(owner.timeout_ns);
        owner.async_state.pending.insert(
            id,
            Pending {
                txid,
                phase: Phase::Flush,
                original: None,
                account: None,
                _budget: None,
                sector: 0,
                count: 0,
                region_base: self.base_sector,
                region_sectors: self.info.sectors,
                bytes: 0,
                cancelled: false,
                deadline,
                expired: false,
                quarantine: None,
            },
        );
        Ok(Ticket {
            owner: self.controller.clone(),
            id,
            base: self.base_sector,
            sectors: self.info.sectors,
        })
    }
    fn ticket(&self, ticket: &Ticket) -> Result<(), Error> {
        if !Rc::ptr_eq(&self.controller, &ticket.owner)
            || self.base_sector != ticket.base
            || self.info.sectors != ticket.sectors
        {
            return Err(Error::Geometry);
        }
        Ok(())
    }
    /// Mark cancellation intent. No Memory is returned and no driver request is invented.
    /// Poll still waits for the original read/write/flush completion (not a cancel ack).
    pub fn cancel(&self, ticket: &Ticket) -> Result<CancelState, Error> {
        self.ticket(ticket)?;
        let mut owner = self.controller.try_borrow_mut().map_err(|_| Error::Busy)?;
        match owner.async_state.pending.get_mut(&ticket.id) {
            Some(pending) => {
                pending.cancelled = true;
                Ok(CancelState::Pending)
            }
            None => Ok(CancelState::Collected),
        }
    }
    /// Nonblocking original-completion pump. reply(txid,now) may file other real replies
    /// in ClientCore, but never waits for one, resubmits, or treats timeout as completion.
    pub fn poll(&self, ticket: &Ticket) -> Result<Poll, Error> {
        self.ticket(ticket)?;
        let mut owner = self.controller.try_borrow_mut().map_err(|_| Error::Busy)?;
        if let Some(ready) = owner.async_state.ready.remove(&ticket.id) {
            return Ok(Poll::Complete(ready.completion));
        }
        owner.poll_original(ticket.id)
    }
    /// Actual original/quarantined operations intersecting this logical byte interval.
    /// Global flush covers every region. GC must also retain its namespace Frozen barrier.
    pub fn pending_range(&self, offset: u64, bytes: u64) -> Result<usize, Error> {
        let sector = u64::from(self.info.sector_size);
        if !offset.is_multiple_of(sector)
            || !bytes.is_multiple_of(sector)
            || offset
                .checked_add(bytes)
                .is_none_or(|end| end > self.capacity())
        {
            return Err(Error::Geometry);
        }
        let start = self
            .base_sector
            .checked_add(offset / sector)
            .ok_or(Error::Geometry)?;
        let end = start.checked_add(bytes / sector).ok_or(Error::Geometry)?;
        let owner = self.controller.try_borrow().map_err(|_| Error::Busy)?;
        Ok(owner
            .async_state
            .pending
            .values()
            .filter(|pending| {
                matches!(pending.phase, Phase::Flush | Phase::FlushAfterWrite)
                    || pending.sector < end
                        && pending.sector.saturating_add(u64::from(pending.count)) > start
            })
            .count()
            + owner
                .async_state
                .ready
                .values()
                .filter(|ready| {
                    ready.flush
                        || ready.sector < end
                            && ready.sector.saturating_add(u64::from(ready.count)) > start
                })
                .count())
    }
}
impl Controller {
    fn admit_async(&mut self) -> Result<u64, Error> {
        if self.client.is_none()
            || self
                .async_state
                .pending
                .values()
                .any(|pending| pending.quarantine.is_some())
        {
            return Err(Error::Closed);
        }
        if self.async_state.limit == 0 {
            return Err(Error::Geometry);
        }
        if self.async_state.pending.len() + self.async_state.ready.len() >= self.async_state.limit {
            return Err(Error::Busy);
        }
        let id = self.async_state.next;
        self.async_state.next = id.checked_add(1).ok_or(Error::Geometry)?;
        Ok(id)
    }
    fn quarantine(&mut self, cause: Error) {
        for pending in self.async_state.pending.values_mut() {
            pending.quarantine = Some(cause.clone());
        }
        self.last_error = Some(cause);
        self.client = None;
    }
    fn poll_original(&mut self, id: u64) -> Result<Poll, Error> {
        let Some(pending) = self.async_state.pending.get(&id) else {
            return Err(Error::Geometry);
        };
        if let Some(error) = &pending.quarantine {
            return Ok(Poll::Quarantined(error.clone()));
        }
        let txid = pending.txid;
        let reply = match self.client.as_mut() {
            Some(client) => client.core().reply(txid, moruna_svc::now()),
            None => {
                self.quarantine(Error::Closed);
                return Ok(Poll::Quarantined(Error::Closed));
            }
        };
        let mut frame = match reply {
            Ok(frame) => frame,
            Err(CallError::TimedOut) => {
                let pending = self
                    .async_state
                    .pending
                    .get_mut(&id)
                    .ok_or(Error::Geometry)?;
                if !pending.expired && moruna_svc::now() >= pending.deadline {
                    pending.expired = true;
                    self.soft_timeouts = self.soft_timeouts.saturating_add(1);
                }
                return Ok(Poll::Pending {
                    soft_timeout: pending.expired,
                    cancel_requested: pending.cancelled,
                });
            }
            Err(CallError::Remote(code)) => {
                // Typed original failure is the driver's ORIGINAL completion. Its contract
                // rejects before issue or replies only after original DMA/reset drained.
                let typed = CallError::<u32>::Remote(code).typed::<block::Error>();
                let error = match typed {
                    CallError::Remote(code) => Error::Driver(CallError::Remote(code)),
                    other => {
                        let cause = Error::Driver(other);
                        self.quarantine(cause.clone());
                        return Ok(Poll::Quarantined(cause));
                    }
                };
                self.last_error = Some(error.clone());
                let pending = self
                    .async_state
                    .pending
                    .remove(&id)
                    .ok_or(Error::Geometry)?;
                return Ok(Poll::Complete(Completion {
                    memory: pending.original,
                    result: Err(error),
                    stable: false,
                    cancel_requested: pending.cancelled,
                }));
            }
            Err(error) => {
                let cause = Error::Driver(error.typed());
                self.quarantine(cause.clone());
                return Ok(Poll::Quarantined(cause));
            }
        };
        let (phase, valid) = {
            let pending = self.async_state.pending.get(&id).ok_or(Error::Geometry)?;
            let mut decoder = frame.decoder();
            let valid = match pending.phase {
                Phase::Read | Phase::Write | Phase::WriteThenSync => Memory::decode(&mut decoder)
                    .ok()
                    .and_then(|returned| {
                        let account = pending.account.as_ref()?;
                        let (kind, _) = returned.0.info().ok()?;
                        (kind == ObjectType::Memory
                            && account.matches(&returned.0).ok()?
                            && decoder.finish().is_ok())
                        .then_some(())
                    })
                    .is_some(),
                Phase::Flush | Phase::FlushAfterWrite => {
                    <()>::decode(&mut decoder).is_ok() && decoder.finish().is_ok()
                }
            };
            (pending.phase, valid)
        };
        if !valid {
            let cause = Error::Driver(CallError::Malformed);
            self.quarantine(cause.clone());
            return Ok(Poll::Quarantined(cause));
        }
        if matches!(phase, Phase::WriteThenSync)
            && !self
                .async_state
                .pending
                .get(&id)
                .ok_or(Error::Geometry)?
                .cancelled
        {
            let mut body = Encoder::new();
            self.lease.encode(&mut body).map_err(|_| Error::Geometry)?;
            let sent = self
                .client
                .as_mut()
                .ok_or(Error::Closed)?
                .core()
                .send(4, body);
            match sent {
                Ok(txid) => {
                    let pending = self
                        .async_state
                        .pending
                        .get_mut(&id)
                        .ok_or(Error::Geometry)?;
                    pending.txid = txid;
                    pending.phase = Phase::FlushAfterWrite;
                    pending.deadline = moruna_svc::deadline_in(self.timeout_ns);
                    pending.expired = false;
                    return Ok(Poll::Pending {
                        soft_timeout: false,
                        cancel_requested: false,
                    });
                }
                Err(error) => {
                    // Write is actually complete, but no flush acknowledgement exists. Return
                    // its original buffer with explicit durability failure, never stable=true.
                    let cause = Error::Driver(error.typed());
                    self.last_error = Some(cause.clone());
                    let pending = self
                        .async_state
                        .pending
                        .remove(&id)
                        .ok_or(Error::Geometry)?;
                    return Ok(Poll::Complete(Completion {
                        memory: pending.original,
                        result: Err(cause),
                        stable: false,
                        cancel_requested: pending.cancelled,
                    }));
                }
            }
        }
        let pending = self
            .async_state
            .pending
            .remove(&id)
            .ok_or(Error::Geometry)?;
        Ok(Poll::Complete(Completion {
            memory: pending.original,
            result: Ok(pending.bytes),
            stable: matches!(phase, Phase::Flush | Phase::FlushAfterWrite),
            cancel_requested: pending.cancelled,
        }))
    }
}

impl Drop for State {
    fn drop(&mut self) {
        // Bounded fail-closed ownership quarantine. Without an actual reset/drain
        // certificate no destructor may release original DMA/account guards merely
        // because its receiver or SDK peer disappeared. MAX_IN_FLIGHT bounds this;
        // native process/kernel teardown remains responsible for final device reset.
        for (_, pending) in core::mem::take(&mut self.pending) {
            core::mem::forget(pending);
        }
    }
}

/// One pump observation, correlated to its actual controller-issued ticket id. The id
/// carries no authority; only the owner actor may route its retained completion memory.
pub struct Observed {
    /// Actual controller ticket sequence.
    pub ticket: u64,
    /// Original completion/pending/quarantine fact.
    pub state: Observation,
}
/// Non-consuming owner-pump observation; originals are collected ONLY by Ticket::poll.
pub enum Observation {
    /// Original request still active.
    Pending {
        /// Actual soft deadline.
        soft_timeout: bool,
        /// Actual cancellation intent.
        cancel_requested: bool,
    },
    /// Original reply validated; real completion/memory is held in controller mailbox.
    Ready,
    /// Unknown original completion; guards remain quarantined.
    Quarantined(Error),
}
impl Ticket {
    /// Nonsecret routing identity within this SAME physical owner, never a wire capability.
    pub fn id(&self) -> u64 {
        self.id
    }
}
impl Device {
    /// A WAIT-only view of the actual existing driver channel for the owner's dispatcher.
    /// It cannot read/write protocol frames or establish a second connection/lease.
    pub fn async_wait_handle(&self) -> Result<moruna_svc::Handle, Error> {
        let mut owner = self.controller.try_borrow_mut().map_err(|_| Error::Busy)?;
        owner
            .client
            .as_mut()
            .ok_or(Error::Closed)?
            .core()
            .channel()
            .0
            .duplicate(Rights::WAIT)
            .map_err(Error::Memory)
    }
    /// Non-consumingly pump every pending ORIGINAL issued through this exact region.
    /// Ready memory remains in the controller mailbox until its actual ticket polls. The service
    /// owner runs this even after a receiver/channel dies, retaining its actor/lease until
    /// real completion. Other regions' replies may be filed but memory is never delivered
    /// through this view. No waits, new connections, resubmits or synthetic completions.
    pub fn pump(&self) -> Result<alloc::vec::Vec<Observed>, Error> {
        let mut owner = self.controller.try_borrow_mut().map_err(|_| Error::Busy)?;
        let ids = owner
            .async_state
            .pending
            .iter()
            .filter(|(_, pending)| {
                pending.region_base == self.base_sector
                    && pending.region_sectors == self.info.sectors
            })
            .map(|(id, _)| *id)
            .collect::<alloc::vec::Vec<_>>();
        let mut observations = alloc::vec::Vec::new();
        for id in ids {
            let span = owner
                .async_state
                .pending
                .get(&id)
                .map(|pending| {
                    (
                        pending.region_base,
                        pending.region_sectors,
                        pending.sector,
                        pending.count,
                        matches!(pending.phase, Phase::Flush | Phase::FlushAfterWrite),
                    )
                })
                .ok_or(Error::Geometry)?;
            let state = match owner.poll_original(id)? {
                Poll::Pending {
                    soft_timeout,
                    cancel_requested,
                } => Observation::Pending {
                    soft_timeout,
                    cancel_requested,
                },
                Poll::Quarantined(error) => Observation::Quarantined(error),
                Poll::Complete(completion) => {
                    owner.async_state.ready.insert(
                        id,
                        Ready {
                            completion,
                            base: span.0,
                            sectors: span.1,
                            sector: span.2,
                            count: span.3,
                            flush: span.4,
                        },
                    );
                    Observation::Ready
                }
            };
            observations.push(Observed { ticket: id, state });
        }
        for (id, ready) in &owner.async_state.ready {
            if ready.base == self.base_sector
                && ready.sectors == self.info.sectors
                && !observations
                    .iter()
                    .any(|observation| observation.ticket == *id)
            {
                observations.push(Observed {
                    ticket: *id,
                    state: Observation::Ready,
                });
            }
        }
        Ok(observations)
    }
}
