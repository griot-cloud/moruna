//! The data-plane port's conformance suite (SDD 29 d.4, KA2): one set of cases every adapter of
//! [`DataPlane`] passes, so the engine above (KD2) and the scheduler that drives it (KD7) see the
//! same events, the same refusals and the same budget from the kernel's slots and from Linux.
//!
//! A case opens tasks on a fresh plane and starts each with a [`Step`] script, which the
//! adapter's [`Harness`] runs as a real task: a model of the kernel's calls on a host, or a
//! process on Linux making the three calls through the task library. The engine's side of every
//! case is this module's own code, written against the trait alone; what each task saw comes
//! back as [`Seen`] entries and how it ended ([`Ending`]), and both are judged here.

use alloc::{collections::BTreeMap, format, string::String, vec, vec::Vec};

use moruna_abi::{Error, limits, plane::request};

use crate::arena::{PAGE, pages};
use crate::port::{BatchId, CallId, DataPlane, Event, PlaneError, Residency, Shape, TaskId};

/// How long one wait for an event may take before a case fails, in milliseconds.
pub const WAIT_MS: u64 = 10_000;

/// One thing a task does, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// `batch_get` on slot `n`: an input's next batch ([`Seen::Batch`]) or its end
    /// ([`Seen::End`]); an output's window ([`Seen::Window`]).
    Get(u32),
    /// Writes `bytes` at `at` into what slot `n` has mapped now: a window takes them
    /// ([`Seen::Wrote`]); a batch, which is read only, faults the task.
    Write { slot: u32, at: u64, bytes: Vec<u8> },
    /// `batch_put` of `len` bytes at `offset` of slot `n`'s window: [`Seen::Put`] or the error.
    Put { slot: u32, offset: u64, len: u64 },
    /// A write into the window last put on slot `n`, which has left the task: it faults, or
    /// fails without changing a byte.
    WriteAfterPut(u32),
    /// `port_call` of `request` to `port`: [`Seen::Reply`] or the error.
    Call { port: u64, request: Vec<u8> },
    /// The task dies at once, holding whatever it holds.
    Crash,
}

/// What a step gave the task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    /// A batch: its length and the [`sum`] of its bytes.
    Batch { len: u64, sum: u64 },
    /// The input's end.
    End,
    /// A window of `len` bytes, mapped writable.
    Window { len: u64 },
    /// The bytes were written.
    Wrote,
    /// The record was put.
    Put,
    /// A port's reply.
    Reply(Vec<u8>),
    /// The call's error.
    Error(Error),
}

/// How a task ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// It ran its script to the end and exited.
    Exited,
    /// It wrote memory it may not write.
    Faulted,
    /// It was ended from outside: killed, crashed, or refused by the engine's check.
    Killed,
}

/// A task's outcome: what each step saw, and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub seen: Vec<Seen>,
    pub ending: Ending,
}

/// FNV-1a over `bytes`: how a task tells the suite which bytes it read without sending them.
pub fn sum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A record the check must refuse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub label: String,
    /// The bytes written into the window.
    pub bytes: Vec<u8>,
    /// How many of them are put.
    pub put: u64,
    /// Words the refusal's reason holds.
    pub says: String,
}

/// The records the suite moves: one the check passes, and the ones it must refuse.
#[derive(Clone, Debug)]
pub struct Fixtures {
    /// A data-plane record (contracts e.7) and its rows.
    pub good: Vec<u8>,
    pub rows: u64,
    pub refusals: Vec<Refusal>,
}

/// What an adapter gives the suite: planes, and tasks that run scripts.
pub trait Harness {
    type Plane: DataPlane;

    /// A fresh plane whose arena may make `budget` bytes.
    fn plane(&mut self, budget: u64) -> Self::Plane;

    /// Starts `task`, which `plane` opened and gave `end` for, running `script`.
    fn start(
        &mut self,
        plane: &mut Self::Plane,
        task: TaskId,
        end: <Self::Plane as DataPlane>::Task,
        script: Vec<Step>,
    ) -> Result<(), String>;

    /// Lets the started tasks make progress: a model runs their steps until each waits or
    /// ends; a process runs on its own.
    fn run(&mut self, plane: &mut Self::Plane);

    /// The deadline `ms` milliseconds from now, on the clock [`DataPlane::next`] reads.
    fn deadline(&self, ms: u64) -> u64;

    /// Ends `task` from outside, as a kill does.
    fn kill(&mut self, plane: &mut Self::Plane, task: TaskId) -> Result<(), String>;

    /// The outcome of `task`, which has ended.
    fn outcome(&mut self, task: TaskId) -> Result<Outcome, String>;
}

