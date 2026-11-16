//! The engine's side of one virtio-blk queue: operations planned into requests (`plan`), each
//! request a descriptor chain on `moruna-virtio`'s split queue, the device's answers taken off
//! its used ring and each operation finished when its last request is.
//!
//! Nothing here calls the kernel. The queue's rings, the slots' headers and bounce sectors and
//! the doorbell are given to it, so the same code drives the device on the Moruna kernel and a
//! model device in a test. A request's body is the operation's own memory, by DMA: its device
//! addresses come from [`Translate`], which on the kernel is the arena's pinned pages. Only an
//! edge (a sector the range starts or ends inside) passes through a slot's bounce sector.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use moruna_mmio::Region;
use moruna_virtio::{Buffer as Desc, Queue};

use super::plan::{self, Part, SECTOR};

/// Request types (virtio 1.2, 5.2.6).
pub const T_IN: u32 = 0;
/// A write.
pub const T_OUT: u32 = 1;
/// A flush of the device's write cache.
pub const T_FLUSH: u32 = 4;
/// The device's serial.
pub const T_GET_ID: u32 = 8;
/// The status a request that succeeded ends with.
pub const S_OK: u8 = 0;
/// The bytes of a serial (virtio 1.2, 5.2.6).
pub const ID_BYTES: usize = 20;
/// One slot of the slots' memory: the 16-byte header, the status byte at 16, the head bounce
/// sector at 512 and the tail bounce sector at 1024. Two slots to a page, so no part a device
/// sees crosses a page.
pub const SLOT: usize = 2048;
const STATUS: usize = 16;
const HEAD: usize = 512;
const TAIL: usize = 1024;

/// The device addresses of this process's memory.
pub trait Translate: Send + Sync {
    /// Appends the `(device address, length)` runs that cover `[addr, addr + len)`, in order;
    /// an error when some of it is memory no device may reach.
    fn runs(&self, addr: u64, len: u64, out: &mut Vec<(u64, u32)>) -> Result<(), String>;
}

/// What an operation does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Bytes of the disk into memory.
    Read,
    /// Bytes of memory onto the disk, whole sectors.
    Write,
    /// The device's write cache onto the disk.
    Flush,
    /// The device's serial, [`ID_BYTES`] bytes, into memory.
    Id,
}

/// How an operation ends: told once, with why it failed.
pub type Done = Box<dyn FnOnce(Result<(), String>) + Send>;

/// One operation as the engine is given it.
pub struct Op {
    /// What it does.
    pub kind: Kind,
    /// Its first byte on the disk.
    pub offset: u64,
    /// Its memory in this process: written by a read, read by a write. The caller keeps it
    /// alive and untouched until `done`, which owns it.
    pub addr: u64,
    /// Its length.
    pub len: u64,
    /// Told when it ends.
    pub done: Done,
}

/// What the engine has moved, for the run's report and the tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Operations finished.
    pub ops: u64,
    /// Requests the device returned.
    pub requests: u64,
    /// Doorbell writes.
    pub notifies: u64,
    /// Bytes the device moved straight to or from the operations' memory.
    pub dma_bytes: u64,
    /// Bytes copied between a bounce sector and an operation's memory.
    pub bounced_bytes: u64,
    /// Requests whose status was not `S_OK`.
    pub failed: u64,
}

struct Waiting {
    op: usize,
    kind: u32,
    part: Part,
    edge: u32,
}

struct Live {
    op: usize,
    slot: usize,
    part: Part,
}

struct State {
    kind: Kind,
    addr: u64,
    parts: usize,
    failed: Option<String>,
    done: Option<Done>,
}

/// The slots' memory, and each slot's device address.
pub struct Slots<S: Region> {
    /// The memory, [`SLOT`] bytes a slot.
    pub memory: S,
    /// The device address of each slot's first byte.
    pub addrs: Vec<u64>,
}

/// One queue of one device.
pub struct Engine<Q: Region, S: Region> {
    queue: Queue<Q>,
    slots: Slots<S>,
    free: Vec<usize>,
    translate: Arc<dyn Translate>,
    max_body: u64,
    capacity: u64,
    writable: bool,
    flush: bool,
    ops: Vec<Option<State>>,
    spare: Vec<usize>,
    waiting: VecDeque<Waiting>,
    live: HashMap<u16, Live>,
    runs: Vec<(u64, u32)>,
    stats: Stats,
    broken: Option<String>,
}

