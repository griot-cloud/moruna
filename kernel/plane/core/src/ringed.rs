//! Ring mode (`architecture/data-os.md` §5): batches between the engine and a task with no
//! copy and no call per batch.
//!
//! [`RingPlane`] is [`KernelPlane`] with tasks of a second kind. A ring task's plane is the
//! same two slots and port a task of the slots has, but each slot is pushed once:
//!
//! - slot 0 holds the task's **input region**, which it maps read only once. The engine lays a
//!   batch out (or decodes it) straight into the region of the task that consumes it, checks it
//!   as it checks its own records, and posts it on the input ring: no copy and no call;
//! - slot 1 holds the task's **rings object** ([`crate::ring`]), which it maps writable once:
//!   the rings, and the windows it writes its records into. It posts a record on the output
//!   ring, with no call;
//! - the port carries the task's waits: a task whose ring is empty calls port 0, and the
//!   engine answers when it posts. The kernel's futex is keyed by process, so a shared page
//!   cannot carry the wake.
//!
//! **The record check stays safe (DP1).** The engine checks a record only on bytes the task
//! can no longer write. In [`Mode::Protect`] the engine takes write access to every window
//! posted since the last time back in one call (`windows_protect`), and gives it back to a
//! window when its batch is freed, again a group at a time. In [`Mode::Unmap`], used while
//! that call answers `BadSyscall`, the task hands the whole rings object back with
//! `batch_put` once it has posted a group, which unmaps it; the engine checks the group and
//! pushes the next rings object, the input ring carried over.
//!
//! **Fan-out** maps one region read only into each of its consumers: a batch is laid out
//! once, posted on each consumer's ring, and its span freed after the last release and the
//! engine's hold (DP-I6).
//!
//! **Pages are zeroed before a task first sees them** (C4's trust boundary): a region or a
//! rings object from the pool that another task has seen is zeroed before it is pushed.

use alloc::{
    collections::{BTreeMap, VecDeque},
    string::String,
    sync::Arc,
    vec::Vec,
};

use moruna_abi::{
    Error, limits,
    plane::{reply, request},
    read_u32, read_u64, write_u64,
};

use crate::arena::pages;
use crate::calls::{Calls, Ready, Taken};
use crate::plane::{Backing, BatchRec, KernelPlane};
use crate::port::{BatchId, CallId, DataPlane, Event, PlaneError, Residency, Shape, TaskId};
use crate::ring::{self, Bad, Engine, Geometry, InEntry, Mode, Shared, layout};
use crate::verify::verify;

/// The engine's own mapping of an object: its words, and its bytes.
pub trait View: Shared {
    /// Copies bytes at `at` out.
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), Error>;
    /// Copies `data` in at `at`.
    fn write(&self, at: u64, data: &[u8]) -> Result<(), Error>;
    /// Where the mapping starts in the engine's address space: how the engine decodes a
    /// batch in place, and tells whether a buffer lies in it.
    fn base(&self) -> usize;
    /// Its length.
    fn len(&self) -> u64;
    /// Whether it is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The calls ring mode makes beyond [`Calls`].
pub trait RingCalls: Calls {
    /// A mapping of the engine's own.
    type Mapping: View;
    /// Maps `len` bytes of `memory` into the engine, writable, until the mapping is dropped.
    fn map(&mut self, memory: &Self::Memory, len: u64) -> Result<Self::Mapping, Error>;
    /// Another handle to `memory` with every right it has: the one pushed, while the
    /// engine keeps the first.
    fn keep(&mut self, memory: &Self::Memory) -> Result<Self::Memory, Error>;
    /// `windows_protect`: write access of the task that maps `slot`'s object, on `ranges`
    /// (offsets and lengths in that object), taken back or given, with one flush.
    fn protect(
        &mut self,
        slot: &Self::Slot,
        ranges: &[[u64; 2]],
        writable: bool,
    ) -> Result<(), Error>;
}

/// The cookie slot 0's push of a region carries; a batch's cookie is its id, from 1.
const REGION_COOKIE: u64 = u64::MAX;
/// How many times the engine reads the rings before it sleeps.
pub const SPINS: u32 = 4096;
/// A wait request: the byte saying what for, padding, the head the task saw.
const WAIT_LEN: usize = 16;

/// Where a ring task's input region comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// A new region of this many bytes.
    New(u64),
    /// The region of another task: fan-out, one layout of each batch for every consumer.
    /// Only a region that has held no batch yet is shared.
    Share(RegionId),
}

/// An input region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RegionId(pub u64);

/// What a ring task's plane holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingShape {
    pub region: Region,
    pub windows: u64,
    pub window_bytes: u64,
}

/// What ring mode has done, for the measurements of calls and copies per batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Batches posted on input rings.
    pub posted: u64,
    /// Records taken from output rings and checked.
    pub records: u64,
    /// Calls that took write access back from a group of windows.
    pub revokes: u64,
    /// Calls that gave it back to a group.
    pub regrants: u64,
    /// Rings objects handed back by a task and replaced (unmap mode).
    pub generations: u64,
    /// Waits the tasks called.
    pub waits: u64,
    /// Times the engine slept on its tasks.
    pub sleeps: u64,
}

/// A window of the current rings object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Win {
    /// Never given: writable since the object was made.
    Fresh,
    /// On the free ring, or taken by the task to write in.
    Given,
    /// Holding a checked record, a batch.
    Batch,
    /// Freed, waiting to be given again (writable again first, in protect mode).
    Idle,
}

struct RegionRec<V> {
    object: u64,
    view: Arc<V>,
    bytes: u64,
    /// Free spans, start to length, in whole pages.
    free: BTreeMap<u64, u64>,
    /// Spans lent to batches, start to length.
    lent: BTreeMap<u64, u64>,
    users: Vec<u32>,
    written: bool,
}

