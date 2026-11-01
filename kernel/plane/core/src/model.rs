//! A model of the kernel's data-plane calls on a host (SDD 29 k), and tasks that run
//! [`Step`] scripts against it: how [`KernelPlane`] is tested off the kernel, and how it runs
//! the conformance suite every adapter of the port passes (KA2).
//!
//! The model keeps what the kernel keeps: a slot's queue, the object its task has mapped and
//! the events that give objects back, bounded at `SLOT_DEPTH`; channels of messages and
//! handles; a share that cannot be written. A task here is a script run in steps by
//! [`ModelHarness::run`], doing what the kernel does for each call: a batch mapped read only,
//! the previous one released, a window unmapped at its put, everything given back at the end.

use alloc::{
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    string::String,
    vec,
    vec::Vec,
};
use core::cell::RefCell;

use moruna_abi::{
    Error, limits,
    plane::{reply, request},
    read_u64, write_u32, write_u64,
};

use crate::KernelPlane;
use crate::arena::PAGE;
use crate::calls::{Calls, Ready, Taken};
use crate::conformance::{Ending, Harness, Outcome, Seen, Step, sum};
use crate::port::TaskId;

/// A memory object: its bytes, and whether this handle may write them.
#[derive(Clone, Debug)]
pub struct Mem {
    pub bytes: Rc<RefCell<Vec<u8>>>,
    pub writable: bool,
}

/// What a slot pushed holds: the object, where the batch or window starts, its length, and
/// the cookie its event gives back.
pub type Pushed = (Mem, u64, u64, u64);

#[derive(Debug)]
pub struct SlotState {
    pub output: bool,
    pub ready: VecDeque<Pushed>,
    pub events: VecDeque<Taken<Mem>>,
    pub current: Option<Pushed>,
    pub closed: bool,
    pub ended: bool,
    /// The reason `slot_refuse` gave, once it was called.
    pub refused: Option<String>,
}

pub type Slot = Rc<RefCell<SlotState>>;

#[derive(Debug)]
pub struct ChannelState {
    /// What each end will read.
    pub queues: [VecDeque<(Vec<u8>, Option<Mem>)>; 2],
    pub open: [bool; 2],
}

/// One end of a channel: side 0 is the engine's, side 1 the kernel's on the task's behalf.
#[derive(Clone, Debug)]
pub struct Chan {
    pub state: Rc<RefCell<ChannelState>>,
    pub side: usize,
}

/// The model of the kernel's calls, with failures a test can ask for.
#[derive(Debug)]
pub struct Model {
    /// Memory objects made.
    pub made: usize,
    pub slots: Vec<Slot>,
    pub fail_memory: bool,
    pub fail_read: bool,
    pub fail_push: Option<Error>,
    /// The most objects one slot holds, as the kernel's `SLOT_DEPTH`.
    pub depth: usize,
}

impl Default for Model {
    fn default() -> Model {
        Model {
            made: 0,
            slots: Vec::new(),
            fail_memory: false,
            fail_read: false,
            fail_push: None,
            depth: limits::SLOT_DEPTH,
        }
    }
}

impl Calls for Model {
    type Memory = Mem;
    type Slot = Slot;
    type Channel = Chan;

    fn memory(&mut self, bytes: u64) -> Result<Mem, Error> {
        if self.fail_memory {
            return Err(Error::NoMemory);
        }
        self.made += 1;
        Ok(Mem {
            bytes: Rc::new(RefCell::new(vec![0u8; bytes as usize])),
            writable: true,
        })
    }

    fn share(&mut self, memory: &Mem) -> Result<Mem, Error> {
        Ok(Mem {
            bytes: memory.bytes.clone(),
            writable: false,
        })
    }

