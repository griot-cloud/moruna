//! DP-T10 to DP-T17 (SDD 29 k): an engine on `moruna-plane`, running the data plane's
//! tests as tasks, each a process of its own with no handle and a plane:
//!
//! - a 64 MiB batch moves from a producer to a consumer in the same frames, the kernel
//!   copying no byte of it (DP-T10);
//! - one batch to three consumers, the same frames, freed once after the last, and charged
//!   once, to the engine's account that made it: a consumer's account pays for its
//!   mapping and not for the batch (DP-T11, KB5-T5);
//! - garbage, a record whose buffers lie outside the region put, and KA10's compressed
//!   bombs are refused before the engine sees them, each task ended with the reason
//!   (DP-T12);
//! - a task reaches nothing it was not given, and cannot write a batch (DP-T13);
//! - port calls to an echo port, measured (DP-T14);
//! - 1,000 batches of 1 MiB through a producer and a consumer, measured (DP-T15);
//! - an unmodified static musl program that speaks Arrow on its standard streams runs
//!   through the Linux personality and gives the right rows, and a program's text
//!   reaches the log ring as records (DP-T16, DP-T17).

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{collections::BTreeMap, format, string::String, vec, vec::Vec};

use linux_protocol::{Bytes, Plane as LinuxPlane, how, linux, log};
use moruna_ipc_check::record::{self, Need};
use moruna_plane::{
    BatchId, CallId, DataPlane, Event, KernelPlane, PlaneError, Shape, TaskId, kernel::Kernel,
};
use moruna_record::{record::Decoded, ring::Ring};
use moruna_svc::{Channel, Console, Handle, deadline_in, now, println, start};
use moruna_sys::{
    Account, Error, Memory, Process, Rights, Usage, charge, map,
    plane::{attach, object_frame},
};
use plane_test::{Role, layout, port, role, values};

moruna_svc::main!(run);

const PAGE: u64 = 4096;
const MIB: u64 = 1 << 20;
const SECONDS: u64 = 1_000_000_000;
/// What the engine's arena may make: the 64 MiB window, and room for the rest.
const BUDGET: u64 = 128 * MIB;
/// DP-T15's batches.
const MANY: u64 = 1000;
/// DP-T15's windows in flight at once.
const IN_FLIGHT: u64 = 4;
/// DP-T14's round trips.
const ECHOES: u64 = 2000;
/// Each task's account, carved out of the engine's (SDD 20 f.18): its process, its
/// thread, its image and the tables of what it maps. The batches and windows it maps are
/// the engine's, and charged to the engine.
const TASK_LIMIT: u64 = 16 * MIB;
/// arrow_echo's account: a static musl program with Arrow, its heap and its threads.
const SHIMLESS_LIMIT: u64 = 128 * MIB;

type Plane = KernelPlane<Kernel>;

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

struct Engine {
    out: Console,
    resource: Handle,
    task_image: Handle,
    ro_image: Handle,
    plane: Plane,
    roles: BTreeMap<TaskId, Vec<u8>>,
    reports: BTreeMap<TaskId, Vec<u64>>,
    waits: Vec<CallId>,
    processes: BTreeMap<TaskId, Process>,
    /// Each live task's account.
    accounts: BTreeMap<TaskId, Account>,
}

impl Engine {
    fn spawn(
        &mut self,
        name: &str,
        ro: bool,
        shape: Shape,
        role: Option<Role<'_>>,
    ) -> Result<TaskId, String> {
        let account = Account::carve(TASK_LIMIT, name).map_err(kernel_error("account_create"))?;
        let process = Process::create_in(&self.resource, name, &account.0)
            .map_err(kernel_error("process_create"))?;
        let image = if ro { &self.ro_image } else { &self.task_image };
        let entry = process
            .map_image(image)
            .map_err(kernel_error("process_map_image"))?;
        let (task, port) = self.plane.open(shape).map_err(plane_error)?;
        let slots = self.plane.slots(task).map_err(plane_error)?;
        attach(&process, &slots, port).map_err(|(e, _)| format!("process_plane: {}", e.name()))?;
        process
            .start_with(entry, &mut [])
            .map_err(kernel_error("process_start"))?;
        if let Some(role) = role {
            self.roles.insert(task, role.encode());
        }
        self.processes.insert(task, process);
        self.accounts.insert(task, account);
        Ok(task)
    }

    /// A handle to `task`'s account, read only, which keeps it after the task has ended.
    fn account(&self, task: TaskId) -> Result<Account, String> {
        let account = self
            .accounts
            .get(&task)
            .ok_or_else(|| format!("task {} has no account", task.0))?;
        account
            .0
            .duplicate(Rights::READ)
            .map(Account)
            .map_err(kernel_error("duplicating an account"))
    }

