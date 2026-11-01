//! The engine's side of the data plane on Linux: [`LinuxPlane`], the port's adapter for tasks
//! that are sandboxed processes (SDD 29 d.4, KA2).
//!
//! Every batch and window is a memfd. A window granted is a fresh memfd the task maps shared
//! and writable when it asks for it; at its put the task unmaps it, and the engine seals it
//! (`F_SEAL_WRITE`, `F_SEAL_SHRINK`, `F_SEAL_GROW`, `F_SEAL_SEAL`) before anything reads it.
//! Linux refuses the seal while any process maps the memfd writable, and after it no process
//! can write a byte of it by any descriptor or mapping, so the record the engine then checks
//! (`moruna_plane::verify`, the check `KernelPlane` runs) is exactly what every consumer maps:
//! DP1 on Linux. A consumer is passed the sealed memfd and maps it private and read only: the
//! object's own pages, no copy, and a write faults. A batch offered to three tasks is three
//! descriptors of one memfd; it is freed once, after the last release and the engine's drop.
//!
//! An object a task has held is never given to another task: the task's descriptor of it may
//! outlive its put or its end. Sealed objects are read only for good, so a freed batch and a
//! window a task held are retired from the arena (closed); only a window no task ever saw goes
//! back to its pool. The budget is the same as the kernel's: what is in use, pooled or not.
//!
//! The events, refusals and bounds are `KernelPlane`'s, which the port's conformance suite
//! (`moruna_plane::conformance`) runs both adapters through.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use moruna_abi::{Error, limits, plane::request};
use moruna_plane::arena::{Arena, PAGE, pages};
use moruna_plane::plane::WAITED_MAX;
use moruna_plane::verify::verify;
use moruna_plane::{BatchId, CallId, DataPlane, Event, PlaneError, Residency, Shape, TaskId};

use crate::sys::{self, Access, Map, SealError};
use crate::wire::{self, Header};

/// The most messages one task's socket is read for in one pass, so one task cannot keep the
/// others waiting.
const MESSAGES_PER_PASS: usize = 256;

/// The kernel error that names what Linux said: the code the port speaks in.
pub fn code(e: &io::Error) -> Error {
    if e.kind() == io::ErrorKind::WouldBlock {
        return Error::ShouldWait;
    }
    match e.raw_os_error() {
        Some(libc::ENOMEM) => Error::NoMemory,
        Some(libc::EMFILE | libc::ENFILE | libc::ENOSPC | libc::EMSGSIZE) => Error::Limit,
        Some(libc::EBADF | libc::ENOTSOCK) => Error::BadHandle,
        Some(libc::EINVAL) => Error::InvalidArgs,
        Some(libc::EPIPE | libc::ECONNRESET | libc::ENOTCONN) => Error::PeerClosed,
        Some(libc::EFAULT) => Error::Fault,
        Some(libc::EACCES | libc::EPERM) => Error::AccessDenied,
        Some(libc::EAGAIN) => Error::ShouldWait,
        Some(libc::ETIMEDOUT) => Error::TimedOut,
        _ => Error::BadState,
    }
}

/// A window granted: the arena object, its memfd, the object's size and the length the task
/// is given.
struct Window {
    object: u64,
    fd: OwnedFd,
    size: u64,
    len: u64,
}

#[derive(Default)]
struct InSlot {
    ready: VecDeque<u64>,
    current: Option<u64>,
    closed: bool,
    /// The task asked for the next batch and has not had an answer.
    waiting: bool,
}

#[derive(Default)]
struct OutSlot {
    ready: VecDeque<Window>,
    current: Option<Window>,
    waiting: bool,
}

struct TaskRec {
    socket: OwnedFd,
    pidfd: Option<OwnedFd>,
    inputs: Vec<InSlot>,
    outputs: Vec<OutSlot>,
    port: bool,
    /// The engine ended the task: it is not served, and its end is waited for.
    cut: bool,
}

struct BatchRec {
    object: u64,
    offset: u64,
    len: u64,
    shares: u64,
    held: bool,
}

