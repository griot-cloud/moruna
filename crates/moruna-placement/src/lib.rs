//! Moruna component 9, the placement engine: the queues between stages, the tier each
//! morsel's bytes are in, the staging log on local disk, and the run manifest.
//!
//! Design: `architecture/sdd/09-placement.md`. The engine owns every morsel between the
//! moment a producer pushes it and the moment a consumer pops it, and its job is to have
//! each morsel's bytes in the tier its consumer declared before the consumer asks, using
//! only DMA through the reactor to move them, within the per-tier budgets the controller
//! sets.
//!
//! It has no thread of its own (PL-I14): every reactor operation is issued from the thread
//! that called `push`, `pop`, a setter or `restore` and returns at once, and its outcome is
//! observed in a `Completion::then` callback on the reactor thread that resolved it. The
//! lock order is the preamble's (4.2): a queue lock, then the lineage index, then the moves
//! map and the segments map, and no lock is ever held across a call into the reactor or the
//! allocator.

#![deny(missing_docs)]
// Section l: `unsafe` is not permitted in this crate. Every DMA source is a `BufferView`
// from a safe constructor and every payload comes back through a safe one (contracts d.3,
// d.4, e.7).
#![forbid(unsafe_code)]
// `MorunaError` carries a morsel's features so a diagnostic needs no debugger (CT-I10), which
// makes it large; the shape of the error type is the contract's (d.14), not this crate's.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use moruna_kernel::{
    Allocator, MorunaError, CheckpointExtras, DeviceId, Fingerprint, Locality, Morsel, NodeId,
    PayloadKind, PayloadSpec, Placement, PlacementStats, Reactor, ResumePoint, ResumePolicy, RunId,
    Seq, Split, StageId, StagingCodec, TIER_COUNT, Tier, TierBudgets, TierKind, TierPref,
};

pub mod lineage;
pub mod locks;
pub mod manifest;
pub mod moves;
pub mod plan;
pub mod queue;
pub mod shutdown;
pub mod staging;
pub mod state;
pub mod stats;

use lineage::Lineage;
use manifest::ManifestHeader;
use moves::MoveInFlight;
use queue::{Queue, QueueCounters};
use staging::Staging;
use stats::PlacementDetail;

/// "Nothing committed yet" in the committed watermark (e.2).
pub(crate) const NO_COMMIT: u64 = u64::MAX;

/// How a queue demotes an entry it cannot keep resident.
///
/// Reserved (b): no configuration field carries it in v1. What decides at run time is the
/// queue's `staging_enabled` flag alone; the enum exists so that the entry state machine and
/// `evicted()` are written once for any stage, not for stage zero.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum DemotionPolicy {
    /// Demote by writing a segment record.
    Write,
    /// Demote by dropping the bytes and listing the entry for recomputation.
    Evict,
}

impl DemotionPolicy {
    /// The policy a queue's `staging_enabled` flag and an entry's recomputability imply
    /// (b): staging on writes, staging off evicts a recomputable entry, and staging off on
    /// a non-recomputable entry has no policy at all (PL-I6).
    pub fn of(staging_enabled: bool, recomputable: bool) -> Option<DemotionPolicy> {
        match (staging_enabled, recomputable) {
            (true, _) => Some(DemotionPolicy::Write),
            (false, true) => Some(DemotionPolicy::Evict),
            (false, false) => None,
        }
    }
}