    /// The next event; port calls are answered here (and returned too, so a test sees a
    /// report arrive).
    fn step(&mut self, deadline: u64) -> Result<Option<Event>, String> {
        let Some(event) = self.plane.next(deadline).map_err(plane_error)? else {
            return Ok(None);
        };
        if let Event::Call {
            call,
            port: number,
            request,
        } = &event
        {
            let answer: Option<Result<Vec<u8>, Error>> = match *number {
                port::ROLE => Some(
                    self.roles
                        .get(&call.task)
                        .cloned()
                        .ok_or(Error::AccessDenied),
                ),
                port::REPORT => {
                    self.reports.insert(call.task, values(request));
                    Some(Ok(Vec::new()))
                }
                port::ECHO => Some(Ok(request.clone())),
                port::WAIT => {
                    self.waits.push(*call);
                    None
                }
                _ => Some(Err(Error::AccessDenied)),
            };
            if let Some(answer) = answer {
                let reply = match &answer {
                    Ok(bytes) => Ok(&bytes[..]),
                    Err(e) => Err(*e),
                };
                match self.plane.answer(*call, reply) {
                    Ok(()) | Err(PlaneError::Closed) => {}
                    Err(e) => return Err(plane_error(e)),
                }
            }
        }
        if let Event::Ended { task } = &event {
            self.processes.remove(task);
            self.roles.remove(task);
            self.accounts.remove(task);
        }
        Ok(Some(event))
    }

    /// Steps until `done` says so, for at most `seconds`.
    fn until(
        &mut self,
        seconds: u64,
        mut done: impl FnMut(&mut Engine, &Event) -> Result<bool, String>,
    ) -> Result<(), String> {
        let deadline = deadline_in(seconds * SECONDS);
        loop {
            match self.step(deadline)? {
                Some(event) => {
                    if done(self, &event)? {
                        return Ok(());
                    }
                }
                None => return Err(String::from("timed out waiting for the plane")),
            }
        }
    }

    /// Steps until `task` has reported, and returns its report.
    fn report(&mut self, task: TaskId, seconds: u64) -> Result<Vec<u64>, String> {
        if let Some(r) = self.reports.remove(&task) {
            return Ok(r);
        }
        self.until(seconds, |e, _| Ok(e.reports.contains_key(&task)))?;
        let r = self.reports.remove(&task).unwrap_or_default();
        match r.first() {
            Some(0) => Ok(r),
            Some(code) => Err(format!("task {} stopped with code {code}", task.0)),
            None => Err(format!("task {} sent an empty report", task.0)),
        }
    }

    /// Answers `task`'s wait, now or when it arrives.
    fn release_wait(&mut self, task: TaskId, seconds: u64) -> Result<(), String> {
        if !self.waits.iter().any(|c| c.task == task) {
            self.until(seconds, |e, _| Ok(e.waits.iter().any(|c| c.task == task)))?;
        }
        let calls: Vec<CallId> = self
            .waits
            .iter()
            .filter(|c| c.task == task)
            .copied()
            .collect();
        self.waits.retain(|c| c.task != task);
        for call in calls {
            self.plane.answer(call, Ok(&[])).map_err(plane_error)?;
        }
        Ok(())
    }

    fn ended(&mut self, task: TaskId, seconds: u64) -> Result<(), String> {
        if !self.processes.contains_key(&task) {
            return Ok(());
        }
        self.until(seconds, |_, ev| {
            Ok(matches!(ev, Event::Ended { task: t } if *t == task))
        })
    }

    /// Steps until a record arrives from `task`; a refusal is the test's failure.
    fn put_from(&mut self, task: TaskId, seconds: u64) -> Result<BatchId, String> {
        let mut got = None;
        self.until(seconds, |_, ev| match ev {
            Event::Put { task: t, batch, .. } if *t == task => {
                got = Some(*batch);
                Ok(true)
            }
            Event::Refused {
                task: t, reason, ..
            } if *t == task => Err(format!("its record was refused: {reason}")),
            _ => Ok(false),
        })?;
        got.ok_or_else(|| String::from("no record"))
    }

    /// The frames of pages `pages` of `batch`'s object.
    fn frames(&self, batch: BatchId, pages: &[u64]) -> Result<Vec<u64>, String> {
        let (memory, offset, _) = self.plane.batch(batch).map_err(plane_error)?;
        pages
            .iter()
            .map(|p| {
                object_frame(memory, ((offset + p * PAGE) / PAGE) as u32)
                    .map_err(kernel_error("plane_probe"))
            })
            .collect()
    }

