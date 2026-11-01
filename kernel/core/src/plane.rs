//! The data plane (SDD 29): slots, a process's plane, and its port calls.
//!
//! A slot is where batches pass between the engine and one task, one at a time, as
//! pages and never as copies. The engine pushes memory objects into it: for an input
//! slot each is a batch, one page-aligned Arrow IPC record (contracts e.7), which the
//! task maps read only with `batch_get`; for an output slot each is a window the task
//! maps writable with `batch_get`, writes one record into and hands back with
//! `batch_put`, at which the window leaves the task's address space before the engine
//! is told. Moving a batch is a change of page tables: the frames the producer wrote
//! are the frames the consumer reads (DP-I1).
//!
//! The kernel does not read the records. The engine checks each record it takes from
//! an output slot before anything else of it sees the bytes, on pages no task can
//! still write (DP-I3), and ends the task with the reason when the check refuses it
//! (`slot_refuse`).
//!
//! A port call is a request to the engine on the task's port channel, answered on the
//! forward's rendezvous (`foreign::exchange`): the thread waits for the reply with its
//! token, and the reply's memory object is mapped into the task read only.
//!
//! Lock order: a process's handle table, then a slot's state, then an address space's
//! areas and a signaller (both leaves).

use alloc::{collections::BTreeSet, collections::VecDeque, format, string::String, sync::Arc};
use alloc::{sync::Weak, vec::Vec};

use moruna_abi::{
    Error, PAGE_SIZE, Rights, Signals, charge, limits,
    plane::{reply, request, slot_event, slot_flags},
    read_u64, write_u32, write_u64,
};
use ostd::early_println;
use ostd::sync::{SpinLock, Waiter};

use crate::{
    account::{Account, Charge},
    channel::{Endpoint, Message},
    foreign,
    memory::MemoryObject,
    process::{Exit, Process, Thread},
    signal::Signaller,
    vm,
};

/// A memory object a slot holds: pushed by the engine, mapped by the task, or given
/// back.
struct Entry {
    memory: Arc<MemoryObject>,
    /// The rights of the handle it was pushed through, which it carries back.
    rights: Rights,
    /// Where the batch or the window starts in the object, a page multiple.
    offset: u64,
    /// The batch's length, or the window's (whole pages).
    len: u64,
    cookie: u64,
}

/// The entry a task has mapped, and where.
struct Mapped {
    /// The process whose address space holds the mapping: the task, or the server
    /// that called `plane_get` for it.
    mapper: Weak<Process>,
    addr: u64,
    /// The bytes mapped: `len` rounded up to whole pages.
    span: u64,
    entry: Entry,
}

/// What a slot gives back to the engine.
enum Event {
    Released(u64),
    Put { entry: Entry, offset: u64, len: u64 },
    Returned(Entry),
}

struct State {
    ready: VecDeque<Entry>,
    current: Option<Mapped>,
    events: VecDeque<Event>,
    /// The engine pushes no more.
    closed: bool,
    attached: bool,
    /// Its task has ended.
    ended: bool,
    task: Weak<Process>,
    /// Its number in its task's plane.
    index: usize,
}

impl State {
    /// What the slot holds for the engine: every entry pushed and not yet given back by
    /// an event the engine took. Each entry becomes exactly one event, so bounding this
    /// by `SLOT_DEPTH` bounds both queues by what they reserved.
    fn in_flight(&self) -> usize {
        self.ready.len() + usize::from(self.current.is_some()) + self.events.len()
    }
}

pub struct Slot {
    output: bool,
    state: SpinLock<State>,
    /// The engine's signals: `READABLE` while an event waits, `WRITABLE` while a push
    /// would be taken, `PEER_CLOSED` once the task has ended, and `SIGNALED` while the
    /// task's `batch_get` would not wait (ABI 2.2): what a server that serves the task's
    /// descriptors waits on, so it never waits inside a request (SDD 30 f.10).
    engine: Signaller,
    /// The task's: `READABLE` while `batch_get` would not wait.
    task: Signaller,
    /// The slot and both its queues, reserved whole when it is made, charged to the
    /// engine's account. What it holds is charged to whoever made it (SDD 20 f.18).
    _charge: Charge,
}

// A slot and both its queues at their depth, reserved when it is made.
const _: () = assert!(
    (core::mem::size_of::<Slot>()
        + core::mem::size_of::<SlotEnd>()
        + limits::SLOT_DEPTH * (core::mem::size_of::<Entry>() + core::mem::size_of::<Event>()))
        as u64
        <= charge::SLOT
);

