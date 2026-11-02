//! The data-plane port on the Moruna kernel (SDD 29 f.1 to f.6): [`KernelPlane`].
//!
//! Ownership moves as handles move. A window is a memory object of the arena pushed into
//! a task's output slot (its handle leaves the engine); the task writes a record into it
//! and puts it; the kernel unmaps it from the task and gives the handle back with the
//! event; the record is checked here, and becomes a batch the engine holds. Offering the
//! batch to a consumer pushes a duplicate of the handle without `WRITE`, so a batch fanned
//! out to three tasks is one set of frames mapped read only three times. Each consumer
//! releases its share when it takes its next batch or ends; when the last share and the
//! engine's own hold are gone, the object goes back to the arena's pool, once, as the next
//! window (contracts CT-I15, placement PL-I16).

use alloc::{collections::BTreeMap, collections::VecDeque, vec, vec::Vec};

use moruna_abi::{
    Error, limits,
    plane::{reply, request},
    read_u32, read_u64, write_u64,
};

use crate::arena::{Arena, pages};
use crate::calls::{Calls, Ready, Taken};
use crate::port::{BatchId, CallId, DataPlane, Event, PlaneError, Residency, Shape, TaskId};
use crate::verify::verify;

/// The most slots and port channels one `next` waits on: a `wait`'s items.
pub const WAITED_MAX: usize = limits::WAIT_MAX_ITEMS;

struct SlotRec<S> {
    slot: S,
    /// Its task has ended and every event is taken.
    drained: bool,
}

struct TaskRec<S, Ch> {
    inputs: Vec<SlotRec<S>>,
    outputs: Vec<SlotRec<S>>,
    port: Option<Ch>,
    port_done: bool,
    ended: bool,
}

impl<S, Ch> TaskRec<S, Ch> {
    fn open_slots(&self) -> impl Iterator<Item = &SlotRec<S>> {
        self.inputs
            .iter()
            .chain(self.outputs.iter())
            .filter(|s| !s.drained)
    }

    fn all_drained(&self) -> bool {
        self.open_slots().next().is_none() && (self.port.is_none() || self.port_done)
    }
}

struct BatchRec {
    /// The arena object holding it.
    object: u64,
    offset: u64,
    len: u64,
    /// Shares held by tasks.
    shares: u64,
    /// The engine's own hold.
    held: bool,
}

/// The engine's side of the data plane over the kernel's calls.
pub struct KernelPlane<C: Calls> {
    calls: C,
    arena: Arena<C::Memory>,
    tasks: BTreeMap<u32, TaskRec<C::Slot, C::Channel>>,
    next_task: u32,
    batches: BTreeMap<u64, BatchRec>,
    next_batch: u64,
    events: VecDeque<Event>,
    /// Bytes of windows granted and not yet back.
    windows: u64,
    /// A port request's buffer, reused.
    buffer: Vec<u8>,
}

fn kernel(e: Error) -> PlaneError {
    PlaneError::Kernel(e)
}

impl<C: Calls> KernelPlane<C> {
    /// A plane over `calls` whose arena may make `budget` bytes of memory objects.
    pub fn new(calls: C, budget: u64) -> KernelPlane<C> {
        KernelPlane {
            calls,
            arena: Arena::new(budget),
            tasks: BTreeMap::new(),
            next_task: 1,
            batches: BTreeMap::new(),
            next_batch: 1,
            events: VecDeque::new(),
            windows: 0,
            buffer: Vec::new(),
        }
    }

    /// The kernel calls, for the engine's other uses of them.
    pub fn calls(&mut self) -> &mut C {
        &mut self.calls
    }

    /// The engine's ends of `task`'s slots, inputs first, in the order `process_plane`
    /// numbers them.
    pub fn slots(&self, task: TaskId) -> Result<Vec<&C::Slot>, PlaneError> {
        let t = self.tasks.get(&task.0).ok_or(PlaneError::Unknown("task"))?;
        Ok(t.inputs
            .iter()
            .chain(t.outputs.iter())
            .map(|s| &s.slot)
            .collect())
    }

    /// The memory object holding `batch`, where it starts in it, and its length: what a
    /// sink writes to the disk from, and what a test compares frames of.
    pub fn batch(&self, batch: BatchId) -> Result<(&C::Memory, u64, u64), PlaneError> {
        let b = self
            .batches
            .get(&batch.0)
            .ok_or(PlaneError::Unknown("batch"))?;
        let memory = self
            .arena
            .handle(b.object)
            .ok_or(PlaneError::Unknown("a batch whose object a slot holds"))?;
        Ok((memory, b.offset, b.len))
    }

