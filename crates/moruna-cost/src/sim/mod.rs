//! The discrete-event simulator (SDD 24 f.8; `architecture/adaptive-runtime.md` section 8).
//!
//! It plays a job's units (one task applied to one batch) on a run's workers, memory and disk
//! bandwidth, under a [`Policy`], with predicted costs, and says when the job finishes and what
//! it held on the way. `explain`'s simulated planner runs it once with medians and
//! [`DRAWS`] times with drawn durations ([`Simulator::forecast`]); the adaptive scheduler's
//! lookahead (KD7) runs it inside a decision, so a [`Simulator`] is built once per workload and
//! run again and again without allocating.
//!
//! - **The unit.** Task `t` has `units` batches, and they start in order. Unit `b` of a task with
//!   `n` units covers rows `[b/n, (b+1)/n)` of its input; it needs every unit of each producer
//!   whose rows overlap those (one, when both have as many units), and they must have finished.
//! - **Events.** A unit starts or finishes; a spill or a read-back completes; a checkpoint is due.
//!   Between events every rate is constant, so time jumps to the next completion.
//! - **Contention.** Each running unit demands worker time (`worker_time / duration` per second)
//!   and disk bandwidth (`(read_bytes + spill_bytes) / duration` bytes per second); a spill or a
//!   read-back demands the whole bandwidth. When the summed demand on a resource exceeds its
//!   capacity, everything using it slows by capacity over demand. A kind's [`Scaling`] curve
//!   slows `k` concurrent units of it to `s(k) / k` each and caps `k` where another adds less
//!   than [`MIN_MARGINAL_GAIN`].
//! - **Memory, a hard limit.** A unit starts only when what is held (reservations, buffers in
//!   memory, state) plus its reservation (its 95th-percentile peak, or its output and state when
//!   larger) plus the inputs it must read back fits the budget. Otherwise the simulator spills by
//!   known-future eviction: the buffer in memory, not pinned by an admitted unit, whose next
//!   consumer is furthest away (in that consumer's units still to start, times its unit's
//!   median), first; every byte costs the same to write and read back on one disk, so the reload
//!   cost per byte weighs every buffer alike and the distance decides. Until the room is made the
//!   unit waits, and nothing else is admitted before it. A unit that cannot fit even alone is
//!   [`CostError::Infeasible`], by name.
//! - **Once per process, once per checkpoint.** A task's first unit pays its start-up, and no
//!   other unit of the task is admitted until the start-up ends, so its workers take other
//!   tasks' units meanwhile and no unit of it runs its body before its process is up. Every
//!   checkpoint interval, each task under way with a checkpoint cost owes a checkpoint, which the
//!   next unit of it to start pays at that batch boundary: its time added to the unit's, its bytes
//!   written through the disk beside the unit's own traffic.

mod forecast;
mod policy;
mod scaling;
mod start;

pub use forecast::{DRAWS, Forecast, SEED, Spread, TaskSpread};
pub use policy::{Admission, Candidate, Policy, TaskStatus, View};
pub use scaling::{MIN_MARGINAL_GAIN, Scaling};
pub use start::{Start, TaskStart};

use crate::error::{CostError, Result};
use crate::estimate::UnitCost;
use crate::sample::Rng;

/// One task of a workload: its units and what each costs.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskLoad {
    /// Its name, which errors and reports use.
    pub name: String,
    /// Its units (batches); at least one.
    pub units: u32,
    /// What one unit costs, as the cost model predicts it.
    pub unit: UnitCost,
    /// What running several of its units at once gives.
    pub scaling: Scaling,
    /// Start-up, seconds, paid once by its first unit.
    pub start_seconds: f64,
    /// Seconds a checkpoint of it takes on a worker.
    pub checkpoint_seconds: f64,
    /// Bytes a checkpoint of it writes.
    pub checkpoint_bytes: f64,
    /// Whether it keeps state across units: one unit at a time, its state held until it ends.
    pub stateful: bool,
}

impl TaskLoad {
    /// A stateless task of `units` units costing `unit` each, scaling linearly, with no start-up
    /// and no checkpoint.
    pub fn new(name: impl Into<String>, units: u32, unit: UnitCost) -> TaskLoad {
        TaskLoad {
            name: name.into(),
            units,
            unit,
            scaling: Scaling::linear(),
            start_seconds: 0.0,
            checkpoint_seconds: 0.0,
            checkpoint_bytes: 0.0,
            stateful: false,
        }
    }
}

/// What the simulator plays: tasks and the edges between them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Workload {
    /// The tasks. Ties in the admission rule go to the later one, so list them so every edge goes
    /// forward.
    pub tasks: Vec<TaskLoad>,
    /// `(producer, consumer)` by place in `tasks`.
    pub edges: Vec<(usize, usize)>,
    /// Seconds between checkpoints; `None`: no checkpoint.
    pub checkpoint_interval_seconds: Option<f64>,
}

/// What the run is given.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Machine {
    /// Workers (cores); at least one.
    pub workers: u32,
    /// The memory budget, bytes.
    pub memory_bytes: f64,
    /// The disk's bandwidth, bytes per second, shared by reads, spills and read-backs.
    pub disk_bytes_per_second: f64,
}

/// One task in one simulation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TaskRun {
    /// When its first unit started, seconds.
    pub start: f64,
    /// When its last unit finished.
    pub end: f64,
    /// Worker time its units held.
    pub worker_seconds: f64,
    /// The most memory it held at once: reservations, its outputs in memory, its state.
    pub peak_memory: f64,
    /// Bytes of its outputs spilled.
    pub spill_bytes: f64,
    /// The most of its units admitted at once.
    pub max_running: u32,
}

/// One simulation's result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Run {
    /// When the last unit finished, seconds from the start.
    pub finish: f64,
    /// The most memory held at once.
    pub peak_memory: f64,
    /// Memory held times how long, byte-seconds.
    pub memory_byte_seconds: f64,
    /// Worker time held by units.
    pub worker_seconds: f64,
    /// Bytes moved on disk: units' reads and spills, the simulator's spills and read-backs,
    /// checkpoints.
    pub disk_bytes: f64,
    /// Bytes the simulator spilled to make room.
    pub spill_bytes: f64,
    /// Bytes it read back.
    pub reload_bytes: f64,
    /// Worker time idle while a unit was ready (waiting for memory, or for the policy): waste,
    /// as `architecture/adaptive-runtime.md` section 0 counts it.
    pub idle_worker_seconds: f64,
    /// Events processed.
    pub events: u64,
    /// Every task, in the workload's order.
    pub tasks: Vec<TaskRun>,
    /// The tasks on the path that set the finish, in order: from the last unit to finish back
    /// through what each unit waited for last (its input, or the unit whose end freed what it
    /// needed).
    pub critical_path: Vec<usize>,
}