impl<V> RegionRec<V> {
    fn reserve(&mut self, len: u64) -> Option<u64> {
        let (&at, &have) = self.free.iter().find(|(_, l)| **l >= len)?;
        self.free.remove(&at);
        if have > len {
            self.free.insert(at + len, have - len);
        }
        self.lent.insert(at, len);
        Some(at)
    }

    fn give_back(&mut self, at: u64) {
        let Some(len) = self.lent.remove(&at) else {
            return;
        };
        let (mut start, mut total) = (at, len);
        if let Some((&before, &blen)) = self.free.range(..at).next_back()
            && before + blen == at
        {
            self.free.remove(&before);
            start = before;
            total += blen;
        }
        if let Some(after) = self.free.remove(&(at + len)) {
            total += after;
        }
        self.free.insert(start, total);
    }

    fn largest_free(&self) -> u64 {
        self.free.values().copied().max().unwrap_or(0)
    }
}

struct RingsObject<V> {
    object: u64,
    view: Arc<V>,
    task: u32,
    /// Batches its windows hold.
    live: u64,
    current: bool,
}

struct RingTask<C: RingCalls> {
    shape: RingShape,
    region: u64,
    input: C::Slot,
    output: C::Slot,
    port: C::Channel,
    input_done: bool,
    output_done: bool,
    port_done: bool,
    /// The owner key of the current rings object.
    rings: u64,
    engine: Engine,
    mode: Mode,
    generation: u64,
    windows: Vec<Win>,
    /// Inputs posted and not released, oldest first.
    inflight: VecDeque<(u64, InEntry)>,
    /// A wait the task called: its token, what for, and the head it saw.
    wait: Option<(u64, u8, u64)>,
    /// Unmap mode: the rings object is handed back and the next not pushed yet.
    handed: bool,
    refused: bool,
}

enum Owner<V> {
    Region(RegionRec<V>),
    Rings(RingsObject<V>),
}

/// [`KernelPlane`] with ring tasks.
pub struct RingPlane<C: RingCalls> {
    /// The plane: its arena, its batches, and its tasks of the slots.
    pub plane: KernelPlane<C>,
    owners: BTreeMap<u64, Owner<C::Mapping>>,
    next_owner: u64,
    tasks: BTreeMap<u32, RingTask<C>>,
    rows: BTreeMap<u64, u64>,
    /// How many times the engine reads the rings before it sleeps.
    pub spins: u32,
    counters: Counters,
    /// A wait returned with something signalled: the slots and ports are read on the next
    /// pass. Otherwise only the rings are, which costs no call.
    signaled: bool,
}

fn kernel(e: Error) -> PlaneError {
    PlaneError::Kernel(e)
}

fn bad(b: Bad) -> String {
    alloc::format!("the task broke the ring protocol: {}", b.0)
}

impl<C: RingCalls> RingPlane<C> {
    /// A plane over `calls` whose arena may make `budget` bytes of memory objects.
    pub fn new(calls: C, budget: u64) -> RingPlane<C> {
        RingPlane {
            plane: KernelPlane::new(calls, budget),
            owners: BTreeMap::new(),
            next_owner: 1,
            tasks: BTreeMap::new(),
            rows: BTreeMap::new(),
            spins: SPINS,
            counters: Counters::default(),
            signaled: true,
        }
    }

    /// What ring mode has done.
    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// Whether `task` is a ring task.
    pub fn is_ring(&self, task: TaskId) -> bool {
        self.tasks.contains_key(&task.0)
    }

    /// How `task` hands its records back.
    pub fn mode(&self, task: TaskId) -> Option<Mode> {
        self.tasks.get(&task.0).map(|t| t.mode)
    }

    /// `task`'s shape, to start it again as a fresh process (C6).
    pub fn shape(&self, task: TaskId) -> Option<RingShape> {
        self.tasks.get(&task.0).map(|t| t.shape)
    }

    /// The input region `task` reads.
    pub fn region_of(&self, task: TaskId) -> Option<RegionId> {
        self.tasks.get(&task.0).map(|t| RegionId(t.region))
    }

    /// The engine's mapping of `region`, and its length: where a batch for its tasks is
    /// decoded in place.
    pub fn region_view(&self, region: RegionId) -> Option<(Arc<C::Mapping>, u64)> {
        match self.owners.get(&region.0) {
            Some(Owner::Region(r)) => Some((r.view.clone(), r.bytes)),
            _ => None,
        }
    }

    /// The engine's ends of a ring task's slots, input then output, as `process_plane`
    /// numbers them.
    pub fn ring_slots(&self, task: TaskId) -> Result<[&C::Slot; 2], PlaneError> {
        let t = self.tasks.get(&task.0).ok_or(PlaneError::Unknown("task"))?;
        Ok([&t.input, &t.output])
    }

    /// The slots of any task, for `process_plane`.
    pub fn slots(&self, task: TaskId) -> Result<Vec<&C::Slot>, PlaneError> {
        match self.tasks.get(&task.0) {
            Some(t) => Ok(alloc::vec![&t.input, &t.output]),
            None => self.plane.slots(task),
        }
    }

    /// A batch's bytes: the engine's mapping that holds them, where they start in it, and
    /// their length; `None` for a batch of a whole object, which [`KernelPlane::batch`]
    /// gives.
    pub fn batch_view(&self, batch: BatchId) -> Option<(Arc<C::Mapping>, u64, u64)> {
        let b = self.plane.batches.get(&batch.0)?;
        let Backing::Ring { owner, .. } = b.backing else {
            return None;
        };
        let view = match self.owners.get(&owner)? {
            Owner::Region(r) => r.view.clone(),
            Owner::Rings(o) => o.view.clone(),
        };
        Some((view, b.offset, b.len))
    }

    fn owner(&mut self, owner: Owner<C::Mapping>) -> u64 {
        let key = self.next_owner;
        self.next_owner += 1;
        self.owners.insert(key, owner);
        key
    }

