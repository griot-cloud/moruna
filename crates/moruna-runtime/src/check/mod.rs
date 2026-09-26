//! `moruna check`: is this a kernel, and what does it cost (15, MH 4.9).
//!
//! The harness every surface calls: the Python `moruna check` (through `moruna-py`), the Rust
//! `moruna` binary when it lands (F8.1), and a Rust author's own test. It takes any
//! [`Kernel`], reads its declarations, generates the synthetic batches of 15 f.1 from the input
//! declaration, runs each through the kernel in process with an arena and a trace exactly as a
//! run would, compares every produced schema with the output declaration (15 f.3), and writes
//! the first profile-store row for the kernel's fingerprint (15 e.4) so the first real run sizes
//! from evidence rather than from the default amplification.
//!
//! It is not a test framework and not a guarantee about real data: it is the guarantee that the
//! function *is a kernel*, and a first measurement of what it costs.

pub mod synth;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use moruna_arena::{Arena, ArenaConfig};
use moruna_kernel::arrow::array::{Array, ArrayData, ArrayRef, make_array};
use moruna_kernel::arrow::buffer::{BooleanBuffer, Buffer as ArrowBuffer, NullBuffer};
use moruna_kernel::arrow::record_batch::{RecordBatch, RecordBatchOptions};
use moruna_kernel::declare::{Declared, Disagreement};
use moruna_kernel::{
    Allocator, GilState, InitCtx, Kernel, KernelKind, KernelState, MorunaError, NoState, Outcome,
    Payload, PayloadKind, Result, RunId, Sampler, SourceSchema, Tier, TraceRecord, TraceSink,
};
use moruna_trace::{TraceConfig, TraceWriter};
use serde_json::{Value, json};

/// The format version of the JSON report and of the fields this harness adds to a profile row.
pub const CHECK_FORMAT: u32 = 1;
/// The profile-store format this harness writes, which is the controller's (11 e.3).
const PROFILE_VERSION: u32 = 1;
/// The controller's initial safety multiplier (preamble section 5, `controller.safety_initial`),
/// recorded in the row the way a run records the multiplier it ended at.
const SAFETY_INITIAL: f32 = 1.5;

/// Batches smaller than this give no amplification sample: their fixed costs (buffer alignment,
/// validity bitmaps) are the whole of their output (15 f.4).
pub const MIN_SAMPLE_BYTES: u64 = 4096;

/// Distinguishes the scratch directories of concurrent checks in one process.
static SCRATCH: AtomicU64 = AtomicU64::new(0);

/// A hook the harness calls once with its arena (`CheckOptions::bind`).
pub type Bind = Box<dyn FnOnce(Arc<dyn Allocator>) + Send>;

/// What the caller tells the harness about the kernel beyond the trait (15 d.1).
pub struct CheckOptions {
    /// The name the report gives the kernel.
    pub name: String,
    /// `"python"`, `"std"` or `"rust"`, for the report.
    pub kind: &'static str,
    /// The hash the fingerprint was computed with, which prefixes it in the report
    /// (`sha256` for Python and standard kernels, whose fingerprints MH 4.9 defines; `blake3`
    /// for a Rust kernel's `Fingerprint::compute`, contracts e.6).
    pub fingerprint_scheme: &'static str,
    /// The seed of the synthetic batches (15 f.1).
    pub seed: u64,
    /// Where the profile row goes; `None` writes none.
    pub profiles_dir: Option<PathBuf>,
    /// A Python kernel's GIL state; `None` for a kernel that never enters the interpreter.
    pub gil: Option<GilState>,
    /// Called once with the arena before the first batch, for a kernel that needs the
    /// allocator bound (a Python kernel's `bind_allocator`, 05 d.1).
    pub bind: Option<Bind>,
}

impl CheckOptions {
    /// Options for a Rust kernel named `name`: seed 0, no profile row, no GIL.
    pub fn new(name: impl Into<String>) -> CheckOptions {
        CheckOptions {
            name: name.into(),
            kind: "rust",
            fingerprint_scheme: "blake3",
            seed: 0,
            profiles_dir: None,
            gil: None,
            bind: None,
        }
    }
}