const NONE: u32 = u32::MAX;
/// A completion within this share of an item's work of the earliest is the same event.
const TOLERANCE: f64 = 1e-9;
/// The floor of a duration, so a zero median has a logarithm (a nanosecond).
const FLOOR: f64 = 1e-9;

/// A task's figures, as the simulator reads them.
#[derive(Clone, Copy, Debug)]
struct Spec {
    units: u32,
    base: u32,
    d50: f64,
    sigma: f64,
    cpu: f64,
    disk: f64,
    need: f64,
    out: f64,
    state: f64,
    start: f64,
    ckpt_s: f64,
    ckpt_b: f64,
    cap: u32,
    scaling: Scaling,
    stateful: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Loc {
    Absent,
    Memory,
    Spilling,
    Disk,
    Loading,
}

/// A unit's output, while a consumer still needs it.
#[derive(Clone, Copy, Debug)]
struct Buffer {
    size: f64,
    uses: u32,
    pins: u32,
    task: u32,
    slot: u32,
    loc: Loc,
    wanted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    Unit { task: u32, unit: u32 },
    Spill { buffer: u32 },
    Reload { buffer: u32 },
}

/// Something under way: a unit, a transfer or a checkpoint.
#[derive(Clone, Copy, Debug)]
struct Item {
    work: Work,
    /// Work left: seconds at full speed for a unit, bytes for a transfer.
    left: f64,
    /// Seconds of its task's start-up still to run before a unit's body starts: wall time,
    /// which no contention slows.
    startup: f64,
    nominal: f64,
    cpu: f64,
    disk: f64,
    need: f64,
    waiting: bool,
    rate: f64,
    since: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Track {
    held: f64,
    peak: f64,
    spill: f64,
    first: f64,
    last: f64,
    worker_s: f64,
    state: f64,
    prefix: u32,
    computing: u32,
    max_running: u32,
    started: bool,
    /// Its first unit is in its start-up: no other unit of it is admitted until it ends.
    starting: bool,
    owes_checkpoint: bool,
}

/// The simulator of one workload on one machine, reusable across runs.
pub struct Simulator {
    machine: Machine,
    names: Vec<String>,
    specs: Vec<Spec>,
    producers_at: Vec<u32>,
    producers: Vec<u32>,
    consumers_at: Vec<u32>,
    consumers: Vec<u32>,
    uses: Vec<u32>,
    interval: Option<f64>,
    total: u32,

