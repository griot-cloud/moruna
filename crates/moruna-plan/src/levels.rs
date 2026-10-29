//! The first planner, `levels` (SDD 26 f.2): one step per task, grouped by dependency level.

use moruna_job::{Catalogue, Job, Role};

use crate::plan::{Estimate, Plan, PlannerId, Resources, Step, Totals, Unit, UnitCost, step_id};
use crate::planner::{PlanError, PlanInputs, Planner, StepQuery};

/// The planner that steps through a job's DAG level by level: every task is a step; a task's
/// level is one more than the deepest level of the tasks it takes input from (a source's is 0),
/// so everything whose inputs are ready together forms one group, and the groups start in level
/// order. A group's steps share the run's resources: each takes the run's memory divided by the
/// group's size, and the run's workers divided by it (at least one). Without a cost model its
/// estimates are declared defaults (f.2); with one, it asks the model about each step.
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
        moruna_job::check_graph(job).map_err(PlanError::Job)?;
        let run = inputs.resources;
        if run.workers == 0 {
            return Err(PlanError::Inputs {
                field: "resources.workers".into(),
                reason: "a run is given at least one worker".into(),
            });
        }
        if run.memory_ceiling_bytes < run.memory_start_bytes {
            return Err(PlanError::Inputs {
                field: "resources.memory_ceiling_bytes".into(),
                reason: format!(
                    "{} is below the start, {}",
                    run.memory_ceiling_bytes, run.memory_start_bytes
                ),
            });
        }
        let order = job
            .topological_order()
            .map_err(|cycle| PlanError::Planner {
                planner: me.to_string(),
                reason: format!(
                    "the job's edges go round a cycle through tasks {}",
                    cycle
                        .iter()
                        .map(|&i| job.tasks[i].id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })?;
        let n = job.tasks.len();
        // Each task's producers, by place, in the job's edge order.
        let mut producers: Vec<Vec<usize>> = vec![Vec::new(); n];
        for edge in &job.edges {
            if let (Some(a), Some(b)) = (
                job.task_index(&edge.from.task),
                job.task_index(&edge.to.task),
            ) && !producers[b].contains(&a)
            {
                producers[b].push(a);
            }
        }
        let mut level = vec![0u32; n];
        for &i in &order {
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

        // What each task gives, by place, as the plan expects it: rows and bytes out.
        let mut flow: Vec<Flow> = vec![(None, None); n];
        let mut steps = Vec::with_capacity(n);
        for &i in &places {
            let task = &job.tasks[i];
            let share = size.get(&level[i]).copied().unwrap_or(1).max(1);
            let resources = Resources {
                workers: (run.workers / share).max(1),
                memory_start_bytes: run.memory_start_bytes / u64::from(share),
                memory_ceiling_bytes: run.memory_ceiling_bytes / u64::from(share),
            };
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
            let (cost, gives) = match inputs.model {
                None => declared(task.role, table, rows_in, bytes_in, resources),
                Some(model) => {
                    let query = StepQuery {
                        job,
                        tasks: vec![task],
                        declarations: vec![catalogue.get(task.role, &task.kind)],
                        table,
                        rows_in,
                        bytes_in,
                        resources,
                    };
                    let cost = model.estimate(&query).map_err(|reason| PlanError::Model {
                        model: model.id(),
                        step: ids[i].clone(),
                        reason,
                    })?;
                    cost.check().map_err(|reason| PlanError::Model {
                        model: model.id(),
                        step: ids[i].clone(),
                        reason,
                    })?;
                    let gives = (
                        Some(cost.rows_out.p50.round() as u64),
                        Some(cost.bytes_out.p50.round() as u64),
                    );
                    (cost, gives)
                }
            };
            flow[i] = gives;
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
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| PlanError::Planner {
                planner: me.to_string(),
                reason: format!("the clock is before the Unix epoch: {e}"),
            })?
            .as_millis() as u64;
        Ok(Plan::new(
            job,
            me,
            inputs.model.map(|m| m.id()),
            created,
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
    let at = |v: Option<u64>, unit: Unit| {
        let v = v.unwrap_or(0) as f64;
        Estimate::declared(v, v, unit)
    };
    let cost = UnitCost {
        peak_memory: Estimate::declared(
            resources.memory_start_bytes as f64,
            resources.memory_ceiling_bytes as f64,
            Unit::Bytes,
        ),
        rows_out: at(rows, Unit::Rows),
        bytes_out: at(bytes, Unit::Bytes),
        read_bytes: at(read, Unit::Bytes),
        ..UnitCost::zero()
    };
    (cost, (rows, bytes))
}