/// Each case's name and its result.
pub type Results = Vec<(&'static str, Result<(), String>)>;

/// Every case, each on a fresh plane.
pub fn run_all<H: Harness>(h: &mut H, f: &Fixtures) -> Results {
    vec![
        ("put_get", put_get(h, f)),
        ("fan_out_to_three", fan_out(h, f)),
        ("residency", residency(h, f)),
        ("refused", refused(h, f)),
        ("put_errors", put_errors(h)),
        ("write_after_put", write_after_put(h, f)),
        ("consumer_write", consumer_write(h, f)),
        ("crash_returns_everything", crash(h, f)),
        ("kill_returns_everything", kill(h, f)),
        ("port_call", port_call(h)),
        ("budget", budget(h, f)),
        ("backpressure", backpressure(h, f)),
        ("names_and_bounds", names_and_bounds(h, f)),
    ]
}

/// Every case, and an error naming each that failed.
pub fn check<H: Harness>(h: &mut H, f: &Fixtures) -> Result<usize, String> {
    let results = run_all(h, f);
    let failed: Vec<String> = results
        .iter()
        .filter_map(|(name, r)| r.as_ref().err().map(|e| format!("{name}: {e}")))
        .collect();
    if failed.is_empty() {
        Ok(results.len())
    } else {
        Err(failed.join("\n"))
    }
}

fn shape(inputs: u32, outputs: u32, port: bool) -> Shape {
    Shape {
        inputs,
        outputs,
        port,
    }
}

fn same<T: PartialEq + core::fmt::Debug>(got: T, want: T, what: &str) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

/// The events of one case, in the order the plane gave them.
struct Log {
    events: Vec<Event>,
}

impl Log {
    fn new() -> Log {
        Log { events: Vec::new() }
    }

    /// Waits until `done` holds of the events so far, or fails naming `what`.
    fn until<H: Harness>(
        &mut self,
        h: &mut H,
        p: &mut H::Plane,
        what: &str,
        done: impl Fn(&[Event]) -> bool,
    ) -> Result<(), String> {
        let mut idle = 0;
        while !done(&self.events) {
            h.run(p);
            match p.next(h.deadline(WAIT_MS)) {
                Ok(Some(e)) => {
                    self.events.push(e);
                    idle = 0;
                }
                Ok(None) => {
                    idle += 1;
                    if idle > 2 {
                        return Err(format!(
                            "waited for {what} and it did not come; the events: {:?}",
                            self.events
                        ));
                    }
                }
                Err(e) => return Err(format!("next, waiting for {what}: {e:?}")),
            }
        }
        Ok(())
    }

    /// The batch `t` put on `output`, once the event is here.
    fn put_by(&self, t: TaskId) -> Option<(BatchId, u64, u64)> {
        self.events.iter().find_map(|e| match e {
            Event::Put {
                task,
                batch,
                rows,
                bytes,
                ..
            } if *task == t => Some((*batch, *rows, *bytes)),
            _ => None,
        })
    }

    fn count(&self, f: impl Fn(&Event) -> bool) -> usize {
        self.events.iter().filter(|e| f(e)).count()
    }

    /// No event of `t` follows its `Ended`, which comes once.
    fn ended_last(&self, t: TaskId) -> Result<(), String> {
        let ends: Vec<usize> = self
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| **e == Event::Ended { task: t })
            .map(|(i, _)| i)
            .collect();
        let [at] = ends.as_slice() else {
            return Err(format!(
                "task {t:?} ended {} times: {:?}",
                ends.len(),
                self.events
            ));
        };
        let after = self.events[at + 1..].iter().any(|e| task_of(e) == Some(t));
        if after {
            return Err(format!(
                "an event of task {t:?} came after its end: {:?}",
                self.events
            ));
        }
        Ok(())
    }
}

fn task_of(e: &Event) -> Option<TaskId> {
    match e {
        Event::Put { task, .. }
        | Event::Released { task, .. }
        | Event::Refused { task, .. }
        | Event::Ended { task } => Some(*task),
        Event::Call { call, .. } => Some(call.task),
        Event::Freed { .. } => None,
    }
}

/// The steps that write `record` into output `slot`'s window and put `put` bytes of it.
fn produce(slot: u32, record: &[u8], put: u64) -> Vec<Step> {
    vec![
        Step::Get(slot),
        Step::Write {
            slot,
            at: 0,
            bytes: record.to_vec(),
        },
        Step::Put {
            slot,
            offset: 0,
            len: put,
        },
    ]
}

/// What a producer of [`produce`] sees with a window of `window` bytes.
fn produced(window: u64) -> Vec<Seen> {
    vec![Seen::Window { len: window }, Seen::Wrote, Seen::Put]
}

fn window_of(record: &[u8]) -> u64 {
    pages(record.len() as u64).unwrap_or(PAGE)
}

/// A producer of shape (1 input, 1 output) that puts `record` on output 1 and then waits on
/// its input, which the engine finishes once the record is a batch: a task that is still
/// there when the engine judges its record.
fn start_producer<H: Harness>(
    h: &mut H,
    p: &mut H::Plane,
    record: &[u8],
    put: u64,
) -> Result<TaskId, String> {
    let (task, end) = p
        .open(shape(1, 1, false))
        .map_err(|e| format!("open: {e:?}"))?;
    p.grant(task, 0, window_of(record))
        .map_err(|e| format!("grant: {e:?}"))?;
    let mut script = produce(1, record, put);
    script.push(Step::Get(0));
    h.start(p, task, end, script)?;
    Ok(task)
}

/// A good record put and made a batch: the producer, the batch, and the log so far. The
/// producer has been told its input ended and has exited.
fn batch_of<H: Harness>(
    h: &mut H,
    p: &mut H::Plane,
    f: &Fixtures,
    log: &mut Log,
) -> Result<(TaskId, BatchId), String> {
    let producer = start_producer(h, p, &f.good, f.good.len() as u64)?;
    log.until(h, p, "the producer's put", |e| {
        e.iter()
            .any(|e| matches!(e, Event::Put { task, .. } if *task == producer))
    })?;
    let (batch, rows, bytes) = log.put_by(producer).ok_or("the put event has no batch")?;
    same(rows, f.rows, "the put's rows")?;
    same(bytes, f.good.len() as u64, "the put's bytes")?;
    p.finish(producer, 0)
        .map_err(|e| format!("finish the producer's input: {e:?}"))?;
    log.until(h, p, "the producer's end", |e| {
        e.contains(&Event::Ended { task: producer })
    })?;
    let mut want = produced(window_of(&f.good));
    want.push(Seen::End);
    same(
        h.outcome(producer)?,
        Outcome {
            seen: want,
            ending: Ending::Exited,
        },
        "the producer",
    )?;
    Ok((producer, batch))
}