    /// The bytes `at .. at + len` of `batch`.
    fn read(&mut self, batch: BatchId, at: u64, len: usize) -> Result<Vec<u8>, String> {
        let (memory, offset, _) = self.plane.batch(batch).map_err(plane_error)?;
        let shared = memory
            .0
            .duplicate(Rights::READ | Rights::MAP)
            .map(Memory)
            .map_err(kernel_error("duplicate"))?;
        let mut buf = vec![0u8; len];
        let first = (offset + at) & !(PAGE - 1);
        let span = (offset + at + len as u64).div_ceil(PAGE) * PAGE - first;
        let mapping = shared
            .map(first, span, map::READ)
            .map_err(kernel_error("mapping a batch"))?;
        mapping
            .read((offset + at - first) as usize, &mut buf)
            .map_err(kernel_error("reading a batch"))?;
        Ok(buf)
    }
}

fn shape(inputs: u32, outputs: u32, port: bool) -> Shape {
    Shape {
        inputs,
        outputs,
        port,
    }
}

/// DP-T10.
fn zero_copy(e: &mut Engine, framing: &[u8]) -> Result<String, String> {
    let len = layout(framing)?.record_len;
    let producer = e.spawn(
        "plane-producer",
        false,
        shape(0, 1, true),
        Some(Role {
            role: role::PRODUCE_BIG,
            a: len,
            b: 0,
            bytes: framing,
        }),
    )?;
    let consumer = e.spawn(
        "plane-consumer",
        false,
        shape(1, 0, true),
        Some(Role {
            role: role::CONSUME_BIG,
            a: 0,
            b: 0,
            bytes: &[],
        }),
    )?;
    e.plane.grant(producer, 0, len).map_err(plane_error)?;
    let batch = e.put_from(producer, 300)?;
    let pages = [0, (len / 2) / PAGE, (len - 1) / PAGE];
    let engine_frames = e.frames(batch, &pages)?;
    let made = e.report(producer, 300)?;
    e.plane.offer(consumer, 0, batch).map_err(plane_error)?;
    e.plane.finish(consumer, 0).map_err(plane_error)?;
    e.release_wait(consumer, 300)?;
    let took = e.report(consumer, 300)?;
    e.ended(consumer, 300)?;
    e.ended(producer, 300)?;
    e.plane.drop_batch(batch).map_err(plane_error)?;
    let (pf, cf) = (&made[1..4], &took[1..4]);
    if pf != engine_frames.as_slice() || cf != engine_frames.as_slice() {
        return Err(format!(
            "frames differ: producer {pf:x?}, engine {engine_frames:x?}, consumer {cf:x?}"
        ));
    }
    if made[4] != 0 || took[4] != 0 {
        return Err(format!(
            "the kernel copied {} bytes for the producer and {} for the consumer",
            made[4], took[4]
        ));
    }
    if took[6] != 1 {
        return Err(String::from(
            "the consumer read values that are not their indices",
        ));
    }
    Ok(format!(
        "a batch of {} bytes (64 MiB of values) went from producer to consumer in the same frames ({:#x}, {:#x}, {:#x} at pages 0, {}, {}), and the kernel copied 0 bytes for the put or the get; the window mapped in {} us, the put took {} us, batch_get mapped {} pages in {} us",
        len,
        engine_frames[0],
        engine_frames[1],
        engine_frames[2],
        pages[1],
        pages[2],
        made[5] / 1000,
        made[6] / 1000,
        len.div_ceil(PAGE),
        took[5] / 1000
    ))
}

