//! The simulated planner, `simulated` (SDD 26 f.7): the job's units played by `moruna-cost`'s
//! simulator on the run's workers, memory and disk, with the cost model's predictions, so a plan's
//! times, memory and resources held are a schedule's, not sums.

use moruna_cost::sim::{
    Admission, DRAWS, Forecast, Machine, SEED, Simulator, Spread, TaskLoad, Workload,
};
use moruna_cost::{CostError, Estimate, Unit, UnitCost};
use moruna_job::{Catalogue, Job};

use crate::costing::{self, Costed, Graph};
use crate::plan::{Plan, PlannerId, Range, Resources, Step, Totals, step_id, weakest};
use crate::planner::{DISK_BYTES_PER_SECOND, PlanError, PlanInputs, Planner};

/// The planner that simulates: every task is a step; the simulator plays its units (one task on
/// one batch, as many as its morsel gives) under today's admission rule, once at the medians and
/// [`Simulated::draws`] times with drawn durations; what overlapped in the medians' run is a
/// group, so steps that ran side by side share one and steps that ran one after another do not.
/// Each step's cost is its task's simulated span, worker time, peak and spill (p50 and p95 over
/// the draws) and its units' rows, bytes, state and reads; the plan's totals are the
/// simulation's: finish, critical path, peak memory, memory-seconds, worker-seconds, disk bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Simulated {
    /// Monte Carlo runs behind each p50 and p95 ([`DRAWS`] by default).
    pub draws: u32,
    /// Their seed ([`SEED`] by default), so the same job planned twice has the same plan.
    pub seed: u64,
}

impl Default for Simulated {
    fn default() -> Simulated {
        Simulated {
            draws: DRAWS,
            seed: SEED,
        }
    }
}

/// What the simulated planner plays for a job: the workload (its tasks in an order every edge
/// goes forward), the machine, and the job's place of each workload task.
#[derive(Clone, Debug, PartialEq)]
pub struct Simulation {
    /// The tasks and edges.
    pub workload: Workload,
    /// The run's workers, memory at the start and disk bandwidth.
    pub machine: Machine,
    /// `places[w]`: the job's place of workload task `w`.
    pub places: Vec<usize>,
}

struct Prepared {
    simulation: Simulation,
    costed: Vec<Costed>,
    graph: Graph,
    ids: Vec<String>,
}

impl Simulated {
    /// Its name, which a setting chooses it by.
    pub const NAME: &'static str = "simulated";
    /// Its version: a change to how it steps or names steps is a new version.
    pub const VERSION: u32 = 1;

    /// What [`Planner::plan`] simulates for `job`: refused like the plan (the job's graph, the
    /// run's resources, no model, a model's unusable estimate).
    pub fn simulation(
        &self,
        job: &Job,
        catalogue: &Catalogue,
        inputs: &PlanInputs<'_>,
    ) -> Result<Simulation, PlanError> {
        Ok(self.prepare(job, catalogue, inputs)?.simulation)
    }