    now: f64,
    held: f64,
    free: u32,
    claim: Option<u32>,
    idle_ready: u32,
    finished: u32,
    last_finished: u32,
    last_finished_at: f64,
    next_checkpoint: f64,
    status: Vec<TaskStatus>,
    track: Vec<Track>,
    buffers: Vec<Buffer>,
    done: Vec<bool>,
    done_at: Vec<f64>,
    cause: Vec<u32>,
    resident: Vec<u32>,
    items: Vec<Item>,
    candidates: Vec<Candidate>,
    run: Run,
}

/// The first unit of a producer of `np` units that consumer unit `b` of `nc` units needs.
fn first_input(b: u32, np: u32, nc: u32) -> u32 {
    (u64::from(b) * u64::from(np) / u64::from(nc)) as u32
}

/// The last unit of a producer of `np` units that consumer unit `b` of `nc` units needs.
fn last_input(b: u32, np: u32, nc: u32) -> u32 {
    ((u64::from(b) + 1) * u64::from(np)).div_ceil(u64::from(nc)) as u32 - 1
}

/// The first unit of a consumer of `nc` units that needs producer unit `k` of `np` units.
fn first_user(k: u32, np: u32, nc: u32) -> u32 {
    (u64::from(k) * u64::from(nc) / u64::from(np)) as u32
}

/// The last unit of a consumer of `nc` units that needs producer unit `k` of `np` units.
fn last_user(k: u32, np: u32, nc: u32) -> u32 {
    let last = ((u64::from(k) + 1) * u64::from(nc)).div_ceil(u64::from(np)) - 1;
    last.min(u64::from(nc) - 1) as u32
}

fn refuse(reason: impl Into<String>) -> CostError {
    CostError::Simulation(reason.into())
}

/// A machine the simulator can play: a worker, a positive memory budget and disk bandwidth.
fn check_machine(machine: &Machine) -> Result<()> {
    if machine.workers == 0 {
        return Err(refuse("the machine has no worker"));
    }
    if !(machine.memory_bytes.is_finite() && machine.memory_bytes > 0.0) {
        return Err(refuse(format!(
            "the memory budget {} is not a positive amount",
            machine.memory_bytes
        )));
    }
    if !(machine.disk_bytes_per_second.is_finite() && machine.disk_bytes_per_second > 0.0) {
        return Err(refuse(format!(
            "the disk bandwidth {} is not a positive amount",
            machine.disk_bytes_per_second
        )));
    }
    Ok(())
}

/// How many of a task's units may run at once on `workers` workers.
fn cap(stateful: bool, scaling: Scaling, workers: u32) -> u32 {
    if stateful { 1 } else { scaling.cap(workers) }
}

/// Nothing may need more than the budget at once: each task's reservation, the most inputs one
/// of its units reads, and its state when all but its last unit have grown it. `Infeasible`,
/// naming the task, otherwise.
fn fits(
    specs: &[Spec],
    producers_at: &[u32],
    producers: &[u32],
    names: &[String],
    memory_bytes: f64,
) -> Result<()> {
    for (c, spec) in specs.iter().enumerate() {
        let mut inputs = 0.0;
        for &p in &producers[producers_at[c] as usize..producers_at[c + 1] as usize] {
            let pspec = &specs[p as usize];
            let mut most = 0u32;
            for b in 0..spec.units {
                let k = last_input(b, pspec.units, spec.units)
                    - first_input(b, pspec.units, spec.units)
                    + 1;
                most = most.max(k);
            }
            inputs += f64::from(most) * pspec.out;
        }
        let state = spec.state * f64::from(spec.units - 1);
        let needs = spec.need + inputs + state;
        if needs > memory_bytes * (1.0 + TOLERANCE) {
            return Err(CostError::Infeasible {
                task: names[c].clone(),
                needs,
                reason: format!(
                    "the run's memory is {} bytes (its reservation {}, its inputs {}, its state {})",
                    memory_bytes, spec.need, inputs, state
                ),
            });
        }
    }
    Ok(())
}

impl Simulator {
    /// The simulator of `workload` on `machine`. Refused when the workload is not a DAG of tasks
    /// with units and usable costs, when the machine has no worker, memory or bandwidth, and
    /// ([`CostError::Infeasible`], naming the task) when a unit of some task needs more memory
    /// at once than the budget: its reservation, its inputs and its task's state.
    pub fn new(workload: &Workload, machine: Machine) -> Result<Simulator> {
        check_machine(&machine)?;
        if let Some(i) = workload.checkpoint_interval_seconds
            && !(i.is_finite() && i > 0.0)
        {
            return Err(refuse(format!(
                "the checkpoint interval {i} is not a positive time"
            )));
        }
        let n = workload.tasks.len();
        let mut producers: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut consumers: Vec<Vec<u32>> = vec![Vec::new(); n];
        for &(from, to) in &workload.edges {
            if from >= n || to >= n {
                return Err(refuse(format!(
                    "the edge ({from}, {to}) names a task the workload of {n} does not have"
                )));
            }
            if from == to {
                return Err(refuse(format!(
                    "task `{}` takes its own output",
                    workload.tasks[from].name
                )));
            }
            if !producers[to].contains(&(from as u32)) {
                producers[to].push(from as u32);
                consumers[from].push(to as u32);
            }
        }
        // A DAG: Kahn's order reaches every task.
        let mut indegree: Vec<usize> = producers.iter().map(Vec::len).collect();
        let mut queue: Vec<usize> = (0..n).filter(|&t| indegree[t] == 0).collect();
        let mut reached = 0;
        while let Some(t) = queue.pop() {
            reached += 1;
            for &c in &consumers[t] {
                indegree[c as usize] -= 1;
                if indegree[c as usize] == 0 {
                    queue.push(c as usize);
                }
            }
        }
        if reached != n {
            return Err(refuse("the workload's edges go round a cycle"));
        }

        let mut specs = Vec::with_capacity(n);
        let mut base = 0u32;
        for task in &workload.tasks {
            if task.units == 0 {
                return Err(refuse(format!("task `{}` has no unit", task.name)));
            }
            task.unit
                .check()
                .map_err(|why| refuse(format!("task `{}`: {why}", task.name)))?;
            for (what, v) in [
                ("start-up", task.start_seconds),
                ("checkpoint time", task.checkpoint_seconds),
                ("checkpoint bytes", task.checkpoint_bytes),
            ] {
                if !(v.is_finite() && v >= 0.0) {
                    return Err(refuse(format!(
                        "task `{}`: its {what} {v} is not a finite amount",
                        task.name
                    )));
                }
            }
            let u = &task.unit;
            let d50 = u.duration.p50;
            let has_consumers = !consumers[specs.len()].is_empty();
            let out = if has_consumers { u.bytes_out.p50 } else { 0.0 };
            let state = u.state_growth.p50;
            let cap = cap(task.stateful, task.scaling, machine.workers);
            specs.push(Spec {
                units: task.units,
                base,
                d50,
                sigma: u.duration.sigma(),
                cpu: if d50 > 0.0 {
                    u.worker_time.p50 / d50
                } else {
                    0.0
                },
                disk: u.read_bytes.p50 + u.spill_bytes.p50,
                need: u.peak_memory.p95.max(out + state),
                out,
                state,
                start: task.start_seconds,
                ckpt_s: task.checkpoint_seconds,
                ckpt_b: task.checkpoint_bytes,
                cap,
                scaling: task.scaling,
                stateful: task.stateful,
            });
            base = base.checked_add(task.units).ok_or_else(|| {
                refuse(format!("the workload has more than {} units", u32::MAX - 1))
            })?;
        }
        if base == NONE {
            return Err(refuse(format!(
                "the workload has more than {} units",
                u32::MAX - 1
            )));
        }

        // Each buffer's uses: every consumer unit whose rows overlap it.
        let mut uses = vec![0u32; base as usize];
        for (p, spec) in specs.iter().enumerate() {
            for &c in &consumers[p] {
                let nc = specs[c as usize].units;
                for k in 0..spec.units {
                    let span = last_user(k, spec.units, nc) - first_user(k, spec.units, nc) + 1;
                    uses[(spec.base + k) as usize] += span;
                }
            }
        }

        let flatten = |lists: Vec<Vec<u32>>| {
            let mut at = Vec::with_capacity(lists.len() + 1);
            let mut flat = Vec::new();
            at.push(0u32);
            for list in lists {
                flat.extend(list);
                at.push(flat.len() as u32);
            }
            (at, flat)
        };
        let (producers_at, producers) = flatten(producers);
        let (consumers_at, consumers) = flatten(consumers);
        let names: Vec<String> = workload.tasks.iter().map(|t| t.name.clone()).collect();
        fits(
            &specs,
            &producers_at,
            &producers,
            &names,
            machine.memory_bytes,
        )?;
        let units = base as usize;
        let buffer = Buffer {
            size: 0.0,
            uses: 0,
            pins: 0,
            task: 0,
            slot: NONE,
            loc: Loc::Absent,
            wanted: false,
        };
        let mut buffers = vec![buffer; units];
        for (t, spec) in specs.iter().enumerate() {
            for k in 0..spec.units {
                buffers[(spec.base + k) as usize].task = t as u32;
            }
        }
        Ok(Simulator {
            machine,
            names,
            specs,
            producers_at,
            producers,
            consumers_at,
            consumers,
            uses,
            interval: workload.checkpoint_interval_seconds,
            total: base,
            now: 0.0,
            held: 0.0,
            free: machine.workers,
            claim: None,
            idle_ready: 0,
            finished: 0,
            last_finished: NONE,
            last_finished_at: 0.0,
            next_checkpoint: f64::INFINITY,
            status: vec![TaskStatus::default(); n],
            track: vec![Track::default(); n],
            buffers,
            done: vec![false; units],
            done_at: vec![0.0; units],
            cause: vec![NONE; units],
            resident: Vec::with_capacity(units.min(1 << 16)),
            items: Vec::with_capacity(machine.workers as usize * 4),
            candidates: Vec::with_capacity(n),
            run: Run {
                tasks: vec![TaskRun::default(); n],
                critical_path: Vec::with_capacity(n),
                ..Run::default()
            },
        })
    }