/// Everything the engine is told once, at `new` (d.1).
#[derive(Clone, Debug)]
pub struct PlacementConfig {
    /// Names the staging subdirectory and the manifest.
    pub run_id: RunId,
    /// `LOCAL_NODE` in v1.
    pub node: NodeId,
    /// Number of queues: kernels plus one (Q0 .. Qn).
    pub stages: u16,
    /// Initial tier budgets; the controller updates them.
    pub budgets: TierBudgets,
    /// `None` means no disk tier and no manifest.
    pub staging_dir: Option<PathBuf>,
    /// `profile.durable_staging.is_guaranteed()`; recorded in the manifest.
    pub durable_staging: bool,
    /// `budget.disk`.
    pub disk_budget: u64,
    /// `staging.segment_bytes`.
    pub segment_bytes: u64,
    /// `Raw` in v1; recorded in every segment record header.
    pub codec: StagingCodec,
    /// The page size direct IO aligns to.
    pub page_bytes: usize,
    /// `profile.gds.is_available() && feature gds && reactor.paths().gds`.
    pub gds: bool,
    /// `devices[0]` is the promotion target for `Device` consumers (b).
    pub devices: Vec<DeviceId>,
    /// BLAKE3 over the source plan (e.5), computed by the facade.
    pub plan_digest: [u8; 32],
    /// Kernel fingerprints per stage 1..n.
    pub fingerprints: Vec<Fingerprint>,
    /// Resume policy per stage 1..n.
    pub resume_policy: Vec<ResumePolicy>,
    /// The resolved configuration table (preamble section 5).
    pub config: serde_json::Value,
    /// `checkpoint.enabled`; false unlinks reclaimable segments at once (f.7).
    pub checkpoint_enabled: bool,
}

impl Default for PlacementConfig {
    fn default() -> PlacementConfig {
        PlacementConfig {
            run_id: RunId([0; 16]),
            node: moruna_kernel::LOCAL_NODE,
            stages: 1,
            budgets: TierBudgets::default(),
            staging_dir: None,
            durable_staging: false,
            disk_budget: 0,
            segment_bytes: 128 * 1024 * 1024,
            codec: StagingCodec::Raw,
            page_bytes: 4096,
            gds: false,
            devices: Vec::new(),
            plan_digest: [0; 32],
            fingerprints: Vec::new(),
            resume_policy: Vec::new(),
            config: serde_json::Value::Null,
            checkpoint_enabled: true,
        }
    }
}

/// The placement engine (component 9).
pub struct PlacementEngine {
    pub(crate) cfg: PlacementConfig,
    pub(crate) alloc: Arc<dyn Allocator>,
    pub(crate) reactor: Arc<dyn Reactor>,
    /// The run's one host tier, read from `alloc.is_pinned()` once at `new` (contracts e.1).
    pub(crate) host_tier: Tier,
    pub(crate) page_bytes: u64,
    pub(crate) run_dir: Option<PathBuf>,
    pub(crate) queues: Vec<Mutex<Queue>>,
    pub(crate) counters: Vec<QueueCounters>,
    pub(crate) reserved: [AtomicU64; TIER_COUNT],
    pub(crate) budgets: RwLock<TierBudgets>,
    pub(crate) disk_bytes: AtomicU64,
    pub(crate) disk_budget: AtomicU64,
    pub(crate) next_segment: AtomicU32,
    pub(crate) next_move: AtomicU64,
    pub(crate) staging: Mutex<Staging>,
    /// Serialises opening a segment so two demotions never create two files for one queue.
    pub(crate) roll: Mutex<()>,
    pub(crate) moves: Mutex<HashMap<u64, MoveInFlight>>,
    pub(crate) lineage: Mutex<BTreeMap<Seq, Lineage>>,
    pub(crate) committed: AtomicU64,
    pub(crate) manifest_refs: Mutex<HashSet<u32>>,
    pub(crate) shutting_down: AtomicBool,
    pub(crate) restored: AtomicBool,
    pub(crate) manifests_written: AtomicU64,
    pub(crate) last_manifest_us: AtomicU64,
    pub(crate) in_flight_bytes: AtomicU64,
    pub(crate) this: Weak<PlacementEngine>,
}