    fn prepare(
        &self,
        job: &Job,
        catalogue: &Catalogue,
        inputs: &PlanInputs<'_>,
    ) -> Result<Prepared, PlanError> {
        let me = self.id();
        let graph = costing::graph(job, &me)?;
        let run = inputs.resources;
        costing::check_run(run)?;
        let model = inputs.model.ok_or_else(|| PlanError::Inputs {
            field: "model".into(),
            reason: format!(
                "the {} planner simulates with a cost model, and none is given",
                Simulated::NAME
            ),
        })?;
        let digest = crate::plan::job_digest(job);
        let ids: Vec<String> = (0..job.tasks.len())
            .map(|i| step_id(&digest, &me, &[i]))
            .collect();
        let costed = costing::cost(job, catalogue, inputs, model, &graph, &ids, &|_| run)?;
        let mut at = vec![0usize; job.tasks.len()];
        for (w, &place) in graph.order.iter().enumerate() {
            at[place] = w;
        }
        let interval = job.checkpoint.enabled.then(|| {
            moruna_job::checkpoint_interval_ms(job.checkpoint.interval_ms).0 as f64 / 1_000.0
        });
        let tasks = graph
            .order
            .iter()
            .map(|&place| {
                let c = &costed[place];
                let mut load = TaskLoad::new(job.tasks[place].id.clone(), c.units, c.unit);
                load.scaling = c.scaling();
                load.start_seconds = costing::start_seconds(&c.kernel);
                if interval.is_some() {
                    let finite = |v: Option<f64>| v.filter(|v| v.is_finite() && *v >= 0.0);
                    load.checkpoint_seconds = finite(c.kernel.checkpoint_seconds).unwrap_or(0.0);
                    load.checkpoint_bytes = finite(c.kernel.checkpoint_bytes).unwrap_or(0.0);
                }
                load.stateful = c.kernel.stateful == Some(true);
                load
            })
            .collect();
        let mut edges = Vec::new();
        for (place, producers) in graph.producers.iter().enumerate() {
            for &p in producers {
                edges.push((at[p], at[place]));
            }
        }
        let machine = Machine {
            workers: run.workers,
            memory_bytes: run.memory_start_bytes as f64,
            disk_bytes_per_second: inputs
                .disk_bytes_per_second
                .unwrap_or(DISK_BYTES_PER_SECOND) as f64,
        };
        Ok(Prepared {
            simulation: Simulation {
                workload: Workload {
                    tasks,
                    edges,
                    checkpoint_interval_seconds: interval,
                },
                machine,
                places: graph.order.clone(),
            },
            costed,
            graph,
            ids,
        })
    }
}

fn estimate(spread: Spread, unit: Unit, like: &Estimate) -> Estimate {
    Estimate {
        p50: spread.p50,
        p95: spread.p95.max(spread.p50),
        unit,
        ..*like
    }
}

fn range(spread: Spread) -> Range {
    Range {
        p50: spread.p50,
        p95: spread.p95.max(spread.p50),
    }
}

/// Each step's group from the medians' run: one more than the latest group of a step it waits
/// for, and of a step that ended before it started; otherwise the group of what it overlapped.
fn groups(forecast: &Forecast, graph: &Graph, places: &[usize]) -> Vec<u32> {
    let n = places.len();
    let tasks = &forecast.median.tasks;
    let mut at = vec![0usize; n];
    for (w, &place) in places.iter().enumerate() {
        at[place] = w;
    }
    let mut by_start: Vec<usize> = (0..n).collect();
    by_start.sort_by(|&a, &b| tasks[a].start.total_cmp(&tasks[b].start).then(a.cmp(&b)));
    let near = 1e-9 * forecast.median.finish.max(1.0);
    let mut group = vec![0u32; n];
    for (i, &w) in by_start.iter().enumerate() {
        let mut g = 0;
        for &p in &graph.producers[places[w]] {
            g = g.max(group[at[p]] + 1);
        }
        for &e in &by_start[..i] {
            if tasks[e].end <= tasks[w].start + near {
                g = g.max(group[e] + 1);
            }
        }
        group[w] = g;
    }
    group
}

impl Planner for Simulated {
    fn id(&self) -> PlannerId {
        PlannerId::new(Simulated::NAME, Simulated::VERSION)
    }

