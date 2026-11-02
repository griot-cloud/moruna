//! A job's tasks as the cost model sees them (SDD 26 f.6): each task's key, its morsel, its
//! units and what one unit costs, the rows and bytes flowing from the sources' statistics
//! through the DAG. Both planners that ask a model read them from here.

use moruna_cost::sim::Scaling;
use moruna_cost::{Capacity, Column, CostKey, CostModel, Digest, Inputs, Kernel, UnitCost};
use moruna_job::{Catalogue, Fact, Job, Role, Task};
use sha2::{Digest as _, Sha256};

use crate::plan::{PlannerId, Resources};
use crate::planner::{DISK_BYTES_PER_SECOND, PlanError, PlanInputs};

/// The key task `task`'s units are costed under when the embedder gives none (SDD 26 f.6): its
/// role and kind; as its fingerprint the SHA-256 of `moruna-plan kind`, its role and its kind
/// (a kind that runs no function of its own, SDD 24 d.1); as its settings digest the SHA-256 of
/// the canonical form of the task alone (its kind, settings and ports, named by place); and an
/// input schema of zeros, since a plan is made before any schema is read.
pub fn cost_key(task: &Task) -> CostKey {
    let digest = |text: &str| Digest(Sha256::digest(text.as_bytes()).into());
    let mut alone = task.clone();
    alone.id = "task".into();
    CostKey {
        role: task.role,
        kind: task.kind.clone(),
        fingerprint: digest(&format!(
            "moruna-plan kind\0{}\0{}",
            task.role.as_str(),
            task.kind
        )),
        settings: digest(&Job::new(vec![alone], Vec::new()).identity_json()),
        schema: Digest([0; 32]),
    }
}

/// The job's tasks in an order every edge goes forward, and each one's producers by place.
pub(crate) struct Graph {
    /// Every task's place, producers first.
    pub order: Vec<usize>,
    /// Each task's producers, by place, in the job's edge order.
    pub producers: Vec<Vec<usize>>,
    /// How many tasks take each task's output.
    pub consumers: Vec<u32>,
}