impl<Q: Region, S: Region> Engine<Q, S> {
    /// An engine for a queue the device is set up with, the slots' memory, a disk of
    /// `capacity` bytes, a body limit from [`plan::max_body`], whether the disk takes writes
    /// and whether it has a write cache to flush.
    pub fn new(
        queue: Queue<Q>,
        slots: Slots<S>,
        translate: Arc<dyn Translate>,
        max_body: u64,
        capacity: u64,
        writable: bool,
        flush: bool,
    ) -> Engine<Q, S> {
        let free = (0..slots.addrs.len()).rev().collect();
        Engine {
            queue,
            slots,
            free,
            translate,
            max_body,
            capacity,
            writable,
            flush,
            ops: Vec::new(),
            spare: Vec::new(),
            waiting: VecDeque::new(),
            live: HashMap::new(),
            runs: Vec::new(),
            stats: Stats::default(),
            broken: None,
        }
    }

    /// The disk's bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// What it has moved.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Requests the device holds.
    pub fn in_flight(&self) -> usize {
        self.live.len()
    }

    /// Requests waiting for room on the queue.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Why the queue can no longer be used, once it cannot.
    pub fn broken(&self) -> Option<&str> {
        self.broken.as_deref()
    }

    /// Takes an operation: refused at once when the disk cannot do it, finished at once when it
    /// moves nothing, and otherwise planned into requests that wait for [`Engine::pump`].
    pub fn submit(&mut self, op: Op) {
        if let Some(why) = &self.broken {
            (op.done)(Err(why.clone()));
            return;
        }
        let end = op.offset.checked_add(op.len);
        let planned: Result<(u32, u32, Vec<Part>), String> = match op.kind {
            Kind::Read if end.is_none_or(|e| e > self.capacity) => Err(format!(
                "a read of {} bytes at {} is past the disk's {} bytes",
                op.len, op.offset, self.capacity
            )),
            Kind::Read => Ok((
                T_IN,
                SECTOR as u32,
                plan::read(op.offset, op.len, self.max_body),
            )),
            Kind::Write if !self.writable => Err("the disk is read only".into()),
            Kind::Write if end.is_none_or(|e| e > self.capacity) => Err(format!(
                "a write of {} bytes at {} is past the disk's {} bytes",
                op.len, op.offset, self.capacity
            )),
            Kind::Write => plan::write(op.offset, op.len, self.max_body)
                .map(|parts| (T_OUT, SECTOR as u32, parts))
                .ok_or_else(|| {
                    format!(
                        "a write of {} bytes at {} is not whole sectors",
                        op.len, op.offset
                    )
                }),
            Kind::Flush if !self.flush => Ok((T_FLUSH, 0, Vec::new())),
            Kind::Flush => Ok((
                T_FLUSH,
                0,
                vec![Part {
                    sector: 0,
                    head: None,
                    body: None,
                    tail: None,
                }],
            )),
            Kind::Id if op.len < ID_BYTES as u64 => {
                Err(format!("a serial is {ID_BYTES} bytes, not {}", op.len))
            }
            Kind::Id => Ok((
                T_GET_ID,
                ID_BYTES as u32,
                vec![Part {
                    sector: 0,
                    head: Some(plan::Edge {
                        skip: 0,
                        dst: 0,
                        len: ID_BYTES,
                    }),
                    body: None,
                    tail: None,
                }],
            )),
        };
        let (kind, edge, parts) = match planned {
            Ok(p) => p,
            Err(why) => {
                (op.done)(Err(why));
                return;
            }
        };
        if parts.is_empty() {
            self.stats.ops += 1;
            (op.done)(Ok(()));
            return;
        }
        let state = State {
            kind: op.kind,
            addr: op.addr,
            parts: parts.len(),
            failed: None,
            done: Some(op.done),
        };
        let index = match self.spare.pop() {
            Some(i) => {
                self.ops[i] = Some(state);
                i
            }
            None => {
                self.ops.push(Some(state));
                self.ops.len() - 1
            }
        };
        for part in parts {
            self.waiting.push_back(Waiting {
                op: index,
                kind,
                part,
                edge,
            });
        }
    }

