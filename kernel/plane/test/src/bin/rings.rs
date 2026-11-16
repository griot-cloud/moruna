//! DS-P (`architecture/data-os.md` §5, stream P): ring mode on the kernel. An engine on
//! `RingPlane` opens a ring task (`ring-task`) and runs:
//!
//! - lockstep: one batch at a time, as an image stage's `apply` is, each laid out in the
//!   task's region and posted with no call, each record taken back after the grouped revoke
//!   through the task's output slot (`windows_protect`'s slot form) and checked, its bytes
//!   the batch's;
//! - pipelined: a group of batches posted before any record is taken, one revoke for the
//!   group;
//! - DP1: once every record is taken, the task writes the window of its last one. The write
//!   faults, the kernel ends the task, and the record the engine checked is unchanged.
//!
//! Every kernel call the engine makes is counted, so calls per batch are measured, and the
//! task reports the bytes the kernel copied for it, which are only its bells' words.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};

use moruna_plane::{
    BatchId, DataPlane, Event, PlaneError, RingPlane, TaskId,
    calls::{Calls, Ready, Taken},
    kernel::{Kernel, Mapped},
    ring::Mode,
    ringed::{Region, RingCalls, RingShape, View},
};
use moruna_svc::{Channel, Console, Handle, deadline_in, println, start};
use moruna_sys::{
    Account, Error, Memory, Process, map,
    plane::{Slot, attach},
};

moruna_svc::main!(run);

const PAGE: u64 = 4096;
const MIB: u64 = 1 << 20;
const SECONDS: u64 = 1_000_000_000;
/// The engine's plane: the task's region and its rings object.
const BUDGET: u64 = 64 * MIB;
/// The task's account: its process, its thread, its image and its tables.
const TASK_LIMIT: u64 = 16 * MIB;
/// Lockstep batches.
const LOCKSTEP: u64 = 200;
/// Batches of the pipelined group.
const GROUP: u64 = 32;
/// The port the task reports on, and the one it waits for every record taken on.
const REPORT: u64 = 2;
const TAKEN: u64 = 1;

/// The kernel's calls, counted.
struct Counted {
    calls: u64,
}

impl Calls for Counted {
    type Memory = Memory;
    type Slot = Slot;
    type Channel = Channel;

    fn memory(&mut self, bytes: u64) -> Result<Memory, Error> {
        self.calls += 1;
        Kernel.memory(bytes)
    }
    fn share(&mut self, memory: &Memory) -> Result<Memory, Error> {
        self.calls += 1;
        Kernel.share(memory)
    }
    fn read(&mut self, memory: &Memory, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        self.calls += 1;
        Kernel.read(memory, offset, buf)
    }
    fn write(&mut self, memory: &Memory, offset: u64, data: &[u8]) -> Result<(), Error> {
        self.calls += 1;
        Kernel.write(memory, offset, data)
    }
    fn slot(&mut self, output: bool) -> Result<Slot, Error> {
        self.calls += 1;
        Kernel.slot(output)
    }
    fn push(
        &mut self,
        slot: &Slot,
        memory: Memory,
        offset: u64,
        len: u64,
        cookie: u64,
    ) -> Result<(), (Error, Memory)> {
        self.calls += 1;
        Kernel.push(slot, memory, offset, len, cookie)
    }
    fn take(&mut self, slot: &Slot) -> Result<Taken<Memory>, Error> {
        self.calls += 1;
        Kernel.take(slot)
    }
    fn close(&mut self, slot: &Slot) -> Result<(), Error> {
        self.calls += 1;
        Kernel.close(slot)
    }
    fn refuse(&mut self, slot: &Slot, reason: &str) -> Result<(), Error> {
        self.calls += 1;
        Kernel.refuse(slot, reason)
    }
    fn channel(&mut self) -> Result<(Channel, Channel), Error> {
        self.calls += 1;
        Kernel.channel()
    }
    fn receive(&mut self, channel: &Channel, buf: &mut [u8]) -> Result<usize, Error> {
        self.calls += 1;
        Kernel.receive(channel, buf)
    }
    fn send(
        &mut self,
        channel: &Channel,
        bytes: &[u8],
        memory: Option<Memory>,
    ) -> Result<(), Error> {
        self.calls += 1;
        Kernel.send(channel, bytes, memory)
    }
    fn wait(
        &mut self,
        slots: &[&Slot],
        channels: &[&Channel],
        deadline: u64,
    ) -> Result<Ready, Error> {
        self.calls += 1;
        Kernel.wait(slots, channels, deadline)
    }
}

