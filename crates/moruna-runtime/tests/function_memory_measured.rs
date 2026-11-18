//! 6.3g: the room a function gets is its measured memory, not a default (system design 3A, "A
//! function's memory is measured, never assumed").
//!
//! A run's budget has two parts: the arena, which holds the batches in flight, and the room
//! above it, where a function keeps its own working memory. `moruna check` measures that
//! memory: warm-up runs the function on small batches, fits what a call holds outside its
//! batch as a line, and writes it with every unit to the observation store, under the
//! function's cost key. A run opened on the same store has the measurement, under the same
//! key, before it sizes anything, and divides its budget from it.
//!
//! The defect this was written against (it failed until 6.3g's build): the run divided its
//! budget without the measurement. The arena's share came from the function's declared
//! `expected_amplification`, or, for a function that declares nothing, from a fixed factor of
//! 4.0, so every undeclared function left two thirds of its allowance above the arena, whether
//! it used none of it or needed more. On the node (6.2 run 3) the four gate functions each held
//! 3 to 21 MiB in an instance, beside 6.9 GiB set aside.
//!
//! Two functions that declare nothing, one measured far below the default and one far above,
//! each warmed up and then run on the store its warm-up wrote: each gets a room sized by its
//! measurement. Everything here is real: the check harness, warm-up, the store, the arena, the
//! controller and the scheduler.

#![allow(clippy::result_large_err)]

mod support;

/// Every heap byte this test program allocates through Rust is counted while a warm-up runs
/// (SDD 24 p.4), so a function's working memory outside the arena is in its measured peak
/// exactly, on a platform whose allocator keeps freed memory resident too.
#[global_allocator]
static ALLOCATOR: moruna_runtime::check::warmup::CountingAllocator =
    moruna_runtime::check::warmup::CountingAllocator;

use std::sync::Arc;
use std::time::Duration;

use arrow::datatypes::{DataType, Field, Schema};
use moruna_discovery::DiscoveryInput;
use moruna_kernel::declare::{Declared, SchemaDecl};
use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, LimitSource,
    NoState, Payload, PayloadSpec, Sampler, Sink, Source, SourceSchema, TierPref, Topology,
};
use moruna_runtime::check::warmup::WarmUp;
use moruna_runtime::check::{CheckOptions, Verdict, WarmUpReport, check};
use moruna_runtime::cost::{CostSettings, RunLearning, StageCost};
use moruna_runtime::{Components, RunReport, RunSpec, Runtime};
use moruna_testkit::{FakeSink, FakeSource};
use support::{Scratch, one_run_at_a_time};

const MIB: u64 = 1024 * 1024;

/// What a run has to divide, above what the test program already holds.
const ROOM: u64 = 768 * MIB;

/// The share of its allowance a run leaves above the arena for a function nothing is known of:
/// the arena takes `2 / (2 + 4.0)`, a third, and the room is the rest.
const DEFAULT_ROOM: f64 = 2.0 / 3.0;

/// A function that declares nothing about its memory and holds `heap` bytes of its own per
/// input byte while it works: none (its batch is all it touches), or many times its input.
struct Function {
    name: &'static str,
    heap: u64,
}

/// One 64-bit integer column: the test source's schema, and what the function declares it takes.
fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![Field::new(
        "value",
        DataType::Int64,
        false,
    )]))
}

impl Kernel for Function {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute(
            "moruna-runtime::tests::function_memory_measured::Function",
            self.name.as_bytes(),
        )
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    /// Nothing declared: no `expected_amplification`, as a registered function has none.
    fn hints(&self) -> KernelHints {
        KernelHints::default()
    }