/// The engine's end of a slot, the object its handles name. When the last one closes
/// the slot is closed: the task's `batch_get` answers the end once its queue is taken.
pub struct SlotEnd(pub Arc<Slot>);

impl Drop for SlotEnd {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl Slot {
    /// A slot of `flags` (`slot_flags::INPUT` or `OUTPUT`), charged to `account`, its
    /// queues reserved whole so that nothing it does later allocates.
    pub fn new(flags: u64, account: &Arc<Account>) -> Result<Slot, Error> {
        let output = match flags {
            slot_flags::INPUT => false,
            slot_flags::OUTPUT => true,
            _ => return Err(Error::InvalidArgs),
        };
        let charge = Charge::new(account, charge::SLOT)?;
        let mut ready = VecDeque::new();
        let mut events = VecDeque::new();
        ready
            .try_reserve_exact(limits::SLOT_DEPTH)
            .map_err(|_| Error::NoMemory)?;
        events
            .try_reserve_exact(limits::SLOT_DEPTH)
            .map_err(|_| Error::NoMemory)?;
        Ok(Slot {
            output,
            state: SpinLock::new(State {
                ready,
                current: None,
                events,
                closed: false,
                attached: false,
                ended: false,
                task: Weak::new(),
                index: 0,
            }),
            engine: Signaller::new(Signals::WRITABLE),
            task: Signaller::new(Signals::NONE),
            _charge: charge,
        })
    }

    pub fn signaller(&self) -> &Signaller {
        &self.engine
    }

    /// Recomputes both sides' signals; call with the state locked.
    fn refresh(&self, s: &State) {
        let mut engine = Signals::NONE;
        if !s.events.is_empty() {
            engine = engine | Signals::READABLE;
        }
        if !s.closed && !s.ended && s.in_flight() < limits::SLOT_DEPTH {
            engine = engine | Signals::WRITABLE;
        }
        if s.ended {
            engine = engine | Signals::PEER_CLOSED;
        }
        let task_ready = !s.ready.is_empty() || s.closed || s.ended;
        if task_ready {
            engine = engine | Signals::SIGNALED;
        }
        self.engine.update(Signals::ALL.difference(engine), engine);
        let task = if task_ready {
            Signals::READABLE
        } else {
            Signals::NONE
        };
        self.task.update(Signals::ALL.difference(task), task);
    }

    /// `slot_push`: the engine hands the slot `memory` from `offset` for `len` bytes:
    /// a batch (read through `READ`) or a window (written through `WRITE`, whole pages).
    pub fn push(
        &self,
        memory: Arc<MemoryObject>,
        rights: Rights,
        offset: u64,
        len: u64,
        cookie: u64,
    ) -> Result<(), Error> {
        let needed = if self.output {
            Rights::READ | Rights::WRITE | Rights::MAP
        } else {
            Rights::READ | Rights::MAP
        };
        if !rights.contains(needed) {
            return Err(Error::AccessDenied);
        }
        if len == 0
            || !offset.is_multiple_of(PAGE_SIZE)
            || (self.output && !len.is_multiple_of(PAGE_SIZE))
        {
            return Err(Error::InvalidArgs);
        }
        let span = vm::pages(len)?;
        if offset
            .checked_add(span)
            .is_none_or(|end| end > memory.size())
        {
            return Err(Error::OutOfRange);
        }
        let mut s = self.state.lock();
        if s.closed || s.ended {
            return Err(Error::BadState);
        }
        if s.in_flight() >= limits::SLOT_DEPTH {
            return Err(Error::ShouldWait);
        }
        s.ready.push_back(Entry {
            memory,
            rights,
            offset,
            len,
            cookie,
        });
        self.refresh(&s);
        Ok(())
    }

    /// `slot_close`, and the engine's last handle closing.
    pub fn close(&self) {
        let mut s = self.state.lock();
        s.closed = true;
        self.refresh(&s);
    }

    /// Binds the slot to `task` as its slot `index`, once.
    fn attach(&self, task: &Arc<Process>, index: usize) -> Result<(), Error> {
        let mut s = self.state.lock();
        if s.attached {
            return Err(Error::BadState);
        }
        s.attached = true;
        s.task = Arc::downgrade(task);
        s.index = index;
        Ok(())
    }

    fn is_attached(&self) -> bool {
        self.state.lock().attached
    }