    /// Hands the device every waiting request the queue and the slots have room for, in order,
    /// and rings the doorbell once if any went. Returns how many went.
    pub fn pump(&mut self, ring: impl FnOnce() -> Result<(), String>) -> usize {
        let mut sent = 0;
        while let Some(next) = self.waiting.front() {
            let Some(&slot) = self.free.last() else {
                break;
            };
            let (op, kind, part, edge) = (next.op, next.kind, next.part, next.edge);
            let chain = match self.chain(op, kind, part, edge, slot) {
                Ok(c) => c,
                Err(why) => {
                    // This operation's memory is out of the device's reach: it fails, the
                    // rest go on.
                    let next = self.waiting.pop_front().expect("a front");
                    self.fail_waiting(next.op, why);
                    continue;
                }
            };
            if chain.len() > self.queue.free() {
                break;
            }
            let next = self.waiting.pop_front().expect("a front");
            self.free.pop();
            if let Err(e) = self.header(slot, next.kind, next.part.sector) {
                self.free.push(slot);
                self.break_down(format!("writing a request header: {}", e.name()));
                return sent;
            }
            match self.queue.add(&chain) {
                Ok(head) => {
                    self.live.insert(
                        head,
                        Live {
                            op: next.op,
                            slot,
                            part: next.part,
                        },
                    );
                    sent += 1;
                }
                Err(e) => {
                    self.free.push(slot);
                    self.waiting.push_front(next);
                    self.break_down(format!("the queue refused a request: {e:?}"));
                    return sent;
                }
            }
        }
        if sent > 0 {
            self.stats.notifies += 1;
            if let Err(why) = ring() {
                self.break_down(format!("the doorbell: {why}"));
            }
        }
        sent
    }

    /// Finishes every request the device has returned; returns how many.
    pub fn poll(&mut self) -> usize {
        let mut n = 0;
        loop {
            let used = match self.queue.pop_used() {
                Ok(Some(u)) => u,
                Ok(None) => return n,
                Err(e) => {
                    self.break_down(format!("the used ring: {e:?}"));
                    return n;
                }
            };
            let Some(live) = self.live.remove(&used.head) else {
                self.break_down(format!(
                    "the device returned chain {} not in flight",
                    used.head
                ));
                return n;
            };
            n += 1;
            self.stats.requests += 1;
            let result = self.finish(&live);
            self.free.push(live.slot);
            if let Err(why) = result {
                self.stats.failed += 1;
                self.part_done(live.op, Some(why));
            } else {
                self.part_done(live.op, None);
            }
        }
    }

    /// Takes no more operations and fails every waiting request with `why`: for a disk being
    /// closed, or a queue that cannot be trusted. An operation with requests in flight ends
    /// when they are back, or at [`Engine::abandon`] once the device is reset: until then the
    /// device may still write its memory.
    pub fn break_down(&mut self, why: String) {
        if self.broken.is_none() {
            self.broken = Some(why.clone());
        }
        let waiting: Vec<usize> = self.waiting.drain(..).map(|w| w.op).collect();
        for op in waiting {
            self.part_done(op, Some(why.clone()));
        }
    }

    /// Fails every request in flight: for after the device was reset, when it can no longer
    /// write any operation's memory.
    pub fn abandon(&mut self) {
        let why = self
            .broken
            .get_or_insert_with(|| "the disk was closed".into())
            .clone();
        let live: Vec<(usize, usize)> = self.live.drain().map(|(_, l)| (l.op, l.slot)).collect();
        for (op, slot) in live {
            self.free.push(slot);
            self.part_done(op, Some(why.clone()));
        }
    }

