//! Fixtures shared by the PL tests: a scratch directory unique to this process, an engine
//! over the testkit's `FakeAllocator` and `FakeReactor` (contracts d.15), and the two
//! payload shapes.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use moruna_kernel::arrow::array::{ArrayData, ArrayRef, make_array};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Allocator, DType, DeviceId, Fingerprint, ManagedTensor, Morsel, NodeId, Origin, Payload,
    PayloadKind, PayloadSpec, Reactor, ResumePolicy, RunId, Seq, Split, StageId, StagingCodec,
    Tier, TierBudgets, TierPref,
};
use moruna_placement::{PlacementConfig, PlacementEngine};
use moruna_testkit::{FakeAllocator, FakeReactor};

/// The page size every test uses; it is the contracts default.
pub const PAGE: usize = 4096;

static SCRATCH: AtomicU64 = AtomicU64::new(0);

/// A directory no other process and no other test can touch: the process id and a counter
/// are both in the name, because several gates run on one machine at the same time
/// (preamble 6.7).
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// A fresh directory for one test.
    pub fn new(label: &str) -> Scratch {
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "moruna-placement-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        match std::fs::create_dir_all(&path) {
            Ok(()) => Scratch { path },
            Err(e) => panic!("scratch {}: {e}", path.display()),
        }
    }

    /// Where it is.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Tier budgets with the same figure for every resident tier and `disk` for the staging cap.
pub fn budgets(host: u64, device: u64, disk: u64) -> TierBudgets {
    let mut per_device = [0u64; 8];
    per_device[0] = device;
    TierBudgets {
        device: per_device,
        pinned_host: host,
        host,
        disk,
    }
}

/// A configuration with one stage per `stages`, a staging directory when one is given, and
/// the identity fields a manifest needs.
pub fn config(stages: u16, dir: Option<PathBuf>, budgets: TierBudgets) -> PlacementConfig {
    let disk = budgets.disk;
    PlacementConfig {
        run_id: RunId([7; 16]),
        node: moruna_kernel::LOCAL_NODE,
        stages,
        budgets,
        staging_dir: dir,
        durable_staging: false,
        disk_budget: disk,
        segment_bytes: 1 << 20,
        codec: StagingCodec::Raw,
        page_bytes: PAGE,
        gds: false,
        devices: vec![DeviceId(0)],
        plan_digest: moruna_placement::manifest::plan_digest(&plan()),
        fingerprints: vec![Fingerprint::compute("test", b"1")],
        resume_policy: vec![ResumePolicy::Reinit],
        config: serde_json::json!({
            "staging": { "segment_bytes": 1u64 << 20 },
            "page": { "bytes": PAGE }
        }),
        checkpoint_enabled: true,
    }
}

/// The source plan every test's manifest is checked against (e.5).
pub fn plan() -> Vec<Split> {
    vec![Split {
        id: 1,
        rows: 1000,
        uncompressed_bytes: 4000,
        estimated: false,
        column_bytes: vec![4000],
        null_counts: vec![None],
        sub_splittable: true,
    }]
}

/// An engine over the two fakes.
pub fn engine(
    cfg: PlacementConfig,
    alloc: &FakeAllocator,
    reactor: &FakeReactor,
) -> Arc<PlacementEngine> {
    let allocator: Arc<dyn Allocator> = Arc::new(alloc.clone());
    let io: Arc<dyn Reactor> = Arc::new(reactor.clone());
    match PlacementEngine::new(cfg, allocator, io) {
        Ok(engine) => engine,
        Err(e) => panic!("PlacementEngine::new: {e}"),
    }
}

/// Where a morsel's rows came from; one split, one row per sequence number.
pub fn origin(seq: Seq) -> Origin {
    Origin {
        split: 1,
        row_start: seq,
        row_end: seq + 1,
        node: NodeId(0),
    }
}

/// A one-column batch of `rows` int32 values whose buffer the arena owns, so
/// `BufferView::of_arrow` and `Payload::table` both work over it (contracts d.3, e.2).
pub fn int_batch(alloc: &FakeAllocator, rows: usize, tier: Tier) -> RecordBatch {
    let bytes: Vec<u8> = (0..rows as i32).flat_map(|v| v.to_le_bytes()).collect();
    let buffer = alloc.arrow_buffer(&bytes, tier);
    let data = ArrayData::builder(DataType::Int32)
        .len(rows)
        .add_buffer(buffer)
        .build();
    let data = match data {
        Ok(data) => data,
        Err(e) => panic!("int_batch: {e}"),
    };
    let column: ArrayRef = make_array(data);
    let schema = Arc::new(Schema::new(vec![Field::new("i", DataType::Int32, false)]));
    match RecordBatch::try_new(schema, vec![column]) {
        Ok(batch) => batch,
        Err(e) => panic!("int_batch: {e}"),
    }
}

/// A table morsel resident in the allocator's host tier.
pub fn table_morsel(alloc: &FakeAllocator, seq: Seq, stage: StageId, rows: usize) -> Morsel {
    let batch = int_batch(alloc, rows, alloc.host_tier());
    let payload = match Payload::table(batch) {
        Ok(payload) => payload,
        Err(e) => panic!("table_morsel: {e}"),
    };
    Morsel::new(seq, stage, payload, origin(seq))
}

/// A tensor morsel of `elements` f32 values in `tier`.
pub fn tensor_morsel_in(
    alloc: &FakeAllocator,
    seq: Seq,
    stage: StageId,
    elements: usize,
    tier: Tier,
) -> Morsel {
    let buffer = alloc.buffer(elements * 4, tier);
    let tensor = match ManagedTensor::from_buffer(buffer, 0, DType::F32, vec![elements as i64]) {
        Ok(tensor) => tensor,
        Err(e) => panic!("tensor_morsel: {e}"),
    };
    let payload = match Payload::tensor(tensor) {
        Ok(payload) => payload,
        Err(e) => panic!("tensor_morsel: {e}"),
    };
    Morsel::new(seq, stage, payload, origin(seq))
}

/// A tensor morsel in the allocator's host tier.
pub fn tensor_morsel(alloc: &FakeAllocator, seq: Seq, stage: StageId, elements: usize) -> Morsel {
    tensor_morsel_in(alloc, seq, stage, elements, alloc.host_tier())
}

/// What a consumer wanting the host tier declares.
pub fn want_host() -> PayloadSpec {
    PayloadSpec {
        kind: PayloadKind::Either,
        tier: TierPref::Host,
    }
}

/// What a consumer wanting a device declares.
pub fn want_device() -> PayloadSpec {
    PayloadSpec {
        kind: PayloadKind::Tensor,
        tier: TierPref::Device,
    }
}

/// Wait until every operation the fake was given has resolved, so a test can observe the
/// state a completion left behind.
pub fn settle(reactor: &FakeReactor) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while reactor.in_flight() > 0 {
        if Instant::now() > deadline {
            panic!(
                "operations still in flight after 10 s: {}",
                reactor.in_flight()
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // A completion resolves the reactor's own bookkeeping before it runs the engine's
    // callback; give the callback a moment to land.
    std::thread::sleep(Duration::from_millis(5));
}

/// Wait until `check` holds, or fail the test.
pub fn until(label: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        if Instant::now() > deadline {
            panic!("{label}: did not hold within 10 s");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A deterministic 64-bit generator, so a property test is reproducible without a crate the
/// preamble's dependency table does not list (section 6.2).
pub struct Rng(u64);

impl Rng {
    /// Seed it.
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    /// The next value.
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value below `bound`.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.next() % bound }
    }
}
