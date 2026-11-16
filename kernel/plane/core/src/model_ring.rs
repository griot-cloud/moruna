//! Ring mode on the model of the kernel's calls (`architecture/data-os.md` §5): the engine's
//! mappings of model objects, `windows_protect` page by page, and a task that speaks the
//! ring protocol, so ring mode's conformance runs on a host.
//!
//! The task maps what its slots hold as the kernel would (slot 0's region read only, slot 1's
//! rings object writable), reads its batches in place, writes each record into a window, and
//! waits on port 0 when it has nothing to do. A write into a page it may not write ends it as
//! a fault, as the kernel's page fault would.

use alloc::{rc::Rc, vec, vec::Vec};
use core::cell::RefCell;

use moruna_abi::{
    Error,
    plane::{reply, request},
    read_u64, write_u32, write_u64,
};

use crate::arena::PAGE;
use crate::calls::Taken;
use crate::conformance::Ending;
use crate::model::{Chan, Mem, Model, Slot, task_end};
use crate::ring::{self, InEntry, Mode, OutEntry, Shared, layout};
use crate::ringed::{RingCalls, View};

/// The engine's mapping of a model object.
#[derive(Clone, Debug)]
pub struct ModelMap {
    pub bytes: Rc<RefCell<Vec<u8>>>,
    len: u64,
    /// The model's count of futex calls.
    futex: Rc<core::cell::Cell<u64>>,
}

impl ModelMap {
    /// The engine's mapping of the whole of `mem`.
    pub fn from_mem(mem: &Mem) -> ModelMap {
        ModelMap {
            bytes: mem.bytes.clone(),
            len: mem.bytes.borrow().len() as u64,
            futex: Rc::new(core::cell::Cell::new(0)),
        }
    }
}

fn load(bytes: &RefCell<Vec<u8>>, at: usize) -> u64 {
    let b = bytes.borrow();
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(w)
}

fn store(bytes: &RefCell<Vec<u8>>, at: usize, value: u64) {
    bytes.borrow_mut()[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

impl Shared for ModelMap {
    fn load(&self, at: usize) -> u64 {
        load(&self.bytes, at)
    }
    fn store(&self, at: usize, value: u64) {
        store(&self.bytes, at, value)
    }
    fn fence(&self) {}
    /// The model cannot sleep: the word has changed, or the deadline is taken as passed.
    fn wait(&self, at: usize, expected: u32, _deadline: u64) -> bool {
        self.futex.set(self.futex.get() + 1);
        load(&self.bytes, at) as u32 != expected
    }
    fn wake(&self, _at: usize) {
        self.futex.set(self.futex.get() + 1);
    }
}

impl View for ModelMap {
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), Error> {
        let b = self.bytes.borrow();
        let at = at as usize;
        buf.copy_from_slice(b.get(at..at + buf.len()).ok_or(Error::OutOfRange)?);
        Ok(())
    }
    fn write(&self, at: u64, data: &[u8]) -> Result<(), Error> {
        let mut b = self.bytes.borrow_mut();
        let at = at as usize;
        b.get_mut(at..at + data.len())
            .ok_or(Error::OutOfRange)?
            .copy_from_slice(data);
        Ok(())
    }
    fn base(&self) -> usize {
        Rc::as_ptr(&self.bytes) as usize
    }
    fn len(&self) -> u64 {
        self.len
    }
}

impl RingCalls for Model {
    type Mapping = ModelMap;

    fn map(&mut self, memory: &Mem, len: u64) -> Result<ModelMap, Error> {
        self.calls += 1;
        if memory.bytes.borrow().len() < len as usize {
            return Err(Error::OutOfRange);
        }
        Ok(ModelMap {
            bytes: memory.bytes.clone(),
            len,
            futex: self.futex.clone(),
        })
    }

    fn keep(&mut self, memory: &Mem) -> Result<Mem, Error> {
        self.calls += 1;
        Ok(memory.clone())
    }

    fn protect(&mut self, slot: &Slot, ranges: &[[u64; 2]], writable: bool) -> Result<(), Error> {
        self.calls += 1;
        if !self.protect {
            return Err(Error::BadSyscall);
        }
        let mut s = slot.borrow_mut();
        // As the kernel's slot form: no process maps the slot's object any more.
        if s.current.is_none() && !ranges.is_empty() {
            return Err(Error::BadState);
        }
        for [at, len] in ranges {
            if at % PAGE != 0 || len % PAGE != 0 {
                return Err(Error::InvalidArgs);
            }
            for page in (*at..at + len).step_by(PAGE as usize) {
                if writable {
                    s.read_only.remove(&page);
                } else {
                    s.read_only.insert(page);
                }
            }
        }
        Ok(())
    }
}

/// The task's mapping of its rings object.
struct TaskMap(Mem);

impl Shared for TaskMap {
    fn load(&self, at: usize) -> u64 {
        load(&self.0.bytes, at)
    }
    fn store(&self, at: usize, value: u64) {
        store(&self.0.bytes, at, value)
    }
    fn fence(&self) {}
}

/// What a model ring task does with a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behave {
    /// Writes the record it was given back.
    Echo,
    /// Writes garbage of the record's length.
    Garbage,
    /// Posts a record in a window it was not given.
    StrayWindow,
    /// Sets its output ring's head far past the ring.
    BadHead,
    /// Writes into its input region.
    WriteRegion,
    /// Writes into the window of the record it posted last, after posting it.
    WriteAfterPost,
    /// Ends after its first batch, without releasing it.
    Crash,
}

