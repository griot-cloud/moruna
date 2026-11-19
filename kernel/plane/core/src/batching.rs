//! Item 6.3p: the ring plane batches by design, not by an accident of the scheduler's timing.
//!
//! A simulation on the host, deterministic: the engine ([`RingPlane`] on the model of the
//! kernel's calls) and a task (the protocol's own task half, [`Task`], waiting for input with
//! [`Task::wait_input`] as `moruna_sys::plane::Rings` does on the kernel) each run on a clock of
//! their own, a CPU each ([`Cpus::Each`]) or one CPU between them ([`Cpus::One`]). A wake reaches
//! the woken side after [`Costs::wake`]: the task is woken at once, as a free CPU or item 1.11e's
//! scheduler wakes it. The calls each side makes into the kernel are counted as the booted DS-P
//! steady counts them: the engine's calls and its futex waits and wakes, and the task's futex
//! waits (each a linger or a sleep) and its wakes of the engine.

extern crate std;

use alloc::{collections::VecDeque, rc::Rc, vec, vec::Vec};
use core::cell::{Cell, RefCell};

use crate::model::Model;
use crate::model_ring::{RingTaskModel, model};
use crate::port::{DataPlane, Event, TaskId};
use crate::ring::{self, OutEntry, Pace, Rest, Shared, Task, layout};
use crate::ringed::{Region, RegionId, RingPlane, RingShape, View};

const US: u64 = 1_000;
const MS: u64 = 1_000_000;
/// The deadline of a wait without one.
const FOREVER: u64 = u64::MAX;
/// The window a record is written in: the fixture's record (4,128 bytes) in whole pages.
const WINDOW: u64 = 8192;

/// What each side's work costs, in nanoseconds of its CPU.
#[derive(Clone, Copy, Debug)]
struct Costs {
    /// The engine lays a batch out in the task's region and posts it.
    post: u64,
    /// It reads a record the task put, and drops it.
    judge: u64,
    /// The task answers a batch.
    compute: u64,
    /// A wake reaches the side it wakes.
    wake: u64,
}

/// A task faster than its engine, as DS-P steady's is on the booted kernel (the engine writes
/// each batch's values; the task reads them and writes one value each).
const FAST_TASK: Costs = Costs {
    post: 100 * US,
    judge: 30 * US,
    compute: 60 * US,
    wake: 5 * US,
};

/// Posts too slow for a lingering task: two of them do not fit in `ring::LINGER_MAX_NS`, so the
/// task sleeps between them, as DS-P's task did on the emulator (6.3p's first boots: posts of
/// 5 ms and more at four CPUs, 17 ms from post to record at one).
const SLOW_POSTS: Costs = Costs {
    post: 6 * MS,
    judge: 300 * US,
    compute: 3 * MS,
    wake: 50 * US,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cpus {
    /// One CPU between the engine and the task: a woken task takes it at once.
    One,
    /// A CPU each.
    Each,
}

/// The engine's program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// Lay a batch out and post it.
    Post,
    /// Take the next record, waiting for it in `next` (blocking).
    Take,
    /// Work elsewhere for this long, calling nothing of the plane: a stage unit that posts and
    /// returns to its worker.
    Busy(u64),
    /// Leave the plane for other work, as a stage unit that returns its worker does
    /// (`RingPlane::leave`).
    Leave,
    /// Look at the plane without blocking (`next(0)`), taking what came.
    Poll,
    /// Close the task's input.
    Close,
}

/// How the task waits for input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Waits {
    /// Sleeps until the next post, as every task did before 6.3p (and as an old task does).
    Sleeps,
    /// Lingers while its engine is posting ([`Task::wait_input`]), for a group of this many
    /// posts.
    Lingers(u64),
    /// The other candidate 6.3p weighed: spins while its engine is posting, for this many of
    /// its measured posting intervals at most, then sleeps until the next post. Measured only.
    Spins(u64),
}

/// The rule 6.3p chose.
const LINGERS: Waits = Waits::Lingers(ring::GROUP);

fn word(bytes: &RefCell<Vec<u8>>, at: usize) -> u64 {
    let b = bytes.borrow();
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(w)
}