    /// The request's descriptor chain from slot `slot`.
    fn chain(
        &mut self,
        op: usize,
        kind: u32,
        part: Part,
        edge: u32,
        slot: usize,
    ) -> Result<Vec<Desc>, String> {
        let base = self.slots.addrs[slot];
        let addr = self.ops[op].as_ref().ok_or("an operation that ended")?.addr;
        let writes = kind != T_OUT;
        let mut chain = Vec::with_capacity(4);
        chain.push(Desc {
            addr: base,
            len: 16,
            device_writes: false,
        });
        if part.head.is_some() {
            chain.push(Desc {
                addr: base + HEAD as u64,
                len: edge,
                device_writes: true,
            });
        }
        if let Some((at, len)) = part.body {
            self.runs.clear();
            self.translate.runs(addr + at, len, &mut self.runs)?;
            chain.extend(self.runs.iter().map(|&(addr, len)| Desc {
                addr,
                len,
                device_writes: writes,
            }));
        }
        if part.tail.is_some() {
            chain.push(Desc {
                addr: base + TAIL as u64,
                len: edge,
                device_writes: true,
            });
        }
        chain.push(Desc {
            addr: base + STATUS as u64,
            len: 1,
            device_writes: true,
        });
        Ok(chain)
    }

    fn header(&self, slot: usize, kind: u32, sector: u64) -> Result<(), moruna_abi::Error> {
        let at = slot * SLOT;
        let m = &self.slots.memory;
        m.write32(at, kind)?;
        m.write32(at + 4, 0)?;
        m.write64(at + 8, sector)?;
        m.write8(at + STATUS, 0xff)
    }

    /// A returned request: its status, then its edges copied out of the bounce.
    fn finish(&mut self, live: &Live) -> Result<(), String> {
        let at = live.slot * SLOT;
        let status = self
            .slots
            .memory
            .read8(at + STATUS)
            .map_err(|e| format!("reading a status: {}", e.name()))?;
        if status != S_OK {
            return Err(format!(
                "the device ended the request at sector {} with status {status}",
                live.part.sector
            ));
        }
        let state = self.ops[live.op]
            .as_ref()
            .ok_or("an operation that ended")?;
        let (kind, addr) = (state.kind, state.addr);
        if let Some((_, len)) = live.part.body {
            self.stats.dma_bytes += len;
        }
        if matches!(kind, Kind::Read | Kind::Id) {
            for (edge, from) in [(live.part.head, HEAD), (live.part.tail, TAIL)] {
                let Some(edge) = edge else { continue };
                let mut bytes = [0u8; SECTOR as usize];
                let bytes = &mut bytes[..edge.len];
                self.slots
                    .memory
                    .read_bytes(at + from + edge.skip, bytes)
                    .map_err(|e| format!("reading a bounce sector: {}", e.name()))?;
                copy_in(addr + edge.dst, bytes);
                self.stats.bounced_bytes += edge.len as u64;
            }
        }
        Ok(())
    }

    fn part_done(&mut self, op: usize, failed: Option<String>) {
        let Some(Some(state)) = self.ops.get_mut(op) else {
            return;
        };
        if let Some(why) = failed {
            state.failed.get_or_insert(why);
        }
        state.parts -= 1;
        if state.parts == 0 {
            let state = self.ops[op].take().expect("a live operation");
            self.spare.push(op);
            self.stats.ops += 1;
            if let Some(done) = state.done {
                done(state.failed.map_or(Ok(()), Err));
            }
        }
    }

    /// An operation whose request could not be built: its other waiting requests go, and it
    /// ends once its requests in flight are back.
    fn fail_waiting(&mut self, op: usize, why: String) {
        let before = self.waiting.len();
        self.waiting.retain(|w| w.op != op);
        // The request already taken off the front is one of them.
        let removed = before - self.waiting.len() + 1;
        if let Some(Some(state)) = self.ops.get_mut(op) {
            state.failed.get_or_insert(why);
            // One is left for `part_done` to count.
            state.parts = state.parts + 1 - removed;
        }
        self.part_done(op, None);
    }
}

/// Copies `bytes` to `addr` of an operation's memory.
#[allow(unsafe_code)]
fn copy_in(addr: u64, bytes: &[u8]) {
    // SAFETY: `addr .. addr + bytes.len()` is inside an operation's memory (an edge's
    // destination lies within `[addr, addr + len)` by `plan::read`), which its caller keeps
    // alive and touches nowhere else until the operation's `done` is told (`Op::addr`); the
    // engine is the only writer meanwhile, and the device has returned the request.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) };
}