    /// `batch_get` (and `plane_get`, the server's form): the next batch mapped read
    /// only into `mapper`, the previous one released; or the window mapped writable.
    /// `(0, 0)` once the engine has closed the slot and nothing is queued. Waits for a
    /// push while there is nothing.
    pub fn get(&self, thread: &Thread, mapper: &Arc<Process>) -> Result<(u64, u64), Error> {
        let (waiter, waker) = Waiter::new_pair();
        let mut watching = false;
        let result = loop {
            if thread.killed() {
                break Err(Error::BadState);
            }
            let mut s = self.state.lock();
            if s.ended {
                break Err(Error::BadState);
            }
            if let Some(m) = &s.current
                && !Weak::ptr_eq(&m.mapper, &Arc::downgrade(mapper))
            {
                break Err(Error::BadState);
            }
            if self.output {
                if let Some(m) = &s.current {
                    break Ok((m.addr, m.entry.len));
                }
            } else if let Some(m) = s.current.take() {
                if let Err(e) = mapper.space.unmap_plane(m.addr, m.span, &m.entry.memory) {
                    s.current = Some(m);
                    break Err(e);
                }
                // The queue was reserved for every object the slot can hold, and a
                // release takes the place of the batch it releases.
                s.events.push_back(Event::Released(m.entry.cookie));
                drop(m);
                self.refresh(&s);
            }
            if let Some(entry) = s.ready.pop_front() {
                let first = (entry.offset / PAGE_SIZE) as usize;
                let span = match vm::pages(entry.len) {
                    Ok(span) => span,
                    Err(e) => {
                        s.ready.push_front(entry);
                        break Err(e);
                    }
                };
                match mapper
                    .space
                    .map_plane(entry.memory.clone(), first, span, self.output)
                {
                    Ok(addr) => {
                        let len = entry.len;
                        s.current = Some(Mapped {
                            mapper: Arc::downgrade(mapper),
                            addr,
                            span,
                            entry,
                        });
                        self.refresh(&s);
                        break Ok((addr, len));
                    }
                    Err(e) => {
                        s.ready.push_front(entry);
                        break Err(e);
                    }
                }
            }
            if s.closed {
                break Ok((0, 0));
            }
            if !watching {
                self.task.observe(&waker);
                thread.set_waker(Some(waker.clone()));
                watching = true;
                // Observed under the lock: a push from here on wakes the waiter.
                continue;
            }
            drop(s);
            waiter.wait();
        };
        if watching {
            self.task.unobserve(&waker);
            thread.set_waker(None);
        }
        result
    }