    /// An object for `task` of at least `bytes`, zeroed when another task has seen it,
    /// mapped into the engine; its arena id, its mapping and its size. The arena keeps its
    /// handle.
    fn object_for(&mut self, task: u32, bytes: u64) -> Result<(u64, C::Mapping, u64), PlaneError> {
        let calls = &mut self.plane.calls;
        let (id, memory, size) = self.plane.arena.take(bytes, |n| calls.memory(n))?;
        let view = match self.plane.calls.map(&memory, size) {
            Ok(v) => v,
            Err(e) => {
                self.plane.arena.untake(id, memory)?;
                return Err(kernel(e));
            }
        };
        self.plane.arena.hold(id, memory)?;
        if !self.plane.arena.seen(id).clean_for(task) {
            zero(&view, 0, size).map_err(kernel)?;
            self.plane.zeroed += size;
            self.plane.arena.zeroed(id);
        }
        self.plane.arena.show(id, task);
        Ok((id, view, size))
    }

    /// Pushes a handle to arena object `id` into `slot`, the arena keeping its own.
    fn push_object(
        &mut self,
        slot_of: impl Fn(&RingTask<C>) -> &C::Slot,
        task: u32,
        id: u64,
        share: bool,
        len: u64,
        cookie: u64,
    ) -> Result<(), PlaneError> {
        let held = self
            .plane
            .arena
            .handle(id)
            .ok_or(PlaneError::Unknown("an object the arena does not hold"))?;
        let pushed = if share {
            self.plane.calls.share(held)
        } else {
            self.plane.calls.keep(held)
        }
        .map_err(kernel)?;
        let t = self.tasks.get(&task).ok_or(PlaneError::Unknown("task"))?;
        match self.plane.calls.push(slot_of(t), pushed, 0, len, cookie) {
            Ok(()) => Ok(()),
            Err((Error::ShouldWait, _)) => Err(PlaneError::Full),
            Err((Error::BadState, _)) => Err(PlaneError::Closed),
            Err((e, _)) => Err(kernel(e)),
        }
    }

    /// Opens a ring task: its region and rings object pushed once, its port opened, every
    /// window given. Returns the task and the kernel's end of its port.
    pub fn open_ring(&mut self, shape: RingShape) -> Result<(TaskId, C::Channel), PlaneError> {
        if self.plane.waited() + self.waited() + 3 > crate::plane::WAITED_MAX {
            return Err(PlaneError::Limit("slots and ports waited on at once"));
        }
        let id = self.plane.next_task;
        let probe = Geometry::new(
            match shape.region {
                Region::New(b) => pages(b).unwrap_or(0),
                Region::Share(r) => match self.owners.get(&r.0) {
                    Some(Owner::Region(rr)) => rr.bytes,
                    _ => return Err(PlaneError::Unknown("region")),
                },
            },
            shape.windows,
            pages(shape.window_bytes).unwrap_or(0),
        )
        .map_err(|b| PlaneError::Limit(b.0))?;
        let object_bytes = probe
            .object_bytes()
            .ok_or(PlaneError::Limit("a rings object past u64"))?;
        let input = self.plane.calls.slot(false).map_err(kernel)?;
        let output = self.plane.calls.slot(true).map_err(kernel)?;
        let (ours, theirs) = self.plane.calls.channel().map_err(kernel)?;
        // The region: new, or one of another task's that has held nothing yet.
        let region = match shape.region {
            Region::New(_) => {
                let (object, view, size) = self.object_for(id, probe.region_bytes)?;
                let mut free = BTreeMap::new();
                free.insert(0, size);
                self.owner(Owner::Region(RegionRec {
                    object,
                    view: Arc::new(view),
                    bytes: size,
                    free,
                    lent: BTreeMap::new(),
                    users: alloc::vec![id],
                    written: false,
                }))
            }
            Region::Share(r) => {
                let Some(Owner::Region(rr)) = self.owners.get_mut(&r.0) else {
                    return Err(PlaneError::Unknown("region"));
                };
                if !rr.lent.is_empty() {
                    return Err(PlaneError::Limit("a region shared after it held a batch"));
                }
                if rr.written {
                    zero(&*rr.view, 0, rr.bytes).map_err(kernel)?;
                    self.plane.zeroed += rr.bytes;
                    rr.written = false;
                }
                rr.users.push(id);
                let object = rr.object;
                self.plane.arena.show(object, id);
                r.0
            }
        };
        let region_bytes = probe.region_bytes;
        let region_object = match self.owners.get(&region) {
            Some(Owner::Region(r)) => r.object,
            _ => return Err(PlaneError::Unknown("region")),
        };
        // Write access is taken back by the engine where the kernel can, and by the task's
        // own put where it cannot yet.
        let mode = match self.plane.calls.protect(&output, &[], false) {
            Ok(()) => Mode::Protect,
            Err(_) => Mode::Unmap,
        };
        let (rings_object, view, _) = self.object_for(id, object_bytes)?;
        let engine = Engine::format(&view, probe, mode, 0);
        let rings = self.owner(Owner::Rings(RingsObject {
            object: rings_object,
            view: Arc::new(view),
            task: id,
            live: 0,
            current: true,
        }));
        self.plane.next_task += 1;
        self.tasks.insert(
            id,
            RingTask {
                shape,
                region,
                input,
                output,
                port: ours,
                input_done: false,
                output_done: false,
                port_done: false,
                rings,
                engine,
                mode,
                generation: 0,
                windows: alloc::vec![Win::Fresh; probe.windows as usize],
                inflight: VecDeque::new(),
                wait: None,
                handed: false,
                refused: false,
            },
        );
        self.push_object(
            |t| &t.input,
            id,
            region_object,
            true,
            region_bytes,
            REGION_COOKIE,
        )?;
        self.push_object(|t| &t.output, id, rings_object, false, object_bytes, rings)?;
        self.give_idle(id)?;
        Ok((TaskId(id), theirs))
    }

