//! The plan's internal check (SDD 26 d.3, f.4): whether a plan a planner made is a plan of the
//! job. An assertion on planners' output, run after every `plan` call and before anything runs
//! a plan; not a gate for people, who do not edit plans.

use std::collections::{BTreeMap, BTreeSet};

use moruna_job::Job;
use serde::Serialize;

use crate::plan::{Plan, Resources, job_digest};

/// What is wrong with a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanRefusalCode {
    /// The plan names another job's digest.
    JobMismatch,
    /// Two steps have one id.
    DuplicateStep,
    /// A step runs a task the job does not have.
    UnknownTask,
    /// A task is run by two steps.
    DuplicateTask,
    /// A task of the job is run by no step.
    MissingTask,
    /// A step runs no task.
    EmptyStep,
    /// A step waits for a step the plan does not have.
    UnknownDependency,
    /// A step does not wait for a step whose output it takes, or waits for one that does not
    /// start before it.
    DependencyViolated,
    /// The steps are not listed in the order their groups start.
    GroupOrder,
    /// A step moves work across a contract barrier.
    BarrierCrossed,
    /// A step, a group or the plan is given more than the job's ceiling.
    OverCeiling,
}

/// One thing wrong with a plan: the step at fault (`None` for the plan as a whole), a code and a
/// sentence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{}: {message}", .step.as_deref().unwrap_or("plan"))]
pub struct PlanRefusal {
    /// The step, by id; `None` for the plan as a whole.
    pub step: Option<String>,
    /// What kind of fault.
    pub code: PlanRefusalCode,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl PlanRefusal {
    fn at(step: &str, code: PlanRefusalCode, message: impl Into<String>) -> PlanRefusal {
        PlanRefusal {
            step: Some(step.to_string()),
            code,
            message: message.into(),
        }
    }

    fn plan(code: PlanRefusalCode, message: impl Into<String>) -> PlanRefusal {
        PlanRefusal {
            step: None,
            code,
            message: message.into(),
        }
    }
}

/// The ceiling a job sets: its budget's elastic maxima where it can grow, its start where it
/// cannot, and what the plan was given where the job leaves either to discovery.
pub fn job_ceiling(job: &Job, given: Resources) -> Resources {
    let budget = &job.budget;
    let elastic = budget.elastic.clone().unwrap_or_default();
    let memory = match (elastic.memory_max_bytes, budget.memory_bytes) {
        (Some(max), Some(start)) => max.max(start),
        (Some(max), None) => max,
        (None, Some(start)) => start,
        (None, None) => given.memory_ceiling_bytes,
    };
    let cpus = budget
        .cpu
        .filter(|c| c.is_finite() && *c > 0.0)
        .map(|c| c.ceil().min(f64::from(u32::MAX)) as u32);
    let workers = match (elastic.cpu_max, cpus) {
        (Some(max), Some(start)) => max.max(start),
        (Some(max), None) => max,
        (None, Some(start)) => start,
        (None, None) => given.workers,
    };
    Resources {
        workers,
        memory_start_bytes: budget.memory_bytes.unwrap_or(given.memory_start_bytes),
        memory_ceiling_bytes: memory,
    }
}

/// Whether `plan` is a plan of `job` (SDD 26 f.4): it names the job's digest; its step ids are
/// unique; every task of the job is run by exactly one step and no step runs a task the job
/// does not have; the steps are listed in the order their groups start; every step waits for
/// the steps whose output it takes, and every step it waits for is in an earlier group; no step
/// moves work across a contract barrier (a barrier task fused with another, or both ends of a
/// barrier edge in one step); and no step, no group and not the plan is given more than the
/// job's ceiling. Every refusal, in that order.
pub fn check_plan(plan: &Plan, job: &Job) -> Result<(), Vec<PlanRefusal>> {
    use PlanRefusalCode as C;
    let mut out = Vec::new();
    let digest = job_digest(job);
    if plan.job_digest != digest {
        out.push(PlanRefusal::plan(
            C::JobMismatch,
            format!(
                "the plan is of job {}, and this job is {digest}",
                plan.job_digest
            ),
        ));
    }

    // Step ids, and which step runs each task.
    let mut by_id: BTreeMap<&str, usize> = BTreeMap::new();
    let mut runs: BTreeMap<&str, &str> = BTreeMap::new();
    for (at, step) in plan.steps.iter().enumerate() {
        if by_id.insert(step.id.as_str(), at).is_some() {
            out.push(PlanRefusal::at(
                &step.id,
                C::DuplicateStep,
                "another step has this id",
            ));
        }
        if step.tasks.is_empty() {
            out.push(PlanRefusal::at(&step.id, C::EmptyStep, "runs no task"));
        }
        for task in &step.tasks {
            if job.task(task).is_none() {
                out.push(PlanRefusal::at(
                    &step.id,
                    C::UnknownTask,
                    format!("runs task `{task}`, which the job does not have"),
                ));
                continue;
            }
            if let Some(first) = runs.insert(task.as_str(), step.id.as_str()) {
                runs.insert(task.as_str(), first);
                out.push(PlanRefusal::at(
                    &step.id,
                    C::DuplicateTask,
                    format!("runs task `{task}`, which step {first} runs already"),
                ));
            }
        }
    }
    for task in &job.tasks {
        if !runs.contains_key(task.id.as_str()) {
            out.push(PlanRefusal::plan(
                C::MissingTask,
                format!("no step runs task `{}`", task.id),
            ));
        }
    }

    // The order of groups, and what each step waits for.
    let mut last_group = 0u32;
    for (at, step) in plan.steps.iter().enumerate() {
        if at > 0 && step.group < last_group {
            out.push(PlanRefusal::at(
                &step.id,
                C::GroupOrder,
                format!(
                    "is in group {} and is listed after a step of group {last_group}",
                    step.group
                ),
            ));
        }
        last_group = last_group.max(step.group);
        for dep in &step.depends_on {
            match by_id.get(dep.as_str()).map(|&d| &plan.steps[d]) {
                None => out.push(PlanRefusal::at(
                    &step.id,
                    C::UnknownDependency,
                    format!("waits for step {dep}, which the plan does not have"),
                )),
                Some(before) if before.group >= step.group => out.push(PlanRefusal::at(
                    &step.id,
                    C::DependencyViolated,
                    format!(
                        "waits for step {dep} of group {}, which does not start before its own \
                         group {}",
                        before.group, step.group
                    ),
                )),
                Some(_) => {}
            }
        }
    }
    for edge in &job.edges {
        let (Some(&from), Some(&to)) = (
            runs.get(edge.from.task.as_str()),
            runs.get(edge.to.task.as_str()),
        ) else {
            continue;
        };
        if from == to {
            // Inside one step, its tasks are listed in an order in which every edge goes forward.
            let listed = by_id.get(from).map(|&s| &plan.steps[s].tasks);
            let place = |task: &str| listed.and_then(|l| l.iter().position(|t| t == task));
            if let (Some(a), Some(b)) = (place(&edge.from.task), place(&edge.to.task))
                && a > b
            {
                out.push(PlanRefusal::at(
                    from,
                    C::DependencyViolated,
                    format!(
                        "lists task `{}` before `{}`, whose output it takes",
                        edge.to.task, edge.from.task
                    ),
                ));
            }
            continue;
        }
        let Some(consumer) = by_id.get(to).map(|&c| &plan.steps[c]) else {
            continue;
        };
        if !consumer.depends_on.iter().any(|d| d == from) {
            out.push(PlanRefusal::at(
                to,
                C::DependencyViolated,
                format!(
                    "takes {}'s output from step {from} and does not wait for it",
                    edge.from
                ),
            ));
        }
    }

    // Contract barriers: nothing is fused across one.
    for step in &plan.steps {
        let tasks: BTreeSet<&str> = step.tasks.iter().map(String::as_str).collect();
        if tasks.len() > 1 {
            for &task in &tasks {
                if job.task(task).is_some_and(|t| t.barrier) {
                    out.push(PlanRefusal::at(
                        &step.id,
                        C::BarrierCrossed,
                        format!("fuses barrier task `{task}` with other tasks"),
                    ));
                }
            }
        }
        for edge in job.edges.iter().filter(|e| e.barrier) {
            if tasks.contains(edge.from.task.as_str()) && tasks.contains(edge.to.task.as_str()) {
                out.push(PlanRefusal::at(
                    &step.id,
                    C::BarrierCrossed,
                    format!(
                        "runs both ends of the barrier edge from {} to {}",
                        edge.from, edge.to
                    ),
                ));
            }
        }
    }

    // Ceilings: the plan's within the job's, each step's within the plan's, each group's start
    // within the plan's.
    let ceiling = job_ceiling(job, plan.resources);
    let given = plan.resources;
    if given.workers > ceiling.workers || given.memory_ceiling_bytes > ceiling.memory_ceiling_bytes
    {
        out.push(PlanRefusal::plan(
            C::OverCeiling,
            format!(
                "is given {} workers and {} bytes at most, past the job's ceiling of {} workers \
                 and {} bytes",
                given.workers,
                given.memory_ceiling_bytes,
                ceiling.workers,
                ceiling.memory_ceiling_bytes
            ),
        ));
    }
    let mut group_start: BTreeMap<u32, u64> = BTreeMap::new();
    for step in &plan.steps {
        let r = step.resources;
        if r.workers > given.workers
            || r.memory_ceiling_bytes > given.memory_ceiling_bytes
            || r.memory_start_bytes > r.memory_ceiling_bytes
        {
            out.push(PlanRefusal::at(
                &step.id,
                C::OverCeiling,
                format!(
                    "is given {} workers and {} to {} bytes, and the run has {} workers and {} \
                     bytes at most",
                    r.workers,
                    r.memory_start_bytes,
                    r.memory_ceiling_bytes,
                    given.workers,
                    given.memory_ceiling_bytes
                ),
            ));
            continue;
        }
        let sum = group_start.entry(step.group).or_default();
        let before = *sum;
        *sum = sum.saturating_add(r.memory_start_bytes);
        if *sum > given.memory_ceiling_bytes && before <= given.memory_ceiling_bytes {
            out.push(PlanRefusal::at(
                &step.id,
                C::OverCeiling,
                format!(
                    "takes group {}'s memory at the start to {} bytes, past the run's {} bytes",
                    step.group, *sum, given.memory_ceiling_bytes
                ),
            ));
        }
    }

    if out.is_empty() { Ok(()) } else { Err(out) }
}