    /// The machine it simulates.
    pub fn machine(&self) -> Machine {
        self.machine
    }

    /// The tasks' names, in the workload's order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// One simulation with every unit at its median duration.
    pub fn run(&mut self, policy: &mut dyn Policy) -> Result<&Run> {
        self.simulate(policy, None, None)?;
        Ok(&self.run)
    }

    /// One simulation with every unit's duration drawn from its estimate's spread
    /// ([`crate::Estimate::quantile`]), the draws taken from `rng` in the order units start.
    pub fn run_sampled(&mut self, policy: &mut dyn Policy, rng: &mut Rng) -> Result<&Run> {
        self.simulate(policy, Some(rng), None)?;
        Ok(&self.run)
    }

    /// One simulation at medians from where a live run is (SDD 24 f.13, CM-I23): `start` says,
    /// per task, which units have finished, which are under way and how much work each has left,
    /// and which finished outputs a consumer still needs are on disk. Time is counted from the
    /// snapshot, so the run's `finish` is the time still to go. What a started task's first unit
    /// paid at start-up is not paid again. Refused, saying what is wrong, when the start does not
    /// fit the workload: a task count that differs, more units started than a task has, a unit
    /// started before its inputs finished, more units under way than workers, a running unit's
    /// input on disk, or a unit on disk that has not finished.
    pub fn run_from(&mut self, start: &Start, policy: &mut dyn Policy) -> Result<&Run> {
        self.simulate(policy, None, Some(start))?;
        Ok(&self.run)
    }

    /// The run was resized (SDD 24 f.13, CM-I24): later simulations play `machine`, each task's
    /// cap follows its workers, and a task that no longer fits is `Infeasible` by name, the
    /// simulator unchanged. Nothing is allocated.
    pub fn resize(&mut self, machine: Machine) -> Result<()> {
        check_machine(&machine)?;
        // What fits depends on memory alone, not on the caps, so it is judged before anything
        // changes.
        fits(
            &self.specs,
            &self.producers_at,
            &self.producers,
            &self.names,
            machine.memory_bytes,
        )?;
        for spec in &mut self.specs {
            spec.cap = cap(spec.stateful, spec.scaling, machine.workers);
        }
        self.machine = machine;
        Ok(())
    }

    fn reset(&mut self) {
        self.now = 0.0;
        self.held = 0.0;
        self.free = self.machine.workers;
        self.claim = None;
        self.idle_ready = 0;
        self.finished = 0;
        self.last_finished = NONE;
        self.last_finished_at = 0.0;
        self.next_checkpoint = self.interval.unwrap_or(f64::INFINITY);
        for (status, spec) in self.status.iter_mut().zip(&self.specs) {
            *status = TaskStatus {
                units: spec.units,
                median_seconds: spec.d50,
                ..TaskStatus::default()
            };
        }
        self.track.fill(Track::default());
        for b in &mut self.buffers {
            b.size = 0.0;
            b.uses = 0;
            b.pins = 0;
            b.slot = NONE;
            b.loc = Loc::Absent;
            b.wanted = false;
        }
        self.done.fill(false);
        self.cause.fill(NONE);
        self.resident.clear();
        self.items.clear();
        self.candidates.clear();
        let tasks = std::mem::take(&mut self.run.tasks);
        let mut path = std::mem::take(&mut self.run.critical_path);
        path.clear();
        self.run = Run {
            tasks,
            critical_path: path,
            ..Run::default()
        };
        self.run.tasks.fill(TaskRun::default());
    }

    fn simulate(
        &mut self,
        policy: &mut dyn Policy,
        mut rng: Option<&mut Rng>,
        start: Option<&Start>,
    ) -> Result<()> {
        self.reset();
        if let Some(start) = start {
            self.apply(start)?;
        }
        policy.reset();
        loop {
            self.admit(policy, &mut rng)?;
            if self.finished == self.total {
                break;
            }
            if self.items.is_empty() {
                return Err(refuse(format!(
                    "at {} s nothing runs and policy `{}` starts nothing of the {} units left",
                    self.now,
                    policy.name(),
                    self.total - self.finished
                )));
            }
            self.rates();
            let mut dt = f64::INFINITY;
            let mut first = None;
            for (i, item) in self.items.iter().enumerate() {
                let t = if item.startup > 0.0 {
                    item.startup
                } else if item.rate > 0.0 {
                    item.left / item.rate
                } else {
                    continue;
                };
                if t < dt {
                    dt = t;
                    first = Some((i, item.startup > 0.0));
                }
            }
            if !dt.is_finite() {
                return Err(refuse(format!(
                    "at {} s every unit under way waits and nothing moves",
                    self.now
                )));
            }
            let to_checkpoint = self.next_checkpoint - self.now;
            let timer = to_checkpoint <= dt;
            if timer {
                dt = to_checkpoint.max(0.0);
                first = None;
            }
            self.run.memory_byte_seconds += self.held * dt;
            self.run.idle_worker_seconds += f64::from(self.idle_ready) * dt;
            self.now += dt;
            let near = TOLERANCE * self.now.max(1.0);
            for item in &mut self.items {
                if item.startup > 0.0 {
                    item.startup -= dt;
                } else {
                    item.left -= item.rate * dt;
                }
            }
            match first {
                Some((i, true)) => self.items[i].startup = 0.0,
                Some((i, false)) => self.items[i].left = 0.0,
                None => {}
            }
            for item in &mut self.items {
                if let Work::Unit { task, .. } = item.work
                    && self.track[task as usize].starting
                    && item.startup <= near
                {
                    item.startup = 0.0;
                    self.track[task as usize].starting = false;
                }
            }
            let mut i = 0;
            while i < self.items.len() {
                let item = self.items[i];
                let complete =
                    item.rate > 0.0 && item.startup <= 0.0 && item.left <= TOLERANCE * item.nominal;
                if complete {
                    self.items.swap_remove(i);
                    self.complete(item);
                } else {
                    i += 1;
                }
            }
            if timer {
                self.due_checkpoints();
            }
            self.run.events += 1;
        }
        self.finish();
        Ok(())
    }

