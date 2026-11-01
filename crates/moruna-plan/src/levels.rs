//! The first planner, `levels` (SDD 26 f.2): one step per task, grouped by dependency level.

use moruna_job::{Catalogue, Job, Role};

use crate::costing;
use crate::plan::{Estimate, Plan, PlannerId, Resources, Step, Totals, Unit, UnitCost, step_id};
use crate::planner::{PlanError, PlanInputs, Planner};

/// The planner that steps through a job's DAG level by level: every task is a step; a task's
/// level is one more than the deepest level of the tasks it takes input from (a source's is 0),
/// so everything whose inputs are ready together forms one group, and the groups start in level
/// order. A group's steps share the run's resources: each takes the run's memory divided by the
/// group's size, and the run's workers divided by it (at least one). Without a cost model its
/// estimates are declared defaults (f.2); with one, each step's cost is its units' over the
/// batches its morsel gives, by arithmetic rather than simulation (f.6).
#[derive(Clone, Copy, Debug, Default)]
pub struct Levels;

impl Levels {
    /// Its name, which a setting chooses it by.
    pub const NAME: &'static str = "levels";
    /// Its version: a change to how it steps or names steps is a new version.
    pub const VERSION: u32 = 1;
}

impl Planner for Levels {
    fn id(&self) -> PlannerId {
        PlannerId::new(Levels::NAME, Levels::VERSION)
    }

    fn plan(
        &self,
        job: &Job,
        catalogue: &Catalogue,
        inputs: &PlanInputs<'_>,
    ) -> Result<Plan, PlanError> {
        let me = self.id();
        let graph = costing::graph(job, &me)?;
        let run = inputs.resources;
        costing::check_run(run)?;
        let n = job.tasks.len();
        let producers = &graph.producers;
        let mut level = vec![0u32; n];
        for &i in &graph.order {
            level[i] = producers[i]
                .iter()
                .map(|&p| level[p] + 1)
                .max()
                .unwrap_or(0);
        }
        // Steps in level order, document order within a level.
        let mut places: Vec<usize> = (0..n).collect();
        places.sort_by_key(|&i| (level[i], i));
        let digest = crate::plan::job_digest(job);
        let ids: Vec<String> = (0..n).map(|i| step_id(&digest, &me, &[i])).collect();
        let mut size = std::collections::BTreeMap::<u32, u32>::new();
        for &l in &level {
            *size.entry(l).or_default() += 1;
        }
        let share = |i: usize| {
            let share = size.get(&level[i]).copied().unwrap_or(1).max(1);
            Resources {
                workers: (run.workers / share).max(1),
                memory_start_bytes: run.memory_start_bytes / u64::from(share),
                memory_ceiling_bytes: run.memory_ceiling_bytes / u64::from(share),
            }
        };
        let costed = match inputs.model {
            Some(model) => Some(costing::cost(
                job, catalogue, inputs, model, &graph, &ids, &share,
            )?),
            None => None,
        };

        // What each task gives, by place, as the plan expects it: rows and bytes out.
        let mut flow: Vec<Flow> = vec![(None, None); n];
        let mut steps = Vec::with_capacity(n);
        for &i in &places {
            let task = &job.tasks[i];
            let resources = share(i);
            let cost = match &costed {
                None => {
                    let (rows_in, bytes_in) = if producers[i].is_empty() {
                        (None, None)
                    } else {
                        let sum = |pick: fn(&Flow) -> Option<u64>| {
                            producers[i].iter().try_fold(0u64, |acc, &p| {
                                pick(&flow[p]).map(|v| acc.saturating_add(v))
                            })
                        };
                        (sum(|f| f.0), sum(|f| f.1))
                    };
                    let table = match task.role {
                        Role::Source => inputs.tables.get(&task.id),
                        _ => None,
                    };
                    let (cost, gives) = declared(task.role, table, rows_in, bytes_in, resources);
                    flow[i] = gives;
                    cost
                }
                Some(costed) => {
                    let c = &costed[i];
                    let start = costing::start_seconds(&c.kernel);
                    costing::aggregate(c, resources.workers, start)
                }
            };
            let mut depends_on: Vec<String> = Vec::new();
            for &p in &producers[i] {
                if !depends_on.contains(&ids[p]) {
                    depends_on.push(ids[p].clone());
                }
            }
            steps.push(Step {
                id: ids[i].clone(),
                tasks: vec![task.id.clone()],
                depends_on,
                group: level[i],
                resources,
                cost,
            });
        }
        let totals = Totals::of(&steps);
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

/// The rows and bytes a step gives, each when known.
type Flow = (Option<u64>, Option<u64>);

/// The declared defaults of one step, with no cost model (SDD 26 f.2): a source gives the rows
/// and file bytes its table's statistics count and reads those bytes; a kernel and a sink give
/// what they take (a count unknown is a declared zero); a step may hold the memory it is given,
/// from its start to its ceiling; times, state, retained memory and spill are declared zero.
/// Every estimate's basis is `Default`, resting on no observation. Beside the cost, the rows and
/// bytes it gives as far as they are known, for the steps after it.
fn declared(
    role: Role,
    table: Option<&crate::planner::TableStats>,
    rows_in: Option<u64>,
    bytes_in: Option<u64>,
    resources: Resources,
) -> (UnitCost, Flow) {
    let (rows, bytes, read) = match role {
        Role::Source => {
            let rows = table.and_then(|t| t.rows);
            let bytes = table.and_then(|t| t.file_bytes);
            (rows, bytes, bytes)
        }
        Role::Kernel | Role::Sink => (rows_in, bytes_in, None),
    };
    let at = |v: Option<u64>, unit: Unit| Estimate::exact(v.unwrap_or(0) as f64, unit);
    let cost = UnitCost {
        peak_memory: Estimate {
            p95: resources.memory_ceiling_bytes as f64,
            ..Estimate::exact(resources.memory_start_bytes as f64, Unit::Bytes)
        },
        rows_out: at(rows, Unit::Rows),
        bytes_out: at(bytes, Unit::Bytes),
        read_bytes: at(read, Unit::Bytes),
        ..UnitCost::zero()
    };
    (cost, (rows, bytes))
}
