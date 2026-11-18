//! 6.3g phase 3 (system design 3A; PM, 2026-11-18): a function's morsel respects each instance's
//! measured room, and a profile remembered at one instance count is not applied unscaled at
//! another.
//!
//! 6.3f's evidence: a Python image function run at four instances was killed for memory. The
//! controller sized its morsels from a profile learned at one instance (88 MB morsels), and four
//! instances sharing the stage's ceiling could not each hold one. The stand-in here is that
//! function: a stage of instances sharing one room, each holding a fixed part (its interpreter)
//! and a large working set per input byte, and killed when what it holds passes its share. Like
//! every image kernel now, it says the largest morsel one instance takes inside its share by the
//! function's measured line (piece <= (room - fixed) / per-byte), and cuts a larger batch into
//! slices of at most that, as an image task cuts its batches into pieces: a stage's target
//! bounds only what the source reads for its first consumer, and a read lands a little above its
//! target (the batch's buffers). A stand-in that says nothing, as every kernel did before, is
//! given morsels its instances cannot hold.
//!
//! Everything else is real: the runtime, the controller and its profile store, the scheduler.

#![allow(clippy::result_large_err)]

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow::datatypes::{DataType, Field, Schema};
use moruna_cost::Memory;
use moruna_discovery::DiscoveryInput;
use moruna_kernel::declare::{Declared, SchemaDecl};
use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, LimitSource,
    MorunaError, NoState, Payload, PayloadSpec, Sampler, Sink, Source, SourceSchema, TierPref,
};
use moruna_runtime::{Components, RunError, RunReport, RunSpec, Runtime};
use moruna_testkit::{FakeSink, FakeSource};
use support::{Scratch, one_run_at_a_time};

const MIB: u64 = 1024 * 1024;

/// What the run has to divide, above what the test program already holds.
const ROOM: u64 = 768 * MIB;

/// The stage's own room, which its instances share (a stage cgroup's ceiling, 6.3f).
const STAGE_ROOM: u64 = 48 * MIB;

/// The data: four splits of eight MiB, each a million rows of one 64-bit integer.
const SPLITS: u32 = 4;
const ROWS_EACH: u64 = 1 << 20;

/// The function's measured line: 4 MiB of its own (an interpreter) and four bytes per input
/// byte, the large per-byte working set of an image function that copies its batch out of Arrow
/// into its own structures.
fn line() -> Memory {
    Memory {
        fixed_bytes: (4 * MIB) as f64,
        per_input_byte: 4.0,
        output_per_input_byte: None,
        spread: 0.0,
        units: 6,
        input_bytes_min: MIB,
        input_bytes_max: 16 * MIB,
    }
}

/// One 64-bit integer column, as the fake source makes.
fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![Field::new(
        "value",
        DataType::Int64,
        false,
    )]))
}

/// The image function's stage: `instances` processes sharing `STAGE_ROOM`.
struct Image {
    instances: usize,
    /// Whether it says what one instance takes, as image kernels do since 6.3g.
    says: bool,
    /// The largest morsel the stage was given.
    largest: AtomicU64,
    /// The most an instance held a piece of at once.
    largest_piece: AtomicU64,
    /// The rows it answered.
    rows: AtomicU64,
}

impl Image {
    fn new(instances: usize, says: bool) -> Image {
        Image {
            instances,
            says,
            largest: AtomicU64::new(0),
            largest_piece: AtomicU64::new(0),
            rows: AtomicU64::new(0),
        }
    }

    /// One instance's share of the stage's room.
    fn share(&self) -> u64 {
        STAGE_ROOM / self.instances as u64
    }

    /// What one instance takes inside its share, by the measured line (system design 3A).
    fn cap(&self) -> u64 {
        line().piece_within(self.share() as f64) as u64
    }
}

impl Kernel for Image {
    /// One identity at every instance count, so a run at four reads what one at one wrote.
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-runtime::tests::instance_room::Image", b"v1")
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: NonZeroUsize::new(self.instances).expect("an instance"),
        }
    }

    /// Nothing declared, as a registered function has nothing.
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

    /// The process's work: the batch in pieces of at most what one instance takes, where it
    /// says that; each piece held as its fixed part and four bytes per input byte, and the
    /// process killed when that passes its share of the stage's room.
    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        let bytes = input.bytes();
        self.largest.fetch_max(bytes, Ordering::SeqCst);
        let cap = if self.says { self.cap() } else { u64::MAX };
        let pieces = bytes.div_ceil(cap).max(1);
        let piece = bytes.div_ceil(pieces);
        self.largest_piece.fetch_max(piece, Ordering::SeqCst);
        let need = line().account(piece as f64);
        if need > self.share() as f64 {
            return Err(MorunaError::Kernel {
                stage: 0,
                seq: 0,
                msg: format!(
                    "the instance was killed for memory: a piece of {piece} bytes needs {need:.0} \
                     of its {} bytes",
                    self.share()
                ),
                features: None,
            });
        }
        for _ in 0..pieces {
            let mut held = vec![0u8; (piece as f64 * line().per_input_byte) as usize];
            for page in held.chunks_mut(4096) {
                page[0] = 1;
            }
            std::hint::black_box(&held);
        }
        self.rows.fetch_add(input.rows(), Ordering::SeqCst);
        Ok(input)
    }

    /// What one instance takes inside its share, when it says it (system design 3A).
    fn morsel_bytes_max(&self) -> Option<u64> {
        self.says.then(|| self.cap())
    }
}