impl PlacementEngine {
    /// Reads `alloc.is_pinned()` once for the host tier, creates the run directory when
    /// `staging_dir` is `Some`, and issues nothing to the reactor (d.1).
    pub fn new(
        cfg: PlacementConfig,
        alloc: Arc<dyn Allocator>,
        reactor: Arc<dyn Reactor>,
    ) -> moruna_kernel::Result<Arc<PlacementEngine>> {
        if cfg.stages == 0 {
            return Err(MorunaError::Config {
                name: "stages",
                msg: "a run has at least one queue".into(),
            });
        }
        if cfg.page_bytes == 0 {
            return Err(MorunaError::Config {
                name: "page.bytes",
                msg: "the page size is zero".into(),
            });
        }
        if cfg.segment_bytes == 0 {
            return Err(MorunaError::Config {
                name: "staging.segment_bytes",
                msg: "the segment size is zero".into(),
            });
        }
        let host_tier = if alloc.is_pinned() {
            Tier::PinnedHost
        } else {
            Tier::Host
        };
        let run_dir = match &cfg.staging_dir {
            Some(dir) => {
                let path = dir.join(format!("moruna-{}", cfg.run_id.to_hex()));
                std::fs::create_dir_all(&path).map_err(|e| MorunaError::Io {
                    op: "create_dir_all",
                    target: path.display().to_string(),
                    msg: e.to_string(),
                })?;
                Some(path)
            }
            None => None,
        };
        let stages = usize::from(cfg.stages);
        let page_bytes = cfg.page_bytes as u64;
        let disk_budget = cfg.disk_budget;
        let queues: Vec<Mutex<Queue>> = (0..stages)
            .map(|stage| Mutex::new(Queue::new(stage as StageId, host_tier)))
            .collect();
        let counters: Vec<QueueCounters> = (0..stages).map(|_| QueueCounters::default()).collect();
        let budgets = RwLock::new(cfg.budgets.clone());
        Ok(Arc::new_cyclic(|this| PlacementEngine {
            cfg,
            alloc,
            reactor,
            host_tier,
            page_bytes,
            run_dir,
            queues,
            counters,
            reserved: std::array::from_fn(|_| AtomicU64::new(0)),
            budgets,
            disk_bytes: AtomicU64::new(0),
            disk_budget: AtomicU64::new(disk_budget),
            next_segment: AtomicU32::new(0),
            next_move: AtomicU64::new(1),
            staging: Mutex::new(Staging::default()),
            roll: Mutex::new(()),
            moves: Mutex::new(HashMap::new()),
            lineage: Mutex::new(BTreeMap::new()),
            committed: AtomicU64::new(NO_COMMIT),
            manifest_refs: Mutex::new(HashSet::new()),
            shutting_down: AtomicBool::new(false),
            restored: AtomicBool::new(false),
            manifests_written: AtomicU64::new(0),
            last_manifest_us: AtomicU64::new(0),
            in_flight_bytes: AtomicU64::new(0),
            this: this.clone(),
        }))
    }

    /// Per-queue tier histograms, segments, evictions (j).
    pub fn detailed_stats(&self) -> PlacementDetail {
        stats::detailed(self)
    }

    /// `staging_dir/moruna-<run_id>/manifest.json`, or `None` without a staging directory.
    pub fn manifest_path(&self) -> Option<PathBuf> {
        self.run_dir.as_ref().map(|dir| dir.join("manifest.json"))
    }

    /// Locate the newest manifest (by `written_ns`) under `staging_dir` for `run_id`, or,
    /// with `None`, the newest manifest of any run in that directory.
    pub fn find_manifest(
        staging_dir: &Path,
        run_id: Option<RunId>,
    ) -> moruna_kernel::Result<Option<PathBuf>> {
        manifest::find_manifest(staging_dir, run_id)
    }

    /// Read only the identity fields of a manifest so the facade can build a
    /// `PlacementConfig` with the manifest's run id before `restore`.
    pub fn read_manifest_header(path: &Path) -> moruna_kernel::Result<ManifestHeader> {
        manifest::read_manifest_header(path)
    }

    /// The run's one host tier (contracts e.1).
    pub fn host_tier(&self) -> Tier {
        self.host_tier
    }

