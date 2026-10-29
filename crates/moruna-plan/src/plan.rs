//! The plan document, version 1 (SDD 26 d.1, e.1): its model, its ids and its rendering.
//!
//! [`Plan`] is the in-memory model. Its form on the wire is FlatBuffers (`schema/plan.fbs`,
//! [`Plan::from_flatbuffer`], [`Plan::to_flatbuffer`]); its JSON ([`Plan::to_json`]) is a
//! rendering for people, and nothing reads it back.

use std::collections::BTreeMap;

use moruna_job::Job;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// The version of the plan document this build writes and reads.
pub const PLAN_VERSION: u32 = 1;

/// What an estimate counts (`architecture/adaptive-runtime.md` section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// Seconds of wall or worker time.
    Seconds,
    /// Bytes.
    Bytes,
    /// Rows.
    Rows,
}

/// Where an estimate's numbers came from (section 5.4), ordered from most to least known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// This function's history, at least one unit of which ran in a real run.
    Measured,
    /// This function's history, all of it from warm-up.
    WarmUp,
    /// The kind's prior: every function of the kind, pooled.
    Prior,
    /// A declared default.
    Default,
}

/// A distribution, not a number: its median, its 95th percentile, and what it rests on
/// (section 4).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Estimate {
    /// The median.
    pub p50: f64,
    /// The 95th percentile; never below the median.
    pub p95: f64,
    /// What it counts.
    pub unit: Unit,
    /// Which fallback answered.
    pub basis: Basis,
    /// The observations it rests on.
    pub observations: u32,
}

impl Estimate {
    /// A declared default: `p50` and `p95` of `unit`, resting on no observation.
    pub fn declared(p50: f64, p95: f64, unit: Unit) -> Estimate {
        Estimate {
            p50,
            p95,
            unit,
            basis: Basis::Default,
            observations: 0,
        }
    }

    /// Whether it is an estimate of `unit` a reader can use: finite, not negative, its 95th
    /// percentile not below its median. The reason when it is not.
    pub fn check(&self, unit: Unit) -> Result<(), String> {
        if self.unit != unit {
            return Err(format!(
                "counts {}, and this estimate counts {}",
                unit_name(self.unit),
                unit_name(unit)
            ));
        }
        if !(self.p50.is_finite() && self.p95.is_finite()) {
            return Err(format!(
                "p50 {} and p95 {} are not both finite",
                self.p50, self.p95
            ));
        }
        if self.p50 < 0.0 {
            return Err(format!("p50 {} is negative", self.p50));
        }
        if self.p95 < self.p50 {
            return Err(format!("p95 {} is below p50 {}", self.p95, self.p50));
        }
        Ok(())
    }
}

fn unit_name(unit: Unit) -> &'static str {
    match unit {
        Unit::Seconds => "seconds",
        Unit::Bytes => "bytes",
        Unit::Rows => "rows",
    }
}

/// What a step takes from the run and gives, over the batches the plan expects it to see: the
/// fields of `UnitCost` (section 4), each its own estimate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct UnitCost {
    /// Wall time.
    pub duration: Estimate,
    /// Worker time.
    pub worker_time: Estimate,
    /// Peak memory.
    pub peak_memory: Estimate,
    /// Memory still held after it.
    pub retained_memory: Estimate,
    /// Rows out.
    pub rows_out: Estimate,
    /// Bytes out.
    pub bytes_out: Estimate,
    /// State added.
    pub state_growth: Estimate,
    /// Bytes read from disk.
    pub read_bytes: Estimate,
    /// Bytes spilled to disk.
    pub spill_bytes: Estimate,
}

impl UnitCost {
    /// Every field a declared zero of its unit.
    pub fn zero() -> UnitCost {
        let s = Estimate::declared(0.0, 0.0, Unit::Seconds);
        let b = Estimate::declared(0.0, 0.0, Unit::Bytes);
        let r = Estimate::declared(0.0, 0.0, Unit::Rows);
        UnitCost {
            duration: s,
            worker_time: s,
            peak_memory: b,
            retained_memory: b,
            rows_out: r,
            bytes_out: b,
            state_growth: b,
            read_bytes: b,
            spill_bytes: b,
        }
    }