/// The outcome of a check (15 e.3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every batch ran and every produced schema agreed with the declaration.
    Agreed,
    /// A batch failed or disagreed.
    Refused,
    /// The kernel cannot be checked: a declaration is missing, it takes tensors, or a declared
    /// type cannot be generated.
    NotCheckable,
}

impl Verdict {
    /// The report's spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Agreed => "agreed",
            Verdict::Refused => "refused",
            Verdict::NotCheckable => "not_checkable",
        }
    }
}

/// One synthetic batch through the kernel.
#[derive(Clone, Debug)]
pub struct BatchOutcome {
    /// Its name in 15 f.1.
    pub name: &'static str,
    /// Rows in.
    pub rows_in: u64,
    /// Rows out; 0 when the kernel failed.
    pub rows_out: u64,
    /// Payload bytes in.
    pub bytes_in: u64,
    /// Payload bytes out.
    pub bytes_out: u64,
    /// Wall nanoseconds inside `apply`.
    pub wall_ns: u64,
    /// `peak_delta / bytes_in` (15 f.4); `None` for an empty batch or a failed one.
    pub amplification: Option<f64>,
    /// The kernel's error, when it failed.
    pub error: Option<String>,
    /// How the produced schema disagreed with the declaration.
    pub disagreements: Vec<Disagreement>,
}

/// The profile row a check writes (15 e.4).
#[derive(Clone, Debug)]
pub struct ProfileRow {
    /// The file, when a directory was given.
    pub path: Option<PathBuf>,
    /// False when the file already existed: a check never overwrites evidence from a run.
    pub written: bool,
    /// The input schema hash that keys it, as hex.
    pub schema_hash: String,
    /// Median amplification.
    pub a_k_p50: f64,
    /// 95th percentile amplification.
    pub a_k_p95: f64,
    /// Sample variance of the amplification.
    pub a_k_var: f64,
    /// Amplification samples.
    pub samples: u64,
    /// Largest state footprint seen, or the declared `state_bytes`.
    pub state_bytes_max: u64,
    /// Wall nanoseconds per input row over the non-empty batches.
    pub wall_ns_per_row: f64,
    /// `"free_threaded"`, `"serialised"` or `"none"`.
    pub gil: &'static str,
}

/// What `moruna check` says about one kernel (15 e.3).
#[derive(Clone, Debug)]
pub struct CheckReport {
    /// The kernel's name.
    pub name: String,
    /// `"python"`, `"std"` or `"rust"`.
    pub kind: &'static str,
    /// `<scheme>:<64 hex>`.
    pub fingerprint: String,
    /// The outcome.
    pub verdict: Verdict,
    /// Why the kernel is not checkable, when it is not.
    pub reason: Option<String>,
    /// The batches run, in order.
    pub batches: Vec<BatchOutcome>,
    /// The profile row, when the kernel agreed.
    pub profile: Option<ProfileRow>,
    /// The seed.
    pub seed: u64,
    /// Trace records written, one per batch run (15 f.2).
    pub trace_records: u64,
    /// Anything else worth saying.
    pub notes: Vec<String>,
}

impl CheckReport {
    /// 0 for an agreeing kernel, 2 otherwise (15 e.5).
    pub fn exit_code(&self) -> i32 {
        match self.verdict {
            Verdict::Agreed => 0,
            Verdict::Refused | Verdict::NotCheckable => 2,
        }
    }

    /// Every refusal, as `(batch, disagreement)`; a failed batch is not a disagreement and is
    /// in `batches[..].error`.
    pub fn refusals(&self) -> Vec<(&'static str, &Disagreement)> {
        self.batches
            .iter()
            .flat_map(|b| b.disagreements.iter().map(move |d| (b.name, d)))
            .collect()
    }