/// A task of the model that speaks the ring protocol.
pub struct RingTaskModel {
    pub input: Slot,
    pub output: Slot,
    pub port: Chan,
    pub behave: Behave,
    region: Option<Mem>,
    rings: Option<Mem>,
    ring: Option<ring::Task>,
    /// The half it had before it handed its rings object back.
    previous: Option<ring::Task>,
    waiting: Option<u64>,
    token: u64,
    pending: Option<(InEntry, Vec<u8>)>,
    posted_since_put: u64,
    /// The object and the offset of the record it posted last.
    last_posted: Option<(Mem, u64)>,
    /// Records it posted.
    pub outputs: u64,
    /// For each batch it read: its cookie, and whether it was read in place from the region
    /// object the engine wrote (the same memory, by pointer).
    pub read: Vec<(u64, usize)>,
    /// How it ended.
    pub ended: Option<Ending>,
    /// Waits it called.
    pub waits: u64,
    /// It sleeps on its bell (the futex); otherwise it waits on port 0, as version 1 did.
    bells: bool,
    /// The bell's value it sleeps on.
    bell: Option<u32>,
    /// Wakes it called on the engine's bell.
    pub engine_wakes: u64,
}

impl RingTaskModel {
    pub fn new(slots: [&Slot; 2], port: Chan, behave: Behave) -> RingTaskModel {
        RingTaskModel {
            input: slots[0].clone(),
            output: slots[1].clone(),
            port,
            behave,
            region: None,
            rings: None,
            ring: None,
            previous: None,
            waiting: None,
            token: 0,
            pending: None,
            posted_since_put: 0,
            last_posted: None,
            outputs: 0,
            read: Vec::new(),
            ended: None,
            waits: 0,
            bells: true,
            bell: None,
            engine_wakes: 0,
        }
    }

    /// The task waits on port 0 instead of its bell, which the engine still answers.
    pub fn with_port_waits(mut self) -> RingTaskModel {
        self.bells = false;
        self
    }

    /// The region object it maps, for a test to compare with the engine's.
    pub fn region_ptr(&self) -> Option<usize> {
        self.region.as_ref().map(|m| Rc::as_ptr(&m.bytes) as usize)
    }

    /// Raises its note and rings the engine's bell, as before a port call.
    pub fn note(&mut self) {
        if let (Some(r), Some(ring)) = (self.rings.clone(), self.ring.as_mut()) {
            ring.note(&TaskMap(r));
        }
    }

    /// Ends the task as the kernel ends a process.
    pub fn end(&mut self, how: Ending) {
        if self.ended.is_some() {
            return;
        }
        task_end(&self.input);
        task_end(&self.output);
        self.port.state.borrow_mut().open[1] = false;
        self.ended = Some(how);
    }