    /// `batch_put` (and `plane_put`): the record of `len` bytes at `addr` of the window
    /// `mapper` has mapped goes to the engine, and the window leaves `mapper`'s address
    /// space first, so no task can write the bytes the engine is about to check.
    pub fn put(&self, mapper: &Arc<Process>, addr: u64, len: u64) -> Result<(), Error> {
        if !self.output {
            return Err(Error::InvalidArgs);
        }
        let mut s = self.state.lock();
        let Some(m) = &s.current else {
            return Err(Error::BadState);
        };
        if !Weak::ptr_eq(&m.mapper, &Arc::downgrade(mapper)) {
            return Err(Error::BadState);
        }
        if len == 0 || !addr.is_multiple_of(PAGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        let within = addr.checked_sub(m.addr).ok_or(Error::OutOfRange)?;
        if within.checked_add(len).is_none_or(|end| end > m.entry.len) {
            return Err(Error::OutOfRange);
        }
        let Some(m) = s.current.take() else {
            return Err(Error::BadState);
        };
        if let Err(e) = mapper.space.unmap_plane(m.addr, m.span, &m.entry.memory) {
            s.current = Some(m);
            return Err(e);
        }
        let offset = m.entry.offset + within;
        s.events.push_back(Event::Put {
            entry: m.entry,
            offset,
            len,
        });
        self.refresh(&s);
        Ok(())
    }

    /// The task has ended: what it held and what was queued for it goes back to the
    /// engine as events, the mapping it had through a server is unmapped, and the
    /// engine sees `PEER_CLOSED` once it has taken them.
    fn detach(&self) {
        let mut s = self.state.lock();
        if s.ended {
            return;
        }
        s.ended = true;
        let output = self.output;
        let back = |entry: Entry| {
            if output {
                Event::Returned(entry)
            } else {
                Event::Released(entry.cookie)
            }
        };
        if let Some(m) = s.current.take() {
            if let Some(p) = m.mapper.upgrade() {
                // A mapping the space no longer has (the task's own, cleared with it)
                // is nothing to unmap; any other refusal is a broken invariant.
                match p.space.unmap_plane(m.addr, m.span, &m.entry.memory) {
                    Ok(()) | Err(Error::BadState) => {}
                    Err(e) => early_println!(
                        "kk: process {} plane window at {:#x} not unmapped at close: {}",
                        p.name,
                        m.addr,
                        e.name()
                    ),
                }
            }
            let event = back(m.entry);
            s.events.push_back(event);
        }
        while let Some(entry) = s.ready.pop_front() {
            let event = back(entry);
            s.events.push_back(event);
        }
        self.refresh(&s);
    }

    /// `slot_refuse`: the record the engine took from this slot failed its check; the
    /// task ends with the reason.
    pub fn refuse(&self, reason: &str) -> Result<(), Error> {
        let (task, index) = {
            let s = self.state.lock();
            if s.ended {
                return Err(Error::BadState);
            }
            (s.task.upgrade().ok_or(Error::BadState)?, s.index)
        };
        task.end(Exit::Fault(format!(
            "its record on slot {index} was refused by the engine's check: {reason}"
        )));
        Ok(())
    }
}

/// What `slot_take` hands back: the 32 bytes of `slot_event` and the memory object
/// with the rights it was pushed through, for a record or a window.
pub struct Taken {
    pub bytes: [u8; slot_event::SIZE],
    pub memory: Option<(Arc<MemoryObject>, Rights)>,
}

impl Slot {
    /// The next event, with `insert` putting its memory object, if any, into the
    /// engine's table: the event leaves the queue only once `insert` has succeeded.
    /// `ShouldWait` with none and the task alive or not yet started, `PeerClosed` with
    /// none and the task ended.
    pub fn take(&self, insert: impl FnOnce(&Taken) -> Result<(), Error>) -> Result<(), Error> {
        let mut s = self.state.lock();
        let Some(front) = s.events.front() else {
            return Err(if s.ended {
                Error::PeerClosed
            } else {
                Error::ShouldWait
            });
        };
        let mut bytes = [0u8; slot_event::SIZE];
        let memory = match front {
            Event::Released(cookie) => {
                write_u32(&mut bytes, slot_event::KIND, slot_event::RELEASED);
                write_u64(&mut bytes, slot_event::COOKIE, *cookie);
                None
            }
            Event::Put { entry, offset, len } => {
                write_u32(&mut bytes, slot_event::KIND, slot_event::PUT);
                write_u64(&mut bytes, slot_event::COOKIE, entry.cookie);
                write_u64(&mut bytes, slot_event::OFFSET, *offset);
                write_u64(&mut bytes, slot_event::LEN, *len);
                Some((entry.memory.clone(), entry.rights))
            }
            Event::Returned(entry) => {
                write_u32(&mut bytes, slot_event::KIND, slot_event::RETURNED);
                write_u64(&mut bytes, slot_event::COOKIE, entry.cookie);
                write_u64(&mut bytes, slot_event::OFFSET, entry.offset);
                write_u64(&mut bytes, slot_event::LEN, entry.len);
                Some((entry.memory.clone(), entry.rights))
            }
        };
        insert(&Taken { bytes, memory })?;
        let taken = s.events.pop_front();
        self.refresh(&s);
        drop(s);
        drop(taken);
        Ok(())
    }
}

/// A task's port: the kernel's end of the channel to the engine, and the tokens of
/// the threads waiting on it for a reply.
pub struct Port {
    endpoint: Arc<Endpoint>,
    waiting: SpinLock<BTreeSet<u64>>,
}

/// A process's plane: its slots, numbered as `process_plane` gave them, and its port.
pub struct Plane {
    slots: Vec<Arc<Slot>>,
    port: Option<Port>,
}

impl Plane {
    /// Binds `slots` to `task` in order, each once, with the port over `endpoint`. On
    /// a refusal nothing stays bound.
    pub fn bind(
        task: &Arc<Process>,
        slots: Vec<Arc<Slot>>,
        endpoint: Option<Arc<Endpoint>>,
    ) -> Result<Plane, Error> {
        if slots.iter().any(|s| s.is_attached()) {
            return Err(Error::BadState);
        }
        for (i, slot) in slots.iter().enumerate() {
            if slots[..i].iter().any(|s| Arc::ptr_eq(s, slot)) {
                return Err(Error::InvalidArgs);
            }
        }
        for (i, slot) in slots.iter().enumerate() {
            if let Err(e) = slot.attach(task, i) {
                for done in &slots[..i] {
                    let mut s = done.state.lock();
                    s.attached = false;
                    s.task = Weak::new();
                }
                return Err(e);
            }
        }
        Ok(Plane {
            slots,
            port: endpoint.map(|endpoint| Port {
                endpoint,
                waiting: SpinLock::new(BTreeSet::new()),
            }),
        })
    }