    /// Put the simulator where `start` says the run is (`run_from`), after a reset.
    fn apply(&mut self, start: &Start) -> Result<()> {
        let n = self.specs.len();
        if start.tasks.len() != n {
            return Err(refuse(format!(
                "the start describes {} tasks and the workload has {n}",
                start.tasks.len()
            )));
        }
        let mut under_way = 0u64;
        for (t, s) in start.tasks.iter().enumerate() {
            let units = self.specs[t].units;
            let started = u64::from(s.done) + s.running.len() as u64;
            if started > u64::from(units) {
                return Err(refuse(format!(
                    "task `{}` has {units} units and the start has {} finished and {} under way",
                    self.names[t],
                    s.done,
                    s.running.len()
                )));
            }
            if let Some(left) = s.running.iter().find(|l| !(l.is_finite() && **l >= 0.0)) {
                return Err(refuse(format!(
                    "a unit of task `{}` under way has {left} seconds left, which is not a time",
                    self.names[t]
                )));
            }
            if let Some(k) = s.on_disk.iter().find(|&&k| k >= s.done) {
                return Err(refuse(format!(
                    "the start puts unit {k} of task `{}` on disk, and it has not finished",
                    self.names[t]
                )));
            }
            under_way += s.running.len() as u64;
        }
        if under_way > u64::from(self.machine.workers) {
            return Err(refuse(format!(
                "the start has {under_way} units under way on {} workers",
                self.machine.workers
            )));
        }
        let started = |t: usize| start.tasks[t].done + start.tasks[t].running.len() as u32;
        for c in 0..n {
            if started(c) == 0 {
                continue;
            }
            let last = started(c) - 1;
            for at in self.producers_at[c]..self.producers_at[c + 1] {
                let p = self.producers[at as usize] as usize;
                let need = last_input(last, self.specs[p].units, self.specs[c].units);
                if start.tasks[p].done <= need {
                    return Err(refuse(format!(
                        "unit {last} of task `{}` has started, and its input, unit {need} of \
                         `{}`, has not finished",
                        self.names[c], self.names[p]
                    )));
                }
            }
        }
        for (t, s) in start.tasks.iter().enumerate() {
            let spec = self.specs[t];
            for b in 0..s.done {
                let gid = (spec.base + b) as usize;
                self.done[gid] = true;
                self.done_at[gid] = 0.0;
            }
            let running = s.running.len() as u32;
            self.finished += s.done;
            let status = &mut self.status[t];
            status.done = s.done;
            status.next_unit = s.done + running;
            status.running = running;
            let track = &mut self.track[t];
            track.prefix = s.done;
            track.started = s.done + running > 0;
            track.max_running = running;
            if spec.state > 0.0 && s.done > 0 && s.done < spec.units {
                let state = spec.state * f64::from(s.done);
                self.track[t].state = state;
                self.add_held(t, state);
            }
        }
        // Every finished output some consumer has not finished with, where the start says it is.
        for p in 0..n {
            let pspec = self.specs[p];
            if pspec.out <= 0.0 {
                continue;
            }
            for k in 0..start.tasks[p].done {
                let (mut uses, mut pins) = (0u32, 0u32);
                for at in self.consumers_at[p]..self.consumers_at[p + 1] {
                    let c = self.consumers[at as usize] as usize;
                    let nc = self.specs[c].units;
                    let (first, last) = (
                        first_user(k, pspec.units, nc),
                        last_user(k, pspec.units, nc),
                    );
                    let done = start.tasks[c].done;
                    let begun = started(c);
                    uses += (last + 1).saturating_sub(first.max(done));
                    pins += begun.min(last + 1).saturating_sub(first.max(done));
                }
                if uses == 0 {
                    continue;
                }
                let gid = pspec.base + k;
                let on_disk = start.tasks[p].on_disk.contains(&k);
                if on_disk && pins > 0 {
                    return Err(refuse(format!(
                        "the start puts unit {k} of task `{}` on disk, and a unit under way \
                         holds it",
                        self.names[p]
                    )));
                }
                let buffer = &mut self.buffers[gid as usize];
                buffer.size = pspec.out;
                buffer.uses = uses;
                buffer.pins = pins;
                self.status[p].output_bytes += pspec.out;
                if on_disk {
                    self.buffers[gid as usize].loc = Loc::Disk;
                } else {
                    self.resident_push(gid);
                    self.add_held(p, pspec.out);
                }
            }
        }
        // The units under way, each with the work it has left, on a worker of its own.
        for (t, s) in start.tasks.iter().enumerate() {
            let spec = self.specs[t];
            for (i, &left) in s.running.iter().enumerate() {
                self.add_held(t, spec.need);
                self.free -= 1;
                self.items.push(Item {
                    work: Work::Unit {
                        task: t as u32,
                        unit: s.done + i as u32,
                    },
                    left,
                    nominal: left.max(FLOOR),
                    cpu: spec.cpu,
                    disk: if spec.d50 > 0.0 {
                        spec.disk / spec.d50
                    } else {
                        0.0
                    },
                    need: spec.need,
                    waiting: false,
                    startup: 0.0,
                    rate: 0.0,
                    since: 0.0,
                });
            }
        }
        Ok(())
    }

    fn admit(&mut self, policy: &mut dyn Policy, rng: &mut Option<&mut Rng>) -> Result<()> {
        self.idle_ready = 0;
        let mut fresh = false;
        while self.free > 0 {
            if !fresh {
                self.gather();
                fresh = true;
            }
            if self.candidates.is_empty() {
                break;
            }
            let claimed = self
                .claim
                .and_then(|t| self.candidates.iter().position(|c| c.task == t as usize));
            let pick = match claimed {
                Some(i) => i,
                None => {
                    let view = View {
                        now: self.now,
                        free_workers: self.free,
                        held_bytes: self.held,
                        budget_bytes: self.machine.memory_bytes,
                        tasks: &self.status,
                        candidates: &self.candidates,
                    };
                    match policy.choose(&view) {
                        Some(i) if i < self.candidates.len() => i,
                        Some(i) => {
                            return Err(refuse(format!(
                                "policy `{}` chose candidate {i} of {}",
                                policy.name(),
                                self.candidates.len()
                            )));
                        }
                        None => {
                            self.idle_ready = self.free;
                            break;
                        }
                    }
                }
            };
            let candidate = self.candidates[pick];
            let before = self.items.len();
            if !self.try_start(candidate, rng)? {
                self.idle_ready = self.free;
                break;
            }
            if self.items.len() > before + 1 {
                // It started read-backs, which change what other candidates must read back.
                fresh = false;
            } else {
                self.refresh(pick);
            }
        }
        Ok(())
    }