    fn map(slot: &Slot) -> Option<Mem> {
        let mut s = slot.borrow_mut();
        if s.current.is_none() {
            let next = s.ready.pop_front()?;
            s.current = Some(next);
        }
        s.current.as_ref().map(|(m, ..)| m.clone())
    }

    fn wait(&mut self, want: u8, seen: u64) {
        if self.bells && want != layout::WAKE {
            if let Some(r) = &self.rings {
                let view = TaskMap(r.clone());
                view.store(layout::TASK_WAITS, 1);
                self.bell = Some(view.load(layout::TASK_BELL) as u32);
                self.waits += 1;
            }
            return;
        }
        if let Some(r) = &self.rings {
            TaskMap(r.clone()).store(layout::TASK_WAITS, 1);
        }
        self.token += 1;
        let mut body = vec![0u8; 16];
        body[0] = want;
        body[8..16].copy_from_slice(&seen.to_le_bytes());
        let mut message = vec![0u8; request::SIZE];
        write_u32(&mut message, request::KIND, request::CALL);
        write_u64(&mut message, request::TOKEN, self.token);
        write_u64(&mut message, request::PORT, layout::WAIT_PORT);
        write_u64(&mut message, request::LEN, body.len() as u64);
        message.extend_from_slice(&body);
        self.port.state.borrow_mut().queues[0].push_back((message, None));
        self.waiting = Some(self.token);
        self.waits += 1;
    }

    /// Whether the engine answered the wait it called.
    fn answered(&mut self) -> bool {
        if let Some(seq) = self.bell {
            // The futex: woken once the engine raised the bell, or the rings object it slept
            // on was replaced (unmap mode).
            let Some(r) = &self.rings else {
                self.bell = None;
                return true;
            };
            let view = TaskMap(r.clone());
            if view.load(layout::TASK_BELL) as u32 == seq {
                return false;
            }
            view.store(layout::TASK_WAITS, 0);
            self.bell = None;
            return true;
        }
        let Some(token) = self.waiting else {
            return true;
        };
        loop {
            let next = self.port.state.borrow_mut().queues[1].pop_front();
            let Some((header, _)) = next else {
                return false;
            };
            if read_u64(&header, reply::TOKEN) == Some(token) {
                self.waiting = None;
                if let Some(r) = &self.rings {
                    TaskMap(r.clone()).store(layout::TASK_WAITS, 0);
                }
                return true;
            }
        }
    }

    /// Hands the rings object back with a put, as `batch_put` does: it leaves the task.
    fn hand_back(&mut self) {
        let mut s = self.output.borrow_mut();
        if let Some((memory, offset, len, cookie)) = s.current.take() {
            s.events.push_back(Taken::Put {
                cookie,
                memory,
                offset,
                len,
            });
        }
        drop(s);
        self.rings = None;
        self.previous = self.ring.take();
        self.posted_since_put = 0;
    }

    fn write_window(&mut self, at: u64, data: &[u8]) -> bool {
        let s = self.output.borrow();
        let Some((m, ..)) = &s.current else {
            return false;
        };
        let first = at & !(PAGE - 1);
        let end = at + data.len() as u64;
        if (first..end)
            .step_by(PAGE as usize)
            .any(|p| s.read_only.contains(&p))
            || !m.writable
        {
            return false;
        }
        m.bytes.borrow_mut()[at as usize..end as usize].copy_from_slice(data);
        true
    }