/// `image` run over the profile store at `profiles`, to the end.
fn run(
    image: &Arc<Image>,
    profiles: &std::path::Path,
    scratch: &Scratch,
) -> Result<RunReport, RunError> {
    let source: Arc<dyn Source> =
        Arc::new(FakeSource::new().splits(SPLITS, ROWS_EACH, ROWS_EACH * 8));
    let mut discovered = moruna_discovery::discover(&DiscoveryInput::default()).expect("discovery");
    let held = moruna_discovery::Sampler::new(&discovered)
        .expect("a sampler")
        .sample()
        .expect("a sample")
        .anon_bytes;
    discovered.limits.memory_ceiling = held + ROOM;
    discovered.limits.memory_kill = None;
    discovered.limits.cpu_quota = 4.0;
    discovered.limits.source = LimitSource::Os;
    discovered.limits.observed_at = 0;

    let sink: Box<dyn Sink> = Box::new(FakeSink::new());
    let function: Arc<dyn Kernel> = Arc::clone(image) as Arc<dyn Kernel>;
    let mut spec = RunSpec::new(source, vec![function], sink);
    let staging = scratch.path().join(format!("staging-{}", image.instances));
    std::fs::create_dir_all(&staging).expect("the staging directory");
    spec.staging_dir = Some(staging);
    spec.staging_limit = Some(256 * MIB);
    spec.profiles_dir = Some(profiles.to_path_buf());
    spec.checkpoint = false;
    let components = Components {
        discovered: Some(discovered),
        ..Components::default()
    };
    Runtime::run_with(spec, CancelToken::new(), components)
}

/// `run`, which must complete.
fn completed(image: &Arc<Image>, profiles: &std::path::Path, scratch: &Scratch) -> RunReport {
    match run(image, profiles, scratch) {
        Ok(report) => report,
        Err(error) => panic!(
            "the run did not complete: {error}; notes: {:?}",
            error.shutdown_notes
        ),
    }
}

/// The most a read lands above its target: the batch's own buffers, a hundredth at most.
fn landed(cap: u64) -> u64 {
    cap + cap / 100
}

/// Rule 1: at four instances, each with a quarter of the stage's room, the stage's morsels are
/// held to what one instance takes: (12 - 4) / 4 = 2 MiB, below the controller's least morsel of
/// 4 MiB, which the cap wins over. No instance holds more than that, and every row arrives,
/// once.
#[test]
fn every_morsel_fits_the_room_of_the_instance_it_goes_to() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("instance-room");
    let image = Arc::new(Image::new(4, true));
    let cap = image.morsel_bytes_max().expect("a cap");
    assert_eq!(cap, 2 * MIB, "(12 MiB - 4 MiB) / 4 bytes a byte");
    let report = completed(&image, &scratch.path().join("profiles"), &scratch);
    let largest = image.largest.load(Ordering::SeqCst);
    assert!(
        largest <= landed(cap),
        "a morsel of {largest} bytes went to a stage whose instances take {cap}"
    );
    assert!(largest > 0, "the function ran");
    assert!(image.largest_piece.load(Ordering::SeqCst) <= cap);
    assert_eq!(
        image.rows.load(Ordering::SeqCst),
        u64::from(SPLITS) * ROWS_EACH,
        "every row, once"
    );
    assert_eq!(
        report.sinks.iter().map(|sink| sink.rows).sum::<u64>(),
        u64::from(SPLITS) * ROWS_EACH
    );
}

/// The defect: a stage whose kernel says nothing of its instances' room, as every kernel did
/// before 6.3g, is given morsels sized for the run's memory as a whole, and an instance with a
/// quarter of the stage's room is killed.
#[test]
fn a_kernel_that_says_nothing_is_given_morsels_its_instances_cannot_hold() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("instance-room-silent");
    let image = Arc::new(Image::new(4, false));
    let failed =
        run(&image, &scratch.path().join("profiles"), &scratch).expect_err("an instance is killed");
    assert!(failed.to_string().contains("killed for memory"), "{failed}");
    assert!(image.largest.load(Ordering::SeqCst) > landed(image.cap()));
}

/// Rule 2, 6.3f's case: a run at one instance, which has the whole room and takes morsels of up
/// to (48 - 4) / 4 = 11 MiB, writes its profile; a run of the same function at four instances
/// reads it, says it was written at another count, and holds each instance to its quarter.
#[test]
fn a_profile_from_one_instance_is_not_applied_unscaled_at_four() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("instance-profile");
    let profiles = scratch.path().join("profiles");

    let one = Arc::new(Image::new(1, true));
    completed(&one, &profiles, &scratch);
    assert!(
        one.largest.load(Ordering::SeqCst) > landed(2 * MIB),
        "at one instance the morsels are larger than four instances can hold"
    );
    let written: Vec<_> = std::fs::read_dir(&profiles)
        .expect("the profile store")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(written.len(), 1, "one profile, the function's");
    let profile: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(written[0].path()).expect("the profile reads"),
    )
    .expect("the profile parses");
    assert_eq!(profile["instances"], 1, "the count it was written at");

    let four = Arc::new(Image::new(4, true));
    let report = completed(&four, &profiles, &scratch);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("was written at 1 instances and this run has 4")),
        "the run says the profile was another count's: {:?}",
        report.notes
    );
    let largest = four.largest.load(Ordering::SeqCst);
    assert!(
        largest <= landed(2 * MIB),
        "a morsel of {largest} bytes at four instances"
    );
    assert_eq!(
        four.rows.load(Ordering::SeqCst),
        u64::from(SPLITS) * ROWS_EACH
    );
}