    fn waited(&self) -> usize {
        self.tasks
            .values()
            .map(|t| {
                usize::from(!t.input_done) + usize::from(!t.output_done) + usize::from(!t.port_done)
            })
            .sum()
    }

    fn rings_view(&self, key: u64) -> Option<Arc<C::Mapping>> {
        match self.owners.get(&key) {
            Some(Owner::Rings(o)) => Some(o.view.clone()),
            _ => None,
        }
    }

    /// Space for a record of `len` bytes in `region`: its offset there. The engine writes
    /// the record through [`Self::region_view`] and then [`Self::adopt`]s it.
    pub fn reserve(&mut self, region: RegionId, len: u64) -> Result<u64, PlaneError> {
        self.settle_ring();
        let size = pages(len)
            .filter(|s| *s > 0)
            .ok_or(PlaneError::Limit("a record of no bytes, or past u64"))?;
        let Some(Owner::Region(r)) = self.owners.get_mut(&region.0) else {
            return Err(PlaneError::Unknown("region"));
        };
        r.reserve(size).ok_or(PlaneError::Budget {
            asked: size,
            free: r.largest_free(),
        })
    }

    /// Gives back space [`Self::reserve`] gave and no batch holds.
    pub fn unreserve(&mut self, region: RegionId, at: u64) {
        if let Some(Owner::Region(r)) = self.owners.get_mut(&region.0) {
            r.give_back(at);
        }
    }

    /// The record of `len` bytes at `at` of `region`, which the engine wrote there, checked
    /// and held as a batch, as [`KernelPlane::load`] holds one. A refusal gives the space back.
    pub fn adopt(
        &mut self,
        region: RegionId,
        at: u64,
        len: u64,
    ) -> Result<(BatchId, u64), PlaneError> {
        let Some(Owner::Region(r)) = self.owners.get_mut(&region.0) else {
            return Err(PlaneError::Unknown("region"));
        };
        if r.lent
            .get(&at)
            .is_none_or(|l| pages(len).is_none_or(|p| p > *l))
        {
            return Err(PlaneError::Unknown("space the region did not reserve"));
        }
        let view = r.view.clone();
        let checked = verify(len, |o, buf| view.read(at + o, buf));
        let record = match checked {
            Ok(record) => record,
            Err(reason) => {
                r.give_back(at);
                return Err(PlaneError::Refused(reason));
            }
        };
        r.written = true;
        let object = r.object;
        let id = self.plane.next_batch;
        self.plane.next_batch += 1;
        self.plane.batches.insert(
            id,
            BatchRec {
                object,
                offset: at,
                len,
                shares: 0,
                held: true,
                backing: Backing::Ring {
                    owner: region.0,
                    at,
                },
            },
        );
        self.rows.insert(id, record.rows);
        Ok((BatchId(id), record.rows))
    }

    /// A record of `len` bytes the engine writes into `region` with `write` (given the
    /// mapping and the offset), checked and held as a batch.
    pub fn load_into(
        &mut self,
        region: RegionId,
        len: u64,
        write: impl FnOnce(&C::Mapping, u64) -> Result<(), Error>,
    ) -> Result<(BatchId, u64), PlaneError> {
        let at = self.reserve(region, len)?;
        let view = match self.owners.get(&region.0) {
            Some(Owner::Region(r)) => r.view.clone(),
            _ => return Err(PlaneError::Unknown("region")),
        };
        if let Err(e) = write(&view, at) {
            self.unreserve(region, at);
            return Err(kernel(e));
        }
        self.adopt(region, at, len)
    }

    /// Posts `batch`, which lies in `task`'s region, on its input ring: no copy, no call.
    pub fn post(&mut self, task: TaskId, batch: BatchId) -> Result<(), PlaneError> {
        self.settle_ring();
        let b = self
            .plane
            .batches
            .get(&batch.0)
            .ok_or(PlaneError::Unknown("batch"))?;
        let t = self.tasks.get(&task.0).ok_or(PlaneError::Unknown("task"))?;
        if b.backing
            != (Backing::Ring {
                owner: t.region,
                at: b.offset,
            })
        {
            return Err(PlaneError::Unknown("a batch outside the task's region"));
        }
        if t.input_done || t.refused {
            return Err(PlaneError::Closed);
        }
        let entry = InEntry {
            offset: b.offset,
            len: b.len,
            cookie: batch.0,
            rows: self.rows.get(&batch.0).copied().unwrap_or(0),
        };
        let view = self
            .rings_view(t.rings)
            .ok_or(PlaneError::Unknown("rings"))?;
        let t = self
            .tasks
            .get_mut(&task.0)
            .ok_or(PlaneError::Unknown("task"))?;
        if t.engine.is_closed() {
            return Err(PlaneError::Closed);
        }
        if !t.engine.post(&*view, entry) {
            return Err(PlaneError::Full);
        }
        t.inflight.push_back((batch.0, entry));
        if let Some(b) = self.plane.batches.get_mut(&batch.0) {
            b.shares += 1;
        }
        self.counters.posted += 1;
        self.answer_wait(task.0)
    }