    /// The tier a queue whose consumer declared `want` promotes toward (b).
    pub(crate) fn target_tier(&self, want: &PayloadSpec) -> Tier {
        match want.tier {
            TierPref::Device => match self.cfg.devices.first() {
                Some(device) => Tier::Device(*device),
                None => self.host_tier,
            },
            TierPref::Host | TierPref::Any => self.host_tier,
        }
    }

    /// A strong handle on the engine, for a `Completion::then` callback (g).
    pub(crate) fn handle(&self) -> Option<Arc<PlacementEngine>> {
        self.this.upgrade()
    }

    /// The budget of one tier slot, from the controller's `TierBudgets` (f.9). The `Device`
    /// slot carries the sum the controller gives, because v1 places on one device (h).
    pub(crate) fn budget_of(&self, slot: usize) -> u64 {
        let budgets = self.budgets.read().unwrap_or_else(|e| e.into_inner());
        if slot == TierKind::Device.index() {
            budgets.device.iter().sum()
        } else if slot == TierKind::PinnedHost.index() {
            budgets.pinned_host
        } else if slot == TierKind::Host.index() {
            budgets.host
        } else if slot == TierKind::Disk.index() {
            budgets.disk
        } else {
            0
        }
    }

    /// Resident bytes in one tier slot, summed over queues (f.9).
    pub(crate) fn resident_of(&self, slot: usize) -> u64 {
        self.counters
            .iter()
            .map(|c| c.bytes[slot].load(Ordering::Relaxed))
            .sum()
    }

    /// Claim `bytes` against a destination tier's budget for a move in flight (PL-I3, f.9).
    /// A failed reservation returns false without side effect.
    pub(crate) fn reserve(&self, slot: usize, bytes: u64) -> bool {
        let budget = self.budget_of(slot);
        loop {
            let reserved = self.reserved[slot].load(Ordering::Acquire);
            let resident = self.resident_of(slot);
            if resident.saturating_add(reserved).saturating_add(bytes) > budget {
                return false;
            }
            if self.reserved[slot]
                .compare_exchange_weak(
                    reserved,
                    reserved + bytes,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Release a reservation when its move completes or fails (f.9).
    pub(crate) fn release(&self, slot: usize, bytes: u64) {
        self.reserved[slot]
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                Some(held.saturating_sub(bytes))
            })
            .ok();
    }

    /// The next move id; also the key of the moves map (g).
    pub(crate) fn next_move_id(&self) -> u64 {
        self.next_move.fetch_add(1, Ordering::Relaxed)
    }

    /// True after `shutdown` (f.15).
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// The committed watermark, or `None` when nothing has been committed (e.2).
    pub(crate) fn committed_seq(&self) -> Option<Seq> {
        match self.committed.load(Ordering::Acquire) {
            NO_COMMIT => None,
            seq => Some(seq),
        }
    }

    fn check_stage(&self, stage: StageId) -> moruna_kernel::Result<usize> {
        let index = usize::from(stage);
        if index >= self.queues.len() {
            return Err(MorunaError::Staging(format!(
                "stage {stage} is outside the run's {} queues",
                self.queues.len()
            )));
        }
        Ok(index)
    }
}

impl Placement for PlacementEngine {
    fn push(&self, stage: StageId, morsel: Morsel) -> moruna_kernel::Result<()> {
        let index = self.check_stage(stage)?;
        self.push_entry(index, morsel, false)
    }

    fn pop(
        &self,
        stage: StageId,
        want: PayloadSpec,
        locality: Locality,
    ) -> moruna_kernel::Result<Option<Morsel>> {
        let index = self.check_stage(stage)?;
        self.pop_entry(index, &want, locality)
    }

    fn pop_blocking(
        &self,
        stage: StageId,
        want: PayloadSpec,
        locality: Locality,
    ) -> moruna_kernel::Result<Option<(Morsel, u64)>> {
        let index = self.check_stage(stage)?;
        self.pop_blocking_entry(index, &want, locality)
    }

    fn peek_resident(&self, stage: StageId, want: PayloadSpec, locality: Locality) -> bool {
        match self.check_stage(stage) {
            Ok(index) => self.peek(index, &want, locality),
            Err(_) => false,
        }
    }