/// The job's graph, refused when it is not a DAG of the job's own tasks.
pub(crate) fn graph(job: &Job, planner: &PlannerId) -> Result<Graph, PlanError> {
    moruna_job::check_graph(job).map_err(PlanError::Job)?;
    let order = job
        .topological_order()
        .map_err(|cycle| PlanError::Planner {
            planner: planner.to_string(),
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
    let mut producers: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut consumers = vec![0u32; n];
    for edge in &job.edges {
        if let (Some(a), Some(b)) = (
            job.task_index(&edge.from.task),
            job.task_index(&edge.to.task),
        ) && !producers[b].contains(&a)
        {
            producers[b].push(a);
            consumers[a] += 1;
        }
    }
    Ok(Graph {
        order,
        producers,
        consumers,
    })
}

/// What a run is given, refused when no run can have it.
pub(crate) fn check_run(run: Resources) -> Result<(), PlanError> {
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
    Ok(())
}

/// One task as the model costs it.
#[derive(Clone, Debug)]
pub(crate) struct Costed {
    /// Its units: its rows over its morsel, or its producers' batches when its rows are not
    /// known.
    pub units: u32,
    /// Its rows in over one unit's rows: its units, counting a last unit cut short by its share.
    /// What a unit's rows, bytes, state, reads and spills are multiplied by for the task's.
    pub weight: f64,
    /// What one unit costs.
    pub unit: UnitCost,
    /// What is known of its kernel.
    pub kernel: Kernel,
    /// Rows it gives over every unit, when its rows are known.
    pub rows_out: Option<u64>,
    /// Bytes it gives.
    pub bytes_out: Option<u64>,
}

impl Costed {
    /// Its scaling curve, from its kernel's speed-ups.
    pub fn scaling(&self) -> Scaling {
        Scaling::from_speedups(self.kernel.speedup_2, self.kernel.speedup_4)
    }
}

/// Start-up of a process of `kernel`: before a run nothing says whether its image is cached, so
/// the cold figure, or the warm one when only it is known; none when neither is.
pub(crate) fn start_seconds(kernel: &Kernel) -> f64 {
    kernel
        .start_cold_seconds
        .or(kernel.start_warm_seconds)
        .filter(|s| s.is_finite() && *s >= 0.0)
        .unwrap_or(0.0)
}

fn fact(f: Fact) -> Option<bool> {
    match f {
        Fact::Yes => Some(true),
        Fact::No => Some(false),
        Fact::BySettings => None,
    }
}

/// Every task of `job` costed by `model`, by place: in the graph's order, each task's rows and
/// bytes in are its table's counts (a source) or what its producers give (any unknown, all
/// unknown); its morsel is [`moruna_cost::morsel`]'s for `share(place)` (the resources its step
/// is given), its units its rows over its morsel; one unit is predicted at its average rows.
/// Refused, naming the step (`ids[place]`), when the model answers with an estimate a plan
/// cannot carry.
pub(crate) fn cost(
    job: &Job,
    catalogue: &Catalogue,
    inputs: &PlanInputs<'_>,
    model: &dyn CostModel,
    graph: &Graph,
    ids: &[String],
    share: &dyn Fn(usize) -> Resources,
) -> Result<Vec<Costed>, PlanError> {
    let n = job.tasks.len();
    let disk = inputs
        .disk_bytes_per_second
        .unwrap_or(DISK_BYTES_PER_SECOND);
    let mut out: Vec<Option<Costed>> = vec![None; n];
    for &i in &graph.order {
        let task = &job.tasks[i];
        let given = share(i);
        let key = inputs
            .keys
            .get(&task.id)
            .cloned()
            .unwrap_or_else(|| cost_key(task));
        let mut kernel = inputs.kernels.get(&task.id).cloned().unwrap_or_default();
        if let Some(declaration) = catalogue.get(task.role, &task.kind) {
            kernel.deterministic = kernel.deterministic.or(fact(declaration.deterministic));
            kernel.preserves_columns = kernel
                .preserves_columns
                .or(fact(declaration.preserves_columns));
        }
        let producers = &graph.producers[i];
        let table = match task.role {
            Role::Source => inputs.tables.get(&task.id),
            _ => None,
        };
        let (rows, bytes) = if task.role == Role::Source {
            (table.and_then(|t| t.rows), table.and_then(|t| t.file_bytes))
        } else {
            let sum = |pick: fn(&Costed) -> Option<u64>| {
                producers.iter().try_fold(0u64, |acc, &p| {
                    out[p]
                        .as_ref()
                        .and_then(pick)
                        .map(|v| acc.saturating_add(v))
                })
            };
            (sum(|c| c.rows_out), sum(|c| c.bytes_out))
        };
        let mut unit_inputs = Inputs::default();
        if let (Some(t), Some(rows)) = (table, rows) {
            unit_inputs.data.columns = t
                .columns
                .iter()
                .map(|(name, stats)| Column {
                    name: name.clone(),
                    null_fraction: stats
                        .nulls
                        .filter(|_| rows > 0)
                        .map(|nulls| nulls as f64 / rows as f64),
                    distinct: stats.distinct,
                    ..Column::default()
                })
                .collect();
        }
        unit_inputs.kernel = kernel.clone();
        unit_inputs.resources.workers = Some(1);
        unit_inputs.resources.given = Capacity {
            workers: Some(given.workers),
            memory_bytes: Some(given.memory_start_bytes),
            disk_bytes: None,
            disk_bytes_per_second: Some(disk),
        };
        unit_inputs.resources.ceiling = Capacity {
            memory_bytes: Some(given.memory_ceiling_bytes),
            ..unit_inputs.resources.given.clone()
        };
        unit_inputs.job.dependencies = Some(producers.len() as u32);
        unit_inputs.job.consumers = Some(graph.consumers[i]);
        unit_inputs.job.contract_barrier = Some(task.barrier);

        let units = match rows {
            Some(rows) => {
                let mut template = unit_inputs.clone();
                template.data.rows = Some(rows.max(1));
                template.data.bytes = bytes;
                let per_unit = given.memory_start_bytes as f64 / f64::from(given.workers.max(1));
                let m = moruna_cost::morsel(model, &key, &template, rows, per_unit, given.workers);
                m.units.max(1)
            }
            None => {
                let most = producers
                    .iter()
                    .filter_map(|&p| out[p].as_ref().map(|c| c.units))
                    .max()
                    .unwrap_or(1);
                most.max(1)
            }
        };
        if let Some(rows) = rows {
            let k = u64::from(units);
            unit_inputs.data.rows = Some(rows.div_ceil(k));
            unit_inputs.data.bytes = bytes.map(|b| b.div_ceil(k));
        }
        let unit = model.predict(&key, &unit_inputs);
        unit.check().map_err(|reason| PlanError::Model {
            model: inputs.model_id().unwrap_or_else(|| model.id().name),
            step: ids[i].clone(),
            reason,
        })?;
        // What it gives over its rows: each unit's answer per row in, times every row in.
        let weight = match (rows, unit_inputs.data.rows) {
            (Some(r), Some(per)) if per > 0 => r as f64 / per as f64,
            _ => f64::from(units),
        };
        let total = |p50: f64| rows.map(|_| (p50 * weight).round().max(0.0) as u64);
        out[i] = Some(Costed {
            units,
            weight,
            rows_out: total(unit.rows_out.p50),
            bytes_out: total(unit.bytes_out.p50),
            unit,
            kernel,
        });
    }
    out.into_iter()
        .enumerate()
        .map(|(i, c)| {
            c.ok_or_else(|| PlanError::Planner {
                planner: "costing".into(),
                reason: format!("task {} is not in the job's order", job.tasks[i].id),
            })
        })
        .collect()
}

/// What a step of `costed`'s units costs over every unit when it is given `workers` and nothing
/// simulates it (SDD 26 f.2, with a model): `min(workers, units)` units at once, scaling by the
/// kind's curve; time and worker time over the units ([`moruna_cost::Estimate::sum`]), time
/// divided by the curve's speed-up and the start-up added; the peak that many units' peaks; rows,
/// bytes, state, reads and spills over its rows ([`Costed::weight`]); retained memory one
/// unit's.
pub(crate) fn aggregate(costed: &Costed, workers: u32, start: f64) -> UnitCost {
    let u = &costed.unit;
    let n = costed.units;
    let w = costed.weight;
    let at_once = workers.min(n).max(1);
    let speedup = costed.scaling().speedup(at_once).max(1.0);
    let mut duration = u.duration.sum(f64::from(n)).scaled(1.0 / speedup);
    duration.p50 += start;
    duration.p95 += start;
    UnitCost {
        duration,
        worker_time: u.worker_time.sum(f64::from(n)),
        peak_memory: u.peak_memory.scaled(f64::from(at_once)),
        retained_memory: u.retained_memory,
        rows_out: u.rows_out.sum(w),
        bytes_out: u.bytes_out.sum(w),
        state_growth: u.state_growth.sum(w),
        read_bytes: u.read_bytes.sum(w),
        spill_bytes: u.spill_bytes.sum(w),
    }
}
