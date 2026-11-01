//! `lookahead` (f.24; adaptive-runtime 9.3, 9.4 steps 3 and 4; RA 5, 9 (D); SDD 24 f.13).
//!
//! The ready units are first put in `residency_greedy`'s order. Then the run's state becomes a
//! simulation (`Snapshot::of`): every source and kernel stage a task, every stage's backlog (up
//! to [`HORIZON_BATCHES`] batches a port) finished units of its producer, in memory or on disk
//! as the queues hold them, every unit under way a unit with its predicted time less what it has
//! run, and the work that backlog makes downstream units still to run; sources read nothing more,
//! so the simulation's horizon is draining what the run holds now. The best [`WIDTH`] units in
//! the greedy order are each played first, at medians, the rest of the simulation by the critical
//! path (of the units that fit, the task with the most predicted work on the longest path from it
//! to the end, ties to the later task: what a run that weighs the path does next); the best
//! [`BEAM`] of those are played again with each of the [`WIDTH`] as their second step. The first step of the earliest predicted finish runs, ties to the greedy
//! order.
//!
//! **The budget.** Each simulation is timed, and a simulation that would end past the decision's
//! budget is not started: the decision stops there with the best it has, or the greedy order if
//! it has nothing, and counts itself `shallower` (9.4 step 4: lookahead falls to greedy; a
//! decision that still ends over budget hands the next to `admission`, which the scheduler
//! counts). A ready unit whose key drifts makes the decision one step deep (9.6: conservative).
//! A simulation that cannot be made or run (a snapshot the simulator refuses, a unit that cannot
//! fit) is counted with its reason, and the greedy order answers.
//!
//! **A resize.** The simulator built for a snapshot is kept; when the next decision's workload is
//! the same and only the machine changed (the run was resized), it is resized and played again
//! (SDD 24 CM-I24) rather than built anew.

use std::time::Instant;

use moruna_cost::sim::{
    Candidate, Machine as SimMachine, Policy, Simulator, Start, TaskLoad, TaskStart, View, Workload,
};
use moruna_cost::{Estimate, Unit, UnitCost};
use moruna_kernel::{
    Action, PolicyCounters, PolicyId, Prediction, Producer, Reads, SchedulerState, SchedulingPolicy,
};

use super::greedy::{ResidencyGreedy, runs};

/// Units played as the first step.
pub const WIDTH: usize = 3;
/// Of those, the best played again with a second step.
pub const BEAM: usize = 2;
/// A port's backlog the snapshot keeps: enough to see what a choice does to memory and to the
/// next units, few enough that a simulation fits a decision's budget.
pub const HORIZON_BATCHES: u64 = 8;
/// What a simulation is taken to cost before one has been timed, nanoseconds.
const FIRST_SIMULATION_NS: u64 = 20_000;

/// The bounded lookahead (module docs).
#[derive(Default)]
pub struct Lookahead {
    counters: PolicyCounters,
    /// One simulation's time, nanoseconds, smoothed.
    simulation_ns: u64,
    /// The simulator of the last snapshot's workload.
    kept: Option<(Workload, Simulator)>,
    /// The continuation's ranks, kept so a play allocates nothing.
    rank: Vec<f64>,
    /// The greedy order it starts from, and its buffers.
    greedy: ResidencyGreedy,
    order: Vec<usize>,
    firsts: Vec<(f64, usize)>,
}

impl std::fmt::Debug for Lookahead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lookahead")
            .field("counters", &self.counters)
            .field("simulation_ns", &self.simulation_ns)
            .finish()
    }
}

/// The simulator's policy for one play: the given tasks' next units first, in order, while they
/// can start, then the critical path.
struct Forced<'a> {
    tasks: &'a [usize],
    next: usize,
    path: &'a Path,
    rank: &'a mut Vec<f64>,
}

/// The workload's shape the critical path is read from: each task's consumers and how many of
/// its units run at once.
#[derive(Debug, Default)]
pub(crate) struct Path {
    consumers: Vec<Vec<usize>>,
    caps: Vec<u32>,
}