    /// Answers the task's wait if what it waits for has come.
    fn answer_wait(&mut self, id: u32) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get_mut(&id) else {
            return Ok(());
        };
        let Some((token, want, seen)) = t.wait else {
            return Ok(());
        };
        let Some(view) = (match self.owners.get(&t.rings) {
            Some(Owner::Rings(o)) => Some(o.view.clone()),
            _ => None,
        }) else {
            return Ok(());
        };
        let ready = match want {
            layout::WAIT_WINDOW => view.load(layout::FREE_HEAD) > seen,
            layout::WAIT_INPUT => view.load(layout::IN_HEAD) > seen || t.engine.is_closed(),
            _ => true,
        } || t.handed;
        if !ready {
            return Ok(());
        }
        t.wait = None;
        send_reply(&mut self.plane.calls, &t.port, token)
    }

    /// Gives the task every idle window of its current rings object: those whose write
    /// access was taken back made writable again first, in one call (protect mode).
    fn give_idle(&mut self, id: u32) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get(&id) else {
            return Ok(());
        };
        if t.handed || t.output_done || t.refused {
            return Ok(());
        }
        let ready: Vec<u64> = t
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| matches!(w, Win::Idle | Win::Fresh))
            .map(|(i, _)| i as u64)
            .collect();
        if ready.is_empty() {
            return Ok(());
        }
        let g = t.engine.geometry;
        let revoked: Vec<[u64; 2]> = ready
            .iter()
            .filter(|w| t.windows[**w as usize] == Win::Idle)
            .map(|w| [g.window_at(*w), g.window_bytes])
            .collect();
        if t.mode == Mode::Protect && !revoked.is_empty() {
            self.plane
                .calls
                .protect(&t.output, &revoked, true)
                .map_err(kernel)?;
            self.counters.regrants += 1;
        }
        let view = self
            .rings_view(t.rings)
            .ok_or(PlaneError::Unknown("rings"))?;
        let t = self.tasks.get_mut(&id).ok_or(PlaneError::Unknown("task"))?;
        for w in &ready {
            // Writable again: given now, or later without another call.
            t.windows[*w as usize] = Win::Fresh;
        }
        let mut broke = None;
        for w in ready {
            match t.engine.give(&*view, w) {
                Ok(true) => t.windows[w as usize] = Win::Given,
                Ok(false) => break,
                Err(b) => {
                    broke = Some(b);
                    break;
                }
            }
        }
        if let Some(b) = broke {
            return self.refuse(id, bad(b));
        }
        self.answer_wait(id)
    }

    /// Bytes ring batches gave back: a region's span, or a window.
    fn settle_ring(&mut self) {
        while let Some((owner, at, batch)) = self.plane.ring_freed.pop_front() {
            self.rows.remove(&batch);
            match self.owners.get_mut(&owner) {
                Some(Owner::Region(r)) => {
                    r.give_back(at);
                    if r.users.is_empty() && r.lent.is_empty() {
                        self.drop_owner(owner);
                    }
                }
                Some(Owner::Rings(o)) => {
                    o.live = o.live.saturating_sub(1);
                    let (task, current, live) = (o.task, o.current, o.live);
                    if current {
                        if let Some(t) = self.tasks.get_mut(&task)
                            && let Some(w) = t.windows.get_mut(at as usize)
                        {
                            *w = Win::Idle;
                        }
                    } else if live == 0 {
                        self.drop_owner(owner);
                    }
                }
                None => {}
            }
        }
    }

    fn drop_owner(&mut self, owner: u64) {
        let object = match self.owners.remove(&owner) {
            Some(Owner::Region(r)) => r.object,
            Some(Owner::Rings(o)) => o.object,
            None => return,
        };
        let _ = self.plane.arena.free(object);
    }

    /// Refuses `id` with `reason`: the kernel ends the task, and the engine is told.
    fn refuse(&mut self, id: u32, reason: String) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get_mut(&id) else {
            return Ok(());
        };
        if t.refused {
            return Ok(());
        }
        t.refused = true;
        match self.plane.calls.refuse(&t.output, &reason) {
            Ok(()) | Err(Error::BadState) => {}
            Err(e) => return Err(kernel(e)),
        }
        self.plane.events.push_back(Event::Refused {
            task: TaskId(id),
            output: 0,
            reason,
        });
        Ok(())
    }

    /// Takes the records the task posted on rings object `key`: each checked, and a batch.
    /// The caller has made sure the task can no longer write them.
    fn take_records(
        &mut self,
        id: u32,
        key: u64,
        group: &[ring::OutEntry],
    ) -> Result<(), PlaneError> {
        let Some(Owner::Rings(o)) = self.owners.get(&key) else {
            return Err(PlaneError::Unknown("rings"));
        };
        let (object, view) = (o.object, o.view.clone());
        for e in group {
            let Some(t) = self.tasks.get(&id) else {
                return Ok(());
            };
            if t.refused {
                return Ok(());
            }
            let at = t.engine.geometry.window_at(e.window) + e.offset;
            match verify(e.len, |o, buf| view.read(at + o, buf)) {
                Ok(record) => {
                    let bid = self.plane.next_batch;
                    self.plane.next_batch += 1;
                    self.plane.batches.insert(
                        bid,
                        BatchRec {
                            object,
                            offset: at,
                            len: e.len,
                            shares: 0,
                            held: true,
                            backing: Backing::Ring {
                                owner: key,
                                at: e.window,
                            },
                        },
                    );
                    self.rows.insert(bid, record.rows);
                    if let Some(Owner::Rings(o)) = self.owners.get_mut(&key) {
                        o.live += 1;
                    }
                    if let Some(t) = self.tasks.get_mut(&id)
                        && t.rings == key
                    {
                        t.windows[e.window as usize] = Win::Batch;
                    }
                    self.counters.records += 1;
                    self.plane.events.push_back(Event::Put {
                        task: TaskId(id),
                        output: 0,
                        batch: BatchId(bid),
                        rows: record.rows,
                        bytes: e.len,
                    });
                }
                Err(reason) => {
                    if let Some(t) = self.tasks.get_mut(&id)
                        && t.rings == key
                    {
                        t.windows[e.window as usize] = Win::Idle;
                    }
                    return self.refuse(id, reason);
                }
            }
        }
        Ok(())
    }

    /// Reads the outputs the task posted on its current rings object, each in a window it
    /// was given: `Err` with the reason it is refused.
    fn posted_group(&mut self, id: u32) -> Result<Vec<ring::OutEntry>, String> {
        let Some(t) = self.tasks.get(&id) else {
            return Ok(Vec::new());
        };
        let view = self
            .rings_view(t.rings)
            .ok_or_else(|| String::from("rings"))?;
        let t = self
            .tasks
            .get_mut(&id)
            .ok_or_else(|| String::from("task"))?;
        let mut group = Vec::new();
        loop {
            match t.engine.take(&*view).map_err(bad)? {
                None => return Ok(group),
                Some(e) => {
                    if t.windows.get(e.window as usize) != Some(&Win::Given)
                        || group.iter().any(|g: &ring::OutEntry| g.window == e.window)
                    {
                        return Err(bad(Bad("a record in a window the task was not given")));
                    }
                    group.push(e);
                }
            }
        }
    }

    /// Everything a ring task has for the engine now.
    fn pump(&mut self, id: u32) -> Result<(), PlaneError> {
        self.pump_released(id)?;
        // A task's slots and port are read only when a wait said something came, when the
        // task says it waits, or when it posted in unmap mode (its hand-back follows): in
        // steady state the rings carry everything and the engine makes no call to look.
        let (slots, port) = match self.tasks.get(&id) {
            Some(t) => {
                let words = self.rings_view(t.rings);
                let waits = words
                    .as_ref()
                    .is_some_and(|v| !t.handed && v.load(layout::TASK_WAITS) != 0);
                let posted = t.mode == Mode::Unmap
                    && words.as_ref().is_some_and(|v| {
                        let mut e = t.engine.clone();
                        matches!(e.posted(&**v), Ok(n) if n > 0)
                    });
                (self.signaled || posted || t.handed, self.signaled || waits)
            }
            None => (false, false),
        };
        if slots {
            self.pump_slots(id)?;
        }
        if port {
            self.pump_port(id)?;
        }
        let protect = self
            .tasks
            .get(&id)
            .is_some_and(|t| t.mode == Mode::Protect && !t.refused);
        if protect {
            match self.posted_group(id) {
                Ok(group) if !group.is_empty() => {
                    let t = self.tasks.get(&id).ok_or(PlaneError::Unknown("task"))?;
                    let g = t.engine.geometry;
                    let ranges: Vec<[u64; 2]> = group
                        .iter()
                        .map(|e| {
                            let at = g.window_at(e.window) + e.offset;
                            [at, pages(e.len).unwrap_or(e.len)]
                        })
                        .collect();
                    // DP1: no record is read before the task can no longer write it.
                    self.plane
                        .calls
                        .protect(&t.output, &ranges, false)
                        .map_err(kernel)?;
                    self.counters.revokes += 1;
                    let key = t.rings;
                    self.take_records(id, key, &group)?;
                }
                Ok(_) => {}
                Err(reason) => self.refuse(id, reason)?,
            }
        }
        self.settle_ring();
        let low = self.tasks.get(&id).is_some_and(|t| {
            let idle = t
                .windows
                .iter()
                .filter(|w| matches!(w, Win::Idle | Win::Fresh))
                .count();
            idle > 0
                && (idle * 2 >= t.windows.len()
                    || t.wait.is_some_and(|w| w.1 == layout::WAIT_WINDOW))
        });
        if low {
            self.give_idle(id)?;
        }
        self.end_if_drained(id)
    }

    fn pump_released(&mut self, id: u32) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get(&id) else {
            return Ok(());
        };
        if t.refused || t.handed {
            return Ok(());
        }
        let view = self
            .rings_view(t.rings)
            .ok_or(PlaneError::Unknown("rings"))?;
        let t = self.tasks.get_mut(&id).ok_or(PlaneError::Unknown("task"))?;
        let n = match t.engine.released(&*view) {
            Ok(n) => n,
            Err(b) => return self.refuse(id, bad(b)),
        };
        for _ in 0..n {
            self.release_one(id)?;
        }
        Ok(())
    }

    fn release_one(&mut self, id: u32) -> Result<(), PlaneError> {
        let Some((batch, _)) = self.tasks.get_mut(&id).and_then(|t| t.inflight.pop_front()) else {
            return Ok(());
        };
        if let Some(b) = self.plane.batches.get_mut(&batch) {
            b.shares = b.shares.saturating_sub(1);
        }
        self.plane.events.push_back(Event::Released {
            task: TaskId(id),
            input: 0,
            batch: BatchId(batch),
        });
        self.plane.settle(batch)
    }

    fn pump_slots(&mut self, id: u32) -> Result<(), PlaneError> {
        // Slot 0: the region's share comes back only at the task's end.
        loop {
            let Some(t) = self.tasks.get_mut(&id) else {
                return Ok(());
            };
            if t.input_done {
                break;
            }
            match self.plane.calls.take(&t.input) {
                Ok(_) => {}
                Err(Error::ShouldWait) => break,
                Err(Error::PeerClosed) => {
                    t.input_done = true;
                    break;
                }
                Err(e) => return Err(kernel(e)),
            }
        }
        // Slot 1: the rings object handed back (unmap mode), or back at the task's end.
        loop {
            let Some(t) = self.tasks.get_mut(&id) else {
                return Ok(());
            };
            if t.output_done {
                break;
            }
            let taken = match self.plane.calls.take(&t.output) {
                Ok(taken) => taken,
                Err(Error::ShouldWait) => break,
                Err(Error::PeerClosed) => {
                    t.output_done = true;
                    break;
                }
                Err(e) => return Err(kernel(e)),
            };
            match taken {
                // The handle pushed was the engine's second; the arena keeps the first.
                Taken::Put { cookie, .. } => self.handed_back(id, cookie)?,
                Taken::Returned { .. } | Taken::Released { .. } => {}
            }
        }
        Ok(())
    }

    /// Unmap mode: the task handed its rings object back, so nothing of it is writable to
    /// the task any more. Its records are checked, and the next rings object is pushed.
    fn handed_back(&mut self, id: u32, key: u64) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get_mut(&id) else {
            return Ok(());
        };
        if t.rings != key || t.mode != Mode::Unmap {
            return Ok(());
        }
        t.handed = true;
        self.counters.generations += 1;
        self.pump_released(id)?;
        match self.posted_group(id) {
            Ok(group) => self.take_records(id, key, &group)?,
            Err(reason) => self.refuse(id, reason)?,
        }
        let Some(t) = self.tasks.get(&id) else {
            return Ok(());
        };
        if t.refused {
            return Ok(());
        }
        // The next rings object, with the input ring as it stands.
        let bytes = t
            .engine
            .geometry
            .object_bytes()
            .ok_or(PlaneError::Limit("a rings object past u64"))?;
        let (object, view, _) = self.object_for(id, bytes)?;
        let t = self.tasks.get_mut(&id).ok_or(PlaneError::Unknown("task"))?;
        let generation = t.generation + 1;
        let entries: Vec<InEntry> = t.inflight.iter().map(|(_, e)| *e).collect();
        let engine = t.engine.carry(&view, t.mode, generation, entries);
        let next = self.owner(Owner::Rings(RingsObject {
            object,
            view: Arc::new(view),
            task: id,
            live: 0,
            current: true,
        }));
        let t = self.tasks.get_mut(&id).ok_or(PlaneError::Unknown("task"))?;
        let old = core::mem::replace(&mut t.rings, next);
        t.engine = engine;
        t.generation = generation;
        t.handed = false;
        t.windows.iter_mut().for_each(|w| *w = Win::Fresh);
        if let Some(Owner::Rings(o)) = self.owners.get_mut(&old) {
            o.current = false;
            if o.live == 0 {
                self.drop_owner(old);
            }
        }
        match self.push_object(|t| &t.output, id, object, false, bytes, next) {
            Ok(()) | Err(PlaneError::Closed) => {}
            Err(e) => return Err(e),
        }
        self.give_idle(id)
    }

    fn pump_port(&mut self, id: u32) -> Result<(), PlaneError> {
        if self.plane.buffer.len() < limits::CHANNEL_MAX_BYTES {
            self.plane.buffer.resize(limits::CHANNEL_MAX_BYTES, 0);
        }
        loop {
            let Some(t) = self.tasks.get_mut(&id) else {
                return Ok(());
            };
            if t.port_done {
                return Ok(());
            }
            let n = match self.plane.calls.receive(&t.port, &mut self.plane.buffer) {
                Ok(n) => n,
                Err(Error::ShouldWait) => return Ok(()),
                Err(Error::PeerClosed) => {
                    t.port_done = true;
                    return Ok(());
                }
                Err(e) => return Err(kernel(e)),
            };
            let message = &self.plane.buffer[..n];
            let kind = read_u32(message, request::KIND);
            let token = read_u64(message, request::TOKEN);
            let port = read_u64(message, request::PORT);
            let len = read_u64(message, request::LEN);
            let (Some(request::CALL), Some(token), Some(port), Some(len)) =
                (kind, token, port, len)
            else {
                return Err(PlaneError::Unknown("a port message that is no request"));
            };
            if request::SIZE as u64 + len != n as u64 {
                return Err(PlaneError::Unknown("a port message that is no request"));
            }
            let body = &message[request::SIZE..];
            if port != layout::WAIT_PORT {
                self.plane.events.push_back(Event::Call {
                    call: CallId {
                        task: TaskId(id),
                        token,
                    },
                    port,
                    request: body.to_vec(),
                });
                continue;
            }
            self.counters.waits += 1;
            let want = body.first().copied().unwrap_or(layout::WAKE);
            let seen = if body.len() >= WAIT_LEN {
                read_u64(body, 8).unwrap_or(0)
            } else {
                0
            };
            if let Some(t) = self.tasks.get_mut(&id) {
                if let Some((old, ..)) = t.wait.take() {
                    // A thread waits once at a time; an earlier wait is answered now.
                    send_reply(&mut self.plane.calls, &t.port, old)?;
                }
                if let Some(t) = self.tasks.get_mut(&id) {
                    t.wait = Some((token, want, seen));
                }
            }
            self.answer_wait(id)?;
        }
    }

    /// Ends a ring task once its slots and port have given everything back.
    fn end_if_drained(&mut self, id: u32) -> Result<(), PlaneError> {
        let done = self
            .tasks
            .get(&id)
            .is_some_and(|t| t.input_done && t.output_done && t.port_done);
        if !done {
            return Ok(());
        }
        while self.tasks.get(&id).is_some_and(|t| !t.inflight.is_empty()) {
            self.release_one(id)?;
        }
        let Some(t) = self.tasks.remove(&id) else {
            return Ok(());
        };
        if let Some(Owner::Region(r)) = self.owners.get_mut(&t.region) {
            r.users.retain(|u| *u != id);
            if r.users.is_empty() && r.lent.is_empty() {
                self.drop_owner(t.region);
            }
        }
        if let Some(Owner::Rings(o)) = self.owners.get_mut(&t.rings) {
            o.current = false;
            if o.live == 0 {
                self.drop_owner(t.rings);
            }
        }
        self.settle_ring();
        self.plane
            .events
            .push_back(Event::Ended { task: TaskId(id) });
        Ok(())
    }

    /// Whether any ring task has something for the engine, read from the rings alone.
    fn ready(&self) -> bool {
        self.tasks.values().any(|t| {
            if t.refused || t.handed {
                return false;
            }
            let Some(Owner::Rings(o)) = self.owners.get(&t.rings) else {
                return false;
            };
            let v = &*o.view;
            let mut e = t.engine.clone();
            matches!(e.posted(v), Ok(n) if n > 0) || matches!(e.released(v), Ok(n) if n > 0)
        })
    }

    fn set_waiting(&mut self, waiting: bool) -> bool {
        let mut any = false;
        let keys: Vec<(u32, u64)> = self.tasks.iter().map(|(i, t)| (*i, t.rings)).collect();
        for (id, key) in keys {
            let Some(view) = self.rings_view(key) else {
                continue;
            };
            if let Some(t) = self.tasks.get_mut(&id)
                && !t.handed
                && !t.refused
            {
                let mut e = t.engine.clone();
                if let Ok(n) = e.set_waiting(&*view, waiting) {
                    any |= n > 0;
                }
            }
        }
        any
    }
}