/// DP-T11, and KB5-T5: the batch is charged once, to the engine's account when its
/// arena made the object, and never to a consumer, whose account pays only for mapping
/// it.
///
/// The batch is a record of 128 Ki `Int64` values laid out from `framing`, so it is many
/// pages and what a consumer would pay for it is plain beside what it pays for mapping it.
fn fan_out(e: &mut Engine, framing: &[u8]) -> Result<String, String> {
    let len = layout(framing)?.record_len;
    let producer = e.spawn(
        "plane-fan-producer",
        false,
        shape(0, 1, true),
        Some(Role {
            role: role::PRODUCE_BIG,
            a: len,
            b: 0,
            bytes: framing,
        }),
    )?;
    let mut consumers = Vec::new();
    for i in 0..3 {
        consumers.push(e.spawn(
            &format!("plane-fan-{i}"),
            false,
            shape(1, 0, true),
            Some(Role {
                role: role::CONSUME_ONE,
                a: 0,
                b: 0,
                bytes: &[],
            }),
        )?);
    }
    let mut watched = Vec::new();
    for c in &consumers {
        let account = e.account(*c)?;
        let base = account.usage().map_err(kernel_error("account_info"))?.used;
        watched.push((account, base));
    }
    let engine_before = Usage::own().map_err(kernel_error("account_info"))?.used;
    let made_before = e.plane.residency().made;
    e.plane.grant(producer, 0, len).map_err(plane_error)?;
    let batch = e.put_from(producer, 120)?;
    e.report(producer, 120)?;
    let frame = e.frames(batch, &[0])?[0];
    for c in &consumers {
        e.plane.offer(*c, 0, batch).map_err(plane_error)?;
        e.plane.finish(*c, 0).map_err(plane_error)?;
    }
    let shares = e.plane.residency().shares;
    // The engine lets go at once: the consumers' shares alone keep the batch.
    e.plane.drop_batch(batch).map_err(plane_error)?;
    let mut released = 0;
    let mut freed = 0;
    let mut freed_after = 0;
    e.until(120, |_, ev| {
        match ev {
            Event::Released { batch: b, .. } if *b == batch => released += 1,
            Event::Freed { batch: b } if *b == batch => {
                freed += 1;
                freed_after = released;
            }
            _ => {}
        }
        Ok(freed == 1 && released == 3)
    })?;
    for c in &consumers {
        let r = e.report(*c, 120)?;
        if r[1] != frame {
            return Err(format!(
                "a consumer read page 0 in frame {:#x}, not {frame:#x}",
                r[1]
            ));
        }
        e.ended(*c, 120)?;
    }
    e.ended(producer, 120)?;
    if shares != 3 || freed_after != 3 {
        return Err(format!(
            "{shares} shares, and the batch was freed after {freed_after} releases"
        ));
    }
    // The engine paid for what its arena made, once, and for its handle table's growth
    // at most twice over (the three duplicates it pushed); nothing for the shares.
    let made = e.plane.residency().made - made_before;
    let object = if made > 0 {
        charge::memory(made / PAGE)
    } else {
        0
    };
    let engine_paid = Usage::own()
        .map_err(kernel_error("account_info"))?
        .used
        .saturating_sub(engine_before);
    let table_step = 2 * charge::HANDLE_SLOTS * charge::HANDLE_SLOT;
    if engine_paid < object || engine_paid > object + table_step {
        return Err(format!(
            "the engine's account moved {engine_paid} bytes for a batch whose object cost {object}"
        ));
    }
    // Each consumer paid for its mapping's area and tables, not the batch's pages.
    let batch_bytes = len;
    let mut paid = Vec::new();
    for (account, base) in &watched {
        let usage = account.usage().map_err(kernel_error("account_info"))?;
        let grew = usage.peak.saturating_sub(*base);
        if grew >= batch_bytes / 4 {
            return Err(format!(
                "consumer account {} grew {grew} bytes reading a batch of {batch_bytes}",
                usage.name()
            ));
        }
        paid.push(grew);
    }
    Ok(format!(
        "one batch of {batch_bytes} bytes to 3 consumers, each reading frame {frame:#x}; 3 shares, 3 releases, then freed once; charged once: the engine paid {engine_paid} bytes ({object} for a new object), the consumers {paid:?} for their mappings"
    ))
}

/// One refusal case of DP-T12.
struct Bad {
    label: String,
    put: u64,
    says: String,
    bytes: Vec<u8>,
}

fn refusals(bytes: &[u8]) -> Result<Vec<Bad>, String> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8], String> {
        let s = bytes
            .get(at..at + n)
            .ok_or_else(|| String::from("the refusals fixture is short"))?;
        at += n;
        Ok(s)
    };
    let word = |b: &[u8]| {
        let mut w = [0u8; 8];
        w[..b.len()].copy_from_slice(b);
        u64::from_le_bytes(w)
    };
    let count = word(take(4)?);
    let mut out = Vec::new();
    for _ in 0..count {
        let n = word(take(4)?) as usize;
        let label = String::from_utf8_lossy(take(n)?).into_owned();
        let put = word(take(8)?);
        let n = word(take(4)?) as usize;
        let says = String::from_utf8_lossy(take(n)?).into_owned();
        let n = word(take(8)?) as usize;
        let bytes = take(n)?.to_vec();
        out.push(Bad {
            label,
            put,
            says,
            bytes,
        });
    }
    Ok(out)
}