    fn evicted(&self, stage: StageId) -> Vec<(Seq, moruna_kernel::Origin)> {
        match self.check_stage(stage) {
            Ok(index) => self.evicted_entries(index),
            Err(_) => Vec::new(),
        }
    }

    fn replace(&self, stage: StageId, morsel: Morsel) -> moruna_kernel::Result<()> {
        let index = self.check_stage(stage)?;
        self.push_entry(index, morsel, true)
    }

    fn shutdown(&self) {
        self.shutdown_engine();
    }

    fn set_committed(&self, seq: Seq) {
        self.commit_to(seq);
    }

    fn checkpoint(&self, extras: &CheckpointExtras) -> moruna_kernel::Result<PathBuf> {
        self.write_manifest(extras)
    }

    fn restore(
        &self,
        manifest: &Path,
        plan: &[Split],
        fingerprints: &[Fingerprint],
    ) -> moruna_kernel::Result<ResumePoint> {
        self.restore_from(manifest, plan, fingerprints)
    }

    fn is_full(&self, stage: StageId) -> bool {
        match self.check_stage(stage) {
            Ok(index) => self.queue_is_full(index),
            Err(_) => true,
        }
    }

    fn set_consumer(&self, stage: StageId, want: PayloadSpec) {
        let Ok(index) = self.check_stage(stage) else {
            return;
        };
        {
            let mut queue = self.lock_queue(index);
            queue.consumer = want;
            queue.target = self.target_tier(&want);
        }
        self.plan_and_issue(index);
    }

    fn set_budgets(&self, budgets: TierBudgets) {
        {
            let mut held = self.budgets.write().unwrap_or_else(|e| e.into_inner());
            self.disk_budget.store(budgets.disk, Ordering::Release);
            *held = budgets;
        }
        self.plan_every_queue();
    }

    fn set_water(&self, stage: StageId, tier: TierKind, low: u64, high: u64) {
        let Ok(index) = self.check_stage(stage) else {
            return;
        };
        {
            let mut queue = self.lock_queue(index);
            queue.water[tier.index()] = (low, high);
            queue.water_set[tier.index()] = true;
        }
        self.plan_and_issue(index);
    }

    fn set_staging(&self, stage: StageId, enabled: bool) {
        let Ok(index) = self.check_stage(stage) else {
            return;
        };
        {
            let mut queue = self.lock_queue(index);
            queue.staging_enabled = enabled;
        }
        self.plan_and_issue(index);
    }

    fn set_promotion_window(&self, stage: StageId, morsels: u16) {
        let Ok(index) = self.check_stage(stage) else {
            return;
        };
        {
            let mut queue = self.lock_queue(index);
            queue.promotion_window = morsels.max(1);
        }
        self.plan_and_issue(index);
    }

    fn close(&self, stage: StageId) {
        let Ok(index) = self.check_stage(stage) else {
            return;
        };
        let waiters = {
            let mut queue = self.lock_queue(index);
            queue.closed = true;
            queue.waiters.clone()
        };
        // Nothing more will be appended to this queue's segment, so it stops being active
        // and can be reclaimed once its records are released (f.7, PL-I8).
        self.retire_active_segment(stage);
        for (_, waiter) in waiters {
            waiter.unpark();
        }
        self.plan_and_issue(index);
    }

    fn stats(&self) -> PlacementStats {
        stats::stats(self)
    }
}

/// The payload kinds a segment record header encodes (e.3).
pub(crate) fn kind_code(kind: PayloadKind) -> u8 {
    match kind {
        PayloadKind::Table | PayloadKind::Either => 0,
        PayloadKind::Tensor => 1,
    }
}

/// The payload kind for a segment record header's `kind` byte (e.3).
pub(crate) fn kind_of_code(code: u8) -> Option<PayloadKind> {
    match code {
        0 => Some(PayloadKind::Table),
        1 => Some(PayloadKind::Tensor),
        _ => None,
    }
}