/// A consumer of one input that is offered `batch`, then finished, and reads to the end.
fn consume<H: Harness>(
    h: &mut H,
    p: &mut H::Plane,
    batch: BatchId,
    script: Vec<Step>,
) -> Result<TaskId, String> {
    let (task, end) = p
        .open(shape(1, 0, false))
        .map_err(|e| format!("open: {e:?}"))?;
    p.offer(task, 0, batch)
        .map_err(|e| format!("offer: {e:?}"))?;
    p.finish(task, 0).map_err(|e| format!("finish: {e:?}"))?;
    h.start(p, task, end, script)?;
    Ok(task)
}

fn read_all() -> Vec<Step> {
    vec![Step::Get(0), Step::Get(0)]
}

fn good_seen(f: &Fixtures) -> Seen {
    Seen::Batch {
        len: f.good.len() as u64,
        sum: sum(&f.good),
    }
}

fn in_use(r: &Residency) -> u64 {
    r.made - r.pooled
}

fn empty(r: Residency, what: &str) -> Result<(), String> {
    if (r.batches, r.resident, r.shares, r.windows, in_use(&r)) == (0, 0, 0, 0, 0) {
        Ok(())
    } else {
        Err(format!("{what}: the arena still holds something: {r:?}"))
    }
}

/// A record put is checked and becomes a batch; a consumer reads the same bytes; the task's
/// release and the engine's drop free it, once.
fn put_get<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    let mut p = h.plane(1 << 24);
    let mut log = Log::new();
    let (producer, batch) = batch_of(h, &mut p, f, &mut log)?;
    log.ended_last(producer)?;
    let consumer = consume(h, &mut p, batch, read_all())?;
    log.until(h, &mut p, "the consumer's end", |e| {
        e.contains(&Event::Ended { task: consumer })
    })?;
    same(
        h.outcome(consumer)?,
        Outcome {
            seen: vec![good_seen(f), Seen::End],
            ending: Ending::Exited,
        },
        "the consumer",
    )?;
    if !log.events.contains(&Event::Released {
        task: consumer,
        input: 0,
        batch,
    }) {
        return Err(format!("no release of the batch: {:?}", log.events));
    }
    log.ended_last(consumer)?;
    same(
        log.count(|e| matches!(e, Event::Freed { .. })),
        0,
        "frees while the engine holds it",
    )?;
    p.drop_batch(batch)
        .map_err(|e| format!("drop_batch: {e:?}"))?;
    log.until(h, &mut p, "the batch freed", |e| {
        e.contains(&Event::Freed { batch })
    })?;
    same(
        p.drop_batch(batch),
        Err(PlaneError::Unknown("batch")),
        "a freed batch dropped again",
    )?;
    empty(p.residency(), "after the batch is freed")
}

/// One batch offered to three consumers: three shares of the same bytes, freed once, after the
/// last release, the engine's hold dropped first.
fn fan_out<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    let mut p = h.plane(1 << 24);
    let mut log = Log::new();
    let (_, batch) = batch_of(h, &mut p, f, &mut log)?;
    let mut consumers = Vec::new();
    for _ in 0..3 {
        let (task, end) = p
            .open(shape(1, 0, false))
            .map_err(|e| format!("open: {e:?}"))?;
        p.offer(task, 0, batch)
            .map_err(|e| format!("offer: {e:?}"))?;
        p.finish(task, 0).map_err(|e| format!("finish: {e:?}"))?;
        consumers.push((task, end));
    }
    same(p.residency().shares, 3, "shares of three consumers")?;
    same(p.residency().batches, 1, "batches")?;
    p.drop_batch(batch)
        .map_err(|e| format!("drop_batch: {e:?}"))?;
    let mut done = Vec::new();
    for (i, (task, end)) in consumers.into_iter().enumerate() {
        h.start(&mut p, task, end, read_all())?;
        log.until(h, &mut p, "a consumer's end", |e| {
            e.contains(&Event::Ended { task })
        })?;
        done.push(task);
        let freed = log.count(|e| *e == Event::Freed { batch });
        same(
            freed,
            usize::from(i == 2),
            "frees after this consumer's end",
        )?;
        same(
            p.residency().shares,
            2 - i as u64,
            "shares after this consumer's end",
        )?;
    }
    for task in done {
        same(
            h.outcome(task)?,
            Outcome {
                seen: vec![good_seen(f), Seen::End],
                ending: Ending::Exited,
            },
            "a consumer",
        )?;
        log.ended_last(task)?;
        if !log.events.contains(&Event::Released {
            task,
            input: 0,
            batch,
        }) {
            return Err(format!("consumer {task:?} released nothing"));
        }
    }
    let freed_at = log
        .events
        .iter()
        .position(|e| *e == Event::Freed { batch })
        .ok_or("never freed")?;
    let last_release = log
        .events
        .iter()
        .rposition(|e| matches!(e, Event::Released { .. }))
        .ok_or("never released")?;
    if freed_at < last_release {
        return Err(format!("freed before the last release: {:?}", log.events));
    }
    empty(p.residency(), "after the fan-out")
}