impl RingCalls for Counted {
    type Mapping = Mapped;

    fn map(&mut self, memory: &Memory, len: u64) -> Result<Mapped, Error> {
        self.calls += 1;
        Kernel.map(memory, len)
    }
    fn keep(&mut self, memory: &Memory) -> Result<Memory, Error> {
        self.calls += 1;
        Kernel.keep(memory)
    }
    fn protect(&mut self, slot: &Slot, ranges: &[[u64; 2]], writable: bool) -> Result<(), Error> {
        self.calls += 1;
        Kernel.protect(slot, ranges, writable)
    }
    fn now(&mut self) -> u64 {
        self.calls += 1;
        Kernel.now()
    }
    fn revokes(&mut self, slot: &Slot) -> bool {
        Kernel.revokes(slot)
    }
}

type Plane = RingPlane<Counted>;

/// Every kernel call the engine has made: its calls, and its futex waits and wakes.
fn calls(p: &mut Plane) -> u64 {
    let c = p.counters();
    p.plane.calls().calls + c.bell_sleeps + c.bells
}

fn plane_error(e: PlaneError) -> String {
    format!("{e:?}")
}

fn kernel_error(what: &str) -> impl FnOnce(Error) -> String + '_ {
    move |e| format!("{what}: {}", e.name())
}

/// What a data handle of the boot holds: a `u64` length and then that many bytes.
fn fixture(name: &str) -> Result<Vec<u8>, String> {
    let handle = start::take(name).ok_or_else(|| format!("no fixture `{name}`"))?;
    let memory = Memory(handle);
    let head = memory
        .map(0, PAGE, map::READ)
        .map_err(kernel_error("mapping a fixture"))?;
    let mut word = [0u8; 8];
    head.read(0, &mut word)
        .map_err(kernel_error("reading a fixture"))?;
    drop(head);
    let len = u64::from_le_bytes(word);
    let span = (8 + len).div_ceil(PAGE) * PAGE;
    let whole = memory
        .map(0, span, map::READ)
        .map_err(kernel_error("mapping a fixture"))?;
    let mut bytes = vec![0u8; len as usize];
    whole
        .read(8, &mut bytes)
        .map_err(kernel_error("reading a fixture"))?;
    Ok(bytes)
}

fn take_resource(out: &Console) -> Option<Handle> {
    let supervisor = Channel(start::take("supervisor")?);
    let mut buf = [0u8; 16];
    let mut handles = [None];
    if let Err(e) = moruna_svc::wait_one(
        &supervisor.0,
        moruna_svc::Signals::READABLE | moruna_svc::Signals::PEER_CLOSED,
        deadline_in(60 * SECONDS),
    ) {
        println!(out, "DS-P ring: FAIL: no process resource: {}", e.name());
        return None;
    }
    supervisor.read(&mut buf, &mut handles).ok()?;
    handles[0].take()
}

/// The record `batch` holds, read through the engine's mapping.
fn bytes_of(p: &Plane, batch: BatchId) -> Result<Vec<u8>, String> {
    let (view, at, len) = p
        .batch_view(batch)
        .ok_or_else(|| format!("{batch:?} is not a ring batch"))?;
    let mut out = vec![0u8; len as usize];
    view.read(at, &mut out)
        .map_err(kernel_error("reading a record"))?;
    Ok(out)
}

struct Run {
    out: Console,
    plane: Plane,
    task: TaskId,
    /// The task's port calls that wait until every record is taken.
    waiting: Vec<moruna_plane::CallId>,
    report: Option<(u64, u64)>,
    ended: bool,
}

impl Run {
    /// The next record the task puts, answering its port calls meanwhile.
    fn record(&mut self) -> Result<(BatchId, u64), String> {
        loop {
            match self
                .plane
                .next(deadline_in(30 * SECONDS))
                .map_err(plane_error)?
            {
                Some(Event::Put { batch, rows, .. }) => return Ok((batch, rows)),
                Some(Event::Released { .. } | Event::Freed { .. }) => {}
                Some(e) => self.event(e)?,
                None => return Err(String::from("no record within 30 s")),
            }
        }
    }