    /// The JSON report of 15 e.3.
    pub fn to_json(&self) -> Value {
        let batches: Vec<Value> = self
            .batches
            .iter()
            .map(|b| {
                json!({
                    "name": b.name,
                    "rows_in": b.rows_in,
                    "rows_out": b.rows_out,
                    "bytes_in": b.bytes_in,
                    "bytes_out": b.bytes_out,
                    "wall_ns": b.wall_ns,
                    "amplification": b.amplification,
                    "error": b.error,
                })
            })
            .collect();
        let refusals: Vec<Value> = self
            .refusals()
            .into_iter()
            .map(|(batch, d)| {
                let (declared, produced) = match d {
                    Disagreement::Type {
                        declared, produced, ..
                    } => (Some(declared.clone()), Some(produced.clone())),
                    Disagreement::Missing { declared, .. } => (Some(declared.clone()), None),
                    Disagreement::Undeclared { produced, .. } => (None, Some(produced.clone())),
                    Disagreement::Position {
                        declared, produced, ..
                    } => (Some(declared.to_string()), Some(produced.to_string())),
                };
                json!({
                    "batch": batch,
                    "column": d.column(),
                    "reason": d.reason(),
                    "declared": declared,
                    "produced": produced,
                    "message": d.to_string(),
                })
            })
            .collect();
        let profile = self.profile.as_ref().map(|p| {
            json!({
                "path": p.path.as_ref().map(|p| p.display().to_string()),
                "written": p.written,
                "schema_hash": p.schema_hash,
                "a_k_p50": p.a_k_p50,
                "a_k_p95": p.a_k_p95,
                "a_k_var": p.a_k_var,
                "samples": p.samples,
                "state_bytes_max": p.state_bytes_max,
                "wall_ns_per_row": p.wall_ns_per_row,
                "gil": p.gil,
            })
        });
        json!({
            "moruna_check": CHECK_FORMAT,
            "kernel": self.name,
            "kind": self.kind,
            "fingerprint": self.fingerprint,
            "verdict": self.verdict.as_str(),
            "exit": self.exit_code(),
            "reason": self.reason,
            "seed": self.seed,
            "batches": batches,
            "refusals": refusals,
            "profile": profile,
            "trace_records": self.trace_records,
            "notes": self.notes,
        })
    }

    /// The human summary of 15 e.3: one line naming the verdict and the fingerprint, then one
    /// line per refusal or failure, then the profile.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} ({}): {}\n  fingerprint {}\n",
            self.name,
            self.kind,
            self.verdict.as_str(),
            self.fingerprint
        );
        if let Some(reason) = &self.reason {
            out.push_str(&format!("  {reason}\n"));
        }
        for batch in &self.batches {
            if let Some(error) = &batch.error {
                let first = error.lines().next().unwrap_or("");
                out.push_str(&format!("  batch {}: kernel failed: {first}\n", batch.name));
            }
        }
        for (batch, d) in self.refusals() {
            out.push_str(&format!("  batch {batch}: {d}\n"));
        }
        if let Some(p) = &self.profile {
            out.push_str(&format!(
                "  profile: amplification p50 {:.3} p95 {:.3}, state {} bytes, {:.1} ns per row, gil {}\n",
                p.a_k_p50, p.a_k_p95, p.state_bytes_max, p.wall_ns_per_row, p.gil
            ));
            match (&p.path, p.written) {
                (Some(path), true) => out.push_str(&format!("  wrote {}\n", path.display())),
                (Some(path), false) => out.push_str(&format!(
                    "  kept {} (a profile was already there)\n",
                    path.display()
                )),
                (None, _) => {}
            }
        }
        for note in &self.notes {
            out.push_str(&format!("  note: {note}\n"));
        }
        out
    }
}