    /// The candidates: every task whose next unit's inputs have finished and whose scaling
    /// allows another unit at once.
    fn gather(&mut self) {
        self.candidates.clear();
        for c in 0..self.specs.len() {
            if let Some(candidate) = self.candidate(c) {
                self.candidates.push(candidate);
            }
        }
    }

    /// After candidate `i` started with no read-back: its task's next unit in its place, and
    /// every candidate's fit against what is held now. Nothing else changes when a unit starts.
    fn refresh(&mut self, i: usize) {
        match self.candidate(self.candidates[i].task) {
            Some(next) => self.candidates[i] = next,
            None => {
                self.candidates.remove(i);
            }
        }
        let room = self.machine.memory_bytes * (1.0 + TOLERANCE);
        for c in &mut self.candidates {
            c.fits = self.held + c.need_bytes + c.reload_bytes <= room;
        }
    }

    /// Task `c`'s next unit, when its inputs have finished and its scaling allows another unit.
    fn candidate(&self, c: usize) -> Option<Candidate> {
        let spec = self.specs[c];
        let status = self.status[c];
        if status.next_unit >= spec.units || status.running >= spec.cap || self.track[c].starting {
            return None;
        }
        let b = status.next_unit;
        let mut reload = 0.0;
        for at in self.producers_at[c]..self.producers_at[c + 1] {
            let p = self.producers[at as usize] as usize;
            let pspec = self.specs[p];
            let last = last_input(b, pspec.units, spec.units);
            if self.track[p].prefix <= last {
                return None;
            }
            for k in first_input(b, pspec.units, spec.units)..=last {
                let buffer = &self.buffers[(pspec.base + k) as usize];
                if matches!(buffer.loc, Loc::Disk | Loc::Spilling) {
                    reload += buffer.size;
                }
            }
        }
        Some(Candidate {
            task: c,
            unit: b,
            need_bytes: spec.need,
            reload_bytes: reload,
            fits: self.held + spec.need + reload <= self.machine.memory_bytes * (1.0 + TOLERANCE),
        })
    }

    /// Start `c` if it fits; else make room by spilling and wait for it. Whether it started.
    fn try_start(&mut self, c: Candidate, rng: &mut Option<&mut Rng>) -> Result<bool> {
        if c.fits {
            self.claim = None;
            self.start_unit(c, rng);
            return Ok(true);
        }
        let mut deficit = self.held + c.need_bytes + c.reload_bytes - self.machine.memory_bytes;
        for item in &self.items {
            if let Work::Spill { buffer } = item.work
                && !self.buffers[buffer as usize].wanted
            {
                deficit -= self.buffers[buffer as usize].size;
            }
        }
        while deficit > 0.0 {
            let Some(victim) = self.victim(c) else {
                break;
            };
            deficit -= self.buffers[victim as usize].size;
            self.start_spill(victim);
        }
        self.claim = Some(c.task as u32);
        if deficit <= 0.0 || !self.items.is_empty() {
            return Ok(false);
        }
        Err(CostError::Infeasible {
            task: self.names[c.task].clone(),
            needs: c.need_bytes + c.reload_bytes,
            reason: format!(
                "with nothing under way and nothing left to spill, {} bytes of the run's {} are \
                 held by state and pinned inputs",
                self.held, self.machine.memory_bytes
            ),
        })
    }

    /// Whether buffer `gid` is an input of candidate `c`.
    fn is_input(&self, gid: u32, c: Candidate) -> bool {
        let buffer = &self.buffers[gid as usize];
        let p = buffer.task as usize;
        let at = self.producers_at[c.task] as usize..self.producers_at[c.task + 1] as usize;
        if !self.producers[at].contains(&(p as u32)) {
            return false;
        }
        let (np, nc) = (self.specs[p].units, self.specs[c.task].units);
        let k = gid - self.specs[p].base;
        (first_input(c.unit, np, nc)..=last_input(c.unit, np, nc)).contains(&k)
    }

    /// Seconds until buffer `gid` is next needed: over its consumers still to use it, the units
    /// they start before the first that needs it, times their unit's median.
    fn next_use(&self, gid: u32) -> f64 {
        let p = self.buffers[gid as usize].task as usize;
        let np = self.specs[p].units;
        let k = gid - self.specs[p].base;
        let mut soonest = f64::INFINITY;
        for at in self.consumers_at[p]..self.consumers_at[p + 1] {
            let c = self.consumers[at as usize] as usize;
            let nc = self.specs[c].units;
            let next = self.status[c].next_unit;
            if next > last_user(k, np, nc) {
                continue;
            }
            let units = first_user(k, np, nc).max(next) - next;
            soonest = soonest.min(f64::from(units) * self.specs[c].d50);
        }
        soonest
    }

    /// The buffer to spill for `c`: in memory, pinned by no admitted unit, not `c`'s input,
    /// needed furthest away; ties to the larger, then the later.
    fn victim(&self, c: Candidate) -> Option<u32> {
        let mut best: Option<(u32, f64, f64)> = None;
        for &gid in &self.resident {
            let buffer = &self.buffers[gid as usize];
            if buffer.pins > 0 || self.is_input(gid, c) {
                continue;
            }
            let when = self.next_use(gid);
            let better = match best {
                None => true,
                Some((g, w, size)) => {
                    when > w
                        || (when == w && (buffer.size > size || (buffer.size == size && gid > g)))
                }
            };
            if better {
                best = Some((gid, when, buffer.size));
            }
        }
        best.map(|(gid, _, _)| gid)
    }

    fn add_held(&mut self, task: usize, delta: f64) {
        let track = &mut self.track[task];
        track.held += delta;
        if track.held > track.peak {
            track.peak = track.held;
        }
        self.held += delta;
        if self.held > self.run.peak_memory {
            self.run.peak_memory = self.held;
        }
    }