impl Policy for Forced<'_> {
    fn name(&self) -> &str {
        "lookahead"
    }

    fn reset(&mut self) {
        self.next = 0;
    }

    fn choose(&mut self, view: &View<'_>) -> Option<usize> {
        if let Some(&task) = self.tasks.get(self.next) {
            self.next += 1;
            if let Some(at) = view
                .candidates
                .iter()
                .position(|c: &Candidate| c.task == task)
            {
                return Some(at);
            }
            // The step cannot start in the simulation: the rest is played by the path.
            self.next = self.tasks.len();
        }
        // Each task's work still to start over the units it runs at once, plus the heaviest of
        // its consumers'; tasks are listed so every edge goes forward, so from the last back.
        let tasks = view.tasks;
        self.rank.clear();
        self.rank.resize(tasks.len(), 0.0);
        for t in (0..tasks.len()).rev() {
            let left = f64::from(tasks[t].units.saturating_sub(tasks[t].next_unit));
            let own = left * tasks[t].median_seconds / f64::from(self.path.caps[t].max(1));
            let down = self.path.consumers[t]
                .iter()
                .map(|&c| self.rank[c])
                .fold(0.0, f64::max);
            self.rank[t] = own + down;
        }
        let any_fits = view.candidates.iter().any(|c| c.fits);
        let mut best: Option<(usize, f64, usize)> = None;
        for (at, c) in view.candidates.iter().enumerate() {
            if any_fits && !c.fits {
                continue;
            }
            let rank = self.rank[c.task];
            match best {
                Some((_, r, task)) if rank < r || (rank == r && c.task < task) => {}
                _ => best = Some((at, rank, c.task)),
            }
        }
        best.map(|(at, _, _)| at)
    }
}

/// The run as a simulation: its workload, where it is, the machine, and each kernel stage's
/// task.
#[derive(Debug)]
pub(crate) struct Snapshot {
    pub(crate) workload: Workload,
    pub(crate) start: Start,
    pub(crate) machine: SimMachine,
    /// By kernel stage, stage 1 first: its task, when it has units in the horizon.
    pub(crate) stage_task: Vec<Option<usize>>,
    /// The shape the continuation's critical path is read from.
    pub(crate) path: Path,
}

/// A unit's cost as the simulator reads it: the prediction's time and output, what it reserves
/// its output's 95th percentile, all of its time a worker's. The output and the reservation are
/// held to `cap`, as the scheduler holds a claim (10 f.22): a larger output goes to disk.
fn unit_cost(p: &Prediction, cap: u64) -> UnitCost {
    let spread = |p50: u64, p95: u64, scale: f64, unit: Unit| Estimate {
        p95: p95.max(p50) as f64 * scale,
        ..Estimate::exact(p50 as f64 * scale, unit)
    };
    let (out50, out95) = (p.out_bytes.p50.min(cap), p.out_bytes.p95.min(cap));
    let mut cost = UnitCost::zero();
    cost.duration = spread(p.duration_ns.p50, p.duration_ns.p95, 1e-9, Unit::Seconds);
    cost.worker_time = cost.duration;
    cost.bytes_out = spread(out50, out95, 1.0, Unit::Bytes);
    cost.peak_memory = spread(out95, out95, 1.0, Unit::Bytes);
    cost
}