    /// Slot `index`; `BadHandle` for a number the plane does not have, so a task names
    /// only the slots it was given (DP-I5).
    pub fn slot(&self, index: u64) -> Result<&Arc<Slot>, Error> {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.slots.get(i))
            .ok_or(Error::BadHandle)
    }

    /// Its task has ended: every slot gives back what it held.
    pub fn detach(&self) {
        for slot in &self.slots {
            slot.detach();
        }
    }
}

/// `port_call`: `bytes` to the engine on `thread`'s port as a request for `port`, and
/// the reply mapped into the process read only; `(0, 0)` for an empty reply. The
/// thread's previous reply is unmapped first.
pub fn port_call(
    thread: &Arc<Thread>,
    plane: &Plane,
    port: u64,
    bytes: Vec<u8>,
    payload_charge: Charge,
) -> Result<(u64, u64), Error> {
    let channel = plane.port.as_ref().ok_or(Error::BadHandle)?;
    let process = &thread.process;
    if let Some((addr, span, memory)) = thread.port_reply.lock().take() {
        // A reply mapping the plane made and nothing else can remove.
        if let Err(e) = process.space.unmap_plane(addr, span, &memory) {
            early_println!(
                "kk: process {} port reply at {:#x} not unmapped: {}",
                process.name,
                addr,
                e.name()
            );
        }
    }
    // The request is the task's while it is queued.
    let charge = Charge::new(
        process.account(),
        Message::cost(request::SIZE + bytes.len(), 0),
    )?;
    let mut message = Vec::new();
    message
        .try_reserve_exact(request::SIZE + bytes.len())
        .map_err(|_| Error::NoMemory)?;
    message.resize(request::SIZE, 0);
    write_u32(&mut message, request::KIND, request::CALL);
    write_u64(&mut message, request::TOKEN, thread.tid);
    write_u64(&mut message, request::PORT, port);
    write_u64(&mut message, request::LEN, bytes.len() as u64);
    message.extend_from_slice(&bytes);
    drop(bytes);
    drop(payload_charge);
    let request = Message {
        bytes: message,
        handles: Vec::new(),
        endpoints: 0,
        charge: Some(charge),
    };
    let answer = foreign::exchange(
        thread,
        &channel.endpoint,
        &channel.waiting,
        request,
        reply::SIZE,
        "its engine",
    );
    let reply = match answer {
        Ok(Some(reply)) => reply,
        // Killed while it waited: the thread ends at its next kernel entry.
        Ok(None) => return Err(Error::BadState),
        Err(_) => return Err(Error::PeerClosed),
    };
    let status = read_u64(&reply.bytes, reply::STATUS).unwrap_or(0);
    if status != 0 {
        let code = u32::try_from(status).ok().and_then(Error::from_code);
        return Err(code.unwrap_or(Error::InvalidArgs));
    }
    let len = read_u64(&reply.bytes, reply::LEN).unwrap_or(0);
    if len == 0 {
        return Ok((0, 0));
    }
    let memory = match reply.handles.first().map(|h| (&h.object, h.rights)) {
        Some((crate::object::Object::Memory(m), rights))
            if rights.contains(Rights::READ | Rights::MAP) && len <= m.size() =>
        {
            m.clone()
        }
        // The engine broke the protocol: the call fails, the task goes on.
        _ => return Err(Error::PeerClosed),
    };
    let span = vm::pages(len)?;
    let addr = process.space.map_plane(memory.clone(), 0, span, false)?;
    *thread.port_reply.lock() = Some((addr, span, memory));
    Ok((addr, len))
}

/// The reason `slot_refuse` was given, as text.
pub fn reason(bytes: &[u8]) -> Result<String, Error> {
    let text = core::str::from_utf8(bytes).map_err(|_| Error::InvalidArgs)?;
    Ok(String::from(text))
}