/// What the arena reports at each step of a batch's life.
fn residency<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    let budget = 1 << 24;
    let mut p = h.plane(budget);
    same(p.residency().budget, budget, "the budget")?;
    empty(p.residency(), "a fresh plane")?;
    let (task, end) = p
        .open(shape(1, 1, false))
        .map_err(|e| format!("open: {e:?}"))?;
    let window = window_of(&f.good);
    p.grant(task, 0, f.good.len() as u64)
        .map_err(|e| format!("grant: {e:?}"))?;
    let r = p.residency();
    same(
        (r.windows, in_use(&r), r.batches),
        (window, window, 0),
        "a window granted (windows, in use, batches)",
    )?;
    let mut script = produce(1, &f.good, f.good.len() as u64);
    script.push(Step::Get(0));
    h.start(&mut p, task, end, script)?;
    let mut log = Log::new();
    log.until(h, &mut p, "the put", |e| {
        e.iter().any(|e| matches!(e, Event::Put { .. }))
    })?;
    let (batch, _, _) = log.put_by(task).ok_or("no put")?;
    let r = p.residency();
    same(
        (r.windows, r.batches, r.resident, r.shares, in_use(&r)),
        (0, 1, f.good.len() as u64, 0, window),
        "a batch (windows, batches, resident, shares, in use)",
    )?;
    p.finish(task, 0).map_err(|e| format!("finish: {e:?}"))?;
    let a = consume(h, &mut p, batch, read_all())?;
    let b = consume(h, &mut p, batch, read_all())?;
    same(p.residency().shares, 2, "two shares")?;
    log.until(h, &mut p, "both consumers' ends", |e| {
        e.contains(&Event::Ended { task: a }) && e.contains(&Event::Ended { task: b })
    })?;
    let r = p.residency();
    same(
        (r.shares, r.batches),
        (0, 1),
        "released and still held (shares, batches)",
    )?;
    p.drop_batch(batch)
        .map_err(|e| format!("drop_batch: {e:?}"))?;
    log.until(h, &mut p, "the free", |e| {
        e.contains(&Event::Freed { batch })
    })?;
    log.until(h, &mut p, "the producer's end", |e| {
        e.contains(&Event::Ended { task })
    })?;
    empty(p.residency(), "everything freed")
}

/// Each record that is not a data-plane record ends its task with the check's reason, is never
/// a batch, and goes back to the arena.
fn refused<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    if f.refusals.is_empty() {
        return Err("the fixtures hold no refusal".into());
    }
    let mut p = h.plane(1 << 26);
    let mut log = Log::new();
    for case in &f.refusals {
        let task = start_producer(h, &mut p, &case.bytes, case.put)?;
        log.until(h, &mut p, &format!("{}: the refusal", case.label), |e| {
            e.contains(&Event::Ended { task })
        })?;
        let reason = log
            .events
            .iter()
            .find_map(|e| match e {
                // Output 0, the task's slot 1: events name outputs, scripts name slots.
                Event::Refused {
                    task: t,
                    output: 0,
                    reason,
                } if *t == task => Some(reason.clone()),
                _ => None,
            })
            .ok_or_else(|| format!("{}: no refusal: {:?}", case.label, log.events))?;
        if !reason.contains(case.says.as_str()) {
            return Err(format!(
                "{}: the reason `{reason}` does not say `{}`",
                case.label, case.says
            ));
        }
        if log.put_by(task).is_some() {
            return Err(format!("{}: the refused record is a batch", case.label));
        }
        // The task is ended once its record is judged, which may be before or after it
        // has noted that its put returned.
        let outcome = h.outcome(task)?;
        let full = produced(window_of(&case.bytes));
        if outcome.seen != full && outcome.seen != full[..2] {
            return Err(format!(
                "{}: the producer saw {:?}, want {full:?}",
                case.label, outcome.seen
            ));
        }
        same(
            outcome.ending,
            Ending::Killed,
            &format!("{}: the producer's end", case.label),
        )?;
        log.ended_last(task)?;
        empty(p.residency(), &format!("{}: after the refusal", case.label))?;
    }
    Ok(())
}

/// `batch_put` refuses what is no record of a window, as the kernel's call does, and the task
/// goes on; the window comes back at its end.
fn put_errors<H: Harness>(h: &mut H) -> Result<(), String> {
    let mut p = h.plane(1 << 24);
    let (task, end) = p
        .open(shape(1, 2, false))
        .map_err(|e| format!("open: {e:?}"))?;
    p.grant(task, 0, 8192)
        .map_err(|e| format!("grant: {e:?}"))?;
    let script = vec![
        // Output 2 has no window yet: nothing to put.
        Step::Put {
            slot: 2,
            offset: 0,
            len: 64,
        },
        Step::Get(1),
        Step::Put {
            slot: 0,
            offset: 0,
            len: 64,
        },
        Step::Put {
            slot: 1,
            offset: 0,
            len: 0,
        },
        Step::Put {
            slot: 1,
            offset: 100,
            len: 64,
        },
        Step::Put {
            slot: 1,
            offset: 0,
            len: 8192 + 1,
        },
        Step::Put {
            slot: 1,
            offset: 8192,
            len: 1,
        },
        Step::Put {
            slot: 9,
            offset: 0,
            len: 64,
        },
        Step::Get(9),
        // The same window is still the slot's.
        Step::Get(1),
        Step::Write {
            slot: 1,
            at: 4096,
            bytes: vec![0x3c; 64],
        },
    ];
    h.start(&mut p, task, end, script)?;
    let mut log = Log::new();
    log.until(h, &mut p, "the task's end", |e| {
        e.contains(&Event::Ended { task })
    })?;
    same(
        h.outcome(task)?,
        Outcome {
            seen: vec![
                Seen::Error(Error::BadState),
                Seen::Window { len: 8192 },
                Seen::Error(Error::InvalidArgs),
                Seen::Error(Error::InvalidArgs),
                Seen::Error(Error::InvalidArgs),
                Seen::Error(Error::OutOfRange),
                Seen::Error(Error::OutOfRange),
                Seen::Error(Error::BadHandle),
                Seen::Error(Error::BadHandle),
                Seen::Window { len: 8192 },
                Seen::Wrote,
            ],
            ending: Ending::Exited,
        },
        "the task",
    )?;
    same(
        log.events.clone(),
        vec![Event::Ended { task }],
        "the events",
    )?;
    empty(p.residency(), "the unput window came back")
}