/// Check one kernel (15 f.1 to f.5). `Err` only when the harness itself cannot start (no arena,
/// no trace); everything the kernel does wrong is in the report.
pub fn check(kernel: Arc<dyn Kernel>, opts: CheckOptions) -> Result<CheckReport> {
    let CheckOptions {
        name,
        kind,
        fingerprint_scheme,
        seed,
        profiles_dir,
        gil,
        bind,
    } = opts;
    let fingerprint = kernel.fingerprint();
    let mut report = CheckReport {
        name,
        kind,
        fingerprint: format!("{fingerprint_scheme}:{}", fingerprint.to_hex()),
        verdict: Verdict::NotCheckable,
        reason: None,
        batches: Vec::new(),
        profile: None,
        seed,
        trace_records: 0,
        notes: Vec::new(),
    };

    // 1. Load: the declarations and the hints.
    let declared: Declared = kernel.declared();
    let (Some(input_decl), Some(output_decl)) = (&declared.input, &declared.output) else {
        let missing = match (&declared.input, &declared.output) {
            (None, None) => "input_schema and output_schema",
            (None, Some(_)) => "input_schema",
            _ => "output_schema",
        };
        report.reason = Some(format!(
            "not checkable: the kernel declares no {missing}; it still runs on the library path"
        ));
        return Ok(report);
    };
    if kernel.accepts().kind == PayloadKind::Tensor {
        report.reason = Some("not checkable: a tensor kernel has no table declaration".into());
        return Ok(report);
    }
    let schema = match input_decl.synthetic_schema() {
        Ok(schema) => schema,
        Err(e) => {
            report.reason = Some(format!("not checkable: {e}"));
            return Ok(report);
        }
    };
    let source_schema = SourceSchema::Table(schema.clone());
    if let Err(e) = kernel
        .accepts()
        .check(&source_schema)
        .and_then(|()| kernel.output_schema(&source_schema).map(|_| ()))
    {
        report.verdict = Verdict::Refused;
        report.reason = Some(format!(
            "refused: the kernel rejects its own declared input at plan time: {e}"
        ));
        return Ok(report);
    }
    let expected = match output_decl.resolve(&schema) {
        Ok(expected) => expected,
        Err(e) => {
            report.verdict = Verdict::Refused;
            report.reason = Some(format!("refused: {e}"));
            return Ok(report);
        }
    };
    let hints = kernel.hints();
    let preferred = synth::preferred_rows(hints.preferred_rows);

    // 2. Synthetic batches, all of them before anything runs, so an ungeneratable type is
    // "not checkable" rather than a refusal halfway through.
    let mut batches = Vec::with_capacity(synth::BATCHES.len());
    for index in 0..synth::BATCHES.len() {
        match synth::batch(&schema, index, seed, preferred) {
            Ok(batch) => batches.push(batch),
            Err(e) => {
                report.reason = Some(format!("not checkable: {e}"));
                return Ok(report);
            }
        }
    }

    // 3. Run, with an arena and a trace, as a run would.
    let harness = Harness::start(&batches)?;
    if let Some(bind) = bind {
        bind(harness.alloc.clone());
    }
    let mut state: Box<dyn KernelState> = match kernel.kind() {
        KernelKind::Stateless => Box::new(NoState),
        KernelKind::Stateful { .. } => {
            match kernel.init(&InitCtx {
                instance: 0,
                device: None,
                alloc: harness.alloc.clone(),
            }) {
                Ok(state) => state,
                Err(e) => {
                    report.verdict = Verdict::Refused;
                    report.reason = Some(format!("refused: init failed: {e}"));
                    harness.finish();
                    return Ok(report);
                }
            }
        }
    };
    let mut state_bytes_max = hints.state_bytes.unwrap_or(0);
    for (index, batch) in batches.into_iter().enumerate() {
        let name = synth::BATCHES[index];
        let outcome = harness.run_one(kernel.as_ref(), state.as_mut(), index, name, batch);
        let mut outcome = outcome?;
        if let Some(footprint) = state.footprint() {
            state_bytes_max = state_bytes_max.max(footprint);
        }
        // 4. Compare (15 f.3).
        if let Some(produced) = outcome.produced.take() {
            outcome.batch.disagreements = expected.compare(&produced);
        }
        report.batches.push(outcome.batch);
    }
    report.trace_records = harness.finish();

    let failed = report
        .batches
        .iter()
        .any(|b| b.error.is_some() || !b.disagreements.is_empty());
    if failed {
        report.verdict = Verdict::Refused;
        return Ok(report);
    }
    report.verdict = Verdict::Agreed;

    // 5. Profile (15 f.4, e.4).
    let row = profile_row(&report.batches, state_bytes_max, gil, &source_schema.hash());
    let row = match profiles_dir {
        None => row,
        Some(_) if row.samples == 0 => {
            report.notes.push(format!(
                "no batch reached {MIN_SAMPLE_BYTES} bytes, so there is no amplification to \
                 record; no profile row written"
            ));
            row
        }
        Some(dir) => write_row(
            row,
            &dir,
            &fingerprint,
            &report.fingerprint,
            &mut report.notes,
        ),
    };
    report.profile = Some(row);
    Ok(report)
}

