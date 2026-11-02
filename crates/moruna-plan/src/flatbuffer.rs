//! The plan document's FlatBuffers form (SDD 26 e.1): [`Plan::from_flatbuffer`], which verifies
//! untrusted bytes before reading them and refuses a value no plan holds, and
//! [`Plan::to_flatbuffer`].

use flatbuffers::{FlatBufferBuilder, ForwardsUOffset, Vector, WIPOffset};

use crate::generated::moruna::plan::fb;
use crate::plan::{
    Basis, Estimate, PLAN_VERSION, Plan, PlannerId, Range, Resources, Step, Target, Totals, Unit,
    UnitCost,
};

/// The FlatBuffers file identifier of a plan document, at bytes 4 to 8 of every buffer.
pub const PLAN_IDENTIFIER: &str = "MPL1";

/// Why bytes are not a plan this build reads.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanBytesError {
    /// The buffer does not carry the plan document's file identifier.
    #[error("not a plan document: the bytes do not carry the identifier {PLAN_IDENTIFIER}")]
    Identifier,
    /// The FlatBuffers verifier refused the bytes: truncated, out of bounds, misaligned or past
    /// the verifier's limits.
    #[error("not a plan document: {0}")]
    Malformed(String),
    /// A document of a version this build does not read.
    #[error(
        "plan refused: version: this build reads version {PLAN_VERSION}, the document says {0}"
    )]
    Version(u32),
    /// A field that verified and does not hold what a plan holds: an enum value this build does
    /// not know, an estimate that is not finite, negative, or of another unit.
    #[error("plan refused: {field}: {reason}")]
    Field {
        /// The field, as a path from the document's root (`steps[2].cost.duration`).
        field: String,
        /// What is wrong with it.
        reason: String,
    },
}

fn field(field: impl Into<String>, reason: impl Into<String>) -> PlanBytesError {
    PlanBytesError::Field {
        field: field.into(),
        reason: reason.into(),
    }
}

/// Whether `bytes` carry the plan document's file identifier.
pub fn is_plan_flatbuffer(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && fb::plan_buffer_has_identifier(bytes)
}

/// How far the verifier walks a buffer before it refuses it.
fn verifier_options() -> flatbuffers::VerifierOptions {
    flatbuffers::VerifierOptions {
        max_depth: 64,
        max_tables: 1 << 20,
        max_apparent_size: 1 << 31,
        ignore_missing_null_terminator: false,
    }
}

impl Plan {
    /// Read a plan from FlatBuffers bytes, which may be anyone's: the identifier, then the
    /// verifier (every offset, length and required field, bounded depth and size), then the
    /// version, then every value a plan holds (known enums, finite estimates of the right unit,
    /// ids that are present). Nothing panics on any input. A plan read is not yet a plan of any
    /// job: [`crate::check_plan`] says whether it is one of the job at hand.
    pub fn from_flatbuffer(bytes: &[u8]) -> Result<Plan, PlanBytesError> {
        if !is_plan_flatbuffer(bytes) {
            return Err(PlanBytesError::Identifier);
        }
        let root = fb::root_as_plan_with_opts(&verifier_options(), bytes)
            .map_err(|e| PlanBytesError::Malformed(e.to_string()))?;
        if root.version() != PLAN_VERSION {
            return Err(PlanBytesError::Version(root.version()));
        }
        let planner = root.planner();
        if planner.name().is_empty() {
            return Err(field("planner.name", "is empty"));
        }
        let mut steps = Vec::new();
        for (i, step) in root.steps().iter().enumerate() {
            steps.push(read_step(i, step)?);
        }
        let plan = Plan {
            id: nonempty("id", root.id())?,
            job_digest: nonempty("job_digest", root.job_digest())?,
            planner: PlannerId::new(planner.name(), planner.version()),
            model: root.model().map(str::to_string),
            created_unix_ms: root.created_unix_ms(),
            resources: read_resources(root.resources()),
            steps,
            totals: read_totals(root.totals())?,
        };
        Ok(plan)
    }

    /// The plan as FlatBuffers bytes, the same bytes every time for the same plan.
    pub fn to_flatbuffer(&self) -> Vec<u8> {
        let mut b = FlatBufferBuilder::new();
        let steps: Vec<_> = self.steps.iter().map(|s| write_step(&mut b, s)).collect();
        let steps = b.create_vector(&steps);
        let totals = write_totals(&mut b, &self.totals);
        let resources = write_resources(&mut b, &self.resources);
        let name = b.create_string(&self.planner.name);
        let planner = fb::Planner::create(
            &mut b,
            &fb::PlannerArgs {
                name: Some(name),
                version: self.planner.version,
            },
        );
        let id = b.create_string(&self.id);
        let digest = b.create_string(&self.job_digest);
        let model = self.model.as_deref().map(|m| b.create_string(m));
        let root = fb::Plan::create(
            &mut b,
            &fb::PlanArgs {
                version: PLAN_VERSION,
                id: Some(id),
                job_digest: Some(digest),
                planner: Some(planner),
                model,
                created_unix_ms: self.created_unix_ms,
                resources: Some(resources),
                steps: Some(steps),
                totals: Some(totals),
            },
        );
        fb::finish_plan_buffer(&mut b, root);
        b.finished_data().to_vec()
    }
}