/// A producer that writes its window after putting it faults, and the batch is the bytes it
/// put: a consumer reads them unchanged.
fn write_after_put<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    let mut p = h.plane(1 << 24);
    let (task, end) = p
        .open(shape(0, 1, false))
        .map_err(|e| format!("open: {e:?}"))?;
    p.grant(task, 0, f.good.len() as u64)
        .map_err(|e| format!("grant: {e:?}"))?;
    let mut script = produce(0, &f.good, f.good.len() as u64);
    script.push(Step::WriteAfterPut(0));
    h.start(&mut p, task, end, script)?;
    let mut log = Log::new();
    log.until(h, &mut p, "the producer's end", |e| {
        e.contains(&Event::Ended { task })
    })?;
    let (batch, _, _) = log
        .put_by(task)
        .ok_or_else(|| format!("no put: {:?}", log.events))?;
    let outcome = h.outcome(task)?;
    same(
        outcome.seen,
        produced(window_of(&f.good)),
        "the producer's steps",
    )?;
    same(outcome.ending, Ending::Faulted, "the producer's end")?;
    let reader = consume(h, &mut p, batch, read_all())?;
    log.until(h, &mut p, "the reader's end", |e| {
        e.contains(&Event::Ended { task: reader })
    })?;
    same(
        h.outcome(reader)?.seen,
        vec![good_seen(f), Seen::End],
        "the batch after the write",
    )?;
    Ok(())
}

/// A consumer that writes its batch faults; another consumer of the same batch reads it
/// unchanged.
fn consumer_write<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    let mut p = h.plane(1 << 24);
    let mut log = Log::new();
    let (_, batch) = batch_of(h, &mut p, f, &mut log)?;
    let writer = consume(
        h,
        &mut p,
        batch,
        vec![
            Step::Get(0),
            Step::Write {
                slot: 0,
                at: 0,
                bytes: vec![0xa5; 64],
            },
        ],
    )?;
    log.until(h, &mut p, "the writer's end", |e| {
        e.contains(&Event::Ended { task: writer })
    })?;
    same(
        h.outcome(writer)?,
        Outcome {
            seen: vec![good_seen(f)],
            ending: Ending::Faulted,
        },
        "the writer",
    )?;
    if !log.events.contains(&Event::Released {
        task: writer,
        input: 0,
        batch,
    }) {
        return Err(format!(
            "the faulted consumer's share was not released: {:?}",
            log.events
        ));
    }
    let reader = consume(h, &mut p, batch, read_all())?;
    log.until(h, &mut p, "the reader's end", |e| {
        e.contains(&Event::Ended { task: reader })
    })?;
    same(
        h.outcome(reader)?.seen,
        vec![good_seen(f), Seen::End],
        "the batch after the write",
    )
}

/// A task that dies holding a batch, two queued, and a window gives every one back, then ends.
fn crash<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    let mut p = h.plane(1 << 24);
    let mut log = Log::new();
    let (_, batch) = batch_of(h, &mut p, f, &mut log)?;
    let (task, end) = p
        .open(shape(1, 1, false))
        .map_err(|e| format!("open: {e:?}"))?;
    for _ in 0..3 {
        p.offer(task, 0, batch)
            .map_err(|e| format!("offer: {e:?}"))?;
    }
    p.grant(task, 0, 8192)
        .map_err(|e| format!("grant: {e:?}"))?;
    p.grant(task, 0, 4096)
        .map_err(|e| format!("grant: {e:?}"))?;
    h.start(
        &mut p,
        task,
        end,
        vec![Step::Get(0), Step::Get(1), Step::Crash],
    )?;
    log.until(h, &mut p, "the crashed task's end", |e| {
        e.contains(&Event::Ended { task })
    })?;
    same(
        h.outcome(task)?,
        Outcome {
            seen: vec![good_seen(f), Seen::Window { len: 8192 }],
            ending: Ending::Killed,
        },
        "the crashed task",
    )?;
    same(
        log.count(|e| {
            *e == Event::Released {
                task,
                input: 0,
                batch,
            }
        }),
        3,
        "releases of the held and the queued batches",
    )?;
    log.ended_last(task)?;
    let r = p.residency();
    same((r.shares, r.windows), (0, 0), "shares and windows after it")?;
    p.drop_batch(batch)
        .map_err(|e| format!("drop_batch: {e:?}"))?;
    log.until(h, &mut p, "the free", |e| {
        e.contains(&Event::Freed { batch })
    })?;
    empty(p.residency(), "after the crash")
}

