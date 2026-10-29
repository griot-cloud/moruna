//! The planner port (SDD 26 d.2): [`Planner`], what it is given ([`PlanInputs`]), the cost
//! model a planner may ask ([`Estimator`]), and [`Planners`], the registry a setting chooses
//! from.

use std::collections::BTreeMap;
use std::sync::Arc;

use moruna_job::{BudgetDoc, Catalogue, Declaration, Job, Refusal, Task};

use crate::check::PlanRefusal;
use crate::plan::{Plan, PlannerId, Resources, UnitCost};

/// The planner a run uses when nothing chooses another.
pub const DEFAULT_PLANNER: &str = crate::levels::Levels::NAME;

/// Why a job was not planned.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum PlanError {
    /// The job itself was refused: every refusal `moruna_job::check` (or `check_graph`) found.
    #[error("job refused: {}", first(.0))]
    Job(Vec<Refusal>),
    /// No planner of that name is registered.
    #[error("no planner `{name}` is registered; the planners are {}", .known.join(", "))]
    UnknownPlanner {
        /// The name asked for.
        name: String,
        /// The names registered.
        known: Vec<String>,
    },
    /// A planner was not added to a registry.
    #[error("planner `{name}` refused: {reason}")]
    Registry {
        /// Its name.
        name: String,
        /// Why.
        reason: String,
    },
    /// What the planner was given cannot be planned with.
    #[error("plan inputs refused: {field}: {reason}")]
    Inputs {
        /// The input, as a path (`resources.workers`).
        field: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The planner could not plan the job.
    #[error("planner {planner} could not plan the job: {reason}")]
    Planner {
        /// The planner.
        planner: String,
        /// Why.
        reason: String,
    },
    /// The cost model failed, or answered with an estimate a plan cannot carry, for a step.
    #[error("cost model {model} for step {step}: {reason}")]
    Model {
        /// The model's id.
        model: String,
        /// The step it was asked about.
        step: String,
        /// Why.
        reason: String,
    },
    /// The plan a planner made is not a plan of the job (`check_plan`): a defect of the planner.
    #[error("plan refused: {}", first_refusal(.0))]
    Refused(Vec<PlanRefusal>),
}

fn first(refusals: &[Refusal]) -> String {
    match refusals {
        [] => "no refusal was named".to_string(),
        [one] => one.to_string(),
        [one, rest @ ..] => format!("{one} (and {} more)", rest.len()),
    }
}

fn first_refusal(refusals: &[PlanRefusal]) -> String {
    match refusals {
        [] => "no refusal was named".to_string(),
        [one] => one.to_string(),
        [one, rest @ ..] => format!("{one} (and {} more)", rest.len()),
    }
}

/// What the plan knows of one column of a table: counts, never values. Its bounds are tenant
/// values and are not here (SDD 26 PN-I7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColumnStats {
    /// Rows whose value is null.
    pub nulls: Option<u64>,
    /// Distinct values.
    pub distinct: Option<u64>,
}

/// What the plan knows of the table a source reads: counts only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableStats {
    /// Rows.
    pub rows: Option<u64>,
    /// Bytes of its files.
    pub file_bytes: Option<u64>,
    /// Its columns by name.
    pub columns: BTreeMap<String, ColumnStats>,
}

/// What a planner is given beside the job and the catalogue (SDD 26 d.2).
#[derive(Clone)]
pub struct PlanInputs<'a> {
    /// The statistics of the table each source task reads, by the task's id.
    pub tables: BTreeMap<String, TableStats>,
    /// What the run is given.
    pub resources: Resources,
    /// A cost model to ask about each step; `None`: the planner's declared defaults.
    pub model: Option<&'a dyn Estimator>,
}

impl<'a> PlanInputs<'a> {
    /// Inputs of `resources`, with no statistics and no model.
    pub fn new(resources: Resources) -> PlanInputs<'a> {
        PlanInputs {
            tables: BTreeMap::new(),
            resources,
            model: None,
        }
    }

    /// The inputs of a run of `job`: the job's budget where it sets one, `discovered` (what
    /// the machine gives, MH 4.1's discovery) where it leaves one to discovery. Workers are the
    /// budget's CPUs rounded up; the memory ceiling is `budget.elastic.memory_max_bytes` when the
    /// budget can grow, its start otherwise.
    pub fn for_job(job: &Job, discovered: Resources) -> PlanInputs<'a> {
        PlanInputs::new(budget_resources(&job.budget, discovered))
    }

    /// The same inputs, asking `model` about each step.
    pub fn with_model(mut self, model: &'a dyn Estimator) -> PlanInputs<'a> {
        self.model = Some(model);
        self
    }

    /// The same inputs, with `stats` for the table source task `task` reads.
    pub fn with_table(mut self, task: impl Into<String>, stats: TableStats) -> PlanInputs<'a> {
        self.tables.insert(task.into(), stats);
        self
    }
}