fn nonempty(name: &str, value: &str) -> Result<String, PlanBytesError> {
    if value.is_empty() {
        return Err(field(name, "is empty"));
    }
    Ok(value.to_string())
}

fn strings(v: Option<Vector<'_, ForwardsUOffset<&str>>>) -> Vec<String> {
    v.map(|v| v.iter().map(str::to_string).collect())
        .unwrap_or_default()
}

fn read_step(i: usize, step: fb::Step<'_>) -> Result<Step, PlanBytesError> {
    let at = |name: &str| format!("steps[{i}].{name}");
    let tasks: Vec<String> = step.tasks().iter().map(str::to_string).collect();
    if let Some(t) = tasks.iter().position(String::is_empty) {
        return Err(field(at(&format!("tasks[{t}]")), "is empty"));
    }
    let depends_on = strings(step.depends_on());
    if let Some(d) = depends_on.iter().position(String::is_empty) {
        return Err(field(at(&format!("depends_on[{d}]")), "is empty"));
    }
    Ok(Step {
        id: nonempty(&at("id"), step.id())?,
        tasks,
        depends_on,
        group: step.group(),
        resources: read_resources(step.resources()),
        cost: read_cost(&at("cost"), step.cost())?,
    })
}

fn read_resources(r: fb::Resources<'_>) -> Resources {
    Resources {
        workers: r.workers(),
        memory_start_bytes: r.memory_start_bytes(),
        memory_ceiling_bytes: r.memory_ceiling_bytes(),
    }
}

fn write_resources<'b>(
    b: &mut FlatBufferBuilder<'b>,
    r: &Resources,
) -> WIPOffset<fb::Resources<'b>> {
    fb::Resources::create(
        b,
        &fb::ResourcesArgs {
            workers: r.workers,
            memory_start_bytes: r.memory_start_bytes,
            memory_ceiling_bytes: r.memory_ceiling_bytes,
        },
    )
}

fn unit(at: &str, u: fb::Unit) -> Result<Unit, PlanBytesError> {
    match u {
        fb::Unit::Seconds => Ok(Unit::Seconds),
        fb::Unit::Bytes => Ok(Unit::Bytes),
        fb::Unit::Rows => Ok(Unit::Rows),
        other => Err(field(
            format!("{at}.unit"),
            format!("{} is not a unit", other.0),
        )),
    }
}

fn basis(at: &str, b: fb::Basis) -> Result<Basis, PlanBytesError> {
    match b {
        fb::Basis::Measured => Ok(Basis::Measured),
        fb::Basis::WarmUp => Ok(Basis::WarmUp),
        fb::Basis::Prior => Ok(Basis::Prior),
        fb::Basis::Default => Ok(Basis::Default),
        other => Err(field(
            format!("{at}.basis"),
            format!("{} is not a basis", other.0),
        )),
    }
}

fn read_estimate(at: &str, e: fb::Estimate<'_>) -> Result<Estimate, PlanBytesError> {
    Ok(Estimate {
        p50: e.p50(),
        p95: e.p95(),
        unit: unit(at, e.unit())?,
        basis: basis(at, e.basis())?,
        observations: e.observations(),
        extrapolated: e.extrapolated(),
        // A drifting mark is a run's (SDD 24 f.11): a planner's estimates are made before any
        // unit runs, so the plan document has no field for it.
        drifting: false,
    })
}

fn read_cost(at: &str, c: fb::UnitCost<'_>) -> Result<UnitCost, PlanBytesError> {
    let e = |name: &str, v: fb::Estimate<'_>| read_estimate(&format!("{at}.{name}"), v);
    let cost = UnitCost {
        duration: e("duration", c.duration())?,
        worker_time: e("worker_time", c.worker_time())?,
        peak_memory: e("peak_memory", c.peak_memory())?,
        retained_memory: e("retained_memory", c.retained_memory())?,
        rows_out: e("rows_out", c.rows_out())?,
        bytes_out: e("bytes_out", c.bytes_out())?,
        state_growth: e("state_growth", c.state_growth())?,
        read_bytes: e("read_bytes", c.read_bytes())?,
        spill_bytes: e("spill_bytes", c.spill_bytes())?,
    };
    for target in Target::ALL {
        cost.get(target)
            .check(target.unit())
            .map_err(|why| field(format!("{at}.{target}"), why))?;
    }
    Ok(cost)
}