    fn event(&mut self, e: Event) -> Result<(), String> {
        match e {
            Event::Call {
                call,
                port,
                request,
            } => match port {
                REPORT if request.len() == 16 => {
                    let word = |at: usize| {
                        let mut w = [0u8; 8];
                        w.copy_from_slice(&request[at..at + 8]);
                        u64::from_le_bytes(w)
                    };
                    self.report = Some((word(0), word(8)));
                    self.plane.answer(call, Ok(&[])).map_err(plane_error)
                }
                TAKEN => {
                    self.waiting.push(call);
                    Ok(())
                }
                _ => self
                    .plane
                    .answer(call, Err(Error::AccessDenied))
                    .map_err(plane_error),
            },
            Event::Refused { reason, .. } => Err(format!("a record refused: {reason}")),
            Event::Ended { .. } => {
                self.ended = true;
                Ok(())
            }
            Event::Put { batch, .. } => {
                let _ = self.plane.drop_batch(batch);
                Err(String::from("a record nobody asked for"))
            }
            Event::Released { .. } | Event::Freed { .. } => Ok(()),
        }
    }

    fn post(&mut self, region: moruna_plane::ringed::RegionId, good: &[u8]) -> Result<(), String> {
        let (b, _) = self
            .plane
            .load_into(region, good.len() as u64, |v, at| v.write(at, good))
            .map_err(plane_error)?;
        self.plane.offer(self.task, 0, b).map_err(plane_error)?;
        self.plane.drop_batch(b).map_err(plane_error)
    }
}

fn lockstep(
    r: &mut Run,
    region: moruna_plane::ringed::RegionId,
    good: &[u8],
) -> Result<(), String> {
    let calls0 = calls(&mut r.plane);
    let c0 = r.plane.counters();
    for i in 0..LOCKSTEP {
        r.post(region, good)?;
        let (out, rows) = r.record()?;
        if rows != 4 || bytes_of(&r.plane, out)? != good {
            return Err(format!("record {i} is not the batch it answers"));
        }
        r.plane.drop_batch(out).map_err(plane_error)?;
    }
    let c = r.plane.counters();
    let spent = calls(&mut r.plane) - calls0;
    println!(
        r.out,
        "DS-P ring: ok: {} batches in lockstep in protect mode, each read in place and its record checked after the revoke: {} engine calls ({}.{:02} a batch), {} revokes, {} regrants, {} sleeps on the bell, {} wakes of the task",
        LOCKSTEP,
        spent,
        spent / LOCKSTEP,
        spent * 100 / LOCKSTEP % 100,
        c.revokes - c0.revokes,
        c.regrants - c0.regrants,
        c.bell_sleeps - c0.bell_sleeps,
        c.bells - c0.bells
    );
    Ok(())
}

fn pipelined(
    r: &mut Run,
    region: moruna_plane::ringed::RegionId,
    good: &[u8],
) -> Result<BatchId, String> {
    let calls0 = calls(&mut r.plane);
    let c0 = r.plane.counters();
    for _ in 0..GROUP {
        r.post(region, good)?;
    }
    let mut held = Vec::new();
    for _ in 0..GROUP {
        let (out, _) = r.record()?;
        if bytes_of(&r.plane, out)? != good {
            return Err(String::from("a record is not its batch"));
        }
        held.push(out);
    }
    let spent = calls(&mut r.plane) - calls0;
    let c = r.plane.counters();
    // The last record stays held, its window revoked, for DP1.
    let last = held.pop().ok_or("no record")?;
    for b in held {
        r.plane.drop_batch(b).map_err(plane_error)?;
    }
    println!(
        r.out,
        "DS-P pipelined: ok: {} batches posted before any record was taken: {} engine calls ({}.{:02} a batch), {} revokes",
        GROUP,
        spent,
        spent / GROUP,
        spent * 100 / GROUP % 100,
        c.revokes - c0.revokes
    );
    if spent >= GROUP {
        return Err(format!("{spent} calls for {GROUP} batches in flight"));
    }
    Ok(last)
}