fn set_word(bytes: &RefCell<Vec<u8>>, at: usize, value: u64) {
    bytes.borrow_mut()[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// The engine, on its clock.
struct Engine {
    plane: RingPlane<Model>,
    task: TaskId,
    region: RegionId,
    record: Vec<u8>,
    ops: VecDeque<Op>,
    costs: Costs,
    cpus: Cpus,
    /// Its clock.
    at: u64,
    /// It waits in `next` for the task, asleep on its bell, until the task rings it.
    blocked: bool,
    /// The rings object, to say the engine sleeps on its bell (the model's futex returns at once).
    rings: Rc<RefCell<Vec<u8>>>,
    /// When each batch in flight was posted, oldest first.
    posted: VecDeque<u64>,
    /// When each batch was posted.
    post_times: Vec<u64>,
    /// For each record taken, the time from its batch's post to its take.
    latency: Vec<u64>,
    /// It reads its clock with no call (rule (d): a sleeping task woken a group of posts at a
    /// time); without one, a sleeping task is woken at each post.
    clock: bool,
}

impl Engine {
    /// When the next operation takes effect; `None` while the engine waits or has nothing to do.
    fn effect(&self) -> Option<u64> {
        if self.blocked {
            return None;
        }
        Some(match self.ops.front()? {
            Op::Post => self.at + self.costs.post,
            Op::Busy(d) => self.at + d,
            Op::Take | Op::Leave | Op::Poll | Op::Close => self.at,
        })
    }

    /// The engine's clock as the plane reads it with no call, when the engine has one.
    fn tick(&mut self) {
        if self.clock {
            self.plane.plane.calls.clock = Some(self.at);
        }
    }

    fn step(&mut self) {
        let Some(op) = self.ops.front().copied() else {
            return;
        };
        match op {
            Op::Post => {
                self.at += self.costs.post;
                self.tick();
                let record = self.record.clone();
                let (b, _) = self
                    .plane
                    .load_into(self.region, record.len() as u64, |v, at| {
                        v.write(at, &record)
                    })
                    .expect("the batch laid out");
                self.plane.offer(self.task, 0, b).expect("posted");
                self.plane.drop_batch(b).expect("the engine's hold");
                self.posted.push_back(self.at);
                self.post_times.push(self.at);
                self.ops.pop_front();
            }
            Op::Take => match {
                self.tick();
                self.plane.next(1).expect("next")
            } {
                Some(Event::Put { batch, .. }) => {
                    let posted = self.posted.pop_front().expect("a batch in flight");
                    self.latency.push(self.at - posted);
                    self.at += self.costs.judge;
                    self.plane.drop_batch(batch).expect("the record dropped");
                    self.ops.pop_front();
                }
                Some(Event::Ended { .. } | Event::Refused { .. } | Event::Call { .. }) => {
                    panic!("the task ended or broke the protocol")
                }
                Some(_) => {}
                None => {
                    // Asleep on its bell: the task's next record wakes it.
                    self.blocked = true;
                    set_word(&self.rings, layout::ENGINE_WAITS, 1);
                }
            },
            Op::Busy(d) => {
                self.at += d;
                self.ops.pop_front();
            }
            Op::Leave => {
                self.tick();
                self.plane.leave();
                self.ops.pop_front();
            }
            Op::Poll => {
                self.tick();
                while let Some(e) = self.plane.next(0).expect("a poll") {
                    if let Event::Put { batch, .. } = e {
                        let posted = self.posted.pop_front().expect("a batch in flight");
                        self.latency.push(self.at - posted);
                        self.plane.drop_batch(batch).expect("the record dropped");
                    }
                }
                self.ops.pop_front();
            }
            Op::Close => {
                self.tick();
                self.plane.finish(self.task, 0).expect("closed");
                self.ops.pop_front();
            }
        }
    }

    /// A CPU each: the engine runs what it does by the task's time `t`.
    fn catch_up(&mut self, t: u64) {
        if self.cpus == Cpus::One {
            return;
        }
        while let Some(e) = self.effect()
            && e <= t
        {
            self.step();
        }
    }

    /// The task sleeps on its bell from its time `at` until `deadline`: the engine runs until it
    /// wakes the task, or until its next operation would come after the deadline. Whether the
    /// task was woken; its clock is moved to when it runs again.
    fn sleep_task(&mut self, at: &Cell<u64>, deadline: u64) -> bool {
        let t0 = at.get();
        if self.cpus == Cpus::One {
            // The CPU was the task's: the engine goes on from here.
            self.at = self.at.max(t0);
        }
        let bells = self.plane.counters().bells;
        loop {
            if self.plane.counters().bells > bells {
                at.set(t0.max(self.at) + self.costs.wake);
                return true;
            }
            match self.effect() {
                Some(e) if e <= deadline => self.step(),
                _ => {
                    assert!(
                        deadline != FOREVER,
                        "a lost wake: the task sleeps with no deadline and the engine {}",
                        if self.blocked {
                            "waits on it"
                        } else {
                            "has nothing left to do"
                        }
                    );
                    // On the one CPU the engine works elsewhere until the task's deadline, when
                    // the task's timer gives it the CPU: what is left of that work waits until
                    // the task sleeps again.
                    if self.cpus == Cpus::One
                        && !self.blocked
                        && let Some(Op::Busy(d)) = self.ops.front().copied()
                        && self.at < deadline
                    {
                        let done = deadline - self.at;
                        self.ops[0] = Op::Busy(d - done);
                        self.at = deadline;
                    }
                    at.set(t0.max(deadline));
                    return false;
                }
            }
        }
    }

    /// The task spins from its time `at` until `until` or a post: on a CPU of its own the engine
    /// runs meanwhile, and the task sees a post as it lands; on the one CPU the engine cannot run.
    /// Whether a post came; the task's clock is moved to when it stopped.
    fn spin_task(&mut self, at: &Cell<u64>, until: u64) -> bool {
        let t0 = at.get();
        if self.cpus == Cpus::One {
            at.set(until.max(t0));
            return false;
        }
        let posted = self.plane.counters().posted;
        while let Some(e) = self.effect()
            && e <= until
        {
            self.step();
            if self.plane.counters().posted > posted {
                at.set(t0.max(self.at));
                return true;
            }
        }
        at.set(until.max(t0));
        false
    }

    /// The task, at `t`, rang the engine's bell and woke it.
    fn woken_by_task(&mut self, t: u64) {
        if !self.blocked {
            return;
        }
        self.blocked = false;
        self.at = match self.cpus {
            Cpus::Each => self.at.max(t + self.costs.wake),
            // It runs when the task waits, which takes the CPU back.
            Cpus::One => self.at.max(t),
        };
    }

    /// Every kernel call the engine has made: its calls, and its futex waits and wakes.
    fn calls(&self) -> u64 {
        self.plane.plane.calls.calls + self.plane.plane.calls.futex.get()
    }
}

/// The task's mapping of its rings object: its futex sleeps while the engine runs.
struct TaskMap {
    bytes: Rc<RefCell<Vec<u8>>>,
    engine: Rc<RefCell<Engine>>,
    /// The task's clock.
    at: Rc<Cell<u64>>,
}

impl Shared for TaskMap {
    fn load(&self, at: usize) -> u64 {
        word(&self.bytes, at)
    }
    fn store(&self, at: usize, value: u64) {
        set_word(&self.bytes, at, value);
    }
    fn fence(&self) {}
    fn wait(&self, at: usize, expected: u32, deadline: u64) -> bool {
        if word(&self.bytes, at) as u32 != expected {
            return true;
        }
        self.engine.borrow_mut().sleep_task(&self.at, deadline)
    }
    fn wake(&self, _at: usize) {
        self.engine.borrow_mut().woken_by_task(self.at.get());
    }
}

/// What a run measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Measured {
    batches: u64,
    /// The engine's kernel calls.
    engine: u64,
    /// The task's futex calls: its waits and its wakes of the engine.
    task: u64,
    /// The engine's wakes of the task, and of them those called as it was about to block.
    bells: u64,
    flushes: u64,
    /// Of them, the wakes of a sleeping task at a group's deadline.
    overdue: u64,
    /// Posts to a lingering task that woke it not.
    deferred: u64,
    /// The mean time from a batch's post to the engine's take of its record.
    latency: u64,
    /// The mean time from a batch's post to the task's record answering it.
    answer: u64,
    /// The task's time spent spinning.
    spun: u64,
    /// When the engine took the last record.
    elapsed: u64,
    /// When the task saw its input end.
    ended: u64,
}

impl Measured {
    /// Calls a batch, in hundredths.
    fn engine_per_batch(&self) -> u64 {
        self.engine * 100 / self.batches
    }
    fn task_per_batch(&self) -> u64 {
        self.task * 100 / self.batches
    }
}

/// A run: what it measured, and the engine and the task as they ended.
struct Run {
    m: Measured,
    engine: Rc<RefCell<Engine>>,
    task: TaskRun,
}

/// Runs `ops` on a plane of `group` (the engine's batching), with a task that waits as `waits`
/// says, a CPU each or one between them: what it measured.
fn measure(ops: &[Op], costs: Costs, cpus: Cpus, waits: Waits, group: u64) -> Measured {
    run(ops, costs, cpus, waits, group, false).m
}

/// [`measure`] with an engine that reads its clock: rule (d), a sleeping task woken a group of
/// posts at a time.
fn measure_d(ops: &[Op], costs: Costs, cpus: Cpus, waits: Waits) -> Measured {
    run(ops, costs, cpus, waits, ring::GROUP, true).m
}

/// [`measure`], the engine and the task kept; `clock`, whether the engine reads its clock.
fn run(ops: &[Op], costs: Costs, cpus: Cpus, waits: Waits, group: u64, clock: bool) -> Run {
    let posts = ops.iter().filter(|o| **o == Op::Post).count() as u64;
    let mut plane = RingPlane::new(model(true), 64 << 20);
    plane.spins = 1;
    plane.group = group;
    let (task, _port) = plane
        .open_ring(RingShape {
            region: Region::New(64 * WINDOW),
            windows: 40,
            window_bytes: WINDOW,
        })
        .expect("a ring task");
    let region = plane.region_of(task).expect("its region");
    let [input, output] = plane.ring_slots(task).expect("its slots");
    let (input, output) = (input.clone(), output.clone());
    let region_bytes = RingTaskModel::map(&input).expect("the region").bytes;
    let rings = RingTaskModel::map(&output).expect("the rings object").bytes;
    let engine = Rc::new(RefCell::new(Engine {
        plane,
        task,
        region,
        record: crate::tests::record(),
        ops: ops.iter().copied().collect(),
        costs,
        cpus,
        at: 0,
        blocked: false,
        rings: rings.clone(),
        posted: VecDeque::new(),
        post_times: Vec::new(),
        latency: Vec::new(),
        clock,
    }));
    let map = TaskMap {
        bytes: rings,
        engine: engine.clone(),
        at: Rc::new(Cell::new(0)),
    };
    let calls0 = engine.borrow().calls();
    let t = run_task(&engine, &map, &region_bytes, waits, costs.compute);
    let e = engine.borrow();
    let c = e.plane.counters();
    assert_eq!(
        e.latency.len() as u64,
        posts,
        "every batch answered and taken"
    );
    assert_eq!(t.answered.len() as u64, posts, "every batch answered once");
    let answer: u64 = t
        .answered
        .iter()
        .zip(&e.post_times)
        .map(|(a, p)| a - p)
        .sum();
    let m = Measured {
        batches: posts,
        engine: e.calls() - calls0,
        task: t.calls,
        bells: c.bells,
        flushes: c.flushes,
        overdue: c.overdue,
        deferred: c.deferred,
        latency: e.latency.iter().sum::<u64>() / posts.max(1),
        answer: answer / posts.max(1),
        spun: t.spun,
        elapsed: e.at,
        ended: map.at.get(),
    };
    drop(e);
    Run { m, engine, task: t }
}

/// What the task did.
struct TaskRun {
    /// Its futex calls.
    calls: u64,
    /// When it posted each record.
    answered: Vec<u64>,
    /// Its time spent spinning.
    spun: u64,
}

/// The task: answers each batch with its record in a window, and waits for input as `waits`
/// says.
fn run_task(
    engine: &Rc<RefCell<Engine>>,
    map: &TaskMap,
    region: &Rc<RefCell<Vec<u8>>>,
    waits: Waits,
    compute: u64,
) -> TaskRun {
    let mut half = Task::attach(map).expect("attached");
    let clock = map.at.clone();
    let mut pace = Pace::new(half.head(map), clock.get());
    let mut calls = 0u64;
    let mut answered = Vec::new();
    let mut spun = 0;
    loop {
        engine.borrow_mut().catch_up(clock.get());
        match half.next(map).expect("an input") {
            Some(e) => {
                clock.set(clock.get() + compute);
                engine.borrow_mut().catch_up(clock.get());
                let w = loop {
                    if let Some(w) = half.window(map).expect("a window") {
                        break w;
                    }
                    let slept = half.rest(map, Rest::Sleep(FOREVER), |t| t.has_window(map));
                    calls += u64::from(slept.called());
                    engine.borrow_mut().catch_up(clock.get());
                };
                let bytes =
                    region.borrow()[e.offset as usize..(e.offset + e.len) as usize].to_vec();
                let at = half.geometry.window_at(w) as usize;
                map.bytes.borrow_mut()[at..at + bytes.len()].copy_from_slice(&bytes);
                assert!(half.post(
                    map,
                    OutEntry {
                        window: w,
                        offset: 0,
                        len: e.len,
                        cookie: e.cookie,
                    }
                ));
                half.release(map);
                calls += u64::from(half.ring(map));
                answered.push(clock.get());
            }
            None if half.ended(map) => {
                calls += u64::from(half.exit(map, 0));
                return TaskRun {
                    calls,
                    answered,
                    spun,
                };
            }
            None => {
                let sleep = |half: &Task| {
                    u32::from(
                        half.rest(map, Rest::Sleep(FOREVER), |t| t.has_input(map))
                            .called(),
                    )
                };
                calls += u64::from(match waits {
                    // The rule as the task on the kernel runs it.
                    Waits::Lingers(ring::GROUP) => {
                        let clock = clock.clone();
                        half.wait_input(map, &mut pace, move || clock.get(), FOREVER)
                    }
                    // Other groups, measured.
                    Waits::Lingers(group) => {
                        let at = clock.get();
                        pace.saw(half.head(map), at);
                        let how = pace.rest_in(at, group, FOREVER);
                        let slept = half.rest(map, how, |t| t.has_input(map));
                        let mut n = u32::from(slept.called());
                        if matches!(how, Rest::Linger(_)) && slept == ring::Slept::TimedOut {
                            n += sleep(&half);
                        }
                        pace.saw(half.head(map), clock.get());
                        n
                    }
                    Waits::Sleeps => sleep(&half),
                    Waits::Spins(group) => {
                        let at = clock.get();
                        pace.saw(half.head(map), at);
                        let bound = pace
                            .interval()
                            .map(|i| i * group)
                            .filter(|b| *b <= ring::LINGER_MAX_NS);
                        let came =
                            bound.is_some_and(|b| engine.borrow_mut().spin_task(&clock, at + b));
                        spun += clock.get() - at;
                        let n = if came { 0 } else { sleep(&half) };
                        pace.saw(half.head(map), clock.get());
                        n
                    }
                });
            }
        }
    }
}

/// DS-P steady's program: groups of 32 posted, then their 32 records taken.
fn steady(groups: usize) -> Vec<Op> {
    let mut ops = Vec::new();
    for _ in 0..groups {
        ops.extend([Op::Post; 32]);
        ops.extend([Op::Take; 32]);
    }
    ops.push(Op::Close);
    ops
}

/// Lockstep: one batch posted, then its record taken, `n` times.
fn lockstep(n: usize) -> Vec<Op> {
    let mut ops = Vec::new();
    for _ in 0..n {
        ops.extend([Op::Post, Op::Take]);
    }
    ops.push(Op::Close);
    ops
}

/// A stage's tasks' program: three batches in flight (`moruna-micro`'s `layout::IN_FLIGHT`),
/// the oldest record taken before each new post.
fn stage(n: usize) -> Vec<Op> {
    let mut ops = vec![Op::Post; 3];
    for _ in 3..n {
        ops.extend([Op::Take, Op::Post]);
    }
    ops.extend([Op::Take; 3]);
    ops.push(Op::Close);
    ops
}

/// The defect, written as a test: a task woken at once that answers faster than its engine
/// posts took one batch a wake, so the plane made a call a batch on each side (DS-P steady:
/// 1.08 to 1.20 calls a batch on the booted kernel). Now, on a CPU each or one between them, the
/// steady state stays under one call a batch on each side, and every post that did not wake the
/// task was answered.
#[test]
fn a_task_woken_at_once_costs_under_a_call_a_batch_on_each_side() {
    for cpus in [Cpus::Each, Cpus::One] {
        let before = measure(&steady(8), FAST_TASK, cpus, Waits::Sleeps, ring::GROUP);
        assert!(
            before.task_per_batch() >= 100,
            "{cpus:?}: a task that sleeps until the next post sleeps once a batch: {before:?}"
        );
        assert_eq!(before.deferred, 0, "a sleeping task is woken at each post");
        let after = measure(&steady(8), FAST_TASK, cpus, LINGERS, ring::GROUP);
        assert!(
            after.engine_per_batch() < 100 && after.task_per_batch() < 100,
            "{cpus:?}: lingering, under a call a batch on each side: {after:?}"
        );
        assert!(after.deferred > 0, "{cpus:?}: posts batched: {after:?}");
        assert!(
            after.engine + after.task < before.engine + before.task,
            "{cpus:?}: fewer calls in all: {before:?} then {after:?}"
        );
    }
}

/// A lone batch, one post and then its answer awaited, costs what it did: the engine wakes a
/// lingering task as it blocks, so the batch waits for no linger (lockstep, 6.3p's condition
/// 5). On a CPU each lockstep's calls are what they were.
///
/// On the one CPU the simulation runs a woken task at once, but only between the engine's
/// operations: a post's wake let the task answer before the engine looked for the record, while
/// the wake as the engine blocks comes inside its `next`, which then sleeps on its bell before the
/// task runs. So it counts the engine's sleep and the task's wake of it for each batch, at most
/// three calls and one more. On the kernel a wake by the running task does not preempt it (item
/// 1.11e), so the engine sleeps before the task answers with either rule: the booted DS-P
/// lockstep at one CPU is the measure.
#[test]
fn a_lone_batch_waits_for_no_linger() {
    for cpus in [Cpus::Each, Cpus::One] {
        let before = measure(&lockstep(64), FAST_TASK, cpus, Waits::Sleeps, ring::GROUP);
        let after = measure(&lockstep(64), FAST_TASK, cpus, LINGERS, ring::GROUP);
        assert_eq!(
            after.latency, before.latency,
            "{cpus:?}: post to answer, the same: {before:?} then {after:?}"
        );
        let (engine, task) = match cpus {
            Cpus::Each => (1, 1),
            Cpus::One => (3 * after.batches, after.batches),
        };
        assert!(
            after.engine <= before.engine + engine && after.task <= before.task + task,
            "{cpus:?}: lockstep's calls: {before:?} then {after:?}"
        );
    }
}

/// A batch posted while its engine works elsewhere, a stage unit that posts and returns to its
/// worker with no `next` after: the task, lingering, is not woken for it, and answers it by its
/// own deadline (a group of posting intervals, within the bound), long before the engine comes
/// back for it. No wake is lost.
#[test]
fn a_post_with_no_next_after_it_is_answered_by_the_task_s_deadline() {
    // Batches the task measures (it lingers after each answer), then a lone post the engine
    // follows with 50 ms of other work.
    let mut ops = lockstep(8);
    ops.pop();
    ops.extend([Op::Post, Op::Busy(50 * MS), Op::Take, Op::Close]);
    for cpus in [Cpus::Each, Cpus::One] {
        let r = run(&ops, FAST_TASK, cpus, LINGERS, ring::GROUP, false);
        let e = r.engine.borrow();
        let posted = *e.post_times.last().expect("the lone post");
        let answered = *r.task.answered.last().expect("its answer");
        // The task's deadline: a group of its measured intervals, each at most a lockstep
        // round (a post, a wake, an answer, a wake, a take).
        let c = FAST_TASK;
        let round = c.post + c.compute + c.judge + 2 * c.wake;
        let bound = ring::GROUP * round + c.compute;
        assert!(
            answered - posted <= bound,
            "{cpus:?}: answered {} us after its post, by the task's deadline: {:?}",
            (answered - posted) / US,
            r.m
        );
        // On the one CPU the task's answer took the CPU from the engine's other work for its
        // compute; on a CPU each it took nothing.
        let lent = if cpus == Cpus::One { c.compute } else { 0 };
        assert_eq!(
            *e.latency.last().expect("taken"),
            50 * MS + lent,
            "{cpus:?}: the record was there when the engine came back, which woke no one"
        );
        assert!(
            r.m.deferred >= r.m.batches - 1 && r.m.flushes < r.m.deferred,
            "{cpus:?}: every post to the lingering task deferred, the lone one never flushed: {:?}",
            r.m
        );
    }
}

/// The input closed while the task lingers: the close wakes it at once, not at its deadline.
#[test]
fn the_close_wakes_a_lingering_task_at_once() {
    let mut ops = lockstep(8);
    ops.pop();
    ops.extend([Op::Busy(10 * US), Op::Close]);
    for cpus in [Cpus::Each, Cpus::One] {
        let r = run(&ops, FAST_TASK, cpus, LINGERS, ring::GROUP, false);
        let e = r.engine.borrow();
        assert_eq!(
            r.m.ended,
            e.at + FAST_TASK.wake,
            "{cpus:?}: the task saw the end a wake after the close: {:?}",
            r.m
        );
    }
}

/// An engine of protocol 4 takes a lingering task's word as a sleep and wakes it at each post: a
/// new task with an old engine is answered in full, waiting out no linger.
#[test]
fn an_old_engine_wakes_a_lingering_task_at_each_post() {
    // The plane with a group of one is protocol 4's engine for posts.
    for cpus in [Cpus::Each, Cpus::One] {
        let old = measure(&steady(4), FAST_TASK, cpus, LINGERS, 1);
        assert_eq!((old.deferred, old.flushes), (0, 0), "{cpus:?}: {old:?}");
        let before = measure(&lockstep(16), FAST_TASK, cpus, Waits::Sleeps, 1);
        let lone = measure(&lockstep(16), FAST_TASK, cpus, LINGERS, 1);
        assert_eq!(
            lone.latency, before.latency,
            "{cpus:?}: no linger waited out"
        );
    }
    // And protocol 4's rule itself, against the word a lingering task writes.
    let s = Words::new();
    let t = task(&s);
    let seq = t.arm(&s, Rest::Linger(10), |_| false).expect("armed");
    assert_eq!(s.load(layout::TASK_WAITS), layout::WAITS_LINGER);
    let mut bell = 0u32;
    assert!(
        protocol_4_ring(&s, &mut bell),
        "protocol 4 wakes any waiter"
    );
    assert_ne!(s.load(layout::TASK_BELL) as u32, seq, "the bell moved");
    assert_eq!(s.wakes.get(), 1);
}

/// A task of before (the Python shim's, and `RingTaskModel`) only ever sleeps: the new engine
/// wakes it at each post, and defers and flushes nothing.
#[test]
fn a_new_engine_wakes_an_old_task_at_each_post() {
    let old = measure(
        &steady(4),
        FAST_TASK,
        Cpus::Each,
        Waits::Sleeps,
        ring::GROUP,
    );
    assert_eq!((old.deferred, old.flushes), (0, 0), "{old:?}");
    assert!(old.bells >= old.batches, "a wake for every post: {old:?}");
    // The model task of the conformance suite, which arms the word with 1 itself: a post to it
    // asleep wakes it, a post to it awake (a window given woke it meanwhile) calls nothing, and
    // no post is deferred.
    let mut p = RingPlane::new(model(true), 64 << 20);
    p.spins = 1;
    let (task, port) = p
        .open_ring(RingShape {
            region: Region::New(1 << 20),
            windows: 4,
            window_bytes: WINDOW,
        })
        .expect("a ring task");
    let slots = p.ring_slots(task).expect("slots");
    let rings = RingTaskModel::map(slots[1])
        .expect("the rings object")
        .bytes;
    let mut t = RingTaskModel::new([slots[0], slots[1]], port, crate::model_ring::Behave::Echo);
    let region = p.region_of(task).expect("region");
    t.step();
    let mut woken = 0;
    for i in 0..8 {
        let record = crate::tests::record();
        let (b, _) = p
            .load_into(region, record.len() as u64, |v, at| v.write(at, &record))
            .expect("laid out");
        let asleep = word(&rings, layout::TASK_WAITS) == layout::WAITS_SLEEP;
        woken += u64::from(asleep);
        let bells = p.counters().bells;
        p.offer(task, 0, b).expect("posted");
        p.drop_batch(b).expect("dropped");
        assert_eq!(
            p.counters().bells,
            bells + u64::from(asleep),
            "post {i}: the task woken if it slept (it did: {asleep})"
        );
        t.step();
        while let Some(e) = p.next(0).expect("next") {
            if let Event::Put { batch, .. } = e {
                p.drop_batch(batch).expect("the record dropped");
            }
        }
    }
    assert!(woken > 0, "the task slept before some post");
    assert_eq!(t.outputs, 8, "every batch answered");
    assert_eq!((p.counters().deferred, p.counters().flushes), (0, 0));
}

/// Words of a rings object, with the wakes called on them counted.
struct Words {
    bytes: RefCell<Vec<u8>>,
    wakes: Cell<u64>,
}

impl Words {
    fn new() -> Words {
        let g = ring::Geometry::new(1 << 20, 2, WINDOW).expect("geometry");
        let w = Words {
            bytes: RefCell::new(vec![0u8; g.object_bytes().expect("size") as usize]),
            wakes: Cell::new(0),
        };
        ring::Engine::format(&w, g, ring::Mode::Protect, 0);
        w
    }
}

impl Shared for Words {
    fn load(&self, at: usize) -> u64 {
        word(&self.bytes, at)
    }
    fn store(&self, at: usize, value: u64) {
        set_word(&self.bytes, at, value);
    }
    fn fence(&self) {}
    fn wake(&self, _at: usize) {
        self.wakes.set(self.wakes.get() + 1);
    }
}

fn task(s: &Words) -> Task {
    Task::attach(s).expect("attached")
}

/// Protocol 4's `Engine::ring`, as the engines before 6.3p run it: the bell raised, and the task
/// woken if its word is anything but zero.
fn protocol_4_ring(s: &impl Shared, bell: &mut u32) -> bool {
    *bell = bell.wrapping_add(1);
    s.store(layout::TASK_BELL, u64::from(*bell));
    s.fence();
    if s.load(layout::TASK_WAITS) == 0 {
        return false;
    }
    s.store(layout::TASK_WAITS, 0);
    s.wake(layout::TASK_BELL);
    true
}

/// The engine's half of the bell: a sleeping task is woken at each post; a lingering one once a
/// group of posts waits, before the engine blocks (a flush), or at the close; a window given
/// wakes a sleeping task only; nothing owed, no call.
#[test]
fn the_engine_wakes_a_lingering_task_per_group_before_it_blocks_and_at_the_close() {
    let s = Words::new();
    let g = ring::Geometry::read(&s).expect("header").0;
    let mut e = ring::Engine::format(&s, g, ring::Mode::Protect, 0);
    let mut t = task(&s);
    let mut cookie = 0;
    let mut post = |e: &mut ring::Engine| {
        cookie += 1;
        let entry = ring::InEntry {
            offset: 0,
            len: 4096,
            cookie,
            rows: 1,
        };
        assert!(e.post(&s, entry));
        e.ring(&s)
    };
    // A sleeping task: each post wakes it.
    t.arm(&s, Rest::Sleep(FOREVER), |_| false).expect("armed");
    assert!(post(&mut e));
    assert_eq!((s.wakes.get(), e.owed()), (1, 0));
    assert_eq!(s.load(layout::TASK_WAITS), 0, "the wake clears the word");
    // An awake task: nothing to wake, nothing owed.
    assert!(!post(&mut e));
    assert!(!e.flush(&s), "nothing owed, no call");
    // A lingering task: woken at the group's last post.
    t.arm(&s, Rest::Linger(1), |_| false).expect("armed");
    for i in 1..ring::GROUP {
        assert!(!post(&mut e), "post {i} of the group is deferred");
        assert_eq!(e.owed(), i);
    }
    assert!(post(&mut e), "the group is complete");
    assert_eq!((s.wakes.get(), e.owed()), (2, 0));
    // Lingering again: one post, then the engine blocks.
    t.arm(&s, Rest::Linger(1), |_| false).expect("armed");
    assert!(!post(&mut e));
    assert!(
        !e.ring_given(&s),
        "a window does not wake a task lingering for input"
    );
    assert!(e.flush(&s), "the engine wakes it before it blocks");
    assert_eq!((s.wakes.get(), e.owed()), (3, 0));
    assert!(!e.flush(&s), "once");
    // Owed, but the task stopped lingering on its own (its deadline): no call, nothing owed.
    t.arm(&s, Rest::Linger(1), |_| false).expect("armed");
    assert!(!post(&mut e));
    Task::disarm(&s);
    assert!(!e.flush(&s));
    assert_eq!((s.wakes.get(), e.owed()), (3, 0));
    // Owed, the task took and released every post at its deadline and lingers again: no call.
    t.arm(&s, Rest::Linger(1), |_| false).expect("armed");
    assert!(!post(&mut e));
    while t.next(&s).expect("an input").is_some() {
        assert!(t.release(&s));
    }
    t.arm(&s, Rest::Linger(1), |_| false).expect("armed again");
    assert!(!e.flush(&s), "nothing unreleased: nothing for it");
    assert_eq!((s.wakes.get(), e.owed()), (3, 0));
    Task::disarm(&s);
    // A window given to a sleeping task wakes it.
    t.arm(&s, Rest::Sleep(FOREVER), |_| false).expect("armed");
    assert!(e.ring_given(&s));
    assert!(!e.ring_given(&s), "awake now");
    // The close wakes a lingering task at once, and an awake one not at all.
    t.arm(&s, Rest::Linger(1), |_| false).expect("armed");
    e.close(&s);
    assert!(e.ring_now(&s));
    assert!(!e.ring_now(&s));
    assert_eq!(s.wakes.get(), 5);
}

/// Rule (d), the defect as the emulator showed it: posts too slow for a lingering task (two do not
/// fit in `LINGER_MAX_NS`), so the task sleeps between them and, woken at each post, takes one
/// batch a wake (DS-P steady 1.00 to 1.08 calls a batch, booted). An engine that reads its clock
/// wakes a sleeping task a group of posts at a time: under a call a batch on each side, on a CPU
/// each and on one.
#[test]
fn a_sleeping_task_is_woken_a_group_of_posts_at_a_time() {
    for cpus in [Cpus::Each, Cpus::One] {
        let before = measure(&steady(8), SLOW_POSTS, cpus, LINGERS, ring::GROUP);
        assert!(
            before.task_per_batch() >= 100,
            "{cpus:?}: without a clock, a sleeping task is woken at each post: {before:?}"
        );
        let after = measure_d(&steady(8), SLOW_POSTS, cpus, LINGERS);
        assert!(
            after.engine_per_batch() < 100 && after.task_per_batch() < 100,
            "{cpus:?}: rule (d), under a call a batch on each side: {after:?}"
        );
        assert!(after.deferred > 0, "{cpus:?}: posts batched: {after:?}");
    }
    // With fast posts the task lingers, and rule (d) keeps today's rule: only the first posts,
    // before the task measured its engine and while it sleeps, are woken a group at a time,
    // which costs no call more.
    for cpus in [Cpus::Each, Cpus::One] {
        let lingers = measure(&steady(8), FAST_TASK, cpus, LINGERS, ring::GROUP);
        let d = measure_d(&steady(8), FAST_TASK, cpus, LINGERS);
        assert!(
            d.engine <= lingers.engine && d.task <= lingers.task && d.deferred >= lingers.deferred,
            "{cpus:?}: {lingers:?} then {d:?}"
        );
    }
}

/// A stage unit that posts to a sleeping task and returns its worker wakes it as it leaves
/// (`RingPlane::leave`): the task answers while the engine works elsewhere, and the record is
/// there when the engine comes back. An engine that left without it would find the batch
/// unanswered and wait for its compute.
#[test]
fn a_unit_that_leaves_wakes_its_sleeping_task() {
    let mut leaves = lockstep(8);
    leaves.pop();
    leaves.extend([Op::Post, Op::Leave, Op::Busy(50 * MS), Op::Take, Op::Close]);
    let mut silent = lockstep(8);
    silent.pop();
    silent.extend([Op::Post, Op::Busy(50 * MS), Op::Take, Op::Close]);
    let c = SLOW_POSTS;
    for cpus in [Cpus::Each, Cpus::One] {
        // On the one CPU the task's answer takes the CPU from the engine's other work.
        let lent = if cpus == Cpus::One {
            c.wake + c.compute
        } else {
            0
        };
        let r = run(&leaves, c, cpus, LINGERS, ring::GROUP, true);
        assert_eq!(
            *r.engine.borrow().latency.last().expect("taken"),
            50 * MS + lent,
            "{cpus:?}: answered while the engine worked elsewhere: {:?}",
            r.m
        );
        assert!(r.m.flushes >= 1, "{cpus:?}: {:?}", r.m);
        let r = run(&silent, c, cpus, LINGERS, ring::GROUP, true);
        assert!(
            *r.engine.borrow().latency.last().expect("taken") >= 50 * MS + c.compute,
            "{cpus:?}: without the leave, the engine waits for the compute: {:?}",
            r.m
        );
    }
}

/// A sleeping task owed a post is woken when the engine looks at the plane (a poll) a group of
/// its posting intervals after the post, though the group is not complete and the engine does
/// not block; a look before that wakes no one.
#[test]
fn a_look_past_a_group_s_deadline_wakes_a_sleeping_task() {
    for (busy, overdue) in [(40 * MS, 1), (MS, 0)] {
        let mut ops = lockstep(8);
        ops.pop();
        ops.extend([
            Op::Post,
            Op::Busy(busy),
            Op::Poll,
            Op::Busy(50 * MS),
            Op::Take,
            Op::Close,
        ]);
        for cpus in [Cpus::Each, Cpus::One] {
            let r = run(&ops, SLOW_POSTS, cpus, LINGERS, ring::GROUP, true);
            assert_eq!(
                r.m.overdue, overdue,
                "{cpus:?}, a look {busy} ns after: {:?}",
                r.m
            );
        }
    }
}

/// The engine's half of rule (d) on its own: no clock, or no interval measured yet, and each
/// post wakes a sleeping task; measured, the group's last post wakes it, as does a look or a post
/// at the group's deadline, a flush, a leave, a window given and the close; a lingering task keeps
/// its rule, which a leave or a look leaves alone.
#[test]
fn the_engine_wakes_a_sleeping_task_a_group_at_a_time_by_its_clock() {
    let s = Words::new();
    let g = ring::Geometry::read(&s).expect("header").0;
    let mut e = ring::Engine::format(&s, g, ring::Mode::Protect, 0);
    let t = task(&s);
    let mut cookie = 0;
    let mut post = |e: &mut ring::Engine, now: Option<u64>| {
        cookie += 1;
        let entry = ring::InEntry {
            offset: 0,
            len: 4096,
            cookie,
            rows: 1,
        };
        assert!(e.post(&s, entry));
        e.ring_at(&s, ring::GROUP, now)
    };
    let sleep = || t.arm(&s, Rest::Sleep(FOREVER), |_| false).expect("armed");
    // No clock: each post wakes a sleeping task.
    sleep();
    assert!(post(&mut e, None));
    // A clock, no interval measured yet: the post wakes it.
    sleep();
    assert!(post(&mut e, Some(MS)));
    assert_eq!(e.posting_interval(), None);
    // An awake task: nothing owed.
    assert!(!post(&mut e, Some(2 * MS)));
    assert_eq!((e.owed(), e.posting_interval()), (0, Some(MS)));
    // Measured: the group's first post is deferred, its last wakes the task.
    sleep();
    assert!(!post(&mut e, Some(3 * MS)));
    assert_eq!(e.owed(), 1);
    assert!(
        !e.due(&s, ring::GROUP, 4 * MS),
        "before the group's deadline"
    );
    assert!(post(&mut e, Some(4 * MS)));
    assert_eq!(e.owed(), 0);
    // A look at the group's deadline wakes it: two intervals after the first post owed.
    sleep();
    assert!(!post(&mut e, Some(5 * MS)));
    assert!(!e.due(&s, ring::GROUP, 6 * MS));
    assert!(e.due(&s, ring::GROUP, 7 * MS));
    // A post past the deadline wakes it too, before the group is complete.
    sleep();
    assert!(
        !e.ring_at(&s, 3, Some(8 * MS)),
        "the bell raised again: deferred"
    );
    assert!(e.ring_at(&s, 3, Some(20 * MS)), "past three intervals");
    // The engine leaves, or blocks: a sleeping task owed posts is woken.
    sleep();
    assert!(!post(&mut e, Some(21 * MS)));
    assert!(e.leave(&s));
    assert!(!e.leave(&s), "once");
    sleep();
    assert!(!post(&mut e, Some(22 * MS)));
    assert!(e.flush(&s));
    // A look finds the task awake: nothing owed.
    sleep();
    assert!(!post(&mut e, Some(23 * MS)));
    Task::disarm(&s);
    assert!(!e.due(&s, ring::GROUP, 90 * MS));
    assert_eq!(e.owed(), 0);
    // A lingering task keeps its rule: a leave or a look leaves it to its deadline; a flush
    // wakes it.
    t.arm(&s, Rest::Linger(FOREVER), |_| false).expect("armed");
    assert!(!post(&mut e, Some(24 * MS)));
    assert!(!e.leave(&s));
    assert!(!e.due(&s, ring::GROUP, 90 * MS));
    assert!(e.flush(&s));
    // A window given and the close still wake a sleeping task at once.
    sleep();
    assert!(!post(&mut e, Some(25 * MS)));
    assert!(e.ring_given(&s));
    assert_eq!(e.owed(), 0);
    sleep();
    e.close(&s);
    assert!(e.ring_now(&s));
}

/// The task's half: a wait that finds what it waits for once it is said makes no call and leaves
/// the word clear; the word says how it waits.
#[test]
fn the_task_says_how_it_waits_and_calls_nothing_when_input_came() {
    let s = Words::new();
    let t = task(&s);
    assert_eq!(t.arm(&s, Rest::Linger(5), |_| true), None);
    assert_eq!(s.load(layout::TASK_WAITS), 0, "cleared again");
    assert_eq!(t.rest(&s, Rest::Sleep(5), |_| true), ring::Slept::Ready);
    assert!(!ring::Slept::Ready.called());
    // `Shared::wait`'s default returns at once, as at a deadline.
    assert_eq!(
        t.rest(&s, Rest::Linger(5), |_| false),
        ring::Slept::TimedOut
    );
    assert!(ring::Slept::TimedOut.called() && ring::Slept::Woken.called());
    assert!(!t.sleep(&s, 5, |_| false), "a sleep to its deadline");
    assert!(t.sleep(&s, 5, |_| true));
    assert_eq!(Rest::Sleep(3).word(), layout::WAITS_SLEEP);
    assert_eq!(Rest::Linger(4).word(), layout::WAITS_LINGER);
    assert_eq!(
        (Rest::Sleep(3).deadline(), Rest::Linger(4).deadline()),
        (3, 4)
    );
    // With nothing measured, `wait_input` sleeps; a linger that times out sleeps after it.
    let mut pace = Pace::new(0, 0);
    assert_eq!(t.wait_input(&s, &mut pace, || 0, FOREVER), 1, "one sleep");
    // Four batches posted and released, a task attached after them, and an interval measured.
    s.store(layout::IN_HEAD, 4);
    s.store(layout::IN_TAIL, 4);
    let t = task(&s);
    let mut pace = Pace::new(0, 0);
    pace.saw(4, 4 * MS);
    assert_eq!(
        t.wait_input(&s, &mut pace, || 5 * MS, FOREVER),
        2,
        "a linger, then a sleep"
    );
}

/// The task's measure of its engine's posts: the first interval, a shorter one taken at once, a
/// longer one by half; a head that went back starts again; it lingers for a group of intervals
/// within the bound, and sleeps past it or with nothing measured.
#[test]
fn the_task_measures_its_engine_s_posting_interval() {
    let mut p = Pace::new(10, 1_000);
    assert_eq!(p.interval(), None);
    assert_eq!(
        p.rest(5_000, FOREVER),
        Rest::Sleep(FOREVER),
        "nothing measured"
    );
    p.saw(10, 9_000);
    assert_eq!(p.interval(), None, "no post since: no sample");
    p.saw(14, 41_000);
    assert_eq!(p.interval(), Some(10_000), "40 us for four posts");
    p.saw(15, 43_000);
    assert_eq!(p.interval(), Some(2_000), "a shorter sample, at once");
    p.saw(16, 53_000);
    assert_eq!(p.interval(), Some(6_000), "a longer one, by half");
    assert_eq!(
        p.rest(100_000, FOREVER),
        Rest::Linger(100_000 + ring::GROUP * 6_000)
    );
    assert_eq!(p.rest_in(0, 4, FOREVER), Rest::Linger(24_000));
    // Posts further apart than the bound allows a group: sleep.
    p.saw(17, 53_000 + 3 * ring::LINGER_MAX_NS);
    assert!(p.interval().expect("measured") * ring::GROUP > ring::LINGER_MAX_NS);
    assert_eq!(p.rest(0, 7), Rest::Sleep(7));
    // A head that went back (a ring of another task) starts the measure again.
    p.saw(3, 1);
    assert_eq!(p, Pace::new(3, 1));
}

/// 6.3p's measurements: each candidate rule on each program, a CPU each and one CPU between the
/// engine and the task (`cargo test -p moruna-plane measures -- --nocapture`).
#[test]
fn measures_the_candidate_rules() {
    let rules: [(&str, Waits, u64, bool); 7] = [
        ("before: the task sleeps", Waits::Sleeps, ring::GROUP, false),
        ("an old engine, lingering task", LINGERS, 1, false),
        ("linger, group 2", LINGERS, 2, false),
        ("linger, group 4", Waits::Lingers(4), 4, false),
        ("linger, group 8", Waits::Lingers(8), 8, false),
        (
            "spin 2 intervals, then sleep",
            Waits::Spins(2),
            ring::GROUP,
            false,
        ),
        ("linger 2, rule (d)", LINGERS, ring::GROUP, true),
    ];
    let mut elsewhere = steady(1);
    elsewhere.pop();
    let mut leaves = elsewhere.clone();
    for _ in 0..16 {
        elsewhere.extend([Op::Post, Op::Busy(400 * US), Op::Take]);
        leaves.extend([Op::Post, Op::Leave, Op::Busy(400 * US), Op::Take]);
    }
    elsewhere.push(Op::Close);
    leaves.push(Op::Close);
    let programs: [(&str, Vec<Op>); 5] = [
        ("DS-P steady", steady(8)),
        ("stage, 3 in flight", stage(256)),
        ("lockstep", lockstep(64)),
        ("posted, engine elsewhere", elsewhere),
        ("posted, the unit leaves", leaves),
    ];
    for (costs, c) in [("fast posts", FAST_TASK), ("slow posts", SLOW_POSTS)] {
        for cpus in [Cpus::Each, Cpus::One] {
            for (program, ops) in &programs {
                for (rule, waits, group, clock) in rules {
                    let m = run(ops, c, cpus, waits, group, clock).m;
                    assert_eq!(
                        m.batches,
                        ops.iter().filter(|o| **o == Op::Post).count() as u64
                    );
                    std::println!(
                        "{costs} | {cpus:?} | {program} | {rule} | engine {}.{:02} task {}.{:02} calls a batch | answered {} us, taken {} us after the post | all in {} us | spun {} us",
                        m.engine_per_batch() / 100,
                        m.engine_per_batch() % 100,
                        m.task_per_batch() / 100,
                        m.task_per_batch() % 100,
                        m.answer / US,
                        m.latency / US,
                        m.elapsed / US,
                        m.spun / US,
                    );
                }
            }
        }
    }
}