/// DP-T12.
fn refuse(e: &mut Engine, cases: &[Bad]) -> Result<String, String> {
    for (i, case) in cases.iter().enumerate() {
        let task = e.spawn(
            &format!("plane-bad-{i}"),
            false,
            shape(0, 1, true),
            Some(Role {
                role: role::PUT,
                a: case.put,
                b: 0,
                bytes: &case.bytes,
            }),
        )?;
        let room = (case.bytes.len() as u64).max(case.put).div_ceil(PAGE) * PAGE;
        e.plane.grant(task, 0, room).map_err(plane_error)?;
        let mut reason = None;
        e.until(120, |_, ev| match ev {
            Event::Refused {
                task: t, reason: r, ..
            } if *t == task => {
                reason = Some(r.clone());
                Ok(true)
            }
            Event::Put { task: t, .. } if *t == task => {
                Err(format!("{}: the record was taken as a batch", case.label))
            }
            _ => Ok(false),
        })?;
        let reason = reason.unwrap_or_default();
        if !reason.contains(&case.says) {
            return Err(format!(
                "{}: refused with \"{reason}\", which does not say \"{}\"",
                case.label, case.says
            ));
        }
        e.ended(task, 120)?;
        println!(e.out, "KD1 check: refused {}: {}", case.label, reason);
    }
    if e.plane.residency().batches != 0 {
        return Err(String::from("a refused record became a batch"));
    }
    Ok(format!(
        "{} records refused before the engine saw them, each task ended with the reason",
        cases.len()
    ))
}

/// DP-T13.
fn isolation(e: &mut Engine, good: &[u8]) -> Result<String, String> {
    let (batch, _) = e.plane.load(good).map_err(plane_error)?;
    let task = e.spawn(
        "plane-isolate",
        false,
        shape(1, 0, true),
        Some(Role {
            role: role::ISOLATE,
            a: 0,
            b: 0,
            bytes: &[],
        }),
    )?;
    e.plane.offer(task, 0, batch).map_err(plane_error)?;
    e.plane.finish(task, 0).map_err(plane_error)?;
    let r = e.report(task, 300)?;
    let (wrong, addr, len) = (r[1], r[3], r[4].div_ceil(PAGE) * PAGE);
    if wrong != 0 {
        return Err(format!(
            "{wrong} probes of its plane answered as they must not"
        ));
    }
    // Not even a holder of the task's process may unmap, protect, map over or write the
    // batch it has mapped.
    let mut refusals = 0;
    if let Some(p) = e.processes.get(&task) {
        let probes = [
            p.unmap(addr, len).err() == Some(Error::InvalidArgs),
            p.protect(addr, len, map::READ | map::WRITE).err() == Some(Error::InvalidArgs),
            p.map(None, 0, len, addr, map::READ | map::WRITE | map::FIXED)
                .err()
                == Some(Error::InvalidArgs),
            p.write(addr, &[1]).err() == Some(Error::Fault),
        ];
        refusals = probes.iter().filter(|ok| **ok).count();
    }
    if refusals != 4 {
        return Err(format!(
            "{refusals} of 4 changes to the task's batch through its process were refused"
        ));
    }
    e.release_wait(task, 120)?;
    e.ended(task, 120)?;
    let ro = e.spawn("plane-ro", true, shape(1, 0, false), None)?;
    e.plane.offer(ro, 0, batch).map_err(plane_error)?;
    e.ended(ro, 120)?;
    e.plane.drop_batch(batch).map_err(plane_error)?;
    Ok(String::from(
        "4096 slot numbers it was not given and 4097 handle values answered bad handle for batch_get, batch_put, plane_get, plane_put, slot_push and slot_close; its input refused a put; a port the engine does not serve refused the call; its batch could not be unmapped, protected, mapped over or written through its process; a task that wrote into its batch faulted",
    ))
}

/// DP-T14.
fn echo(e: &mut Engine) -> Result<String, String> {
    let task = e.spawn(
        "plane-echo",
        false,
        shape(0, 0, true),
        Some(Role {
            role: role::ECHO,
            a: ECHOES,
            b: 0,
            bytes: &[],
        }),
    )?;
    let r = e.report(task, 300)?;
    e.ended(task, 120)?;
    if r[1] != 0 {
        return Err(format!("{} replies were not the request", r[1]));
    }
    Ok(format!(
        "{ECHOES} round trips of 64 bytes to the engine's echo port, each reply mapped in: least {} us, median {} us",
        r[2] / 1000,
        r[3] / 1000
    ))
}