    fn declared(&self) -> Declared {
        Declared {
            input: Some(SchemaDecl::from_schema(&schema())),
            output: Some(SchemaDecl::Relative {
                adds: Vec::new(),
                drops: Vec::new(),
                changes: Vec::new(),
            }),
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        if self.heap > 0 {
            let mut held = vec![0u8; (input.bytes() * self.heap) as usize];
            for page in held.chunks_mut(4096) {
                page[0] = 1;
            }
            std::hint::black_box(&held);
        }
        Ok(input)
    }
}

/// `function` checked and warmed up into the store at `store`: what `moruna check` does.
fn warmed(function: &Arc<dyn Kernel>, store: &std::path::Path) -> WarmUpReport {
    let mut warm = WarmUp::new(
        CostSettings::directory(store)
            .open()
            .expect("the store opens"),
    );
    warm.budget = Duration::from_secs(120);
    let mut opts = CheckOptions::new("function");
    opts.warm_up = Some(warm);
    let report = check(Arc::clone(function), opts).expect("the harness runs");
    assert_eq!(report.verdict, Verdict::Agreed, "{}", report.summary());
    match report.warm_up {
        Some(Ok(warm_up)) => warm_up,
        Some(Err(failure)) => panic!(
            "warm-up failed: {} ({:?})",
            failure.reason, failure.progress
        ),
        None => panic!("no warm-up"),
    }
}

/// The share of its allowance the run left above the arena, from the report's own account of
/// how it sized the arena (`arena sized at A bytes: S of the B byte allowance`).
fn room_share(report: &RunReport) -> f64 {
    let note = report
        .notes
        .iter()
        .find(|note| note.starts_with("arena sized at "))
        .unwrap_or_else(|| panic!("the run says how it sized its arena: {:?}", report.notes));
    let number_after = |marker: &str| -> f64 {
        let rest = &note[note.find(marker).expect("the note's wording") + marker.len()..];
        rest.split(' ')
            .next()
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("a byte count after `{marker}` in: {note}")) as f64
    };
    let arena = number_after("arena sized at ");
    let allowance = number_after(" of the ");
    assert!(allowance > 0.0 && arena <= allowance, "{note}");
    1.0 - arena / allowance
}

/// `function` run over eight 1 MiB splits with [`ROOM`] to divide, on the store its warm-up
/// wrote, keyed as warm-up keyed it: the room it left above the arena, as a share.
fn run_room(function: &Arc<dyn Kernel>, warm_up: &WarmUpReport, scratch: &Scratch) -> f64 {
    let source: Arc<dyn Source> = Arc::new(FakeSource::new().splits(8, 131_072, 131_072 * 8));
    assert_eq!(
        source.schema().hash(),
        SourceSchema::Table(schema()).hash(),
        "the run reads the schema the function was warmed up on"
    );
    let learning = RunLearning {
        online: CostSettings::directory(scratch.path().join("observations"))
            .open_online()
            .expect("the store opens for the run"),
        stages: vec![StageCost {
            kind: warm_up.key.kind.clone(),
            settings: warm_up.key.settings,
            fingerprint: None,
        }],
    };
    // The run has the measurement under the function's own key before it sizes anything.
    let keys = learning
        .keys(
            std::slice::from_ref(function),
            &Topology::chain(1),
            &[source.schema()],
        )
        .expect("the run's cost keys");
    assert_eq!(
        keys[0], warm_up.key,
        "the run asks for the function's cost under the key warm-up measured it under"
    );

    let mut discovered = moruna_discovery::discover(&DiscoveryInput::default()).expect("discovery");
    let held = moruna_discovery::Sampler::new(&discovered)
        .expect("a sampler")
        .sample()
        .expect("a sample")
        .anon_bytes;
    discovered.limits.memory_ceiling = held + ROOM;
    discovered.limits.memory_kill = None;
    discovered.limits.cpu_quota = 2.0;
    discovered.limits.source = LimitSource::Os;
    discovered.limits.observed_at = 0;

    let sink: Box<dyn Sink> = Box::new(FakeSink::new());
    let mut spec = RunSpec::new(source, vec![Arc::clone(function)], sink);
    spec.staging_dir = Some(scratch.path().join("staging"));
    spec.staging_limit = Some(256 * MIB);
    spec.profiles_dir = Some(scratch.path().join("profiles"));
    spec.checkpoint = false;
    std::fs::create_dir_all(scratch.path().join("staging")).expect("the staging directory");
    let components = Components {
        discovered: Some(discovered),
        learning: Some(learning),
        ..Components::default()
    };
    let report = match Runtime::run_with(spec, CancelToken::new(), components) {
        Ok(report) => report,
        Err(error) => panic!(
            "the run did not complete: {error}; notes: {:?}",
            error.shutdown_notes
        ),
    };
    room_share(&report)
}

