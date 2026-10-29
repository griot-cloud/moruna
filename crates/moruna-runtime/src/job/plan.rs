//! A run's plan (SDD 26 f.5): a run starts from a plan. The caller's, or, when it gives none, the
//! default planner's ([`moruna_plan::Levels`]) for the run's resources: the job's budget where it
//! sets one, what discovery finds where it leaves one to discovery.

use moruna_discovery::DiscoveryInput;
use moruna_job::{Catalogue, Job, SpecError};
use moruna_kernel::{MorunaError, Result};
use moruna_plan::{Levels, Plan, PlanError, PlanInputs, Resources, plan_with};

use super::refusal;

/// The resources discovery finds for `job`: the memory ceiling and the CPUs of the machine (or
/// the explicit figures the document gives, which discovery takes as they are).
pub fn discovered(job: &Job) -> Result<Resources> {
    let found = moruna_discovery::discover(&DiscoveryInput {
        explicit_budget: job.budget.memory_bytes,
        explicit_cpu: job.budget.cpu,
        explicit_staging_dir: job.staging.dir.as_deref().map(std::path::PathBuf::from),
        explicit_spill_limit: job.staging.limit_bytes,
        profile_override: None,
    })?;
    let workers = found
        .limits
        .cpu_quota
        .ceil()
        .clamp(1.0, f64::from(u32::MAX)) as u32;
    Ok(Resources {
        workers,
        memory_start_bytes: found.limits.memory_ceiling,
        memory_ceiling_bytes: found.limits.memory_ceiling,
    })
}

/// The default planner's plan of `job` for a run given `discovered` where the job's budget
/// leaves a figure to discovery, checked (`moruna_plan::plan_with`). Every kind is resolved by
/// the build, not here: the planner reads the job's DAG and budget, so it is given no
/// declaration.
pub fn levels(job: &Job, discovered: Resources) -> Result<Plan> {
    plan_with(
        &Levels,
        job,
        &Catalogue::empty(),
        &PlanInputs::for_job(job, discovered),
    )
    .map_err(plan_refusal)
}

/// The plan of a run of `job` when the caller gives none: [`levels`] for what discovery finds.
pub fn default_plan(job: &Job) -> Result<Plan> {
    levels(job, discovered(job)?)
}

/// A planning failure as the runtime's error. A job the planner refused is refused at its first
/// field, as the build refuses it (`spec refused: <field>: <reason>`); anything else is the plan
/// refused, `Plan`, naming what was wrong.
pub fn plan_refusal(error: PlanError) -> MorunaError {
    match error {
        PlanError::Job(refusals) => match refusals.into_iter().next() {
            Some(first) => refusal(SpecError::new(first.field, first.message)),
            None => MorunaError::Plan("the job was refused and no refusal was named".into()),
        },
        other => MorunaError::Plan(other.to_string()),
    }
}

/// The places of the job's tasks in the order the plan starts them: its steps in order, each
/// step's tasks in its order. A plan that passes `check_plan` lists every task once, so this is
/// every task, in an order in which every edge goes forward.
pub fn task_order(job: &Job, plan: &Plan) -> Vec<usize> {
    plan.steps
        .iter()
        .flat_map(|s| s.tasks.iter())
        .filter_map(|t| job.task_index(t))
        .collect()
}

/// Why [`explain`] gave no plan.
#[derive(Debug)]
pub enum ExplainError {
    /// The kinds of the registry cannot be checked against (a schema outside the subset the
    /// catalogue reads, SDD 22 JB-I7).
    Kinds(String),
    /// Discovery failed, so the run's resources are not known.
    Discovery(MorunaError),
    /// The job, the planner's name or the planner refused.
    Plan(PlanError),
}

impl core::fmt::Display for ExplainError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ExplainError::Kinds(why) => {
                write!(
                    f,
                    "the kinds of this binary cannot be checked against: {why}"
                )
            }
            ExplainError::Discovery(e) => write!(f, "discovery failed: {e}"),
            ExplainError::Plan(e) => write!(f, "{e}"),
        }
    }
}

impl ExplainError {
    /// As the runtime's error: what the Python surface raises.
    pub fn into_error(self) -> MorunaError {
        match self {
            ExplainError::Kinds(why) => MorunaError::Config {
                name: "kinds",
                msg: why,
            },
            ExplainError::Discovery(e) => e,
            ExplainError::Plan(e) => plan_refusal(e),
        }
    }
}

/// `explain` as `moruna explain` and `moruna.explain(...)` run it (SDD 26 d.5): the plan of `job`
/// by the planner `planner` names (the default when it names none), for the run's resources
/// (the job's budget, what discovery finds where it leaves one to discovery), the job checked
/// first against the kinds `kinds` declares.
pub fn explain(
    job: &Job,
    planner: Option<&str>,
    kinds: &crate::kinds::Registry,
) -> std::result::Result<Plan, ExplainError> {
    let catalogue =
        Catalogue::from_listing(kinds.listing()).map_err(|e| ExplainError::Kinds(e.to_string()))?;
    let found = discovered(job).map_err(ExplainError::Discovery)?;
    moruna_plan::explain(
        job,
        &catalogue,
        &PlanInputs::for_job(job, found),
        &moruna_plan::Planners::standard(),
        planner.unwrap_or(moruna_plan::DEFAULT_PLANNER),
    )
    .map_err(ExplainError::Plan)
}