    fn read(&mut self, memory: &Mem, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        if self.fail_read {
            return Err(Error::Fault);
        }
        let b = memory.bytes.borrow();
        let at = offset as usize;
        let src = b.get(at..at + buf.len()).ok_or(Error::OutOfRange)?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write(&mut self, memory: &Mem, offset: u64, data: &[u8]) -> Result<(), Error> {
        let mut b = memory.bytes.borrow_mut();
        let at = offset as usize;
        b.get_mut(at..at + data.len())
            .ok_or(Error::OutOfRange)?
            .copy_from_slice(data);
        Ok(())
    }

    fn slot(&mut self, output: bool) -> Result<Slot, Error> {
        let s = Rc::new(RefCell::new(SlotState {
            output,
            ready: VecDeque::new(),
            events: VecDeque::new(),
            current: None,
            closed: false,
            ended: false,
            refused: None,
        }));
        self.slots.push(s.clone());
        Ok(s)
    }

    fn push(
        &mut self,
        slot: &Slot,
        memory: Mem,
        offset: u64,
        len: u64,
        cookie: u64,
    ) -> Result<(), (Error, Mem)> {
        if let Some(e) = self.fail_push.take() {
            return Err((e, memory));
        }
        let mut s = slot.borrow_mut();
        if s.closed || s.ended {
            return Err((Error::BadState, memory));
        }
        if s.ready.len() + s.events.len() + usize::from(s.current.is_some()) >= self.depth {
            return Err((Error::ShouldWait, memory));
        }
        s.ready.push_back((memory, offset, len, cookie));
        Ok(())
    }

    fn take(&mut self, slot: &Slot) -> Result<Taken<Mem>, Error> {
        let mut s = slot.borrow_mut();
        match s.events.pop_front() {
            Some(t) => Ok(t),
            None if s.ended => Err(Error::PeerClosed),
            None => Err(Error::ShouldWait),
        }
    }

    fn close(&mut self, slot: &Slot) -> Result<(), Error> {
        slot.borrow_mut().closed = true;
        Ok(())
    }

    fn refuse(&mut self, slot: &Slot, reason: &str) -> Result<(), Error> {
        let mut s = slot.borrow_mut();
        if s.ended {
            return Err(Error::BadState);
        }
        s.refused = Some(String::from(reason));
        Ok(())
    }

    fn channel(&mut self) -> Result<(Chan, Chan), Error> {
        let state = Rc::new(RefCell::new(ChannelState {
            queues: [VecDeque::new(), VecDeque::new()],
            open: [true, true],
        }));
        Ok((
            Chan {
                state: state.clone(),
                side: 0,
            },
            Chan { state, side: 1 },
        ))
    }

    fn receive(&mut self, channel: &Chan, buf: &mut [u8]) -> Result<usize, Error> {
        let mut c = channel.state.borrow_mut();
        let peer = 1 - channel.side;
        match c.queues[channel.side].pop_front() {
            Some((bytes, _)) => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            None if !c.open[peer] => Err(Error::PeerClosed),
            None => Err(Error::ShouldWait),
        }
    }

    fn send(&mut self, channel: &Chan, bytes: &[u8], memory: Option<Mem>) -> Result<(), Error> {
        let mut c = channel.state.borrow_mut();
        let peer = 1 - channel.side;
        if !c.open[peer] {
            return Err(Error::PeerClosed);
        }
        c.queues[peer].push_back((bytes.to_vec(), memory));
        Ok(())
    }

    fn wait(
        &mut self,
        slots: &[&Slot],
        channels: &[&Chan],
        _deadline: u64,
    ) -> Result<Ready, Error> {
        let ready = slots
            .iter()
            .any(|s| !s.borrow().events.is_empty() || s.borrow().ended)
            || channels.iter().any(|c| {
                let st = c.state.borrow();
                !st.queues[c.side].is_empty() || !st.open[1 - c.side]
            });
        Ok(if ready { Ready::Some } else { Ready::TimedOut })
    }
}

/// `batch_get` as the kernel does it on an input slot: the previous batch released, the next
/// one's bytes; `None` while there is none.
pub fn task_get(slot: &Slot) -> Option<Vec<u8>> {
    let mut s = slot.borrow_mut();
    if !s.output
        && let Some((_, _, _, cookie)) = s.current.take()
    {
        s.events.push_back(Taken::Released { cookie });
    }
    let entry = s.ready.pop_front()?;
    let (m, offset, len, _) = &entry;
    let bytes = m.bytes.borrow()[*offset as usize..(*offset + *len) as usize].to_vec();
    s.current = Some(entry);
    Some(bytes)
}

/// `record` written into the mapped window and `batch_put` of `len` bytes of it.
pub fn task_put(slot: &Slot, record: &[u8], len: u64) {
    let mut s = slot.borrow_mut();
    let (m, offset, _, cookie) = s.current.take().expect("a window");
    m.bytes.borrow_mut()[..record.len()].copy_from_slice(record);
    s.events.push_back(Taken::Put {
        cookie,
        memory: m,
        offset,
        len,
    });
}

/// The task's end as the kernel gives it: every batch it held or had queued released, every
/// window given back, then `PEER_CLOSED`.
pub fn task_end(slot: &Slot) {
    let mut s = slot.borrow_mut();
    if s.ended {
        return;
    }
    s.ended = true;
    let output = s.output;
    let mut back: Vec<Taken<Mem>> = Vec::new();
    for (m, _, _, cookie) in s.current.take().into_iter().chain(s.ready.drain(..)) {
        back.push(if output {
            Taken::Returned { cookie, memory: m }
        } else {
            Taken::Released { cookie }
        });
    }
    s.events.extend(back);
}

struct Task {
    slots: Vec<Slot>,
    port: Option<Chan>,
    script: VecDeque<Step>,
    seen: Vec<Seen>,
    ending: Option<Ending>,
    /// The token of the call the task waits on.
    waiting: Option<u64>,
}

/// What a step did: the task goes on, waits, or has ended.
enum Did {
    Next,
    Wait,
    End(Ending),
}

impl Task {
    fn end(&mut self, ending: Ending) {
        for slot in &self.slots {
            task_end(slot);
        }
        if let Some(port) = &self.port {
            port.state.borrow_mut().open[1] = false;
        }
        self.ending = Some(ending);
    }

