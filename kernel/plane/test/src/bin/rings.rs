//! DS-P (`architecture/data-os.md` §5, stream P): ring mode on the kernel. An engine on
//! `RingPlane` runs a native task (`ring-task`) that applies a batch function, `y = 3 * x + 7`,
//! to batches of one `Int64` column of 128 Ki values (1 MiB):
//!
//! - each batch is made in the task's region, where the task reads it in place: no copy and no
//!   call to hand it over;
//! - each record the task writes is taken back after the grouped revoke through the task's
//!   output slot (`windows_protect`'s slot form), checked, and every one of its values judged;
//! - lockstep, one batch at a time as an image stage's `apply` is, in one task; then a steady
//!   state of groups of 32 in flight, in another;
//! - DP1: once every record is taken, the second task writes the window of its last one. The
//!   write faults, the kernel ends the task, and the record the engine checked is unchanged.
//!
//! Every kernel call the engine makes is counted, and each task reports its own calls on the
//! bells and the bytes the kernel copied for it, which are only its bells' words.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};

use moruna_ipc_check::{frame, record};
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
/// The engine's plane: each task's region and its rings object.
const BUDGET: u64 = 96 * MIB;
/// The task's account: its process, its thread, its image and its tables.
const TASK_LIMIT: u64 = 16 * MIB;
/// Lockstep batches.
const LOCKSTEP: u64 = 100;
/// Batches in flight at once in the steady state, and how many groups of them.
const GROUP: u64 = 32;
const GROUPS: u64 = 8;
/// Values of a batch.
const ROWS: u64 = 128 << 10;
/// Values the engine writes or judges at a time.
const CHUNK: usize = 8192;
/// The port the task reports on, and the one it waits for every record taken on.
const REPORT: u64 = 2;
const TAKEN: u64 = 1;

/// The kernel's calls, counted; the plane's memory is charged to its own account.
struct Counted {
    calls: u64,
    account: Account,
}

impl Calls for Counted {
    type Memory = Memory;
    type Slot = Slot;
    type Channel = Channel;