    fn resident_push(&mut self, gid: u32) {
        self.buffers[gid as usize].slot = self.resident.len() as u32;
        self.buffers[gid as usize].loc = Loc::Memory;
        self.resident.push(gid);
    }

    fn resident_remove(&mut self, gid: u32) {
        let slot = self.buffers[gid as usize].slot as usize;
        self.resident.swap_remove(slot);
        if let Some(&moved) = self.resident.get(slot) {
            self.buffers[moved as usize].slot = slot as u32;
        }
        self.buffers[gid as usize].slot = NONE;
    }

    fn start_spill(&mut self, gid: u32) {
        self.resident_remove(gid);
        let size = self.buffers[gid as usize].size;
        self.buffers[gid as usize].loc = Loc::Spilling;
        self.items.push(Item {
            work: Work::Spill { buffer: gid },
            left: size,
            nominal: size,
            cpu: 0.0,
            disk: self.machine.disk_bytes_per_second,
            need: 0.0,
            waiting: false,
            startup: 0.0,
            rate: 0.0,
            since: self.now,
        });
    }

    fn start_reload(&mut self, gid: u32) {
        let buffer = self.buffers[gid as usize];
        self.buffers[gid as usize].loc = Loc::Loading;
        self.add_held(buffer.task as usize, buffer.size);
        self.items.push(Item {
            work: Work::Reload { buffer: gid },
            left: buffer.size,
            nominal: buffer.size,
            cpu: 0.0,
            disk: self.machine.disk_bytes_per_second,
            need: 0.0,
            waiting: false,
            startup: 0.0,
            rate: 0.0,
            since: self.now,
        });
    }

    fn start_unit(&mut self, c: Candidate, rng: &mut Option<&mut Rng>) {
        let t = c.task;
        let spec = self.specs[t];
        let b = c.unit;
        let duration = match rng {
            Some(rng) if spec.sigma > 0.0 => {
                ((spec.d50 + FLOOR) * (spec.sigma * rng.normal()).exp() - FLOOR).max(0.0)
            }
            _ => spec.d50,
        };
        let start = if self.track[t].started {
            0.0
        } else {
            spec.start
        };
        let bandwidth = self.machine.disk_bytes_per_second;
        let checkpoint = if self.track[t].owes_checkpoint {
            self.track[t].owes_checkpoint = false;
            self.run.disk_bytes += spec.ckpt_b;
            (spec.ckpt_s.max(spec.ckpt_b / bandwidth), spec.ckpt_b)
        } else {
            (0.0, 0.0)
        };
        let nominal = duration + checkpoint.0;
        self.add_held(t, spec.need);
        let mut waiting = false;
        let mut data: Option<(u32, f64)> = None;
        for at in self.producers_at[t]..self.producers_at[t + 1] {
            let p = self.producers[at as usize] as usize;
            let pspec = self.specs[p];
            for k in
                first_input(b, pspec.units, spec.units)..=last_input(b, pspec.units, spec.units)
            {
                let gid = pspec.base + k;
                let at = self.done_at[gid as usize];
                if data.is_none_or(|(_, when)| at > when) {
                    data = Some((gid, at));
                }
                if pspec.out <= 0.0 {
                    continue;
                }
                self.buffers[gid as usize].pins += 1;
                match self.buffers[gid as usize].loc {
                    Loc::Disk => {
                        self.start_reload(gid);
                        waiting = true;
                    }
                    Loc::Spilling => {
                        self.buffers[gid as usize].wanted = true;
                        waiting = true;
                    }
                    Loc::Loading => waiting = true,
                    Loc::Memory | Loc::Absent => {}
                }
            }
        }
        let gid = spec.base + b;
        let near = TOLERANCE * self.now.max(1.0);
        self.cause[gid as usize] = match data {
            Some((unit, at)) if at >= self.now - near => unit,
            _ if self.last_finished != NONE && self.last_finished_at >= self.now - near => {
                self.last_finished
            }
            Some((unit, _)) => unit,
            None => NONE,
        };
        let status = &mut self.status[t];
        status.next_unit += 1;
        status.running += 1;
        let track = &mut self.track[t];
        track.max_running = track.max_running.max(status.running);
        if !track.started {
            track.started = true;
            track.first = self.now;
            track.starting = start > 0.0;
        }
        self.free -= 1;
        let (cpu, disk) = if nominal > 0.0 {
            (
                spec.cpu * duration / nominal,
                (spec.disk + checkpoint.1) / nominal,
            )
        } else {
            (0.0, 0.0)
        };
        self.items.push(Item {
            work: Work::Unit {
                task: t as u32,
                unit: b,
            },
            left: nominal,
            nominal,
            cpu,
            disk,
            need: spec.need,
            waiting,
            startup: start,
            rate: 0.0,
            since: self.now,
        });
    }

    fn due_checkpoints(&mut self) {
        if let Some(interval) = self.interval {
            self.next_checkpoint += interval;
        }
        for t in 0..self.specs.len() {
            let spec = self.specs[t];
            let under_way = self.track[t].started && self.status[t].done < spec.units;
            if under_way && (spec.ckpt_s > 0.0 || spec.ckpt_b > 0.0) {
                self.track[t].owes_checkpoint = true;
            }
        }
    }

    fn rates(&mut self) {
        for track in &mut self.track {
            track.computing = 0;
        }
        let (mut cpu, mut disk, mut transfers) = (0.0, 0.0, 0.0);
        for item in &self.items {
            match item.work {
                Work::Unit { task, .. } if !item.waiting && item.startup <= 0.0 => {
                    self.track[task as usize].computing += 1;
                    cpu += item.cpu;
                    disk += item.disk;
                }
                Work::Unit { .. } => {}
                Work::Spill { .. } | Work::Reload { .. } => transfers += 1.0,
            }
        }
        let bandwidth = self.machine.disk_bytes_per_second;
        disk += transfers * bandwidth;
        let workers = f64::from(self.machine.workers);
        let f_cpu = if cpu > workers { workers / cpu } else { 1.0 };
        let f_disk = if disk > bandwidth {
            bandwidth / disk
        } else {
            1.0
        };
        for item in &mut self.items {
            item.rate = match item.work {
                Work::Unit { .. } if item.waiting || item.startup > 0.0 => 0.0,
                Work::Unit { task, .. } => {
                    let spec = &self.specs[task as usize];
                    let each = spec.scaling.each(self.track[task as usize].computing);
                    let cpu = if item.cpu > 0.0 { f_cpu } else { 1.0 };
                    let disk = if item.disk > 0.0 { f_disk } else { 1.0 };
                    each * cpu.min(disk)
                }
                Work::Spill { .. } | Work::Reload { .. } => bandwidth * f_disk,
            };
        }
    }