/// What one batch produced beyond its outcome: the schema to compare.
struct RunOne {
    batch: BatchOutcome,
    produced: Option<moruna_kernel::arrow::datatypes::SchemaRef>,
}

/// The arena, the sampler and the trace one check runs with.
struct Harness {
    alloc: Arc<dyn Allocator>,
    sampler: Option<moruna_discovery::Sampler>,
    trace: Arc<TraceWriter>,
    scratch: PathBuf,
}

impl Harness {
    fn start(batches: &[RecordBatch]) -> Result<Harness> {
        let discovered = moruna_discovery::discover(&moruna_discovery::DiscoveryInput::default())?;
        let page = discovered.limits.page_bytes.max(4096) as u64;
        // Room for every input twice over (the landed copy and whatever the kernel slices),
        // plus a floor, rounded to the arena's granule.
        let inputs: u64 = batches
            .iter()
            .map(|b| b.get_array_memory_size() as u64)
            .sum();
        let granule = page.max(64 << 10);
        let host_bytes = (inputs * 2 + (32 << 20)).div_ceil(granule) * granule;
        let alloc: Arc<dyn Allocator> = Arena::new(ArenaConfig {
            host_bytes,
            host_tier: moruna_kernel::TierKind::Host,
            device_bytes: Vec::new(),
            page_bytes: discovered.limits.page_bytes,
            huge_pages: moruna_kernel::Guarantee::Absent,
            memlock: moruna_kernel::Guarantee::Absent,
            register_rdma: false,
        })?;
        let sampler = moruna_discovery::Sampler::new(&discovered).ok();
        let scratch = std::env::temp_dir().join(format!(
            "moruna-check-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&scratch).map_err(|e| MorunaError::Io {
            op: "create_dir",
            target: scratch.display().to_string(),
            msg: e.to_string(),
        })?;
        let trace = TraceWriter::start(TraceConfig {
            path: None,
            staging_dir: scratch.clone(),
            channel_capacity: 64,
            memory_limit: 16 << 20,
            run_id: RunId([0u8; 16]),
        })?;
        Ok(Harness {
            alloc,
            sampler,
            trace,
            scratch,
        })
    }

    fn anon(&self) -> u64 {
        self.sampler
            .as_ref()
            .map(|s| s.sample().anon_bytes)
            .unwrap_or(0)
    }

    fn run_one(
        &self,
        kernel: &dyn Kernel,
        state: &mut dyn KernelState,
        index: usize,
        name: &'static str,
        batch: RecordBatch,
    ) -> Result<RunOne> {
        let rows_in = batch.num_rows() as u64;
        let landed = land(&batch, self.alloc.as_ref())?;
        drop(batch);
        let payload = Payload::table_with(landed, self.alloc.as_ref())?;
        let bytes_in = payload.bytes();
        let tier_in = payload.tier();
        if let Some(sampler) = &self.sampler {
            sampler.reset_peak();
        }
        let anon_before = self.anon();
        let started = Instant::now();
        let t_start_ns = crate::report::now_ns();
        let result = kernel.apply(state, payload);
        let wall_ns = started.elapsed().as_nanos() as u64;
        let t_end_ns = crate::report::now_ns();
        let anon_after = self.anon();
        let mut outcome = BatchOutcome {
            name,
            rows_in,
            rows_out: 0,
            bytes_in,
            bytes_out: 0,
            wall_ns,
            amplification: None,
            error: None,
            disagreements: Vec::new(),
        };
        let mut produced = None;
        let mut tier_out = tier_in;
        match result {
            Ok(Payload::Table(out, tier)) => {
                outcome.rows_out = out.num_rows() as u64;
                outcome.bytes_out = out.get_array_memory_size() as u64;
                tier_out = tier;
                if bytes_in >= MIN_SAMPLE_BYTES {
                    // 15 f.4: the bytes the kernel allocated for its output outside the arena,
                    // per input byte. The process's anonymous growth is in the trace record and
                    // not in the figure: at a check's batch sizes it is allocator noise, and a
                    // noisy seed is worse than a low one, which the first probe corrects.
                    let fresh = outside_arena_bytes(&out, self.alloc.as_ref());
                    outcome.amplification = Some(fresh as f64 / bytes_in as f64);
                }
                produced = Some(out.schema());
            }
            Ok(Payload::Tensor(_, _)) => {
                outcome.error =
                    Some("the kernel returned a tensor for a table declaration".to_string());
            }
            Err(e) => outcome.error = Some(e.to_string()),
        }
        self.trace.record(TraceRecord {
            seq: index as u64,
            stage: 1,
            worker: 0,
            instance: u16::MAX,
            t_start_ns,
            t_end_ns,
            rows_in,
            bytes_in,
            rows_out: outcome.rows_out,
            bytes_out: outcome.bytes_out,
            tier_in: tier_index(tier_in),
            tier_out: tier_index(tier_out),
            feat_mean_string_len: 0.0,
            feat_null_ratio: 0.0,
            feat_column_bytes: Vec::new(),
            knob_morsel_target: bytes_in,
            knob_active_workers: 1,
            knob_read_ahead: 0,
            mem_anon_before: anon_before,
            mem_anon_peak: anon_after.max(anon_before),
            dev_mem_peak: 0,
            cpu_time_us: wall_ns / 1_000,
            throttled_delta_us: 0,
            q_bytes_before: Vec::new(),
            q_bytes_after: Vec::new(),
            staging_bytes_delta: 0,
            placement_miss_wait_us: 0,
            state_bytes: state.footprint().unwrap_or(0),
            sizer: 0,
            outcome: if outcome.error.is_some() {
                Outcome::Error
            } else {
                Outcome::Probe
            },
            error: outcome.error.clone(),
        });
        Ok(RunOne {
            batch: outcome,
            produced,
        })
    }