/// DP-T15.
fn throughput(e: &mut Engine, framing: &[u8]) -> Result<String, String> {
    let len = layout(framing)?.record_len;
    let producer = e.spawn(
        "plane-bench-producer",
        false,
        shape(0, 1, true),
        Some(Role {
            role: role::PRODUCE_MANY,
            a: MANY,
            b: len,
            bytes: framing,
        }),
    )?;
    let consumer = e.spawn(
        "plane-bench-consumer",
        false,
        shape(1, 0, true),
        Some(Role {
            role: role::CONSUME_MANY,
            a: MANY,
            b: 0,
            bytes: &[],
        }),
    )?;
    let made_before = e.plane.residency().made;
    for _ in 0..IN_FLIGHT {
        e.plane.grant(producer, 0, len).map_err(plane_error)?;
    }
    let mut granted = IN_FLIGHT;
    let mut released = 0u64;
    let mut first = 0u64;
    let mut last = 0u64;
    let mut waiting: Vec<BatchId> = Vec::new();
    e.until(600, |e, ev| {
        match ev {
            Event::Put { task, batch, .. } if *task == producer => {
                if first == 0 {
                    first = now();
                }
                waiting.push(*batch);
            }
            Event::Released { task, .. } if *task == consumer => {
                released += 1;
                if released == MANY {
                    last = now();
                }
            }
            Event::Freed { .. } if granted < MANY => {
                e.plane.grant(producer, 0, len).map_err(plane_error)?;
                granted += 1;
            }
            Event::Refused { reason, .. } => return Err(format!("a record was refused: {reason}")),
            _ => {}
        }
        while let Some(b) = waiting.first().copied() {
            match e.plane.offer(consumer, 0, b) {
                Ok(()) => {
                    waiting.remove(0);
                    e.plane.drop_batch(b).map_err(plane_error)?;
                }
                Err(PlaneError::Full) => break,
                Err(other) => return Err(plane_error(other)),
            }
        }
        Ok(released == MANY)
    })?;
    e.plane.finish(consumer, 0).map_err(plane_error)?;
    let made = e.report(producer, 120)?;
    let took = e.report(consumer, 120)?;
    e.ended(producer, 120)?;
    e.ended(consumer, 120)?;
    if took[1] != 0 {
        return Err(format!("{} batches arrived out of order", took[1]));
    }
    let grew = e.plane.residency().made - made_before;
    let elapsed = (last - first).max(1);
    let per_second = MANY * SECONDS / elapsed;
    let mib_per_second = MANY * SECONDS / elapsed;
    Ok(format!(
        "{MANY} batches of 1 MiB from producer to consumer in {} ms: {per_second} batches/s ({mib_per_second} MiB/s), over {IN_FLIGHT} windows ({} MiB made); the producer's window least {} us median {} us, batch_put least {} us median {} us; the consumer's batch_get least {} us median {} us",
        elapsed / 1_000_000,
        grew / MIB,
        made[1] / 1000,
        made[2] / 1000,
        made[3] / 1000,
        made[4] / 1000,
        took[2] / 1000,
        took[3] / 1000
    ))
}

fn b(s: &str) -> Bytes {
    Bytes(s.as_bytes().to_vec())
}

/// The log ring `run_plane` writes a task's text into: a memory object the engine
/// formats, and a duplicate for the server.
struct LogRing {
    memory: Memory,
    bytes: u64,
}

impl LogRing {
    fn new(bytes: u64) -> Result<LogRing, String> {
        let memory = Memory::create(bytes).map_err(kernel_error("memory_create"))?;
        let mut mapping = memory
            .map(0, bytes, map::READ | map::WRITE)
            .map_err(kernel_error("mapping the ring"))?;
        let region = mapping
            .shared_region()
            .map_err(kernel_error("the ring's region"))?;
        Ring::init(region).map_err(|e| format!("formatting the ring: {e:?}"))?;
        drop(mapping);
        Ok(LogRing { memory, bytes })
    }

    fn give(&self) -> Result<Handle, String> {
        self.memory
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .map_err(kernel_error("duplicate"))
    }

    /// Every record in the ring: its message id, its stream header and its body.
    fn drain(&self) -> Result<Vec<(String, String, String)>, String> {
        let mut mapping = self
            .memory
            .map(0, self.bytes, map::READ | map::WRITE)
            .map_err(kernel_error("mapping the ring"))?;
        let region = mapping
            .shared_region()
            .map_err(kernel_error("the ring's region"))?;
        let ring = Ring::attach(region).map_err(|e| format!("attaching the ring: {e:?}"))?;
        let mut consumer = ring
            .consumer()
            .map_err(|e| format!("the ring's consumer: {e:?}"))?;
        let mut out = Vec::new();
        while let Some(bytes) = consumer.pop() {
            match moruna_record::record::decode(bytes) {
                Decoded::Record(r, _) => {
                    let stream = r
                        .headers
                        .iter()
                        .find(|(k, _)| *k == log::STREAM)
                        .map(|(_, v)| String::from(*v))
                        .unwrap_or_default();
                    out.push((
                        String::from(r.msg_id),
                        stream,
                        String::from_utf8_lossy(r.body).into_owned(),
                    ));
                }
                other => return Err(format!("a log record that does not decode: {other:?}")),
            }
        }
        Ok(out)
    }
}

fn int64s(bytes: &[u8]) -> Vec<i64> {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| i64::from_le_bytes(*c))
        .collect()
}