    /// Each field by its name, with the unit it counts, in the order of section 4.
    pub fn fields(&self) -> [(&'static str, Unit, &Estimate); 9] {
        [
            ("duration", Unit::Seconds, &self.duration),
            ("worker_time", Unit::Seconds, &self.worker_time),
            ("peak_memory", Unit::Bytes, &self.peak_memory),
            ("retained_memory", Unit::Bytes, &self.retained_memory),
            ("rows_out", Unit::Rows, &self.rows_out),
            ("bytes_out", Unit::Bytes, &self.bytes_out),
            ("state_growth", Unit::Bytes, &self.state_growth),
            ("read_bytes", Unit::Bytes, &self.read_bytes),
            ("spill_bytes", Unit::Bytes, &self.spill_bytes),
        ]
    }

    /// Whether every field is a usable estimate of its unit; the first that is not, by name.
    pub fn check(&self) -> Result<(), String> {
        for (name, unit, estimate) in self.fields() {
            estimate
                .check(unit)
                .map_err(|why| format!("{name}: {why}"))?;
        }
        Ok(())
    }
}

/// Workers, and memory at the start and at most: what a run is given, and what a step is given
/// of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Resources {
    /// Workers (cores) it may use at once.
    pub workers: u32,
    /// Memory at the start, in bytes.
    pub memory_start_bytes: u64,
    /// Memory at most, in bytes.
    pub memory_ceiling_bytes: u64,
}

/// One step of a plan: a task of the job, or a fused group of them, with what it waits for, what
/// may run beside it, what it is given and what it is estimated to cost.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Step {
    /// Stable: the same job planned by the same planner gives the same id ([`step_id`]).
    pub id: String,
    /// The job's tasks it runs, by id: one, or a fused group.
    pub tasks: Vec<String>,
    /// The steps it waits for, by id.
    pub depends_on: Vec<String>,
    /// Its group: the steps of one group may run beside each other, and groups start in their
    /// order.
    pub group: u32,
    /// What it is given.
    pub resources: Resources,
    /// What it is estimated to take and give, over every batch it sees.
    pub cost: UnitCost,
}

/// A planner's name and version, stored in every plan it makes: a change of either is a change
/// of the steps' ids.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct PlannerId {
    /// Its name, which a setting chooses it by.
    pub name: String,
    /// Its version.
    pub version: u32,
}

impl PlannerId {
    /// Planner `name` at `version`.
    pub fn new(name: impl Into<String>, version: u32) -> PlannerId {
        PlannerId {
            name: name.into(),
            version,
        }
    }
}

impl core::fmt::Display for PlannerId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// A median and a 95th percentile.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Range {
    /// The median.
    pub p50: f64,
    /// The 95th percentile.
    pub p95: f64,
}

impl Range {
    /// Whether it is finite, not negative and ordered; the reason when not.
    pub fn check(&self) -> Result<(), String> {
        Estimate::declared(self.p50, self.p95, Unit::Bytes).check(Unit::Bytes)
    }
}

/// The job's totals (section 4): its finish, its critical path, its peak memory and the
/// resources it holds.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Totals {
    /// The weakest basis of any step's estimate.
    pub basis: Basis,
    /// The fewest observations any step's estimate rests on.
    pub observations: u32,
    /// Seconds from the first step's start to the last step's end.
    pub finish: Range,
    /// The steps on the longest path, in order.
    pub critical_path: Vec<String>,
    /// The most memory held at once.
    pub peak_memory_bytes: Range,
    /// Memory held times how long it is held.
    pub memory_byte_seconds: Range,
    /// Worker time over every step.
    pub worker_seconds: Range,
    /// Bytes read from and spilled to disk.
    pub disk_bytes: Range,
}