/// A function measured far below the default and one measured far above it, neither declaring
/// anything: each run's room above the arena follows the function's measurement, not the
/// factor of 4.0 that stands in for one.
#[test]
fn a_function_gets_the_room_its_warm_up_measured() {
    let _serial = one_run_at_a_time();

    // Far below: nothing of its own beside its batch.
    let lean: Arc<dyn Kernel> = Arc::new(Function {
        name: "lean",
        heap: 0,
    });
    // Far above: twelve bytes of its own per input byte, three times what the default assumes.
    let hungry: Arc<dyn Kernel> = Arc::new(Function {
        name: "hungry",
        heap: 12,
    });

    let lean_scratch = Scratch::new("function_memory_lean");
    let hungry_scratch = Scratch::new("function_memory_hungry");
    let lean_warm = warmed(&lean, &lean_scratch.path().join("observations"));
    let hungry_warm = warmed(&hungry, &hungry_scratch.path().join("observations"));

    // Warm-up measured both, on small batches: the lean function's peak is about its batch,
    // the hungry one's its batch and twelve times it again.
    let lean_peak = lean_warm
        .kernel
        .peak_per_byte
        .expect("lean's measured peak");
    let hungry_peak = hungry_warm
        .kernel
        .peak_per_byte
        .expect("hungry's measured peak");
    assert!(lean_peak < 3.0, "lean's peak per input byte: {lean_peak}");
    assert!(
        hungry_peak >= 12.0,
        "hungry's peak per input byte: {hungry_peak}"
    );
    // And what each holds outside its batch, as a line over the batch's bytes: next to
    // nothing, and twelve bytes a byte, fitted from every unit of the sizes.
    let lean_own = lean_warm.kernel.memory.as_ref().expect("lean's own memory");
    let hungry_own = hungry_warm
        .kernel
        .memory
        .as_ref()
        .expect("hungry's own memory");
    assert!(
        lean_own.per_input_byte < 1.0,
        "lean's own memory: {lean_own:?}"
    );
    assert!(
        (hungry_own.per_input_byte - 12.0).abs() < 1.0 && hungry_own.units >= 3,
        "hungry's own memory: {hungry_own:?}"
    );

    let lean_room = run_room(&lean, &lean_warm, &lean_scratch);
    let hungry_room = run_room(&hungry, &hungry_warm, &hungry_scratch);
    eprintln!(
        "room above the arena, as a share of the allowance: {lean_room:.3} for the function \
         measured at {:.3} bytes a byte of input outside its batch, {hungry_room:.3} for the one \
         measured at {:.3}; {DEFAULT_ROOM:.3} is what a function nothing is known of is left",
        lean_own.per_input_byte, hungry_own.per_input_byte
    );

    // Every departure is said, not only the first: both functions are the defect.
    let mut departures = Vec::new();
    if lean_room >= DEFAULT_ROOM * 0.75 {
        departures.push(format!(
            "the function measured at {lean_peak:.2} bytes a byte of input, its batch included, \
             was left {lean_room:.2} of its allowance above the arena: the default's \
             {DEFAULT_ROOM:.2}, for a function that uses none of it"
        ));
    }
    if hungry_room <= DEFAULT_ROOM * 1.2 {
        departures.push(format!(
            "the function measured at {hungry_peak:.2} bytes a byte of input was left \
             {hungry_room:.2} of its allowance above the arena: the default's {DEFAULT_ROOM:.2}, \
             sized for 4.0"
        ));
    }
    if hungry_room <= lean_room {
        departures.push(format!(
            "the room does not follow the measurement: {hungry_room:.2} for the function that \
             needs it, {lean_room:.2} for the one that does not"
        ));
    }
    assert!(
        departures.is_empty(),
        "a function's room is not its measurement:\n{}",
        departures.join("\n")
    );
}