    fn memory(&mut self, bytes: u64) -> Result<Memory, Error> {
        self.calls += 1;
        Memory::create_in(bytes, &self.account.0)
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

/// The value `x` of row `j` of batch `n`.
fn x_of(n: u64, j: u64) -> i64 {
    (n * 1_000_000 + j) as i64
}

/// The batch function the task applies.
fn f(x: i64) -> i64 {
    x.wrapping_mul(3).wrapping_add(7)
}

/// A batch's layout, as its framing declares it: the record's length, where its values start,
/// and its validity bitmap, if the column has one.
struct Layout {
    framing: Vec<u8>,
    values: u64,
    validity: Option<(u64, u64)>,
    len: u64,
}

impl Layout {
    fn of(framing: Vec<u8>) -> Result<Layout, String> {
        let batch = 8 + frame::message_len(&framing)?;
        let body = frame::body_length(framing.get(batch..).ok_or("no batch message")?)?;
        let len = framing.len() as u64 + u64::try_from(body).map_err(|_| "a negative body")?;
        let record = record::check_record(&framing, len)?;
        if record.rows != ROWS {
            return Err(format!("the framing declares {} rows", record.rows));
        }
        let values = record
            .buffers
            .iter()
            .find(|(_, l)| *l == ROWS * 8)
            .map(|(at, _)| *at)
            .ok_or("no buffer of the values")?;
        let validity = record
            .buffers
            .iter()
            .copied()
            .find(|(_, l)| *l > 0 && *l != ROWS * 8);
        Ok(Layout {
            framing,
            values,
            validity,
            len,
        })
    }
}

struct Run {
    out: Console,
    plane: Plane,
    resource: Handle,
    image: Handle,
    layout: Layout,
    task: TaskId,
    /// The task's port calls that wait until every record is taken.
    waiting: Vec<moruna_plane::CallId>,
    /// What the task reported: the kernel's copies for it, its sleeps and its wakes.
    report: Option<(u64, u64, u64)>,
    ended: bool,
    batches: u64,
    /// The task's process and account, kept while it lives.
    held: Option<(Process, Account)>,
}

impl Run {
    fn start(&mut self, name: &str, region: u64, windows: u64) -> Result<(), String> {
        let account = Account::carve(TASK_LIMIT, name).map_err(kernel_error("account_create"))?;
        let process = Process::create_in(&self.resource, name, &account.0)
            .map_err(kernel_error("process_create"))?;
        let entry = process
            .map_image(&self.image)
            .map_err(kernel_error("process_map_image"))?;
        let (task, port) = self
            .plane
            .open_ring(RingShape {
                region: Region::New(region),
                windows,
                window_bytes: self.layout.len.div_ceil(PAGE) * PAGE,
            })
            .map_err(plane_error)?;
        if self.plane.mode(task) != Some(Mode::Protect) {
            return Err(String::from("the plane did not choose protect mode"));
        }
        let slots = self.plane.ring_slots(task).map_err(plane_error)?;
        attach(&process, &slots, Some(port))
            .map_err(|(e, _)| format!("process_plane: {}", e.name()))?;
        process
            .start_with(entry, &mut [])
            .map_err(kernel_error("process_start"))?;
        self.task = task;
        self.report = None;
        self.ended = false;
        self.held = Some((process, account));
        Ok(())
    }

    /// Makes batch `n` in the task's region and posts it: no copy, no call.
    fn post(&mut self, n: u64) -> Result<(), String> {
        let region = self.plane.region_of(self.task).ok_or("no region")?;
        let layout = &self.layout;
        let mut chunk = vec![0u8; CHUNK * 8];
        let (b, rows) = self
            .plane
            .load_into(region, layout.len, |v, at| {
                v.write(at, &layout.framing)?;
                // Every value is valid.
                if let Some((bitmap, bytes)) = layout.validity {
                    let ones = vec![0xffu8; bytes as usize];
                    v.write(at + bitmap, &ones)?;
                }
                let mut j = 0;
                while j < ROWS {
                    let k = (ROWS - j).min(CHUNK as u64);
                    let (words, _) = chunk.as_chunks_mut::<8>();
                    for (i, w) in words.iter_mut().take(k as usize).enumerate() {
                        *w = x_of(n, j + i as u64).to_le_bytes();
                    }
                    v.write(at + layout.values + j * 8, &chunk[..k as usize * 8])?;
                    j += k;
                }
                Ok(())
            })
            .map_err(plane_error)?;
        if rows != ROWS {
            return Err(format!("batch {n} holds {rows} rows"));
        }
        self.plane.offer(self.task, 0, b).map_err(plane_error)?;
        self.plane.drop_batch(b).map_err(plane_error)
    }

    /// The next record the task puts, answering its port calls meanwhile.
    fn record(&mut self) -> Result<BatchId, String> {
        loop {
            match self
                .plane
                .next(deadline_in(30 * SECONDS))
                .map_err(plane_error)?
            {
                Some(Event::Put { batch, rows, .. }) => {
                    if rows != ROWS {
                        return Err(format!("a record of {rows} rows"));
                    }
                    return Ok(batch);
                }
                Some(Event::Released { .. } | Event::Freed { .. }) => {}
                Some(e) => self.event(e)?,
                None => return Err(String::from("no record within 30 s")),
            }
        }
    }

    /// Judges every value of the record answering batch `n`.
    fn judge(&mut self, record: BatchId, n: u64) -> Result<(), String> {
        let (view, at, len) = self
            .plane
            .batch_view(record)
            .ok_or("the record is not a ring batch")?;
        if len != self.layout.len {
            return Err(format!("a record of {len} bytes"));
        }
        let mut chunk = vec![0u8; CHUNK * 8];
        let mut j = 0;
        while j < ROWS {
            let k = (ROWS - j).min(CHUNK as u64);
            let bytes = &mut chunk[..k as usize * 8];
            view.read(at + self.layout.values + j * 8, bytes)
                .map_err(kernel_error("reading a record"))?;
            let (words, _) = bytes.as_chunks::<8>();
            for (i, w) in words.iter().enumerate() {
                let row = j + i as u64;
                if i64::from_le_bytes(*w) != f(x_of(n, row)) {
                    return Err(format!("batch {n} row {row} is not f(x)"));
                }
            }
            j += k;
        }
        self.batches += 1;
        Ok(())
    }

    fn event(&mut self, e: Event) -> Result<(), String> {
        match e {
            Event::Call {
                call,
                port,
                request,
            } => match port {
                REPORT if request.len() == 24 => {
                    let word = |at: usize| {
                        let mut w = [0u8; 8];
                        w.copy_from_slice(&request[at..at + 8]);
                        u64::from_le_bytes(w)
                    };
                    self.report = Some((word(0), word(8), word(16)));
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

    /// Ends the task's input and waits for its report and its question.
    fn wind_up(&mut self) -> Result<(u64, u64, u64), String> {
        self.plane.finish(self.task, 0).map_err(plane_error)?;
        let deadline = deadline_in(60 * SECONDS);
        while self.waiting.is_empty() {
            match self.plane.next(deadline).map_err(plane_error)? {
                Some(e) => self.event(e)?,
                None => return Err(String::from("the task never asked to write")),
            }
        }
        let (copies, sleeps, wakes) = self.report.ok_or("the task reported nothing")?;
        if copies != 4 * sleeps {
            return Err(format!(
                "the kernel copied {copies} bytes for the task, which slept {sleeps} times: \
                 more than its bells' words"
            ));
        }
        Ok((copies, sleeps, wakes))
    }

    /// A failed test ends its task, which would otherwise sleep on its bell for good.
    fn fail(&mut self) -> i64 {
        if let Some((process, _)) = self.held.take() {
            let _ = process.kill();
        }
        1
    }

    /// Answers the task's question, and waits for its end.
    fn end(&mut self, write: bool) -> Result<(), String> {
        for call in core::mem::take(&mut self.waiting) {
            let answer: Result<&[u8], Error> = if write {
                Ok(b"write")
            } else {
                Err(Error::AccessDenied)
            };
            self.plane.answer(call, answer).map_err(plane_error)?;
        }
        // A task that faults rings nothing: its end is read at each look, every 250 ms.
        let deadline = deadline_in(60 * SECONDS);
        while !self.ended {
            if moruna_svc::now() >= deadline {
                return Err(String::from("the task did not end"));
            }
            if let Some(e) = self
                .plane
                .next(deadline_in(SECONDS / 4))
                .map_err(plane_error)?
            {
                self.event(e)?;
            }
        }
        self.held = None;
        Ok(())
    }
}

/// `a / b` with two decimals.
fn ratio(a: u64, b: u64) -> String {
    format!("{}.{:02}", a / b, a * 100 / b % 100)
}

fn lockstep(r: &mut Run) -> Result<(), String> {
    r.start("ring-lockstep", 4 * MIB, 4)?;
    let calls0 = calls(&mut r.plane);
    let c0 = r.plane.counters();
    for n in 0..LOCKSTEP {
        r.post(n)?;
        let out = r.record()?;
        r.judge(out, n)?;
        r.plane.drop_batch(out).map_err(plane_error)?;
    }
    let spent = calls(&mut r.plane) - calls0;
    let c = r.plane.counters();
    let (copies, sleeps, wakes) = r.wind_up()?;
    r.end(false)?;
    println!(
        r.out,
        "DS-P lockstep: ok: {} batches of {} rows, one at a time, each made in the task's region and read in place, y = 3x + 7 judged on every row: engine {} calls a batch ({} revokes, {} regrants, {} sleeps on its bell, {} wakes of the task); task {} calls a batch ({} sleeps, {} wakes); the kernel copied {} bytes for the task, its bells' words",
        LOCKSTEP,
        ROWS,
        ratio(spent, LOCKSTEP),
        c.revokes - c0.revokes,
        c.regrants - c0.regrants,
        c.bell_sleeps - c0.bell_sleeps,
        c.bells - c0.bells,
        ratio(sleeps + wakes, LOCKSTEP),
        sleeps,
        wakes,
        copies
    );
    Ok(())
}

fn steady(r: &mut Run) -> Result<BatchId, String> {
    r.start(
        "ring-steady",
        (GROUP + 2) * (r.layout.len.div_ceil(PAGE) * PAGE),
        GROUP,
    )?;
    let mut last = None;
    let mut first_group = 0;
    let c0 = r.plane.counters();
    let calls0 = calls(&mut r.plane);
    for g in 0..GROUPS {
        if g == 1 {
            first_group = calls(&mut r.plane) - calls0;
        }
        for i in 0..GROUP {
            r.post(g * GROUP + i)?;
        }
        for i in 0..GROUP {
            let out = r.record()?;
            r.judge(out, g * GROUP + i)?;
            if g + 1 == GROUPS && i + 1 == GROUP {
                // The last record stays held, its window revoked, for DP1.
                last = Some(out);
            } else {
                r.plane.drop_batch(out).map_err(plane_error)?;
            }
        }
    }
    let spent = calls(&mut r.plane) - calls0;
    let c = r.plane.counters();
    let steady_calls = spent - first_group;
    let steady_batches = (GROUPS - 1) * GROUP;
    let (copies, sleeps, wakes) = r.wind_up()?;
    println!(
        r.out,
        "DS-P steady: ok: {} batches of {} rows in groups of {} in flight, y = 3x + 7 judged on every row: engine {} calls a batch in the steady state ({} calls for {} batches after the first group; {} revokes and {} regrants in all); task {} calls a batch ({} sleeps, {} wakes); the kernel copied {} bytes for the task, its bells' words",
        GROUPS * GROUP,
        ROWS,
        GROUP,
        ratio(steady_calls, steady_batches),
        steady_calls,
        steady_batches,
        c.revokes - c0.revokes,
        c.regrants - c0.regrants,
        ratio(sleeps + wakes, GROUPS * GROUP),
        sleeps,
        wakes,
        copies
    );
    if steady_calls >= steady_batches || sleeps + wakes >= GROUPS * GROUP {
        return Err(String::from("a call a batch or more in the steady state"));
    }
    last.ok_or_else(|| String::from("no record"))
}

fn revoked(r: &mut Run, last: BatchId) -> Result<(), String> {
    let (view, at, len) = r.plane.batch_view(last).ok_or("no view")?;
    let mut checked = vec![0u8; len as usize];
    view.read(at, &mut checked)
        .map_err(kernel_error("reading a record"))?;
    println!(r.out, "DS-P revoke: ring-steady writes a posted window");
    r.end(true)?;
    let mut now = vec![0u8; len as usize];
    view.read(at, &mut now)
        .map_err(kernel_error("reading a record"))?;
    if now != checked {
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
    let framing = match fixture("mib") {
        Ok(g) => g,
        Err(why) => {
            println!(out, "DS-P ring: FAIL: {}", why);
            return 1;
        }
    };
    let account = match Account::carve(BUDGET, "rings.arena") {
        Ok(a) => a,
        Err(e) => {
            println!(out, "DS-P ring: FAIL: arena account: {}", e.name());
            return 1;
        }
    };
    let layout = match Layout::of(framing) {
        Ok(l) => l,
        Err(why) => {
            println!(out, "DS-P ring: FAIL: the batch's framing: {}", why);
            return 1;
        }
    };
    let mut r = Run {
        out,
        plane: RingPlane::new(Counted { calls: 0, account }, BUDGET),
        resource,
        image,
        layout,
        task: TaskId(0),
        waiting: Vec::new(),
        report: None,
        ended: false,
        batches: 0,
        held: None,
    };
    if let Err(why) = lockstep(&mut r) {
        println!(r.out, "DS-P lockstep: FAIL: {}", why);
        return r.fail();
    }
    let last = match steady(&mut r) {
        Ok(last) => last,
        Err(why) => {
            println!(r.out, "DS-P steady: FAIL: {}", why);
            return r.fail();
        }
    };
    if let Err(why) = revoked(&mut r, last) {
        println!(r.out, "DS-P revoke: FAIL: {}", why);
        return r.fail();
    }
    println!(
        r.out,
        "DS-P ring: ok: {} batches answered and judged, the plane zeroed {} bytes another task had seen",
        r.batches,
        r.plane.plane.zeroed()
    );
    0
}