    /// A record the engine itself has (a source's, a fixture): written into an arena
    /// object, checked as a task's record is, and held as a batch.
    pub fn load(&mut self, record: &[u8]) -> Result<(BatchId, u64), PlaneError> {
        self.load_with(record.len() as u64, |calls, memory| {
            calls.write(memory, 0, record)
        })
    }

    /// A record of `len` bytes the engine writes itself, with `write`, into an arena
    /// object of at least that size, then checked and held as a batch, as [`Self::load`]:
    /// how a source or a stage lays a batch out where its tasks will map it, with one copy
    /// of each buffer and no record built beside it (SDD 30 f.6). A refusal, or a write
    /// that fails, gives the object back to the arena.
    pub fn load_with(
        &mut self,
        len: u64,
        write: impl FnOnce(&mut C, &C::Memory) -> Result<(), Error>,
    ) -> Result<(BatchId, u64), PlaneError> {
        let calls = &mut self.calls;
        let (object, memory, _) = self.arena.take(len, |n| calls.memory(n))?;
        let written = write(&mut self.calls, &memory);
        let checked = match written {
            Ok(()) => {
                let calls = &mut self.calls;
                verify(len, |at, buf| calls.read(&memory, at, buf))
            }
            Err(e) => {
                self.arena.untake(object, memory)?;
                return Err(kernel(e));
            }
        };
        self.arena.hold(object, memory)?;
        let record = match checked {
            Ok(record) => record,
            Err(reason) => {
                self.arena.free(object)?;
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
            .map(|t| t.open_slots().count() + usize::from(t.port.is_some() && !t.port_done))
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
        self.arena.free(object)?;
        self.events.push_back(Event::Freed {
            batch: BatchId(batch),
        });
        Ok(())
    }

    /// A record `task` put on `output`, in arena object `object`: checked, then a batch,
    /// or refused and the task ended.
    fn arrived(
        &mut self,
        task: u32,
        output: u32,
        object: u64,
        memory: C::Memory,
        offset: u64,
        len: u64,
    ) -> Result<(), PlaneError> {
        let checked = {
            let calls = &mut self.calls;
            verify(len, |at, buf| calls.read(&memory, offset + at, buf))
        };
        self.arena.hold(object, memory)?;
        self.windows = self.windows.saturating_sub(self.arena.bytes(object));
        match checked {
            Ok(record) => {
                let id = self.next_batch;
                self.next_batch += 1;
                self.batches.insert(
                    id,
                    BatchRec {
                        object,
                        offset,
                        len,
                        shares: 0,
                        held: true,
                    },
                );
                self.events.push_back(Event::Put {
                    task: TaskId(task),
                    output,
                    batch: BatchId(id),
                    rows: record.rows,
                    bytes: len,
                });
            }
            Err(reason) => {
                self.arena.free(object)?;
                let t = self.tasks.get(&task).ok_or(PlaneError::Unknown("task"))?;
                let slot = t
                    .outputs
                    .get(output as usize)
                    .ok_or(PlaneError::Unknown("output"))?;
                match self.calls.refuse(&slot.slot, &reason) {
                    // The task may have ended on its own meanwhile.
                    Ok(()) | Err(Error::BadState) => {}
                    Err(e) => return Err(kernel(e)),
                }
                self.events.push_back(Event::Refused {
                    task: TaskId(task),
                    output,
                    reason,
                });
            }
        }
        Ok(())
    }

    /// Takes every event every open slot has, and every port request, into the queue.
    fn poll(&mut self) -> Result<(), PlaneError> {
        let ids: Vec<u32> = self.tasks.keys().copied().collect();
        for id in ids {
            self.poll_task(id)?;
        }
        Ok(())
    }

    fn poll_task(&mut self, id: u32) -> Result<(), PlaneError> {
        let (inputs, outputs) = match self.tasks.get(&id) {
            Some(t) if !t.ended => (t.inputs.len(), t.outputs.len()),
            _ => return Ok(()),
        };
        for which in 0..inputs + outputs {
            let output = which >= inputs;
            let index = if output { which - inputs } else { which };
            loop {
                let taken = {
                    let Some(t) = self.tasks.get_mut(&id) else {
                        return Ok(());
                    };
                    let rec = if output {
                        &mut t.outputs[index]
                    } else {
                        &mut t.inputs[index]
                    };
                    if rec.drained {
                        break;
                    }
                    match self.calls.take(&rec.slot) {
                        Ok(taken) => taken,
                        Err(Error::ShouldWait) => break,
                        Err(Error::PeerClosed) => {
                            rec.drained = true;
                            break;
                        }
                        Err(e) => return Err(kernel(e)),
                    }
                };
                match taken {
                    Taken::Released { cookie } => {
                        if let Some(b) = self.batches.get_mut(&cookie) {
                            b.shares = b.shares.saturating_sub(1);
                        }
                        self.events.push_back(Event::Released {
                            task: TaskId(id),
                            input: index as u32,
                            batch: BatchId(cookie),
                        });
                        self.settle(cookie)?;
                    }
                    Taken::Put {
                        cookie,
                        memory,
                        offset,
                        len,
                    } => self.arrived(id, index as u32, cookie, memory, offset, len)?,
                    Taken::Returned { cookie, memory } => {
                        self.windows = self.windows.saturating_sub(self.arena.bytes(cookie));
                        self.arena.untake(cookie, memory)?;
                    }
                }
            }
        }
        self.poll_port(id)?;
        if let Some(t) = self.tasks.get_mut(&id)
            && t.all_drained()
        {
            t.ended = true;
            self.tasks.remove(&id);
            self.events.push_back(Event::Ended { task: TaskId(id) });
        }
        Ok(())
    }

    fn poll_port(&mut self, id: u32) -> Result<(), PlaneError> {
        if self.buffer.len() < limits::CHANNEL_MAX_BYTES {
            self.buffer.resize(limits::CHANNEL_MAX_BYTES, 0);
        }
        loop {
            let Some(t) = self.tasks.get_mut(&id) else {
                return Ok(());
            };
            let Some(port) = &t.port else {
                return Ok(());
            };
            if t.port_done {
                return Ok(());
            }
            let n = match self.calls.receive(port, &mut self.buffer) {
                Ok(n) => n,
                Err(Error::ShouldWait) => return Ok(()),
                Err(Error::PeerClosed) => {
                    t.port_done = true;
                    return Ok(());
                }
                Err(e) => return Err(kernel(e)),
            };
            let message = &self.buffer[..n];
            let kind = read_u32(message, request::KIND);
            let token = read_u64(message, request::TOKEN);
            let port_number = read_u64(message, request::PORT);
            let len = read_u64(message, request::LEN);
            match (kind, token, port_number, len) {
                (Some(request::CALL), Some(token), Some(port_number), Some(len))
                    if request::SIZE as u64 + len == n as u64 =>
                {
                    self.events.push_back(Event::Call {
                        call: CallId {
                            task: TaskId(id),
                            token,
                        },
                        port: port_number,
                        request: message[request::SIZE..].to_vec(),
                    });
                }
                // The kernel writes every request; anything else is not one.
                _ => return Err(PlaneError::Unknown("a port message that is no request")),
            }
        }
    }
}

impl<C: Calls> DataPlane for KernelPlane<C> {
    /// The kernel's end of the task's port channel, which `process_plane` takes; the
    /// slots are [`KernelPlane::slots`].
    type Task = Option<C::Channel>;

    fn open(&mut self, shape: Shape) -> Result<(TaskId, Self::Task), PlaneError> {
        let count = shape.inputs as usize + shape.outputs as usize;
        if count > limits::PLANE_SLOTS {
            return Err(PlaneError::Limit("slots of one task"));
        }
        if self.waited() + count + usize::from(shape.port) > WAITED_MAX {
            return Err(PlaneError::Limit("slots and ports waited on at once"));
        }
        let mut inputs = Vec::new();
        for _ in 0..shape.inputs {
            inputs.push(SlotRec {
                slot: self.calls.slot(false).map_err(kernel)?,
                drained: false,
            });
        }
        let mut outputs = Vec::new();
        for _ in 0..shape.outputs {
            outputs.push(SlotRec {
                slot: self.calls.slot(true).map_err(kernel)?,
                drained: false,
            });
        }
        let (port, end) = if shape.port {
            let (ours, theirs) = self.calls.channel().map_err(kernel)?;
            (Some(ours), Some(theirs))
        } else {
            (None, None)
        };
        let id = self.next_task;
        self.next_task += 1;
        self.tasks.insert(
            id,
            TaskRec {
                inputs,
                outputs,
                port,
                port_done: false,
                ended: false,
            },
        );
        Ok((TaskId(id), end))
    }

    fn offer(&mut self, task: TaskId, input: u32, batch: BatchId) -> Result<(), PlaneError> {
        let b = self
            .batches
            .get(&batch.0)
            .ok_or(PlaneError::Unknown("batch"))?;
        let (object, offset, len) = (b.object, b.offset, b.len);
        let t = self.tasks.get(&task.0).ok_or(PlaneError::Unknown("task"))?;
        let slot = &t
            .inputs
            .get(input as usize)
            .ok_or(PlaneError::Unknown("input"))?
            .slot;
        let held = self
            .arena
            .handle(object)
            .ok_or(PlaneError::Unknown("a batch whose object a slot holds"))?;
        let share = self.calls.share(held).map_err(kernel)?;
        match self.calls.push(slot, share, offset, len, batch.0) {
            Ok(()) => {}
            Err((Error::ShouldWait, _)) => return Err(PlaneError::Full),
            Err((Error::BadState, _)) => return Err(PlaneError::Closed),
            Err((e, _)) => return Err(kernel(e)),
        }
        if let Some(b) = self.batches.get_mut(&batch.0) {
            b.shares += 1;
        }
        Ok(())
    }

    fn finish(&mut self, task: TaskId, input: u32) -> Result<(), PlaneError> {
        let t = self.tasks.get(&task.0).ok_or(PlaneError::Unknown("task"))?;
        let slot = &t
            .inputs
            .get(input as usize)
            .ok_or(PlaneError::Unknown("input"))?
            .slot;
        self.calls.close(slot).map_err(kernel)
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
        let calls = &mut self.calls;
        let (id, memory, size) = self.arena.take(len, |n| calls.memory(n))?;
        let t = self.tasks.get(&task.0).ok_or(PlaneError::Unknown("task"))?;
        let slot = &t.outputs[output as usize].slot;
        match self.calls.push(slot, memory, 0, len, id) {
            Ok(()) => {
                self.windows += size;
                Ok(())
            }
            Err((e, memory)) => {
                self.arena.untake(id, memory)?;
                Err(match e {
                    Error::ShouldWait => PlaneError::Full,
                    Error::BadState => PlaneError::Closed,
                    e => kernel(e),
                })
            }
        }
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
            let mut slots = Vec::new();
            let mut channels = Vec::new();
            for t in self.tasks.values() {
                slots.extend(t.open_slots().map(|s| &s.slot));
                if let Some(c) = &t.port
                    && !t.port_done
                {
                    channels.push(c);
                }
            }
            if slots.is_empty() && channels.is_empty() {
                return Ok(None);
            }
            if self
                .calls
                .wait(&slots, &channels, deadline)
                .map_err(kernel)?
                == Ready::TimedOut
            {
                return Ok(None);
            }
        }
    }

    fn answer(&mut self, call: CallId, reply: Result<&[u8], Error>) -> Result<(), PlaneError> {
        let mut header = vec![0u8; reply::SIZE];
        write_u64(&mut header, reply::TOKEN, call.token);
        let memory = match reply {
            Err(e) => {
                write_u64(&mut header, reply::STATUS, e.code() as u64);
                None
            }
            Ok([]) => None,
            Ok(bytes) => {
                let size =
                    pages(bytes.len() as u64).ok_or(PlaneError::Limit("a reply past u64"))?;
                let made = self.calls.memory(size).map_err(kernel)?;
                self.calls.write(&made, 0, bytes).map_err(kernel)?;
                let shared = self.calls.share(&made).map_err(kernel)?;
                write_u64(&mut header, reply::LEN, bytes.len() as u64);
                Some(shared)
            }
        };
        let t = self
            .tasks
            .get(&call.task.0)
            .ok_or(PlaneError::Unknown("task"))?;
        let port = t.port.as_ref().ok_or(PlaneError::Unknown("port"))?;
        match self.calls.send(port, &header, memory) {
            Ok(()) => Ok(()),
            Err(Error::PeerClosed) => Err(PlaneError::Closed),
            Err(e) => Err(kernel(e)),
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