/// A task killed while it waits gives back what it holds and what was queued for it.
fn kill<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    let mut p = h.plane(1 << 24);
    let mut log = Log::new();
    let (_, batch) = batch_of(h, &mut p, f, &mut log)?;
    let (task, end) = p
        .open(shape(1, 1, true))
        .map_err(|e| format!("open: {e:?}"))?;
    p.offer(task, 0, batch)
        .map_err(|e| format!("offer: {e:?}"))?;
    p.offer(task, 0, batch)
        .map_err(|e| format!("offer: {e:?}"))?;
    p.grant(task, 0, 4096)
        .map_err(|e| format!("grant: {e:?}"))?;
    // It takes a batch and its window, then waits on a call the engine never answers: the
    // call says it is waiting.
    h.start(
        &mut p,
        task,
        end,
        vec![
            Step::Get(0),
            Step::Get(1),
            Step::Call {
                port: 1,
                request: b"waiting".to_vec(),
            },
        ],
    )?;
    log.until(h, &mut p, "the waiting call", |e| {
        e.iter()
            .any(|e| matches!(e, Event::Call { call, .. } if call.task == task))
    })?;
    h.kill(&mut p, task)?;
    log.until(h, &mut p, "the killed task's end", |e| {
        e.contains(&Event::Ended { task })
    })?;
    same(
        h.outcome(task)?,
        Outcome {
            seen: vec![good_seen(f), Seen::Window { len: 4096 }],
            ending: Ending::Killed,
        },
        "the killed task",
    )?;
    same(
        log.count(|e| {
            *e == Event::Released {
                task,
                input: 0,
                batch,
            }
        }),
        2,
        "releases of the held and the queued batch",
    )?;
    log.ended_last(task)?;
    same(p.residency().windows, 0, "windows after the kill")?;
    p.drop_batch(batch)
        .map_err(|e| format!("drop_batch: {e:?}"))?;
    log.until(h, &mut p, "the free", |e| {
        e.contains(&Event::Freed { batch })
    })?;
    empty(p.residency(), "after the kill")
}

/// A port call round trip: bytes, an empty reply and an error; a call past the bound; a task
/// with no port.
fn port_call<H: Harness>(h: &mut H) -> Result<(), String> {
    let mut p = h.plane(1 << 20);
    let (task, end) = p
        .open(shape(0, 0, true))
        .map_err(|e| format!("open: {e:?}"))?;
    let big = vec![7u8; request::MAX + 1];
    let large: Vec<u8> = (0..request::MAX).map(|i| (i % 251) as u8).collect();
    h.start(
        &mut p,
        task,
        end,
        vec![
            Step::Call {
                port: 5,
                request: b"abc".to_vec(),
            },
            Step::Call {
                port: 6,
                request: Vec::new(),
            },
            Step::Call {
                port: 7,
                request: b"x".to_vec(),
            },
            Step::Call {
                port: 8,
                request: big,
            },
            Step::Call {
                port: 9,
                request: large.clone(),
            },
        ],
    )?;
    let mut log = Log::new();
    let mut answered: BTreeMap<u64, CallId> = BTreeMap::new();
    while answered.len() < 4 {
        log.until(h, &mut p, "a port call", |e| {
            e.iter().filter(|e| matches!(e, Event::Call { .. })).count() > answered.len()
        })?;
        let Some(Event::Call {
            call,
            port,
            request,
        }) = log
            .events
            .iter()
            .rev()
            .find(|e| matches!(e, Event::Call { .. }))
            .cloned()
        else {
            return Err("no call".into());
        };
        same(call.task, task, "the caller")?;
        let reply: Result<Vec<u8>, Error> = match port {
            5 => {
                same(request.as_slice(), b"abc", "the request")?;
                Ok(b"reply".to_vec())
            }
            6 => {
                same(request.len(), 0, "an empty request")?;
                Ok(Vec::new())
            }
            7 => Err(Error::AccessDenied),
            9 => {
                same(sum(&request), sum(&large), "the largest request")?;
                Ok(request.iter().rev().copied().collect())
            }
            other => return Err(format!("a call to port {other}")),
        };
        p.answer(call, reply.as_deref().map_err(|e| *e))
            .map_err(|e| format!("answer: {e:?}"))?;
        answered.insert(port, call);
    }
    log.until(h, &mut p, "the caller's end", |e| {
        e.contains(&Event::Ended { task })
    })?;
    let reversed: Vec<u8> = large.iter().rev().copied().collect();
    same(
        h.outcome(task)?,
        Outcome {
            seen: vec![
                Seen::Reply(b"reply".to_vec()),
                Seen::Reply(Vec::new()),
                Seen::Error(Error::AccessDenied),
                Seen::Error(Error::Limit),
                Seen::Reply(reversed),
            ],
            ending: Ending::Exited,
        },
        "the caller",
    )?;
    log.ended_last(task)?;
    let late = answered.get(&5).copied().ok_or("no call 5")?;
    same(
        p.answer(late, Ok(b"late")),
        Err(PlaneError::Unknown("task")),
        "an answer after the end",
    )?;
    // A task with no port.
    let (task, end) = p
        .open(shape(0, 0, false))
        .map_err(|e| format!("open: {e:?}"))?;
    h.start(
        &mut p,
        task,
        end,
        vec![Step::Call {
            port: 1,
            request: b"x".to_vec(),
        }],
    )?;
    log.until(h, &mut p, "the portless task's end", |e| {
        e.contains(&Event::Ended { task })
    })?;
    same(
        h.outcome(task)?.seen,
        vec![Seen::Error(Error::BadHandle)],
        "a call with no port",
    )
}