fn revoked(r: &mut Run, last: BatchId, good: &[u8]) -> Result<(), String> {
    r.plane.finish(r.task, 0).map_err(plane_error)?;
    // The task reports its copies, then waits until every record is taken: they are.
    let deadline = deadline_in(60 * SECONDS);
    while r.waiting.is_empty() {
        match r.plane.next(deadline).map_err(plane_error)? {
            Some(e) => r.event(e)?,
            None => return Err(String::from("the task never asked to write")),
        }
    }
    let (copies, waits) = r.report.ok_or("the task reported nothing")?;
    if copies != 4 * waits {
        return Err(format!(
            "the kernel copied {copies} bytes for the task, which slept {waits} times: \
             more than its bells' words"
        ));
    }
    println!(
        r.out,
        "DS-P copies: ok: the kernel copied {} bytes for ring-task, the {} words of its {} sleeps on its bell, and none of a batch or a record",
        copies,
        waits,
        waits
    );
    println!(r.out, "DS-P revoke: ring-task writes a posted window");
    for call in core::mem::take(&mut r.waiting) {
        r.plane.answer(call, Ok(b"go")).map_err(plane_error)?;
    }
    while !r.ended {
        match r.plane.next(deadline).map_err(plane_error)? {
            Some(e) => r.event(e)?,
            None => return Err(String::from("the task did not end")),
        }
    }
    if bytes_of(&r.plane, last)? != good {
        return Err(String::from("the checked record changed after the revoke"));
    }
    r.plane.drop_batch(last).map_err(plane_error)?;
    println!(
        r.out,
        "DS-P revoke: ok: the window of the last record faulted on the task's write, and the record the engine checked after the revoke is unchanged"
    );
    Ok(())
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(resource) = take_resource(&out) else {
        return 1;
    };
    let Some(image) = start::take("task-image") else {
        println!(out, "DS-P ring: FAIL: no task image");
        return 90;
    };
    let good = match fixture("good") {
        Ok(g) => g,
        Err(why) => {
            println!(out, "DS-P ring: FAIL: {}", why);
            return 1;
        }
    };
    let started = (|| -> Result<(Plane, TaskId, Process, Account), String> {
        let mut plane = RingPlane::new(Counted { calls: 0 }, BUDGET);
        let account =
            Account::carve(TASK_LIMIT, "ring-task").map_err(kernel_error("account_create"))?;
        let process = Process::create_in(&resource, "ring-task", &account.0)
            .map_err(kernel_error("process_create"))?;
        let entry = process
            .map_image(&image)
            .map_err(kernel_error("process_map_image"))?;
        let (task, port) = plane
            .open_ring(RingShape {
                region: Region::New(MIB),
                windows: GROUP,
                window_bytes: 2 * PAGE,
            })
            .map_err(plane_error)?;
        if plane.mode(task) != Some(Mode::Protect) {
            return Err(String::from("the plane did not choose protect mode"));
        }
        let slots = plane.ring_slots(task).map_err(plane_error)?;
        attach(&process, &slots, Some(port))
            .map_err(|(e, _)| format!("process_plane: {}", e.name()))?;
        process
            .start_with(entry, &mut [])
            .map_err(kernel_error("process_start"))?;
        Ok((plane, task, process, account))
    })();
    let (plane, task, _process, _account) = match started {
        Ok(s) => s,
        Err(why) => {
            println!(out, "DS-P ring: FAIL: {}", why);
            return 1;
        }
    };
    let Some(region) = plane.region_of(task) else {
        println!(out, "DS-P ring: FAIL: no region");
        return 1;
    };
    let mut r = Run {
        out,
        plane,
        task,
        waiting: Vec::new(),
        report: None,
        ended: false,
    };
    if let Err(why) = lockstep(&mut r, region, &good) {
        println!(r.out, "DS-P ring: FAIL: {}", why);
        return 1;
    }
    let last = match pipelined(&mut r, region, &good) {
        Ok(last) => last,
        Err(why) => {
            println!(r.out, "DS-P pipelined: FAIL: {}", why);
            return 1;
        }
    };
    if let Err(why) = revoked(&mut r, last, &good) {
        println!(r.out, "DS-P revoke: FAIL: {}", why);
        return 1;
    }
    0
}