/// The data-plane port over sealed memfds and a `SOCK_SEQPACKET` socket per task.
pub struct LinuxPlane {
    arena: Arena<OwnedFd>,
    tasks: BTreeMap<u32, TaskRec>,
    next_task: u32,
    batches: BTreeMap<u64, BatchRec>,
    next_batch: u64,
    events: VecDeque<Event>,
    /// Bytes of windows granted and not yet back.
    windows: u64,
    /// A message's buffer, reused.
    buffer: Vec<u8>,
    /// What Linux last said when one of its calls failed the plane, for the error's words.
    last_error: Option<String>,
}

impl LinuxPlane {
    /// A plane whose arena may make `budget` bytes of memfds.
    pub fn new(budget: u64) -> LinuxPlane {
        LinuxPlane {
            arena: Arena::new(budget),
            tasks: BTreeMap::new(),
            next_task: 1,
            batches: BTreeMap::new(),
            next_batch: 1,
            events: VecDeque::new(),
            windows: 0,
            buffer: vec![0u8; wire::MESSAGE_MAX],
            last_error: None,
        }
    }

    /// The process running `task`: the engine ends it through this pidfd when its record is
    /// refused. A task with none is cut off instead (its socket shut down).
    pub fn attach(&mut self, task: TaskId, pidfd: OwnedFd) -> Result<(), PlaneError> {
        let t = self
            .tasks
            .get_mut(&task.0)
            .ok_or(PlaneError::Unknown("task"))?;
        t.pidfd = Some(pidfd);
        Ok(())
    }

    /// The memfd holding `batch`, where the record starts in it, and its length.
    pub fn batch(&self, batch: BatchId) -> Result<(BorrowedFd<'_>, u64, u64), PlaneError> {
        let b = self
            .batches
            .get(&batch.0)
            .ok_or(PlaneError::Unknown("batch"))?;
        let fd = self.arena.handle(b.object).ok_or(PlaneError::Unknown(
            "a batch whose object the arena does not hold",
        ))?;
        Ok((fd.as_fd(), b.offset, b.len))
    }

    /// `batch` mapped read only into the engine: its pages, no copy. The mapping outlives the
    /// batch's hold if it is kept; its pages go when both are gone.
    pub fn map(&mut self, batch: BatchId) -> Result<Map, PlaneError> {
        let (fd, offset, len) = self.batch(batch)?;
        let len = usize::try_from(len).map_err(|_| PlaneError::Limit("a batch past usize"))?;
        let mapped = Map::new(fd, offset, len, Access::Read);
        mapped.map_err(|e| self.system(e))
    }

