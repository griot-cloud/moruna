//! `explain` (SDD 26 d.4): a job in, Moruna's plan of it out, checked, before anything runs.

use moruna_job::{Catalogue, Job};

use crate::check::check_plan;
use crate::plan::Plan;
use crate::planner::{PlanError, PlanInputs, Planner, Planners};

/// The plan of `job` by the planner `planner` names in `planners`: the job is checked against
/// `catalogue` first (`moruna_job::check`, every refusal), then planned and the plan checked
/// ([`plan_with`]). What an embedder calls in its own process to show a job's plan before it
/// submits it, and what `moruna explain` prints.
pub fn explain(
    job: &Job,
    catalogue: &Catalogue,
    inputs: &PlanInputs<'_>,
    planners: &Planners,
    planner: &str,
) -> Result<Plan, PlanError> {
    let planner = planners.get(planner)?;
    moruna_job::check(job, catalogue).map_err(PlanError::Job)?;
    plan_with(planner, job, catalogue, inputs)
}

/// `planner`'s plan of `job`, asserted to be a plan of it (PN-I4): the plan names the planner
/// that made it and passes [`check_plan`]. Every caller of a planner goes through here, so no
/// plan that fails the check is ever run or shown.
pub fn plan_with(
    planner: &dyn Planner,
    job: &Job,
    catalogue: &Catalogue,
    inputs: &PlanInputs<'_>,
) -> Result<Plan, PlanError> {
    let plan = planner.plan(job, catalogue, inputs)?;
    let id = planner.id();
    if plan.planner != id {
        return Err(PlanError::Planner {
            planner: id.to_string(),
            reason: format!("its plan names planner {} as its maker", plan.planner),
        });
    }
    if plan.id != plan.content_id() {
        return Err(PlanError::Planner {
            planner: id.to_string(),
            reason: format!(
                "its plan's id {} is not the plan's content address {}",
                plan.id,
                plan.content_id()
            ),
        });
    }
    check_plan(&plan, job).map_err(PlanError::Refused)?;
    Ok(plan)
}