/// The budget refuses past itself with the same numbers on every adapter, and what comes back
/// is the budget's again.
fn budget<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    let window = window_of(&f.good);
    let budget = window + 2 * PAGE;
    let mut p = h.plane(budget);
    let (task, end) = p
        .open(shape(1, 1, false))
        .map_err(|e| format!("open: {e:?}"))?;
    p.grant(task, 0, window)
        .map_err(|e| format!("grant: {e:?}"))?;
    same(
        p.grant(task, 0, 3 * PAGE),
        Err(PlaneError::Budget {
            asked: 3 * PAGE,
            free: 2 * PAGE,
        }),
        "a window past the budget",
    )?;
    let mut script = produce(1, &f.good, f.good.len() as u64);
    script.push(Step::Get(0));
    h.start(&mut p, task, end, script)?;
    let mut log = Log::new();
    log.until(h, &mut p, "the put", |e| {
        e.iter().any(|e| matches!(e, Event::Put { .. }))
    })?;
    let (batch, _, _) = log.put_by(task).ok_or("no put")?;
    same(
        p.grant(task, 0, 3 * PAGE),
        Err(PlaneError::Budget {
            asked: 3 * PAGE,
            free: 2 * PAGE,
        }),
        "a window past the budget while the batch is held",
    )?;
    p.drop_batch(batch)
        .map_err(|e| format!("drop_batch: {e:?}"))?;
    log.until(h, &mut p, "the free", |e| {
        e.contains(&Event::Freed { batch })
    })?;
    p.grant(task, 0, budget)
        .map_err(|e| format!("the whole budget once the batch is freed: {e:?}"))?;
    same(
        p.grant(task, 0, PAGE),
        Err(PlaneError::Budget {
            asked: PAGE,
            free: 0,
        }),
        "a window past a spent budget",
    )?;
    p.finish(task, 0).map_err(|e| format!("finish: {e:?}"))?;
    log.until(h, &mut p, "the producer's end", |e| {
        e.contains(&Event::Ended { task })
    })?;
    empty(p.residency(), "the window granted and never taken")
}

/// A slot holds `SLOT_DEPTH` batches; the next offer is `Full` until the task takes one.
fn backpressure<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    let mut p = h.plane(1 << 24);
    let mut log = Log::new();
    let (_, batch) = batch_of(h, &mut p, f, &mut log)?;
    let (task, end) = p
        .open(shape(1, 0, false))
        .map_err(|e| format!("open: {e:?}"))?;
    for i in 0..limits::SLOT_DEPTH {
        p.offer(task, 0, batch)
            .map_err(|e| format!("offer {i}: {e:?}"))?;
    }
    same(
        p.offer(task, 0, batch),
        Err(PlaneError::Full),
        "one past the depth",
    )?;
    let total = limits::SLOT_DEPTH + 1;
    let mut script = vec![Step::Get(0); total];
    script.push(Step::Get(0));
    h.start(&mut p, task, end, script)?;
    log.until(h, &mut p, "a release", |e| {
        e.iter()
            .any(|e| matches!(e, Event::Released { task: t, .. } if *t == task))
    })?;
    p.offer(task, 0, batch)
        .map_err(|e| format!("an offer once the task took one: {e:?}"))?;
    p.finish(task, 0).map_err(|e| format!("finish: {e:?}"))?;
    same(
        p.offer(task, 0, batch),
        Err(PlaneError::Closed),
        "a finished input",
    )?;
    log.until(h, &mut p, "the reader's end", |e| {
        e.contains(&Event::Ended { task })
    })?;
    let mut want = vec![good_seen(f); total];
    want.push(Seen::End);
    same(
        h.outcome(task)?,
        Outcome {
            seen: want,
            ending: Ending::Exited,
        },
        "the reader",
    )?;
    same(
        log.count(|e| matches!(e, Event::Released { task: t, .. } if *t == task)),
        total,
        "releases",
    )?;
    p.drop_batch(batch)
        .map_err(|e| format!("drop_batch: {e:?}"))?;
    log.until(h, &mut p, "the free", |e| {
        e.contains(&Event::Freed { batch })
    })?;
    empty(p.residency(), "after the reader")
}

/// Unknown tasks, slots, batches and calls, and the plane's bounds.
fn names_and_bounds<H: Harness>(h: &mut H, f: &Fixtures) -> Result<(), String> {
    let mut p = h.plane(1 << 24);
    same(p.next(0), Ok(None), "nothing to wait on")?;
    let mut log = Log::new();
    let (_, batch) = batch_of(h, &mut p, f, &mut log)?;
    let (task, end) = p
        .open(shape(1, 1, true))
        .map_err(|e| format!("open: {e:?}"))?;
    let nobody = TaskId(u32::MAX);
    same(
        p.offer(nobody, 0, batch),
        Err(PlaneError::Unknown("task")),
        "offer to no task",
    )?;
    same(
        p.offer(task, 1, batch),
        Err(PlaneError::Unknown("input")),
        "offer to no input",
    )?;
    same(
        p.offer(task, 0, BatchId(u64::MAX)),
        Err(PlaneError::Unknown("batch")),
        "offer of no batch",
    )?;
    same(
        p.finish(nobody, 0),
        Err(PlaneError::Unknown("task")),
        "finish of no task",
    )?;
    same(
        p.finish(task, 1),
        Err(PlaneError::Unknown("input")),
        "finish of no input",
    )?;
    same(
        p.grant(task, 1, PAGE),
        Err(PlaneError::Unknown("output")),
        "grant on no output",
    )?;
    same(
        p.grant(task, 0, 0),
        Err(PlaneError::Limit("a window of no bytes, or past u64")),
        "a window of no bytes",
    )?;
    same(
        p.drop_batch(BatchId(u64::MAX)),
        Err(PlaneError::Unknown("batch")),
        "drop of no batch",
    )?;
    same(
        p.answer(
            CallId {
                task: nobody,
                token: 1,
            },
            Ok(b"x"),
        ),
        Err(PlaneError::Unknown("task")),
        "an answer to no task",
    )?;
    same(
        p.open(shape(limits::PLANE_SLOTS as u32 + 1, 0, false))
            .err(),
        Some(PlaneError::Limit("slots of one task")),
        "a plane of 65 slots",
    )?;
    // Two slots and a port are waited on already; this fills the rest.
    let rest = crate::plane::WAITED_MAX as u32 - 3;
    let (full, full_end) = p
        .open(shape(rest, 0, false))
        .map_err(|e| format!("open to the bound: {e:?}"))?;
    same(
        p.open(shape(1, 0, false)).err(),
        Some(PlaneError::Limit("slots and ports waited on at once")),
        "one past what next waits on",
    )?;
    h.start(&mut p, full, full_end, Vec::new())?;
    h.start(&mut p, task, end, Vec::new())?;
    log.until(h, &mut p, "both ends", |e| {
        e.contains(&Event::Ended { task }) && e.contains(&Event::Ended { task: full })
    })?;
    same(
        p.open(shape(1, 0, false)).map(|_| ()).err(),
        None,
        "room again once they ended",
    )?;
    p.drop_batch(batch)
        .map_err(|e| format!("drop_batch: {e:?}"))?;
    log.until(h, &mut p, "the free", |e| {
        e.contains(&Event::Freed { batch })
    })?;
    Ok(())
}