impl Snapshot {
    /// The snapshot of `state` (module docs). The reason when the state cannot be one.
    pub(crate) fn of(state: &SchedulerState<'_>) -> Result<Snapshot, String> {
        let topology = state.topology;
        let sources = usize::from(topology.sources());
        let stages = state.stages;
        if stages.len() != usize::from(topology.stages()) {
            return Err(format!(
                "the state describes {} stages and the run has {}",
                stages.len(),
                topology.stages()
            ));
        }
        let tasks = sources + stages.len();
        let producer = |queue| match topology.link(queue).map(|l| l.from) {
            Some(Producer::Source(s)) => Some(usize::from(s)),
            Some(Producer::Kernel(k)) => usize::from(k).checked_sub(1).map(|i| sources + i),
            None => None,
        };
        let backlog = |batches: u64| batches.min(HORIZON_BATCHES);
        // Units finished, from the stages that use them: a producer has finished what each
        // consumer has started and holds in its backlog. From the last stage back, so a stage's
        // consumers are counted before it is.
        let mut done = vec![0u64; tasks];
        for (index, view) in stages.iter().enumerate().rev() {
            let started = done[sources + index] + view.running_ns.len() as u64;
            for port in &view.ports {
                let Some(p) = producer(port.queue) else {
                    return Err(format!("queue {} has no producer", port.queue));
                };
                done[p] = done[p].max(started + backlog(port.batches));
            }
        }
        // Units in the horizon: a source's are what it has read; a stage's are what it has
        // started, its backlog, and what its producers have still to make.
        let mut units = done.clone();
        for (index, view) in stages.iter().enumerate() {
            let t = sources + index;
            let mut total = done[t] + view.running_ns.len() as u64;
            for port in &view.ports {
                total += backlog(port.batches);
                if let Some(p) = producer(port.queue) {
                    total += units[p] - done[p];
                }
            }
            units[t] = total;
        }
        // Every task with a unit in the horizon, in the run's order (sources, then stages, so
        // every edge goes forward).
        let mut place = vec![None; tasks];
        let mut loads = Vec::new();
        let mut start = Start::default();
        for t in 0..tasks {
            if units[t] == 0 {
                continue;
            }
            let Ok(count) = u32::try_from(units[t]) else {
                return Err(format!("task {t} has {} units", units[t]));
            };
            place[t] = Some(loads.len());
            let (cost, running) = if t < sources {
                // A source's units are its batches waiting in every queue it feeds: no time, and
                // their size the mean of what waits.
                let (mut batches, mut bytes) = (0u64, 0u64);
                for view in stages {
                    for port in &view.ports {
                        if producer(port.queue) == Some(t) {
                            batches += port.batches;
                            bytes += port.memory_bytes + port.disk_bytes;
                        }
                    }
                }
                let mut cost = UnitCost::zero();
                cost.bytes_out =
                    Estimate::exact(bytes.checked_div(batches).unwrap_or(0) as f64, Unit::Bytes);
                (cost, Vec::new())
            } else {
                let view = &stages[t - sources];
                let median = view.unit.duration_ns.p50;
                let left = view
                    .running_ns
                    .iter()
                    .map(|ran| median.saturating_sub(*ran) as f64 * 1e-9)
                    .collect();
                (unit_cost(&view.unit, state.machine.claim_cap_bytes), left)
            };
            let mut load = TaskLoad::new(format!("task {t}"), count, cost);
            if t >= sources {
                let view = &stages[t - sources];
                if view.cap == 1 {
                    load.scaling = moruna_cost::sim::Scaling::serial();
                }
            }
            loads.push(load);
            start.tasks.push(TaskStart {
                done: done[t] as u32,
                running,
                on_disk: Vec::new(),
            });
        }
        // Edges, and the backlog the queues hold on disk: the last batches of each queue's
        // backlog, never one a unit under way holds.
        let mut edges = Vec::new();
        for (index, view) in stages.iter().enumerate() {
            let c = sources + index;
            let Some(to) = place[c] else { continue };
            let started = done[c] + view.running_ns.len() as u64;
            for port in &view.ports {
                let Some(p) = producer(port.queue) else {
                    continue;
                };
                let Some(from) = place[p] else { continue };
                if !edges.contains(&(from, to)) {
                    edges.push((from, to));
                }
                let held = port.memory_bytes + port.disk_bytes;
                let kept = backlog(port.batches);
                let on_disk = (u128::from(kept) * u128::from(port.disk_bytes))
                    .checked_div(u128::from(held))
                    .unwrap_or(0) as u64;
                let end = started + kept;
                let pinned = stages
                    .iter()
                    .enumerate()
                    .filter(|(i, v)| {
                        v.ports
                            .iter()
                            .any(|q| producer(q.queue) == Some(p) && sources + i != c)
                    })
                    .map(|(i, v)| done[sources + i] + v.running_ns.len() as u64)
                    .chain(std::iter::once(started))
                    .max()
                    .unwrap_or(started);
                let first = end.saturating_sub(on_disk).max(pinned);
                for unit in first..end {
                    let unit = unit as u32;
                    let slot = &mut start.tasks[from].on_disk;
                    if !slot.contains(&unit) {
                        slot.push(unit);
                    }
                }
            }
        }
        let running: usize = stages.iter().map(|v| v.running_ns.len()).sum();
        let machine = SimMachine {
            workers: u32::from(state.machine.workers).max(running as u32).max(1),
            memory_bytes: state.machine.budget_bytes.max(1) as f64,
            disk_bytes_per_second: state.machine.disk_bytes_per_second.max(1) as f64,
        };
        let mut path = Path {
            consumers: vec![Vec::new(); loads.len()],
            caps: vec![machine.workers; loads.len()],
        };
        for &(from, to) in &edges {
            path.consumers[from].push(to);
        }
        for (index, view) in stages.iter().enumerate() {
            if let Some(t) = place[sources + index] {
                path.caps[t] = u32::from(view.cap.max(1)).min(machine.workers);
            }
        }
        Ok(Snapshot {
            workload: Workload {
                tasks: loads,
                edges,
                checkpoint_interval_seconds: None,
            },
            start,
            machine,
            stage_task: (0..stages.len()).map(|i| place[sources + i]).collect(),
            path,
        })
    }
}

impl Lookahead {
    /// What it counted so far.
    pub fn counters(&self) -> &PolicyCounters {
        &self.counters
    }

    fn failed(&mut self, why: String) {
        self.counters.simulation_errors += 1;
        self.counters.simulation_error.get_or_insert(why);
    }

    /// The simulator of `snapshot`: the kept one when its workload is the same, resized when the
    /// machine changed (CM-I24); a new one otherwise.
    fn simulator(&mut self, snapshot: &Snapshot) -> Result<&mut Simulator, String> {
        let reuse = matches!(&self.kept, Some((workload, _)) if *workload == snapshot.workload);
        if !reuse {
            let sim =
                Simulator::new(&snapshot.workload, snapshot.machine).map_err(|e| e.to_string())?;
            self.kept = Some((snapshot.workload.clone(), sim));
        }
        let Some((_, sim)) = self.kept.as_mut() else {
            return Err("no simulator was kept".to_string());
        };
        if sim.machine() != snapshot.machine {
            sim.resize(snapshot.machine).map_err(|e| e.to_string())?;
            self.counters.resizes += 1;
        }
        Ok(sim)
    }