/// Writes zeros over `len` bytes of `view` from `at`.
fn zero<V: View>(view: &V, at: u64, len: u64) -> Result<(), Error> {
    const CHUNK: usize = 4096;
    let zeros = [0u8; CHUNK];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(CHUNK as u64) as usize;
        view.write(at + done, &zeros[..n])?;
        done += n as u64;
    }
    Ok(())
}

/// Answers a wait with an empty reply.
fn send_reply<C: Calls>(calls: &mut C, port: &C::Channel, token: u64) -> Result<(), PlaneError> {
    let mut header = [0u8; reply::SIZE];
    write_u64(&mut header, reply::TOKEN, token);
    match calls.send(port, &header, None) {
        Ok(()) | Err(Error::PeerClosed) => Ok(()),
        Err(e) => Err(kernel(e)),
    }
}

impl<C: RingCalls> DataPlane for RingPlane<C> {
    type Task = Option<C::Channel>;

    fn open(&mut self, shape: Shape) -> Result<(TaskId, Self::Task), PlaneError> {
        self.plane.open(shape)
    }

    /// For a ring task, `batch` must lie in its region: it is posted on its ring.
    fn offer(&mut self, task: TaskId, input: u32, batch: BatchId) -> Result<(), PlaneError> {
        if self.tasks.contains_key(&task.0) {
            if input != 0 {
                return Err(PlaneError::Unknown("input"));
            }
            return self.post(task, batch);
        }
        self.plane.offer(task, input, batch)
    }