    /// Runs until it has to wait or ends; whether it did anything.
    pub fn step(&mut self) -> bool {
        if self.ended.is_some() {
            return false;
        }
        if self.input.borrow().refused.is_some() || self.output.borrow().refused.is_some() {
            self.end(Ending::Killed);
            return true;
        }
        if !self.answered() {
            return false;
        }
        if self.region.is_none() {
            match Self::map(&self.input) {
                Some(m) => self.region = Some(m),
                None => return false,
            }
        }
        if self.rings.is_none() {
            let Some(m) = Self::map(&self.output) else {
                return false;
            };
            let attached = match &self.previous {
                Some(p) => ring::Task::reattach(&TaskMap(m.clone()), p),
                None => ring::Task::attach(&TaskMap(m.clone())),
            };
            match attached {
                Ok(t) => self.ring = Some(t),
                Err(_) => {
                    self.end(Ending::Faulted);
                    return true;
                }
            }
            self.rings = Some(m);
        }
        let mut did = false;
        loop {
            let (Some(rings), Some(region)) = (self.rings.clone(), self.region.clone()) else {
                return did;
            };
            let view = TaskMap(rings);
            let Some(ring) = self.ring.as_mut() else {
                return did;
            };
            let (entry, out) = match self.pending.take() {
                Some(p) => p,
                None => match ring.next(&view) {
                    Err(_) => {
                        self.end(Ending::Faulted);
                        return true;
                    }
                    Ok(None) => {
                        if ring.ended(&view) {
                            // A task that ends on its own says so first.
                            ring.note(&view);
                            if ring.mode == Mode::Unmap && self.posted_since_put > 0 {
                                self.hand_back();
                            }
                            self.end(Ending::Exited);
                            return true;
                        }
                        if ring.mode == Mode::Unmap && self.posted_since_put > 0 {
                            self.hand_back();
                            return true;
                        }
                        let seen = view.load(layout::IN_HEAD);
                        self.wait(layout::WAIT_INPUT, seen);
                        return did;
                    }
                    Ok(Some(e)) => {
                        let bytes = region.bytes.borrow();
                        let record = bytes[e.offset as usize..(e.offset + e.len) as usize].to_vec();
                        drop(bytes);
                        self.read
                            .push((e.cookie, Rc::as_ptr(&region.bytes) as usize));
                        if self.behave == Behave::WriteRegion && !region.writable {
                            self.end(Ending::Faulted);
                            return true;
                        }
                        let out = match self.behave {
                            Behave::Garbage => vec![0x5a; record.len()],
                            _ => record,
                        };
                        (e, out)
                    }
                },
            };
            did = true;
            let ring = self.ring.as_mut().expect("attached");
            let w = match ring.window(&view) {
                Ok(Some(w)) => w,
                Ok(None) => {
                    let seen = view.load(layout::FREE_HEAD);
                    self.pending = Some((entry, out));
                    if ring.mode == Mode::Unmap && self.posted_since_put > 0 {
                        self.hand_back();
                        return true;
                    }
                    self.wait(layout::WAIT_WINDOW, seen);
                    return true;
                }
                Err(_) => {
                    self.end(Ending::Faulted);
                    return true;
                }
            };
            let at = ring.geometry.window_at(w);
            if !self.write_window(at, &out) {
                self.end(Ending::Faulted);
                return true;
            }
            let ring = self.ring.as_mut().expect("attached");
            let window = if self.behave == Behave::StrayWindow {
                ring.geometry.windows
            } else {
                w
            };
            ring.post(
                &view,
                OutEntry {
                    window,
                    offset: 0,
                    len: out.len() as u64,
                    cookie: entry.cookie,
                },
            );
            if self.behave == Behave::BadHead {
                view.store(layout::OUT_HEAD, u64::MAX / 2);
            }
            self.last_posted = self.rings.clone().map(|m| (m, at));
            self.posted_since_put += 1;
            self.outputs += 1;
            if self.behave == Behave::Crash {
                self.end(Ending::Killed);
                return true;
            }
            ring.release(&view);
            // The engine's bell: a wake only when it sleeps on it. A task that waits on port
            // 0 rings it too; the engine answers its waits there.
            if ring.ring(&view) {
                self.engine_wakes += 1;
            }
        }
    }

    /// Writes into the window of its last record, after it was posted: whether the write
    /// went through (it must not, once the engine has checked the record). The task reaches
    /// the record only while it still maps the object the record is in.
    pub fn write_after_post(&mut self) -> bool {
        let Some((posted, at)) = self.last_posted.clone() else {
            return false;
        };
        let mapped = self
            .output
            .borrow()
            .current
            .as_ref()
            .is_some_and(|(m, ..)| Rc::ptr_eq(&m.bytes, &posted.bytes));
        mapped && self.write_window(at, &[0xee])
    }
}

/// A model with `windows_protect` answering or not.
pub fn model(protect: bool) -> Model {
    Model {
        protect,
        ..Model::default()
    }
}