/// The framing of `batch`, read through the engine's mapping, and its check.
fn checked(e: &mut Engine, batch: BatchId) -> Result<record::Record, String> {
    let (_, _, len) = e.plane.batch(batch).map_err(plane_error)?;
    let head = e.read(batch, 0, PAGE.min(len) as usize)?;
    let end = match record::framing_need(&head)? {
        Need::Framing(n) => n,
        Need::More(n) => return Err(format!("a framing of more than a page ({n})")),
    };
    record::check_record(&head[..end], len)
}

/// The values of the `Int64` column `name` of `batch`.
fn column(
    e: &mut Engine,
    batch: BatchId,
    r: &record::Record,
    name: &str,
) -> Result<Vec<i64>, String> {
    let c = r
        .columns
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| format!("no column {name}"))?;
    let (at, len) = *c
        .buffers
        .get(1)
        .ok_or_else(|| format!("column {name} has no values"))?;
    Ok(int64s(&e.read(batch, at, len as usize)?))
}

/// DP-T16 and DP-T17.
fn shimless(e: &mut Engine, client: &mut linux::Client, input: &[u8]) -> Result<String, String> {
    let (batch, rows) = e.plane.load(input).map_err(plane_error)?;
    let r = checked(e, batch)?;
    let x_in = column(e, batch, &r, "x")?;
    let (task, _) = e.plane.open(shape(1, 1, false)).map_err(plane_error)?;
    e.plane.offer(task, 0, batch).map_err(plane_error)?;
    e.plane.finish(task, 0).map_err(plane_error)?;
    e.plane.grant(task, 0, MIB).map_err(plane_error)?;
    let ring = LogRing::new(64 * 1024)?;
    let slots = e.plane.slots(task).map_err(plane_error)?;
    // The personality waits on its handles to the slots for their SIGNALED (SDD 30 f.10).
    let give = |h: &Handle| {
        h.duplicate(Rights::WRITE | Rights::TRANSFER | Rights::WAIT)
            .map_err(kernel_error("duplicate"))
    };
    // The task's account, carved out of the engine's: the kernel holds arrow_echo to it.
    let account = Account::carve(SHIMLESS_LIMIT, "plane-arrow-echo")
        .map_err(kernel_error("account_create"))?;
    let plane = LinuxPlane {
        assets: None,
        input: Some(give(&slots[0].0)?),
        output: Some(give(&slots[1].0)?),
        port: None,
        log: ring.give()?,
        log_bytes: ring.bytes,
        account: account.0,
    };
    let ended = client
        .run_plane(
            b("/kc1/bin/arrow_echo"),
            vec![b("/kc1/bin/arrow_echo")],
            vec![b("HOME=/tmp")],
            plane,
        )
        .map_err(|err| format!("run_plane arrow_echo: {err:?}"))?;
    if ended.how != how::EXITED || ended.code != 0 {
        let lines = ring.drain().unwrap_or_default();
        return Err(format!(
            "arrow_echo ended {} with code {}; it wrote {lines:?}",
            ended.how, ended.code
        ));
    }
    let out = e.put_from(task, 120)?;
    e.until(120, |_, ev| {
        Ok(matches!(ev, Event::Ended { task: t } if *t == task))
    })?;
    e.plane.drop_batch(batch).map_err(plane_error)?;
    let r = checked(e, out)?;
    let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
    let x_out = column(e, out, &r, "x")?;
    let n_out = column(e, out, &r, "n")?;
    e.plane.drop_batch(out).map_err(plane_error)?;
    let n_want: Vec<i64> = (0..rows as i64).collect();
    if names != ["x", "s", "n"] || r.rows != rows || x_out != x_in || n_out != n_want {
        return Err(format!(
            "arrow_echo gave columns {names:?} of {} rows; x {}, n {}",
            r.rows,
            if x_out == x_in { "as read" } else { "changed" },
            if n_out == n_want {
                "the row numbers"
            } else {
                "wrong"
            }
        ));
    }
    let refused = ended.refused.len();
    // DP-T17: a program's text, with no output slot, reaches the ring as records.
    let (task, _) = e.plane.open(shape(0, 0, false)).map_err(plane_error)?;
    let logs = LogRing::new(64 * 1024)?;
    let plane = LinuxPlane {
        assets: None,
        input: None,
        output: None,
        port: None,
        log: logs.give()?,
        log_bytes: logs.bytes,
        account: Account::carve(SHIMLESS_LIMIT, "plane-hello")
            .map_err(kernel_error("account_create"))?
            .0,
    };
    let ended = client
        .run_plane(
            b("/kc1/bin/hello"),
            vec![b("/kc1/bin/hello"), b("one"), b("two")],
            vec![b("HOME=/tmp")],
            plane,
        )
        .map_err(|err| format!("run_plane hello: {err:?}"))?;
    let _ = task;
    let lines = logs.drain()?;
    let hello = lines
        .iter()
        .find(|(_, stream, body)| stream == log::STDOUT && body.starts_with("hello from musl"));
    if ended.how != how::EXITED || ended.code != 0 || hello.is_none() || !ended.output.0.is_empty()
    {
        return Err(format!(
            "hello ended {} with code {}; the ring held {lines:?}; the answer carried {} bytes of output",
            ended.how,
            ended.code,
            ended.output.0.len()
        ));
    }
    let id = hello.map(|(id, _, _)| id.clone()).unwrap_or_default();
    Ok(format!(
        "DP-T16: an unmodified static musl arrow_echo read {rows} rows on its descriptor 0 from an input slot and wrote them back with its column n on descriptor 1, put as a record: columns {names:?}, x as read, n the row numbers ({refused} calls refused); DP-T17: hello's {} lines reached the log ring as records ({id}, stdout)",
        lines.len()
    ))
}