    fn step(&mut self, step: &Step, token: &mut u64) -> Did {
        match step {
            Step::Get(n) => {
                let Some(slot) = self.slots.get(*n as usize) else {
                    self.seen.push(Seen::Error(Error::BadHandle));
                    return Did::Next;
                };
                let mut s = slot.borrow_mut();
                if s.output {
                    if s.current.is_none() {
                        match s.ready.pop_front() {
                            Some(w) => s.current = Some(w),
                            None => return Did::Wait,
                        }
                    }
                    let len = s.current.as_ref().map_or(0, |w| w.2);
                    drop(s);
                    self.seen.push(Seen::Window { len });
                    return Did::Next;
                }
                // The kernel releases the slot's batch as the call starts, before it waits.
                if let Some((_, _, _, cookie)) = s.current.take() {
                    s.events.push_back(Taken::Released { cookie });
                }
                if s.ready.is_empty() && !s.closed {
                    return Did::Wait;
                }
                drop(s);
                match task_get(slot) {
                    Some(bytes) => self.seen.push(Seen::Batch {
                        len: bytes.len() as u64,
                        sum: sum(&bytes),
                    }),
                    None => self.seen.push(Seen::End),
                }
                Did::Next
            }
            Step::Write { slot, at, bytes } => {
                let Some(slot) = self.slots.get(*slot as usize) else {
                    return Did::End(Ending::Faulted);
                };
                let s = slot.borrow();
                let Some((m, offset, len, _)) = &s.current else {
                    return Did::End(Ending::Faulted);
                };
                let end = at.checked_add(bytes.len() as u64);
                if !m.writable || end.is_none_or(|e| e > *len) {
                    return Did::End(Ending::Faulted);
                }
                let from = (*offset + *at) as usize;
                m.bytes.borrow_mut()[from..from + bytes.len()].copy_from_slice(bytes);
                drop(s);
                self.seen.push(Seen::Wrote);
                Did::Next
            }
            Step::Put { slot, offset, len } => {
                let Some(s) = self.slots.get(*slot as usize) else {
                    self.seen.push(Seen::Error(Error::BadHandle));
                    return Did::Next;
                };
                let mut s = s.borrow_mut();
                let refused = if !s.output || *len == 0 || offset % PAGE != 0 {
                    Some(Error::InvalidArgs)
                } else {
                    match &s.current {
                        None => Some(Error::BadState),
                        Some((_, _, window, _))
                            if offset.checked_add(*len).is_none_or(|e| e > *window) =>
                        {
                            Some(Error::OutOfRange)
                        }
                        Some(_) => None,
                    }
                };
                if let Some(e) = refused {
                    drop(s);
                    self.seen.push(Seen::Error(e));
                    return Did::Next;
                }
                let Some((memory, start, _, cookie)) = s.current.take() else {
                    return Did::End(Ending::Faulted);
                };
                s.events.push_back(Taken::Put {
                    cookie,
                    memory,
                    offset: start + offset,
                    len: *len,
                });
                drop(s);
                self.seen.push(Seen::Put);
                Did::Next
            }
            // The window left the task's address space at its put.
            Step::WriteAfterPut(_) => Did::End(Ending::Faulted),
            Step::Call {
                port,
                request: bytes,
            } => {
                if self.waiting.is_none() {
                    let Some(chan) = &self.port else {
                        self.seen.push(Seen::Error(Error::BadHandle));
                        return Did::Next;
                    };
                    if bytes.len() > request::MAX {
                        self.seen.push(Seen::Error(Error::Limit));
                        return Did::Next;
                    }
                    *token += 1;
                    let mut message = vec![0u8; request::SIZE];
                    write_u32(&mut message, request::KIND, request::CALL);
                    write_u64(&mut message, request::TOKEN, *token);
                    write_u64(&mut message, request::PORT, *port);
                    write_u64(&mut message, request::LEN, bytes.len() as u64);
                    message.extend_from_slice(bytes);
                    chan.state.borrow_mut().queues[0].push_back((message, None));
                    self.waiting = Some(*token);
                }
                let Some(chan) = &self.port else {
                    return Did::End(Ending::Faulted);
                };
                loop {
                    let next = chan.state.borrow_mut().queues[1].pop_front();
                    let Some((header, memory)) = next else {
                        return Did::Wait;
                    };
                    if read_u64(&header, reply::TOKEN) != self.waiting {
                        continue;
                    }
                    self.waiting = None;
                    let status = read_u64(&header, reply::STATUS).unwrap_or(0);
                    let len = read_u64(&header, reply::LEN).unwrap_or(0) as usize;
                    let seen = if status != 0 {
                        Seen::Error(Error::from_code(status as u32).unwrap_or(Error::BadState))
                    } else {
                        match memory {
                            Some(m) => Seen::Reply(m.bytes.borrow()[..len].to_vec()),
                            None => Seen::Reply(Vec::new()),
                        }
                    };
                    self.seen.push(seen);
                    return Did::Next;
                }
            }
            Step::Crash => Did::End(Ending::Killed),
        }
    }
}

/// The conformance suite's harness for [`KernelPlane`] over [`Model`].
#[derive(Default)]
pub struct ModelHarness {
    tasks: BTreeMap<u32, Task>,
    token: u64,
}

impl Harness for ModelHarness {
    type Plane = KernelPlane<Model>;