impl Totals {
    /// The totals of `steps` in plan order, by the plan's own arithmetic rather than by
    /// simulation (SDD 26 f.3): the finish is the longest path of the steps' durations through
    /// their dependencies (the p50 path and the p95 path each on its own), the critical path the
    /// path of the p50 finish (more steps first when two finish together, then the earlier in
    /// plan order), the peak the largest group's sum of its steps' peaks, and the resources
    /// held each step's sum. A dependency on a step not before it in the list is not followed.
    pub fn of(steps: &[Step]) -> Totals {
        let at: BTreeMap<&str, usize> = steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.as_str(), i))
            .collect();
        // For each step: the p50 finish, the steps on its path, its predecessor on the path,
        // and the p95 finish.
        let mut p50 = vec![0.0f64; steps.len()];
        let mut length = vec![0usize; steps.len()];
        let mut before: Vec<Option<usize>> = vec![None; steps.len()];
        let mut p95 = vec![0.0f64; steps.len()];
        for (i, step) in steps.iter().enumerate() {
            let mut best: Option<usize> = None;
            let mut worst95 = 0.0f64;
            for dep in &step.depends_on {
                let Some(&d) = at.get(dep.as_str()) else {
                    continue;
                };
                if d >= i {
                    continue;
                }
                worst95 = worst95.max(p95[d]);
                let better = match best {
                    None => true,
                    Some(b) => match p50[d].total_cmp(&p50[b]) {
                        core::cmp::Ordering::Greater => true,
                        core::cmp::Ordering::Less => false,
                        core::cmp::Ordering::Equal => length[d] > length[b],
                    },
                };
                if better {
                    best = Some(d);
                }
            }
            before[i] = best;
            p50[i] = best.map_or(0.0, |b| p50[b]) + step.cost.duration.p50;
            length[i] = best.map_or(0, |b| length[b]) + 1;
            p95[i] = worst95 + step.cost.duration.p95;
        }
        let mut end: Option<usize> = None;
        for i in 0..steps.len() {
            let better = match end {
                None => true,
                Some(e) => match p50[i].total_cmp(&p50[e]) {
                    core::cmp::Ordering::Greater => true,
                    core::cmp::Ordering::Less => false,
                    core::cmp::Ordering::Equal => length[i] > length[e],
                },
            };
            if better {
                end = Some(i);
            }
        }
        let mut critical_path = Vec::new();
        let mut walk = end;
        while let Some(i) = walk {
            critical_path.push(steps[i].id.clone());
            walk = before[i];
        }
        critical_path.reverse();

        let mut groups: BTreeMap<u32, (f64, f64)> = BTreeMap::new();
        let mut held = (0.0f64, 0.0f64);
        let mut workers = (0.0f64, 0.0f64);
        let mut disk = (0.0f64, 0.0f64);
        let mut basis = Basis::Measured;
        let mut observations = u32::MAX;
        for step in steps {
            let c = &step.cost;
            let group = groups.entry(step.group).or_default();
            group.0 += c.peak_memory.p50;
            group.1 += c.peak_memory.p95;
            held.0 += c.peak_memory.p50 * c.duration.p50;
            held.1 += c.peak_memory.p95 * c.duration.p95;
            workers.0 += c.worker_time.p50;
            workers.1 += c.worker_time.p95;
            disk.0 += c.read_bytes.p50 + c.spill_bytes.p50;
            disk.1 += c.read_bytes.p95 + c.spill_bytes.p95;
            for (_, _, estimate) in c.fields() {
                basis = basis.max(estimate.basis);
                observations = observations.min(estimate.observations);
            }
        }
        if steps.is_empty() {
            basis = Basis::Default;
            observations = 0;
        }
        let peak = groups
            .values()
            .fold((0.0f64, 0.0f64), |m, g| (m.0.max(g.0), m.1.max(g.1)));
        let range = |(p50, p95): (f64, f64)| Range { p50, p95 };
        Totals {
            basis,
            observations,
            finish: Range {
                p50: end.map_or(0.0, |e| p50[e]),
                p95: p95.iter().copied().fold(0.0, f64::max),
            },
            critical_path,
            peak_memory_bytes: range(peak),
            memory_byte_seconds: range(held),
            worker_seconds: range(workers),
            disk_bytes: range(disk),
        }
    }
}