    /// Flush and close the trace; the records it holds.
    fn finish(self) -> u64 {
        let records = match self.trace.finish() {
            Ok(view) => view.len(),
            Err(_) => 0,
        };
        let _ = std::fs::remove_dir_all(&self.scratch);
        records
    }
}

fn tier_index(tier: Tier) -> u8 {
    match tier {
        Tier::Host => 0,
        Tier::PinnedHost => 1,
        Tier::Device(_) => 2,
        Tier::Disk(_) => 3,
        Tier::Remote(_, _) => 4,
    }
}

/// Bytes of `batch` whose buffers the arena does not own: what the kernel allocated for its
/// output, as opposed to what it passed through from its input.
fn outside_arena_bytes(batch: &RecordBatch, alloc: &dyn Allocator) -> u64 {
    fn walk(data: &ArrayData, alloc: &dyn Allocator, total: &mut u64) {
        if let Some(nulls) = data.nulls()
            && !alloc.contains(nulls.buffer().as_ptr())
        {
            *total += nulls.buffer().len() as u64;
        }
        for buffer in data.buffers() {
            if !alloc.contains(buffer.as_ptr()) {
                *total += buffer.len() as u64;
            }
        }
        for child in data.child_data() {
            walk(child, alloc, total);
        }
    }
    let mut total = 0;
    for column in batch.columns() {
        walk(&column.to_data(), alloc, &mut total);
    }
    total
}

/// Copy a synthetic batch into the arena, as a source's decoder would have written it there.
fn land(batch: &RecordBatch, alloc: &dyn Allocator) -> Result<RecordBatch> {
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for column in batch.columns() {
        columns.push(make_array(copy_data(&column.to_data(), alloc)?));
    }
    RecordBatch::try_new_with_options(
        batch.schema(),
        columns,
        &RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
    )
    .map_err(|e| MorunaError::Plan(format!("landing a synthetic batch: {e}")))
}

fn copy_data(data: &ArrayData, alloc: &dyn Allocator) -> Result<ArrayData> {
    let mut builder = ArrayData::builder(data.data_type().clone())
        .len(data.len())
        .offset(data.offset());
    if let Some(nulls) = data.nulls() {
        let bits = copy_buffer(nulls.buffer(), alloc)?;
        builder = builder.nulls(Some(NullBuffer::new(BooleanBuffer::new(
            bits,
            nulls.offset(),
            nulls.len(),
        ))));
    }
    for buffer in data.buffers() {
        builder = builder.add_buffer(copy_buffer(buffer, alloc)?);
    }
    for child in data.child_data() {
        builder = builder.add_child_data(copy_data(child, alloc)?);
    }
    builder
        .build()
        .map_err(|e| MorunaError::Plan(format!("landing a synthetic batch: {e}")))
}