    fn plan(
        &self,
        job: &Job,
        catalogue: &Catalogue,
        inputs: &PlanInputs<'_>,
    ) -> Result<Plan, PlanError> {
        let me = self.id();
        let Prepared {
            simulation,
            costed,
            graph,
            ids,
        } = self.prepare(job, catalogue, inputs)?;
        let places = &simulation.places;
        let refused = |e: CostError| match e {
            CostError::Infeasible {
                task,
                needs,
                reason,
            } => {
                let place = job.task_index(&task).unwrap_or(0);
                PlanError::Infeasible {
                    step: ids[place].clone(),
                    task,
                    reason: format!("a unit needs {needs} bytes and {reason}"),
                }
            }
            other => PlanError::Planner {
                planner: me.to_string(),
                reason: other.to_string(),
            },
        };
        let mut sim = Simulator::new(&simulation.workload, simulation.machine).map_err(refused)?;
        let forecast = sim
            .forecast(&mut Admission, self.draws, self.seed)
            .map_err(refused)?;
        let group = groups(&forecast, &graph, places);
        let run = inputs.resources;

        let mut order: Vec<usize> = (0..places.len()).collect();
        order.sort_by(|&a, &b| {
            group[a]
                .cmp(&group[b])
                .then(
                    forecast.median.tasks[a]
                        .start
                        .total_cmp(&forecast.median.tasks[b].start),
                )
                .then(a.cmp(&b))
        });
        let mut steps = Vec::with_capacity(places.len());
        for &w in &order {
            let place = places[w];
            let c = &costed[place];
            let u = &c.unit;
            let n = c.weight;
            let spread = &forecast.tasks[w];
            let simulated = &forecast.median.tasks[w];
            let own_spill = u.spill_bytes.sum(n);
            let cost = UnitCost {
                duration: estimate(spread.span, Unit::Seconds, &u.duration),
                worker_time: estimate(spread.worker_seconds, Unit::Seconds, &u.worker_time),
                peak_memory: estimate(spread.peak_memory, Unit::Bytes, &u.peak_memory),
                retained_memory: u.retained_memory,
                rows_out: u.rows_out.sum(n),
                bytes_out: u.bytes_out.sum(n),
                state_growth: u.state_growth.sum(n),
                read_bytes: u.read_bytes.sum(n),
                spill_bytes: Estimate {
                    p50: own_spill.p50 + spread.spill_bytes.p50,
                    p95: own_spill.p95 + spread.spill_bytes.p95.max(spread.spill_bytes.p50),
                    ..own_spill
                },
            };
            let mut depends_on: Vec<String> = Vec::new();
            for &p in &graph.producers[place] {
                if !depends_on.contains(&ids[p]) {
                    depends_on.push(ids[p].clone());
                }
            }
            steps.push(Step {
                id: ids[place].clone(),
                tasks: vec![job.tasks[place].id.clone()],
                depends_on,
                group: group[w],
                resources: Resources {
                    workers: simulated.max_running.clamp(1, run.workers),
                    memory_start_bytes: (simulated.peak_memory.ceil() as u64)
                        .min(run.memory_ceiling_bytes),
                    memory_ceiling_bytes: run.memory_ceiling_bytes,
                },
                cost,
            });
        }
        // A group's starts are shares of the run's memory: the steps' simulated peaks came at
        // different moments, so where they add past the run's they are scaled to it.
        let mut sums = std::collections::BTreeMap::<u32, u128>::new();
        for s in &steps {
            *sums.entry(s.group).or_default() += u128::from(s.resources.memory_start_bytes);
        }
        for s in &mut steps {
            let sum = sums.get(&s.group).copied().unwrap_or(0);
            let room = u128::from(run.memory_ceiling_bytes);
            if sum > room {
                s.resources.memory_start_bytes =
                    (u128::from(s.resources.memory_start_bytes) * room / sum) as u64;
            }
        }
        let (basis, observations) = weakest(&steps);
        let totals = Totals {
            basis,
            observations,
            finish: range(forecast.finish),
            critical_path: forecast
                .median
                .critical_path
                .iter()
                .map(|&w| ids[places[w]].clone())
                .collect(),
            peak_memory_bytes: range(forecast.peak_memory),
            memory_byte_seconds: range(forecast.memory_byte_seconds),
            worker_seconds: range(forecast.worker_seconds),
            disk_bytes: range(forecast.disk_bytes),
        };
        Ok(Plan::new(
            job,
            me.clone(),
            inputs.model_id(),
            crate::plan::now_ms(&me)?,
            run,
            steps,
            totals,
        ))
    }
}