    fn finish(&mut self, task: TaskId, input: u32) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get(&task.0) else {
            return self.plane.finish(task, input);
        };
        if input != 0 {
            return Err(PlaneError::Unknown("input"));
        }
        let view = self
            .rings_view(t.rings)
            .ok_or(PlaneError::Unknown("rings"))?;
        let t = self
            .tasks
            .get_mut(&task.0)
            .ok_or(PlaneError::Unknown("task"))?;
        t.engine.close(&*view);
        match self.plane.calls.close(&t.input) {
            Ok(()) | Err(Error::BadState) => {}
            Err(e) => return Err(kernel(e)),
        }
        self.answer_wait(task.0)
    }

    /// A ring task's windows are given by the plane itself.
    fn grant(&mut self, task: TaskId, output: u32, bytes: u64) -> Result<(), PlaneError> {
        if self.tasks.contains_key(&task.0) {
            return if output == 0 {
                Ok(())
            } else {
                Err(PlaneError::Unknown("output"))
            };
        }
        self.plane.grant(task, output, bytes)
    }

    fn next(&mut self, deadline: u64) -> Result<Option<Event>, PlaneError> {
        loop {
            self.settle_ring();
            if let Some(e) = self.plane.events.pop_front() {
                return Ok(Some(e));
            }
            self.plane.poll()?;
            let ids: Vec<u32> = self.tasks.keys().copied().collect();
            for id in ids {
                self.pump(id)?;
            }
            self.signaled = false;
            if !self.plane.events.is_empty() {
                continue;
            }
            // Read the rings a while before sleeping: a task that answers within it costs
            // the engine no call.
            let mut ready = false;
            for _ in 0..self.spins {
                if self.ready() {
                    ready = true;
                    break;
                }
                core::hint::spin_loop();
            }
            if ready {
                continue;
            }
            if self.set_waiting(true) {
                self.set_waiting(false);
                continue;
            }
            let mut slots = Vec::new();
            let mut channels = Vec::new();
            for t in self.plane.tasks.values() {
                slots.extend(t.open_slots().map(|s| &s.slot));
                if let Some(c) = t.open_port() {
                    channels.push(c);
                }
            }
            for t in self.tasks.values() {
                if !t.input_done {
                    slots.push(&t.input);
                }
                if !t.output_done {
                    slots.push(&t.output);
                }
                if !t.port_done {
                    channels.push(&t.port);
                }
            }
            if slots.is_empty() && channels.is_empty() {
                return Ok(None);
            }
            self.counters.sleeps += 1;
            let woke = self.plane.calls.wait(&slots, &channels, deadline);
            self.set_waiting(false);
            let woke = woke.map_err(kernel)?;
            self.signaled = woke == Ready::Some;
            if woke == Ready::TimedOut {
                // A last look: a post the task made as the engine went to sleep.
                if self.ready() {
                    continue;
                }
                return Ok(None);
            }
        }
    }

    fn answer(&mut self, call: CallId, reply: Result<&[u8], Error>) -> Result<(), PlaneError> {
        let Some(t) = self.tasks.get(&call.task.0) else {
            return self.plane.answer(call, reply);
        };
        let mut header = alloc::vec![0u8; reply::SIZE];
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
                let made = self.plane.calls.memory(size).map_err(kernel)?;
                self.plane.calls.write(&made, 0, bytes).map_err(kernel)?;
                let shared = self.plane.calls.share(&made).map_err(kernel)?;
                write_u64(&mut header, reply::LEN, bytes.len() as u64);
                Some(shared)
            }
        };
        match self.plane.calls.send(&t.port, &header, memory) {
            Ok(()) => Ok(()),
            Err(Error::PeerClosed) => Err(PlaneError::Closed),
            Err(e) => Err(kernel(e)),
        }
    }

    fn drop_batch(&mut self, batch: BatchId) -> Result<(), PlaneError> {
        let r = self.plane.drop_batch(batch);
        self.settle_ring();
        r
    }

    fn residency(&self) -> Residency {
        self.plane.residency()
    }
}