    /// What Linux last said when one of its calls failed the plane.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn system(&mut self, e: io::Error) -> PlaneError {
        let error = PlaneError::Kernel(code(&e));
        self.last_error = Some(e.to_string());
        error
    }

    /// A record the engine itself has, checked as a task's is and held as a batch.
    pub fn load(&mut self, record: &[u8]) -> Result<(BatchId, u64), PlaneError> {
        self.load_with(record.len() as u64, |window| {
            window[..record.len()].copy_from_slice(record);
            Ok(())
        })
    }

    /// A record of `len` bytes the engine writes itself with `fill`, straight into the memfd
    /// that becomes the batch (no copy before it): the mapping `fill` writes is gone before
    /// the seal, and the record is checked after it.
    pub fn load_with(
        &mut self,
        len: u64,
        fill: impl FnOnce(&mut [u8]) -> Result<(), String>,
    ) -> Result<(BatchId, u64), PlaneError> {
        let mut made = None;
        let (object, fd, _) = self.arena.take(len, |n| {
            sys::memfd(n).map_err(|e| {
                let c = code(&e);
                made = Some(e.to_string());
                c
            })
        })?;
        if let Some(said) = made {
            self.last_error = Some(said);
        }
        let size = usize::try_from(len).map_err(|_| PlaneError::Limit("a record past usize"))?;
        let filled = Map::new(fd.as_fd(), 0, size, Access::Write)
            .map_err(|e| e.to_string())
            .and_then(|mut m| fill(m.bytes_mut()));
        if let Err(why) = filled {
            self.arena.untake(object, fd)?;
            return Err(PlaneError::Refused(format!(
                "the engine's record could not be written: {why}"
            )));
        }
        if let Err(e) = sys::seal(fd.as_fd()) {
            self.arena.hold(object, fd)?;
            drop(self.arena.retire(object)?);
            return Err(PlaneError::Refused(format!(
                "the engine's record could not be sealed: {e:?}"
            )));
        }
        let checked = verify(len, |at, buf| {
            sys::pread_exact(fd.as_fd(), at, buf).map_err(|e| code(&e))
        });
        self.arena.hold(object, fd)?;
        let record = match checked {
            Ok(record) => record,
            Err(reason) => {
                self.arena.retire(object)?;
                return Err(PlaneError::Refused(reason));
            }
        };
        let id = self.next_batch;
        self.next_batch += 1;
        self.batches.insert(
            id,
            BatchRec {
                object,
                offset: 0,
                len,
                shares: 0,
                held: true,
            },
        );
        Ok((BatchId(id), record.rows))
    }

    fn waited(&self) -> usize {
        self.tasks
            .values()
            .map(|t| t.inputs.len() + t.outputs.len() + usize::from(t.port))
            .sum()
    }

    /// A share or the hold of `batch` has gone: free it once nothing holds it.
    fn settle(&mut self, batch: u64) -> Result<(), PlaneError> {
        let Some(b) = self.batches.get(&batch) else {
            return Ok(());
        };
        if b.shares > 0 || b.held {
            return Ok(());
        }
        let object = b.object;
        self.batches.remove(&batch);
        drop(self.arena.retire(object)?);
        self.events.push_back(Event::Freed {
            batch: BatchId(batch),
        });
        Ok(())
    }

    /// `task`'s share of `batch` on `input` has ended.
    fn release(&mut self, task: u32, input: usize, batch: u64) -> Result<(), PlaneError> {
        if let Some(b) = self.batches.get_mut(&batch) {
            b.shares = b.shares.saturating_sub(1);
        }
        self.events.push_back(Event::Released {
            task: TaskId(task),
            input: input as u32,
            batch: BatchId(batch),
        });
        self.settle(batch)
    }

    /// Sends `header` and `fd` to `task`. A task gone is not an error here (its end is read
    /// from its socket); a task whose socket is full does not read its answers and is ended.
    fn say(
        &mut self,
        task: u32,
        header: Header,
        fd: Option<BorrowedFd<'_>>,
    ) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get(&task) else {
            return Ok(());
        };
        match sys::send(t.socket.as_fd(), &header.bytes(), fd, false) {
            Ok(()) => Ok(()),
            Err(e) if code(&e) == Error::PeerClosed => Ok(()),
            Err(e) if code(&e) == Error::ShouldWait => {
                self.last_error = Some(format!("task {task} does not read its answers: {e}"));
                self.cut(task)
            }
            Err(e) => Err(self.system(e)),
        }
    }

    /// Ends `task` from the engine's side: killed through its pidfd, or cut off. What it holds
    /// comes back when its socket says it has gone.
    fn cut(&mut self, task: u32) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get_mut(&task) else {
            return Ok(());
        };
        t.cut = true;
        let ended = match &t.pidfd {
            Some(pidfd) => sys::kill(pidfd.as_fd()),
            None => sys::shutdown(t.socket.as_fd()),
        };
        ended.map_err(|e| self.system(e))
    }

    /// The record `task` put on output `k` was refused: the task is ended with `reason`.
    fn refuse(&mut self, task: u32, k: usize, reason: String) -> Result<(), PlaneError> {
        self.events.push_back(Event::Refused {
            task: TaskId(task),
            output: k as u32,
            reason,
        });
        self.cut(task)
    }

    /// Answers a waiting `batch_get` on input `n` of `task`, when it can be.
    fn serve_input(&mut self, task: u32, n: usize) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get_mut(&task) else {
            return Ok(());
        };
        let Some(slot) = t.inputs.get_mut(n) else {
            return Ok(());
        };
        if !slot.waiting || t.cut {
            return Ok(());
        }
        let Some(batch) = slot.ready.pop_front() else {
            if slot.closed {
                slot.waiting = false;
                return self.say(task, Header::new(wire::END, n as u32), None);
            }
            return Ok(());
        };
        slot.current = Some(batch);
        slot.waiting = false;
        let b = self
            .batches
            .get(&batch)
            .ok_or(PlaneError::Unknown("batch"))?;
        let mut header = Header::new(wire::BATCH, n as u32);
        header.a = b.offset;
        header.b = b.len;
        let object = b.object;
        let fd = self
            .arena
            .handle(object)
            .ok_or(PlaneError::Unknown(
                "a batch whose object the arena does not hold",
            ))?
            .try_clone()
            .map_err(|e| self.system(e))?;
        self.say(task, header, Some(fd.as_fd()))
    }

    /// Answers a waiting `batch_get` on output `k` of `task`, when a window is there.
    fn serve_output(&mut self, task: u32, k: usize) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get_mut(&task) else {
            return Ok(());
        };
        let slot_number = (t.inputs.len() + k) as u32;
        let Some(slot) = t.outputs.get_mut(k) else {
            return Ok(());
        };
        if !slot.waiting || t.cut {
            return Ok(());
        }
        if slot.current.is_none() {
            let Some(w) = slot.ready.pop_front() else {
                return Ok(());
            };
            slot.current = Some(w);
        }
        slot.waiting = false;
        let Some(w) = &slot.current else {
            return Ok(());
        };
        let mut header = Header::new(wire::WINDOW, slot_number);
        header.b = w.len;
        let fd = w.fd.try_clone().map_err(|e| self.system(e))?;
        self.say(task, header, Some(fd.as_fd()))
    }

    /// `batch_get` on `slot`.
    fn on_get(&mut self, task: u32, slot: u32) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get_mut(&task) else {
            return Ok(());
        };
        let n = slot as usize;
        let inputs = t.inputs.len();
        if n < inputs {
            // The slot's batch is released as the call starts, before it waits.
            let previous = t.inputs[n].current.take();
            t.inputs[n].waiting = true;
            if let Some(batch) = previous {
                self.release(task, n, batch)?;
            }
            return self.serve_input(task, n);
        }
        if n - inputs < t.outputs.len() {
            t.outputs[n - inputs].waiting = true;
            return self.serve_output(task, n - inputs);
        }
        self.say(task, Header::error(wire::GET, slot, Error::BadHandle), None)
    }

    /// `batch_put` of `len` bytes at `offset` of `slot`'s window.
    fn on_put(&mut self, task: u32, slot: u32, offset: u64, len: u64) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get_mut(&task) else {
            return Ok(());
        };
        let n = slot as usize;
        let inputs = t.inputs.len();
        // The kernel's order: a slot the plane does not have, then the arguments, then the
        // window.
        let refused = if n >= inputs + t.outputs.len() {
            Some(Error::BadHandle)
        } else if n < inputs || len == 0 || !offset.is_multiple_of(PAGE) {
            Some(Error::InvalidArgs)
        } else {
            match &t.outputs[n - inputs].current {
                None => Some(Error::BadState),
                Some(w) if offset.checked_add(len).is_none_or(|e| e > w.len) => {
                    Some(Error::OutOfRange)
                }
                Some(_) => None,
            }
        };
        if let Some(e) = refused {
            return self.say(task, Header::error(wire::PUT, slot, e), None);
        }
        let k = n - inputs;
        let Some(w) = t.outputs[k].current.take() else {
            return Ok(());
        };
        self.windows = self.windows.saturating_sub(w.size);
        // The task has lost write access once this succeeds, by any descriptor or mapping.
        let sealed = match sys::seal(w.fd.as_fd()) {
            Ok(()) => match sys::stat(w.fd.as_fd()) {
                Ok((size, _, _)) if size == w.size => Ok(()),
                Ok((size, _, _)) => Err(format!(
                    "the task changed its window on slot {slot} from {} to {size} bytes before \
                     it put the record",
                    w.size
                )),
                Err(e) => Err(format!("the window on slot {slot} could not be read: {e}")),
            },
            Err(SealError::Mapped) => Err(format!(
                "the task kept its window on slot {slot} mapped writable after it put the \
                 record, so the record could not be sealed"
            )),
            Err(SealError::Forbidden) => Err(format!(
                "the task sealed its window on slot {slot} against the engine before it put \
                 the record"
            )),
            Err(SealError::Io(e)) => Err(format!(
                "the window on slot {slot} could not be sealed: {e}"
            )),
        };
        if let Err(reason) = sealed {
            self.arena.hold(w.object, w.fd)?;
            drop(self.arena.retire(w.object)?);
            return self.refuse(task, k, reason);
        }
        // The put returns as soon as the window is the engine's; the check is off its path.
        self.say(task, Header::new(wire::PUT_DONE, slot), None)?;
        let checked = verify(len, |at, buf| {
            sys::pread_exact(w.fd.as_fd(), offset + at, buf).map_err(|e| code(&e))
        });
        self.arena.hold(w.object, w.fd)?;
        match checked {
            Ok(record) => {
                let id = self.next_batch;
                self.next_batch += 1;
                self.batches.insert(
                    id,
                    BatchRec {
                        object: w.object,
                        offset,
                        len,
                        shares: 0,
                        held: true,
                    },
                );
                self.events.push_back(Event::Put {
                    task: TaskId(task),
                    output: k as u32,
                    batch: BatchId(id),
                    rows: record.rows,
                    bytes: len,
                });
                Ok(())
            }
            Err(reason) => {
                drop(self.arena.retire(w.object)?);
                self.refuse(task, k, reason)
            }
        }
    }

    /// `port_call`: its header, and the bytes after it.
    fn on_call(&mut self, task: u32, header: Header, len: usize) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get(&task) else {
            return Ok(());
        };
        let mut reply = Header::new(wire::REPLY, 0);
        reply.a = header.a;
        let carried = len - wire::HEADER;
        let refused = if !t.port {
            Some(Error::BadHandle)
        } else if header.c > request::MAX as u64 {
            Some(Error::Limit)
        } else if header.c != carried as u64 {
            Some(Error::InvalidArgs)
        } else {
            None
        };
        if let Some(e) = refused {
            reply.b = u64::from(e.code());
            return self.say(task, reply, None);
        }
        self.events.push_back(Event::Call {
            call: CallId {
                task: TaskId(task),
                token: header.a,
            },
            port: header.b,
            request: self.buffer[wire::HEADER..len].to_vec(),
        });
        Ok(())
    }

    /// `task` has gone: every batch it held or had queued released, every window back, then
    /// its end.
    fn end(&mut self, task: u32) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.remove(&task) else {
            return Ok(());
        };
        for (n, slot) in t.inputs.into_iter().enumerate() {
            for batch in slot.current.into_iter().chain(slot.ready) {
                self.release(task, n, batch)?;
            }
        }
        for slot in t.outputs {
            // A window the task was given may still be in its hands: retired. One it never saw
            // goes back to the pool.
            if let Some(w) = slot.current {
                self.windows = self.windows.saturating_sub(w.size);
                self.arena.hold(w.object, w.fd)?;
                drop(self.arena.retire(w.object)?);
            }
            for w in slot.ready {
                self.windows = self.windows.saturating_sub(w.size);
                self.arena.untake(w.object, w.fd)?;
            }
        }
        self.events.push_back(Event::Ended { task: TaskId(task) });
        Ok(())
    }

    /// Reads what `task` has sent, answering what can be answered now.
    fn poll_task(&mut self, task: u32) -> Result<(), PlaneError> {
        for _ in 0..MESSAGES_PER_PASS {
            let Some(t) = self.tasks.get(&task) else {
                return Ok(());
            };
            let got = sys::recv(t.socket.as_fd(), &mut self.buffer, false);
            let cut = t.cut;
            let received = match got {
                Ok(r) => r,
                Err(e) if code(&e) == Error::ShouldWait => return Ok(()),
                Err(e) if code(&e) == Error::PeerClosed => return self.end(task),
                Err(e) => return Err(self.system(e)),
            };
            // A task passes the engine no descriptor: any it sent are closed here.
            drop(received.fds);
            if received.len == 0 {
                let t = self.tasks.get(&task).ok_or(PlaneError::Unknown("task"))?;
                match sys::hung_up(t.socket.as_fd()) {
                    Ok(true) => return self.end(task),
                    // An empty message is no request.
                    Ok(false) => continue,
                    Err(e) => return Err(self.system(e)),
                }
            }
            if cut {
                continue;
            }
            let Some(header) = Header::read(&self.buffer[..received.len]) else {
                continue;
            };
            if received.truncated {
                let mut refusal = Header::error(header.kind, header.slot, Error::Limit);
                if header.kind == wire::CALL {
                    refusal = Header::new(wire::REPLY, 0);
                    refusal.a = header.a;
                    refusal.b = u64::from(Error::Limit.code());
                }
                self.say(task, refusal, None)?;
                continue;
            }
            match header.kind {
                wire::GET => self.on_get(task, header.slot)?,
                wire::PUT => self.on_put(task, header.slot, header.a, header.b)?,
                wire::CALL => self.on_call(task, header, received.len)?,
                other => self.say(
                    task,
                    Header::error(other, header.slot, Error::BadSyscall),
                    None,
                )?,
            }
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<(), PlaneError> {
        let ids: Vec<u32> = self.tasks.keys().copied().collect();
        for id in ids {
            self.poll_task(id)?;
        }
        Ok(())
    }
}

impl DataPlane for LinuxPlane {
    /// The task's end of its plane socket: the process it is given to holds it as descriptor 3
    /// (`crate::task::Plane::take`).
    type Task = OwnedFd;

    fn open(&mut self, shape: Shape) -> Result<(TaskId, OwnedFd), PlaneError> {
        let count = shape.inputs as usize + shape.outputs as usize;
        if count > limits::PLANE_SLOTS {
            return Err(PlaneError::Limit("slots of one task"));
        }
        if self.waited() + count + usize::from(shape.port) > WAITED_MAX {
            return Err(PlaneError::Limit("slots and ports waited on at once"));
        }
        let (ours, theirs) = sys::seqpacket_pair().map_err(|e| self.system(e))?;
        let id = self.next_task;
        self.next_task += 1;
        self.tasks.insert(
            id,
            TaskRec {
                socket: ours,
                pidfd: None,
                inputs: (0..shape.inputs).map(|_| InSlot::default()).collect(),
                outputs: (0..shape.outputs).map(|_| OutSlot::default()).collect(),
                port: shape.port,
                cut: false,
            },
        );
        Ok((TaskId(id), theirs))
    }

    fn offer(&mut self, task: TaskId, input: u32, batch: BatchId) -> Result<(), PlaneError> {
        if !self.batches.contains_key(&batch.0) {
            return Err(PlaneError::Unknown("batch"));
        }
        let t = self
            .tasks
            .get_mut(&task.0)
            .ok_or(PlaneError::Unknown("task"))?;
        let cut = t.cut;
        let slot = t
            .inputs
            .get_mut(input as usize)
            .ok_or(PlaneError::Unknown("input"))?;
        if slot.closed || cut {
            return Err(PlaneError::Closed);
        }
        if slot.ready.len() + usize::from(slot.current.is_some()) >= limits::SLOT_DEPTH {
            return Err(PlaneError::Full);
        }
        slot.ready.push_back(batch.0);
        if let Some(b) = self.batches.get_mut(&batch.0) {
            b.shares += 1;
        }
        self.serve_input(task.0, input as usize)
    }

    fn finish(&mut self, task: TaskId, input: u32) -> Result<(), PlaneError> {
        let t = self
            .tasks
            .get_mut(&task.0)
            .ok_or(PlaneError::Unknown("task"))?;
        let slot = t
            .inputs
            .get_mut(input as usize)
            .ok_or(PlaneError::Unknown("input"))?;
        slot.closed = true;
        self.serve_input(task.0, input as usize)
    }

    fn grant(&mut self, task: TaskId, output: u32, bytes: u64) -> Result<(), PlaneError> {
        let len = pages(bytes)
            .filter(|l| *l > 0)
            .ok_or(PlaneError::Limit("a window of no bytes, or past u64"))?;
        if !self
            .tasks
            .get(&task.0)
            .is_some_and(|t| (output as usize) < t.outputs.len())
        {
            return Err(PlaneError::Unknown("output"));
        }
        let mut said = None;
        let taken = self.arena.take(len, |n| {
            sys::memfd(n).map_err(|e| {
                let c = code(&e);
                said = Some(e.to_string());
                c
            })
        });
        if said.is_some() {
            self.last_error = said;
        }
        let (object, fd, size) = taken?;
        let t = self
            .tasks
            .get_mut(&task.0)
            .ok_or(PlaneError::Unknown("task"))?;
        let cut = t.cut;
        let slot = &mut t.outputs[output as usize];
        let full = slot.ready.len() + usize::from(slot.current.is_some()) >= limits::SLOT_DEPTH;
        if cut || full {
            self.arena.untake(object, fd)?;
            return Err(if cut {
                PlaneError::Closed
            } else {
                PlaneError::Full
            });
        }
        slot.ready.push_back(Window {
            object,
            fd,
            size,
            len,
        });
        self.windows += size;
        self.serve_output(task.0, output as usize)
    }

    fn next(&mut self, deadline: u64) -> Result<Option<Event>, PlaneError> {
        loop {
            if let Some(e) = self.events.pop_front() {
                return Ok(Some(e));
            }
            self.poll()?;
            if let Some(e) = self.events.pop_front() {
                return Ok(Some(e));
            }
            if self.tasks.is_empty() {
                return Ok(None);
            }
            let ready = {
                let sockets: Vec<BorrowedFd<'_>> =
                    self.tasks.values().map(|t| t.socket.as_fd()).collect();
                sys::wait(&sockets, deadline)
            };
            match ready {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(e) => return Err(self.system(e)),
            }
        }
    }

    fn answer(&mut self, call: CallId, reply: Result<&[u8], Error>) -> Result<(), PlaneError> {
        let t = self
            .tasks
            .get(&call.task.0)
            .ok_or(PlaneError::Unknown("task"))?;
        if !t.port {
            return Err(PlaneError::Unknown("port"));
        }
        let mut header = Header::new(wire::REPLY, 0);
        header.a = call.token;
        let memory = match reply {
            Err(e) => {
                header.b = u64::from(e.code());
                None
            }
            Ok([]) => None,
            Ok(bytes) => {
                let size =
                    pages(bytes.len() as u64).ok_or(PlaneError::Limit("a reply past u64"))?;
                let made = sys::memfd(size)
                    .and_then(|fd| sys::pwrite_all(fd.as_fd(), 0, bytes).map(|()| fd))
                    .map_err(|e| self.system(e))?;
                sys::seal(made.as_fd()).map_err(|e| {
                    self.last_error = Some(format!("a reply could not be sealed: {e:?}"));
                    PlaneError::Kernel(Error::BadState)
                })?;
                header.c = bytes.len() as u64;
                Some(made)
            }
        };
        let t = self
            .tasks
            .get(&call.task.0)
            .ok_or(PlaneError::Unknown("task"))?;
        match sys::send(
            t.socket.as_fd(),
            &header.bytes(),
            memory.as_ref().map(|m| m.as_fd()),
            false,
        ) {
            Ok(()) => Ok(()),
            Err(e) if code(&e) == Error::PeerClosed => Err(PlaneError::Closed),
            Err(e) => Err(self.system(e)),
        }
    }

    fn drop_batch(&mut self, batch: BatchId) -> Result<(), PlaneError> {
        let b = self
            .batches
            .get_mut(&batch.0)
            .ok_or(PlaneError::Unknown("batch"))?;
        if !b.held {
            return Err(PlaneError::Unknown("a batch the engine no longer holds"));
        }
        b.held = false;
        self.settle(batch.0)
    }

    fn residency(&self) -> Residency {
        Residency {
            budget: self.arena.budget(),
            made: self.arena.made(),
            resident: self.batches.values().map(|b| b.len).sum(),
            batches: self.batches.len() as u64,
            shares: self.batches.values().map(|b| b.shares).sum(),
            windows: self.windows,
            pooled: self.arena.pooled(),
        }
    }
}