    fn plane(&mut self, budget: u64) -> KernelPlane<Model> {
        self.tasks.clear();
        KernelPlane::new(Model::default(), budget)
    }

    fn start(
        &mut self,
        plane: &mut KernelPlane<Model>,
        task: TaskId,
        end: Option<Chan>,
        script: Vec<Step>,
    ) -> Result<(), String> {
        let slots = plane
            .slots(task)
            .map_err(|e| alloc::format!("slots: {e:?}"))?
            .into_iter()
            .cloned()
            .collect();
        self.tasks.insert(
            task.0,
            Task {
                slots,
                port: end,
                script: script.into(),
                seen: Vec::new(),
                ending: None,
                waiting: None,
            },
        );
        Ok(())
    }

    fn run(&mut self, _plane: &mut KernelPlane<Model>) {
        for task in self.tasks.values_mut() {
            if task.ending.is_some() {
                continue;
            }
            // The kernel ends a task whose record the engine refused.
            if task.slots.iter().any(|s| s.borrow().refused.is_some()) {
                task.end(Ending::Killed);
                continue;
            }
            loop {
                let Some(step) = task.script.front().cloned() else {
                    task.end(Ending::Exited);
                    break;
                };
                match task.step(&step, &mut self.token) {
                    Did::Next => {
                        task.script.pop_front();
                    }
                    Did::Wait => break,
                    Did::End(ending) => {
                        task.end(ending);
                        break;
                    }
                }
            }
        }
    }

    fn deadline(&self, _ms: u64) -> u64 {
        0
    }

    fn kill(&mut self, _plane: &mut KernelPlane<Model>, task: TaskId) -> Result<(), String> {
        let t = self
            .tasks
            .get_mut(&task.0)
            .ok_or_else(|| alloc::format!("no task {task:?}"))?;
        t.end(Ending::Killed);
        Ok(())
    }

    fn outcome(&mut self, task: TaskId) -> Result<Outcome, String> {
        let t = self
            .tasks
            .get(&task.0)
            .ok_or_else(|| alloc::format!("no task {task:?}"))?;
        let ending = t
            .ending
            .ok_or_else(|| alloc::format!("task {task:?} has not ended"))?;
        Ok(Outcome {
            seen: t.seen.clone(),
            ending,
        })
    }
}