fn copy_buffer(source: &ArrowBuffer, alloc: &dyn Allocator) -> Result<ArrowBuffer> {
    let bytes = source.as_slice();
    let mut buffer = alloc.alloc(bytes.len().max(1), Tier::Host)?;
    buffer[..bytes.len()].copy_from_slice(bytes);
    Ok(buffer
        .into_arrow_buffer()?
        .slice_with_length(0, bytes.len()))
}

/// The nearest-rank percentile of `sorted` (ascending); 0 for none.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// The row of 15 e.4 from the batches that produced a measurement.
fn profile_row(
    batches: &[BatchOutcome],
    state_bytes_max: u64,
    gil: Option<GilState>,
    schema_hash: &[u8; 32],
) -> ProfileRow {
    let mut samples: Vec<f64> = batches.iter().filter_map(|b| b.amplification).collect();
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = samples.len();
    let mean = if n > 0 {
        samples.iter().sum::<f64>() / n as f64
    } else {
        0.0
    };
    let var = if n > 1 {
        samples.iter().map(|s| (s - mean) * (s - mean)).sum::<f64>() / (n - 1) as f64
    } else {
        0.0
    };
    let rows: u64 = batches.iter().map(|b| b.rows_in).sum();
    let wall: u64 = batches
        .iter()
        .filter(|b| b.rows_in > 0)
        .map(|b| b.wall_ns)
        .sum();
    ProfileRow {
        path: None,
        written: false,
        schema_hash: hex(schema_hash),
        a_k_p50: percentile(&samples, 0.5),
        a_k_p95: percentile(&samples, 0.95),
        a_k_var: var,
        samples: n as u64,
        state_bytes_max,
        wall_ns_per_row: if rows > 0 {
            wall as f64 / rows as f64
        } else {
            0.0
        },
        gil: match gil {
            None => "none",
            Some(GilState::FreeThreaded) => "free_threaded",
            Some(GilState::Serialised) => "serialised",
        },
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write the row where the controller will look for it (11 e.3): `<fingerprint>-<schema>.json`
/// under `dir`, in the controller's format plus this harness's own fields, which the controller
/// ignores. An existing file is left alone: it is evidence from a run, or an earlier check.
fn write_row(
    mut row: ProfileRow,
    dir: &std::path::Path,
    fingerprint: &moruna_kernel::Fingerprint,
    printed: &str,
    notes: &mut Vec<String>,
) -> ProfileRow {
    let path = dir.join(format!("{}-{}.json", fingerprint.to_hex(), row.schema_hash));
    row.path = Some(path.clone());
    if path.exists() {
        notes.push(format!(
            "a profile for this fingerprint and input schema exists; not overwritten: {}",
            path.display()
        ));
        return row;
    }
    let value = json!({
        "version": PROFILE_VERSION,
        "fingerprint": fingerprint.to_hex(),
        "schema_hash": row.schema_hash,
        "updated": rfc3339_now(),
        "a_k_p50": row.a_k_p50,
        "a_k_p95": row.a_k_p95,
        "a_k_dev_p95": 0.0,
        "a_k_samples": row.samples,
        "a_k_var": row.a_k_var,
        "state_bytes_max": row.state_bytes_max,
        "final_target": 0,
        "final_workers": 1,
        "final_safety": SAFETY_INITIAL,
        "runs": 1,
        "prediction_error_p95": 0.0,
        "source": "moruna check",
        "check_format": CHECK_FORMAT,
        "check_fingerprint": printed,
        "wall_ns_per_row": row.wall_ns_per_row,
        "gil": row.gil,
    });
    let written = std::fs::create_dir_all(dir).and_then(|()| {
        let temp = dir.join(format!(
            ".{}.tmp.{}",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("profile.json"),
            std::process::id()
        ));
        let text = serde_json::to_string_pretty(&value).map_err(std::io::Error::other)?;
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, &path)
    });
    match written {
        Ok(()) => row.written = true,
        Err(e) => notes.push(format!("profile not written: {e}")),
    }
    row
}

/// Now as an RFC 3339 timestamp in UTC, the profile store's `updated` (11 e.3).
fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let time = secs % 86_400;
    // Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}