fn write_estimate<'b>(b: &mut FlatBufferBuilder<'b>, e: &Estimate) -> WIPOffset<fb::Estimate<'b>> {
    fb::Estimate::create(
        b,
        &fb::EstimateArgs {
            p50: e.p50,
            p95: e.p95,
            unit: match e.unit {
                Unit::Seconds => fb::Unit::Seconds,
                Unit::Bytes => fb::Unit::Bytes,
                Unit::Rows => fb::Unit::Rows,
            },
            basis: write_basis(e.basis),
            observations: e.observations,
            extrapolated: e.extrapolated,
        },
    )
}

fn write_basis(b: Basis) -> fb::Basis {
    match b {
        Basis::Measured => fb::Basis::Measured,
        Basis::WarmUp => fb::Basis::WarmUp,
        Basis::Prior => fb::Basis::Prior,
        Basis::Default => fb::Basis::Default,
    }
}

fn write_cost<'b>(b: &mut FlatBufferBuilder<'b>, c: &UnitCost) -> WIPOffset<fb::UnitCost<'b>> {
    let args = fb::UnitCostArgs {
        duration: Some(write_estimate(b, &c.duration)),
        worker_time: Some(write_estimate(b, &c.worker_time)),
        peak_memory: Some(write_estimate(b, &c.peak_memory)),
        retained_memory: Some(write_estimate(b, &c.retained_memory)),
        rows_out: Some(write_estimate(b, &c.rows_out)),
        bytes_out: Some(write_estimate(b, &c.bytes_out)),
        state_growth: Some(write_estimate(b, &c.state_growth)),
        read_bytes: Some(write_estimate(b, &c.read_bytes)),
        spill_bytes: Some(write_estimate(b, &c.spill_bytes)),
    };
    fb::UnitCost::create(b, &args)
}

fn string_vector<'b>(
    b: &mut FlatBufferBuilder<'b>,
    items: &[String],
) -> WIPOffset<Vector<'b, ForwardsUOffset<&'b str>>> {
    let offsets: Vec<_> = items.iter().map(|s| b.create_string(s)).collect();
    b.create_vector(&offsets)
}

fn write_step<'b>(b: &mut FlatBufferBuilder<'b>, s: &Step) -> WIPOffset<fb::Step<'b>> {
    let id = b.create_string(&s.id);
    let tasks = string_vector(b, &s.tasks);
    let depends_on = string_vector(b, &s.depends_on);
    let resources = write_resources(b, &s.resources);
    let cost = write_cost(b, &s.cost);
    fb::Step::create(
        b,
        &fb::StepArgs {
            id: Some(id),
            tasks: Some(tasks),
            depends_on: Some(depends_on),
            group: s.group,
            resources: Some(resources),
            cost: Some(cost),
        },
    )
}

fn read_range(at: &str, r: &fb::Range) -> Result<Range, PlanBytesError> {
    let range = Range {
        p50: r.p50(),
        p95: r.p95(),
    };
    range.check().map_err(|why| field(at, why))?;
    Ok(range)
}

fn read_totals(t: fb::Totals<'_>) -> Result<Totals, PlanBytesError> {
    Ok(Totals {
        basis: basis("totals", t.basis())?,
        observations: t.observations(),
        finish: read_range("totals.finish", t.finish())?,
        critical_path: strings(t.critical_path()),
        peak_memory_bytes: read_range("totals.peak_memory_bytes", t.peak_memory_bytes())?,
        memory_byte_seconds: read_range("totals.memory_byte_seconds", t.memory_byte_seconds())?,
        worker_seconds: read_range("totals.worker_seconds", t.worker_seconds())?,
        disk_bytes: read_range("totals.disk_bytes", t.disk_bytes())?,
    })
}

fn write_totals<'b>(b: &mut FlatBufferBuilder<'b>, t: &Totals) -> WIPOffset<fb::Totals<'b>> {
    let critical_path = string_vector(b, &t.critical_path);
    let range = |r: &Range| fb::Range::new(r.p50, r.p95);
    let (finish, peak, held, workers, disk) = (
        range(&t.finish),
        range(&t.peak_memory_bytes),
        range(&t.memory_byte_seconds),
        range(&t.worker_seconds),
        range(&t.disk_bytes),
    );
    fb::Totals::create(
        b,
        &fb::TotalsArgs {
            basis: write_basis(t.basis),
            observations: t.observations,
            finish: Some(&finish),
            critical_path: Some(critical_path),
            peak_memory_bytes: Some(&peak),
            memory_byte_seconds: Some(&held),
            worker_seconds: Some(&workers),
            disk_bytes: Some(&disk),
        },
    )
}