    fn complete(&mut self, item: Item) {
        match item.work {
            Work::Unit { task, unit } => self.finish_unit(task as usize, unit, item),
            Work::Spill { buffer } => {
                let b = self.buffers[buffer as usize];
                self.buffers[buffer as usize].loc = Loc::Disk;
                self.add_held(b.task as usize, -b.size);
                self.track[b.task as usize].spill += b.size;
                self.run.spill_bytes += b.size;
                self.run.disk_bytes += b.size;
                if b.wanted {
                    self.buffers[buffer as usize].wanted = false;
                    self.start_reload(buffer);
                }
            }
            Work::Reload { buffer } => {
                let size = self.buffers[buffer as usize].size;
                self.resident_push(buffer);
                self.run.reload_bytes += size;
                self.run.disk_bytes += size;
                for i in 0..self.items.len() {
                    if let Work::Unit { task, unit } = self.items[i].work
                        && self.items[i].waiting
                    {
                        self.items[i].waiting = !self.inputs_in_memory(task as usize, unit);
                    }
                }
            }
        }
    }

    fn inputs_in_memory(&self, t: usize, b: u32) -> bool {
        let spec = &self.specs[t];
        for at in self.producers_at[t]..self.producers_at[t + 1] {
            let pspec = &self.specs[self.producers[at as usize] as usize];
            if pspec.out <= 0.0 {
                continue;
            }
            for k in
                first_input(b, pspec.units, spec.units)..=last_input(b, pspec.units, spec.units)
            {
                if self.buffers[(pspec.base + k) as usize].loc != Loc::Memory {
                    return false;
                }
            }
        }
        true
    }

    fn finish_unit(&mut self, t: usize, b: u32, item: Item) {
        let spec = self.specs[t];
        let gid = spec.base + b;
        self.add_held(t, -item.need);
        self.done[gid as usize] = true;
        self.done_at[gid as usize] = self.now;
        let held = self.now - item.since;
        {
            let track = &mut self.track[t];
            while track.prefix < spec.units && self.done[(spec.base + track.prefix) as usize] {
                track.prefix += 1;
            }
            track.last = self.now;
            track.worker_s += held;
        }
        self.run.worker_seconds += held;
        self.run.disk_bytes += spec.disk;
        self.status[t].running -= 1;
        self.status[t].done += 1;
        self.finished += 1;
        self.free += 1;
        if spec.out > 0.0 {
            let buffer = &mut self.buffers[gid as usize];
            buffer.size = spec.out;
            buffer.uses = self.uses[gid as usize];
            self.resident_push(gid);
            self.add_held(t, spec.out);
            self.status[t].output_bytes += spec.out;
        }
        if spec.state > 0.0 {
            self.track[t].state += spec.state;
            self.add_held(t, spec.state);
        }
        if self.status[t].done == spec.units && self.track[t].state > 0.0 {
            let state = self.track[t].state;
            self.track[t].state = 0.0;
            self.add_held(t, -state);
        }
        for at in self.producers_at[t]..self.producers_at[t + 1] {
            let p = self.producers[at as usize] as usize;
            let pspec = self.specs[p];
            for k in
                first_input(b, pspec.units, spec.units)..=last_input(b, pspec.units, spec.units)
            {
                if pspec.out <= 0.0 {
                    break;
                }
                let input = pspec.base + k;
                let buffer = &mut self.buffers[input as usize];
                buffer.pins -= 1;
                buffer.uses -= 1;
                if buffer.uses == 0 {
                    let (loc, size) = (buffer.loc, buffer.size);
                    buffer.loc = Loc::Absent;
                    if loc == Loc::Memory {
                        self.resident_remove(input);
                        self.add_held(p, -size);
                    }
                    self.status[p].output_bytes -= size;
                }
            }
        }
        self.last_finished = gid;
        self.last_finished_at = self.now;
    }

    fn task_of(&self, gid: u32) -> usize {
        self.specs.partition_point(|s| s.base <= gid) - 1
    }

    fn finish(&mut self) {
        self.run.finish = self.now;
        for (t, track) in self.track.iter().enumerate() {
            self.run.tasks[t] = TaskRun {
                start: track.first,
                end: track.last,
                worker_seconds: track.worker_s,
                peak_memory: track.peak,
                spill_bytes: track.spill,
                max_running: track.max_running,
            };
        }
        let mut path = std::mem::take(&mut self.run.critical_path);
        path.clear();
        let mut unit = self.last_finished;
        while unit != NONE {
            let t = self.task_of(unit);
            if path.last() != Some(&t) {
                path.push(t);
            }
            unit = self.cause[unit as usize];
        }
        path.reverse();
        let mut i = 0;
        while i < path.len() {
            if path[..i].contains(&path[i]) {
                path.remove(i);
            } else {
                i += 1;
            }
        }
        self.run.critical_path = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_ranges_cover_the_rows() {
        // Equal units: one to one.
        for b in 0..5 {
            assert_eq!((first_input(b, 5, 5), last_input(b, 5, 5)), (b, b));
            assert_eq!((first_user(b, 5, 5), last_user(b, 5, 5)), (b, b));
        }
        // Two producer units, four consumer units: each producer unit feeds two.
        assert_eq!((first_input(1, 2, 4), last_input(1, 2, 4)), (0, 0));
        assert_eq!((first_input(2, 2, 4), last_input(2, 2, 4)), (1, 1));
        assert_eq!((first_user(1, 2, 4), last_user(1, 2, 4)), (2, 3));
        // Four into two, and three into two (the middle unit feeds both).
        assert_eq!((first_input(0, 4, 2), last_input(0, 4, 2)), (0, 1));
        assert_eq!((first_input(0, 3, 2), last_input(0, 3, 2)), (0, 1));
        assert_eq!((first_input(1, 3, 2), last_input(1, 3, 2)), (1, 2));
        assert_eq!((first_user(1, 3, 2), last_user(1, 3, 2)), (0, 1));
    }
}