// The scripts and outcomes as bytes, for a harness whose tasks are processes: each item a
// `u8` tag, then little-endian `u32` slots, `u64` numbers, and `u64`-length byte strings.

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u64(out, b.len() as u64);
    out.extend_from_slice(b);
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(n)
            .filter(|e| *e <= self.bytes.len())
            .ok_or("the bytes end inside an item")?;
        let s = &self.bytes[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let b = self.take(8)?;
        let mut w = [0u8; 8];
        w.copy_from_slice(b);
        Ok(u64::from_le_bytes(w))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let n = usize::try_from(self.u64()?).map_err(|_| "a length past usize")?;
        Ok(self.take(n)?.to_vec())
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// `script` as bytes.
pub fn encode_script(script: &[Step]) -> Vec<u8> {
    let mut out = Vec::new();
    for step in script {
        match step {
            Step::Get(n) => {
                out.push(1);
                out.extend_from_slice(&n.to_le_bytes());
            }
            Step::Write { slot, at, bytes } => {
                out.push(2);
                out.extend_from_slice(&slot.to_le_bytes());
                put_u64(&mut out, *at);
                put_bytes(&mut out, bytes);
            }
            Step::Put { slot, offset, len } => {
                out.push(3);
                out.extend_from_slice(&slot.to_le_bytes());
                put_u64(&mut out, *offset);
                put_u64(&mut out, *len);
            }
            Step::WriteAfterPut(n) => {
                out.push(4);
                out.extend_from_slice(&n.to_le_bytes());
            }
            Step::Call { port, request } => {
                out.push(5);
                put_u64(&mut out, *port);
                put_bytes(&mut out, request);
            }
            Step::Crash => out.push(6),
        }
    }
    out
}

/// The script [`encode_script`] wrote.
pub fn decode_script(bytes: &[u8]) -> Result<Vec<Step>, String> {
    let mut r = Reader { bytes, at: 0 };
    let mut out = Vec::new();
    while !r.done() {
        out.push(match r.u8()? {
            1 => Step::Get(r.u32()?),
            2 => Step::Write {
                slot: r.u32()?,
                at: r.u64()?,
                bytes: r.bytes()?,
            },
            3 => Step::Put {
                slot: r.u32()?,
                offset: r.u64()?,
                len: r.u64()?,
            },
            4 => Step::WriteAfterPut(r.u32()?),
            5 => Step::Call {
                port: r.u64()?,
                request: r.bytes()?,
            },
            6 => Step::Crash,
            t => return Err(format!("a step of tag {t}")),
        });
    }
    Ok(out)
}

/// `seen` as bytes: one item, so a task can write each as it happens.
pub fn encode_seen(seen: &Seen) -> Vec<u8> {
    let mut out = Vec::new();
    match seen {
        Seen::Batch { len, sum } => {
            out.push(1);
            put_u64(&mut out, *len);
            put_u64(&mut out, *sum);
        }
        Seen::End => out.push(2),
        Seen::Window { len } => {
            out.push(3);
            put_u64(&mut out, *len);
        }
        Seen::Wrote => out.push(4),
        Seen::Put => out.push(5),
        Seen::Reply(b) => {
            out.push(6);
            put_bytes(&mut out, b);
        }
        Seen::Error(e) => {
            out.push(7);
            out.extend_from_slice(&e.code().to_le_bytes());
        }
    }
    out
}

/// The items [`encode_seen`] wrote, one after another.
pub fn decode_seen(bytes: &[u8]) -> Result<Vec<Seen>, String> {
    let mut r = Reader { bytes, at: 0 };
    let mut out = Vec::new();
    while !r.done() {
        out.push(match r.u8()? {
            1 => Seen::Batch {
                len: r.u64()?,
                sum: r.u64()?,
            },
            2 => Seen::End,
            3 => Seen::Window { len: r.u64()? },
            4 => Seen::Wrote,
            5 => Seen::Put,
            6 => Seen::Reply(r.bytes()?),
            7 => {
                let code = r.u32()?;
                Seen::Error(Error::from_code(code).ok_or_else(|| format!("error code {code}"))?)
            }
            t => return Err(format!("a seen item of tag {t}")),
        });
    }
    Ok(out)
}