impl core::fmt::Debug for PlanInputs<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PlanInputs")
            .field("tables", &self.tables)
            .field("resources", &self.resources)
            .field("model", &self.model.map(|m| m.id()))
            .finish()
    }
}

/// What a budget gives, `discovered` filling what it leaves to discovery.
pub fn budget_resources(budget: &BudgetDoc, discovered: Resources) -> Resources {
    let memory_start = budget.memory_bytes.unwrap_or(discovered.memory_start_bytes);
    let elastic = budget.elastic.clone().unwrap_or_default();
    let memory_ceiling = match (elastic.memory_max_bytes, budget.memory_bytes) {
        (Some(max), _) => max.max(memory_start),
        (None, Some(_)) => memory_start,
        (None, None) => discovered.memory_ceiling_bytes.max(memory_start),
    };
    let workers = match budget.cpu {
        Some(cpu) if cpu.is_finite() && cpu > 0.0 => cpu.ceil().min(f64::from(u32::MAX)) as u32,
        _ => discovered.workers,
    };
    Resources {
        workers,
        memory_start_bytes: memory_start,
        memory_ceiling_bytes: memory_ceiling,
    }
}

/// What a planner asks a cost model about one step (SDD 26 d.2).
pub struct StepQuery<'a> {
    /// The job.
    pub job: &'a Job,
    /// The step's tasks, in the job's order.
    pub tasks: Vec<&'a Task>,
    /// Each task's declaration, when the catalogue has its kind.
    pub declarations: Vec<Option<&'a Declaration>>,
    /// For a source step, the statistics of the table it reads, when the embedder gave them.
    pub table: Option<&'a TableStats>,
    /// The rows the step takes in, as the planner expects them; `None` when not known.
    pub rows_in: Option<u64>,
    /// The bytes it takes in.
    pub bytes_in: Option<u64>,
    /// What the step is given.
    pub resources: Resources,
}

/// The cost model a planner asks about a step: the seam `moruna-cost`'s model is adapted to
/// (SDD 26 d.2, `architecture/adaptive-runtime.md` section 5). Defined here while that crate is
/// not on `main`; its answer has the shape of section 4's `UnitCost` field for field.
pub trait Estimator: Send + Sync {
    /// The model's name and version, stored in every plan it was asked for.
    fn id(&self) -> String;
    /// What the step costs over every batch it sees.
    fn estimate(&self, query: &StepQuery<'_>) -> Result<UnitCost, String>;
}

/// A planner: one job in, Moruna's plan of it out (SDD 26 d.2). A planner is an adapter of this
/// port, registered in [`Planners`] by its name and chosen by a setting; what it returns is
/// checked against the job ([`crate::check_plan`]) before anything runs it.
pub trait Planner: Send + Sync {
    /// Its name and version.
    fn id(&self) -> PlannerId;
    /// The plan of `job`, whose kinds `catalogue` declares, given `inputs`.
    fn plan(
        &self,
        job: &Job,
        catalogue: &Catalogue,
        inputs: &PlanInputs<'_>,
    ) -> Result<Plan, PlanError>;
}

/// The planners a setting can name, by name (SDD 26 d.2).
#[derive(Clone, Default)]
pub struct Planners {
    by_name: BTreeMap<String, Arc<dyn Planner>>,
}

impl Planners {
    /// No planner.
    pub fn empty() -> Planners {
        Planners::default()
    }

    /// Moruna's planners: `levels` ([`crate::Levels`]).
    pub fn standard() -> Planners {
        let mut planners = Planners::empty();
        planners.by_name.insert(
            crate::levels::Levels::NAME.to_string(),
            Arc::new(crate::levels::Levels),
        );
        planners
    }

    /// Register `planner` under its name. Refused when the name is not a kind name (letters,
    /// digits, `_`, `-`, `.`, starting with a letter) or is taken.
    pub fn add(&mut self, planner: Arc<dyn Planner>) -> Result<(), PlanError> {
        let name = planner.id().name;
        if !moruna_job::is_kind_name(&name) {
            return Err(PlanError::Registry {
                name,
                reason: "a planner's name follows the rule for a kind's name".to_string(),
            });
        }
        if self.by_name.contains_key(&name) {
            return Err(PlanError::Registry {
                name,
                reason: "a planner of that name is registered already".to_string(),
            });
        }
        self.by_name.insert(name, planner);
        Ok(())
    }

    /// The planner called `name`.
    pub fn get(&self, name: &str) -> Result<&dyn Planner, PlanError> {
        self.by_name
            .get(name)
            .map(|p| p.as_ref())
            .ok_or_else(|| PlanError::UnknownPlanner {
                name: name.to_string(),
                known: self.names(),
            })
    }

    /// The registered names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.by_name.keys().cloned().collect()
    }
}