/// Moruna's plan of one job: its steps in the order they start, and the job's totals.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Plan {
    /// The plan's content address: 32 lowercase hex characters of the SHA-256 of everything in
    /// it but this id and its creation time ([`Plan::content_id`]).
    pub id: String,
    /// The digest of the job it plans ([`job_digest`]).
    pub job_digest: String,
    /// The planner that made it.
    pub planner: PlannerId,
    /// The cost model the planner asked, by its id; `None`: no model was asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// When it was made, in milliseconds since the Unix epoch.
    pub created_unix_ms: u64,
    /// What the run is given, which the steps' resources are split from.
    pub resources: Resources,
    /// The steps, in the order they start.
    pub steps: Vec<Step>,
    /// The job's totals.
    pub totals: Totals,
}

impl Plan {
    /// A plan of `job` by `planner`: its digest is the job's, and its id its content address.
    pub fn new(
        job: &Job,
        planner: PlannerId,
        model: Option<String>,
        created_unix_ms: u64,
        resources: Resources,
        steps: Vec<Step>,
        totals: Totals,
    ) -> Plan {
        let mut plan = Plan {
            id: String::new(),
            job_digest: job_digest(job),
            planner,
            model,
            created_unix_ms,
            resources,
            steps,
            totals,
        };
        plan.id = plan.content_id();
        plan
    }

    /// The content address: 32 lowercase hex characters of the SHA-256 of the plan's JSON with
    /// its id empty and its creation time zero, so the same job planned the same way twice has
    /// the same id.
    pub fn content_id(&self) -> String {
        let mut bare = self.clone();
        bare.id = String::new();
        bare.created_unix_ms = 0;
        // A struct's fields serialise in their declared order, whatever `serde_json`'s features.
        let text = serde_json::to_string(&bare).unwrap_or_default();
        let mut hex = hex(&Sha256::digest(text.as_bytes()));
        hex.truncate(32);
        hex
    }

    /// The step with id `id`.
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// The step that runs task `task`.
    pub fn step_of(&self, task: &str) -> Option<&Step> {
        self.steps
            .iter()
            .find(|s| s.tasks.iter().any(|t| t == task))
    }

    /// The groups in their order, each its steps' ids in plan order.
    pub fn groups(&self) -> Vec<Vec<String>> {
        let mut groups: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for step in &self.steps {
            groups.entry(step.group).or_default().push(step.id.clone());
        }
        groups.into_values().collect()
    }

    /// The plan as indented JSON, for a person to read, with `"moruna_plan": 1` and its groups.
    /// JSON is a rendering of the document, not a form of it: nothing reads it back.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.to_value())
            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// [`Plan::to_json`] as a value.
    pub fn to_value(&self) -> serde_json::Value {
        let mut value =
            serde_json::to_value(self).unwrap_or_else(|e| serde_json::Value::String(e.to_string()));
        if let serde_json::Value::Object(map) = &mut value {
            map.insert("moruna_plan".into(), serde_json::Value::from(PLAN_VERSION));
            map.insert(
                "groups".into(),
                serde_json::to_value(self.groups()).unwrap_or_default(),
            );
        }
        value
    }
}

/// The content address of `job` (MH 4.1): `sha256:` and the 64 lowercase hex characters of the
/// SHA-256 of its identity ([`Job::identity_json`]), the canonical form without `resume` and
/// `report`, its tasks named by their places.
pub fn job_digest(job: &Job) -> String {
    format!(
        "sha256:{}",
        hex(&Sha256::digest(job.identity_json().as_bytes()))
    )
}

/// The id of the step that runs the tasks at `places` (their indices in the job's `tasks`) of
/// the job with digest `job_digest`, planned by `planner` (SDD 26 f.1): `s` and the first 16 hex
/// characters of the SHA-256 of all four, so the same job planned by the same planner gives the
/// same ids, and a host that renames its tasks keeps them (its digest names tasks by place).
pub fn step_id(job_digest: &str, planner: &PlannerId, places: &[usize]) -> String {
    let places: Vec<String> = places.iter().map(usize::to_string).collect();
    let text = format!(
        "moruna-plan step\0{job_digest}\0{}\0{}\0{}",
        planner.name,
        planner.version,
        places.join(",")
    );
    let mut id = hex(&Sha256::digest(text.as_bytes()));
    id.truncate(16);
    format!("s{id}")
}

/// Whether `id` is a step id: `s` and 16 lowercase hex characters.
pub fn is_step_id(id: &str) -> bool {
    id.len() == 17
        && id.starts_with('s')
        && id[1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