fn check(out: &Console, name: &str, r: Result<String, String>) -> bool {
    match r {
        Ok(line) => {
            println!(out, "KD1 {}: ok: {}", name, line);
            true
        }
        Err(why) => {
            println!(out, "KD1 {}: FAIL: {}", name, why);
            false
        }
    }
}

fn take_resource(out: &Console) -> Option<Handle> {
    let supervisor = Channel(start::take("supervisor")?);
    let mut buf = [0u8; 16];
    let mut handles = [None];
    match moruna_svc::wait_one(
        &supervisor.0,
        moruna_svc::Signals::READABLE | moruna_svc::Signals::PEER_CLOSED,
        deadline_in(60 * SECONDS),
    ) {
        Ok(_) => {}
        Err(e) => {
            println!(out, "KD1 plane: FAIL: no process resource: {}", e.name());
            return None;
        }
    }
    supervisor.read(&mut buf, &mut handles).ok()?;
    handles[0].take()
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(resource) = take_resource(&out) else {
        return 1;
    };
    let (Some(task_image), Some(ro_image), Some(linux)) = (
        start::take("task-image"),
        start::take("ro-image"),
        start::take("linux"),
    ) else {
        println!(out, "KD1 plane: FAIL: a start slot is missing");
        return 90;
    };
    let fixtures = (
        fixture("good"),
        fixture("big"),
        fixture("mib"),
        fixture("refusals"),
        fixture("echo"),
    );
    let (Ok(good), Ok(big), Ok(mib), Ok(bad), Ok(echo_in)) = fixtures else {
        println!(out, "KD1 plane: FAIL: a fixture could not be read");
        return 1;
    };
    let cases = match refusals(&bad) {
        Ok(c) => c,
        Err(why) => {
            println!(out, "KD1 plane: FAIL: {}", why);
            return 1;
        }
    };
    let mut engine = Engine {
        out,
        resource,
        task_image,
        ro_image,
        plane: KernelPlane::new(Kernel, BUDGET),
        roles: BTreeMap::new(),
        reports: BTreeMap::new(),
        waits: Vec::new(),
        processes: BTreeMap::new(),
        accounts: BTreeMap::new(),
    };
    let mut ok = true;
    let r = zero_copy(&mut engine, &big);
    ok &= check(&engine.out, "zero copy", r);
    let r = fan_out(&mut engine, &mib);
    ok &= check(&engine.out, "fan-out", r);
    let r = refuse(&mut engine, &cases);
    ok &= check(&engine.out, "check", r);
    let r = isolation(&mut engine, &good);
    ok &= check(&engine.out, "isolation", r);
    let r = echo(&mut engine);
    ok &= check(&engine.out, "port", r);
    let r = throughput(&mut engine, &mib);
    ok &= check(&engine.out, "throughput", r);
    let mut client = match moruna_svc::connect(&Channel(linux))
        .map_err(|err| format!("{err:?}"))
        .and_then(|c| {
            linux::Client::connect(c, deadline_in(120 * SECONDS)).map_err(|err| format!("{err:?}"))
        }) {
        Ok(c) => c,
        Err(why) => {
            println!(
                engine.out,
                "KD1 shimless: FAIL: connecting to moruna.linux: {}", why
            );
            return 1;
        }
    };
    client.core().set_timeout(600 * SECONDS);
    let r = shimless(&mut engine, &mut client, &echo_in);
    ok &= check(&engine.out, "shimless", r);
    let residency = engine.plane.residency();
    println!(
        engine.out,
        "KD1 plane: arena made {} MiB, {} batches resident, {} MiB pooled",
        residency.made / MIB,
        residency.batches,
        residency.pooled / MIB
    );
    if ok { 0 } else { 1 }
}