    /// The predicted finish with `steps` played first, timed into the smoothed cost.
    fn play(&mut self, snapshot: &Snapshot, steps: &[usize]) -> Result<f64, String> {
        let began = Instant::now();
        let mut rank = std::mem::take(&mut self.rank);
        let finish = match self.simulator(snapshot) {
            Ok(sim) => {
                let mut forced = Forced {
                    tasks: steps,
                    next: 0,
                    path: &snapshot.path,
                    rank: &mut rank,
                };
                sim.run_from(&snapshot.start, &mut forced)
                    .map(|run| run.finish)
                    .map_err(|e| e.to_string())
            }
            Err(why) => Err(why),
        };
        self.rank = rank;
        let took = began.elapsed().as_nanos() as u64;
        self.simulation_ns = if self.simulation_ns == 0 {
            took
        } else {
            (3 * self.simulation_ns + took) / 4
        };
        self.counters.simulations += 1;
        finish
    }

    /// Whether another simulation fits what is left of `budget_ns` since `began`.
    fn fits(&self, began: Instant, budget_ns: u64) -> bool {
        let cost = if self.simulation_ns == 0 {
            FIRST_SIMULATION_NS
        } else {
            self.simulation_ns
        };
        began.elapsed().as_nanos() as u64 + cost <= budget_ns
    }
}

impl SchedulingPolicy for Lookahead {
    fn id(&self) -> PolicyId {
        PolicyId {
            name: super::LOOKAHEAD,
            version: 1,
        }
    }

    fn decide(&mut self, state: &SchedulerState<'_>, actions: &mut Vec<Action>) {
        let began = Instant::now();
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        order.extend_from_slice(self.greedy.order(state));
        let chosen = self.choose(state, &order, began);
        if let Some(rank) = chosen {
            runs(state, &order[rank..=rank], actions);
            for (r, &at) in order.iter().enumerate() {
                if r != rank {
                    runs(state, &[at], actions);
                }
            }
        } else {
            runs(state, &order, actions);
        }
        self.order = order;
    }

    fn reads(&self) -> Reads {
        Reads {
            stages: true,
            critical_path: true,
            waiting: true,
        }
    }

    fn counters(&self) -> PolicyCounters {
        self.counters.clone()
    }
}

impl Lookahead {
    /// The rank in `order` of the first step of the earliest predicted finish; `None` when the
    /// greedy order answers (one unit, no simulation run, or a snapshot refused).
    fn choose(
        &mut self,
        state: &SchedulerState<'_>,
        order: &[usize],
        began: Instant,
    ) -> Option<usize> {
        if order.len() < 2 {
            return None;
        }
        let snapshot = match Snapshot::of(state) {
            Ok(snapshot) => snapshot,
            Err(why) => {
                self.failed(why);
                return None;
            }
        };
        let depth = if state.ready.iter().any(|r| r.predicted.drifting) {
            1
        } else {
            2
        };
        let task_of = |at: usize| {
            usize::from(state.ready[at].stage)
                .checked_sub(1)
                .and_then(|i| snapshot.stage_task.get(i).copied().flatten())
        };
        let mut shallower = false;
        // (finish, rank in the greedy order)
        let mut firsts = std::mem::take(&mut self.firsts);
        firsts.clear();
        for (rank, &at) in order.iter().take(WIDTH).enumerate() {
            let Some(task) = task_of(at) else { continue };
            if !self.fits(began, state.budget_ns) {
                shallower = true;
                break;
            }
            match self.play(&snapshot, &[task]) {
                Ok(finish) => firsts.push((finish, rank)),
                Err(why) => {
                    self.failed(why);
                    break;
                }
            }
        }
        if depth > 1 && !shallower {
            firsts.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            'beam: for first in firsts.iter_mut().take(BEAM) {
                let Some(t1) = task_of(order[first.1]) else {
                    continue;
                };
                for &second in order.iter().take(WIDTH) {
                    let Some(t2) = task_of(second) else { continue };
                    if !self.fits(began, state.budget_ns) {
                        shallower = true;
                        break 'beam;
                    }
                    match self.play(&snapshot, &[t1, t2]) {
                        Ok(finish) => first.0 = first.0.min(finish),
                        Err(why) => {
                            self.failed(why);
                            break 'beam;
                        }
                    }
                }
            }
        }
        if shallower {
            self.counters.shallower += 1;
        }
        let best = firsts
            .iter()
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
            .map(|&(_, rank)| rank);
        self.firsts = firsts;
        best
    }
}
