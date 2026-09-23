//! The placement contract and the resume types (contracts d.10).

use std::path::{Path, PathBuf};

use crate::fingerprint::Fingerprint;
use crate::ids::{Seq, StageId};
use crate::morsel::{Morsel, Origin};
use crate::payload::PayloadSpec;
use crate::source::Split;
use crate::tier::{TIER_COUNT, TierKind};

/// The bytes the runtime may hold per tier.
#[derive(Clone, Debug, Default)]
pub struct TierBudgets {
    /// Per device, indexed by `DeviceId`.
    pub device: [u64; 8],
    /// Page-locked host memory.
    pub pinned_host: u64,
    /// Ordinary host memory.
    pub host: u64,
    /// The staging cap.
    pub disk: u64,
}

/// One queue's live state.
#[derive(Clone, Debug, Default)]
pub struct QueueStats {
    /// The stage whose output this queue holds.
    pub stage: StageId,
    /// Bytes per tier, indexed by `Tier::index`: Device (summed), PinnedHost, Host, Disk, Remote.
    pub bytes_by_tier: [u64; TIER_COUNT],
    /// Entries in the queue.
    pub count: u64,
    /// Pops that waited on a move.
    pub misses: u64,
    /// Microseconds waited on moves, summed.
    pub miss_wait_us: u64,
    /// Demotions completed.
    pub demotions: u64,
    /// Promotions completed.
    pub promotions: u64,
}

/// Every queue's state plus the bytes in flight.
#[derive(Clone, Debug, Default)]
pub struct PlacementStats {
    /// One entry per queue, by stage.
    pub queues: Vec<QueueStats>,
    /// Bytes in moves the engine has issued and not yet seen complete.
    pub in_flight_bytes: u64,
}

/// Which node's memory a pop may be satisfied from. Reserved for the multi-node
/// extension; v1 callers pass `Any` and, with one node, the two are equivalent.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum Locality {
    /// Only entries resident on the calling node.
    Local,
    /// Any node; the engine may issue a remote move to satisfy the pop.
    #[default]
    Any,
}

/// Pieces of a checkpoint the placement engine does not own but records in the
/// manifest on behalf of the scheduler (placement e.5).
#[derive(Clone, Debug, Default)]
pub struct CheckpointExtras {
    /// `(stage, instance, bytes)` from `KernelState::checkpoint` for `Checkpoint` kernels.
    pub kernel_states: Vec<(StageId, usize, Vec<u8>)>,
    /// From `Sink::checkpoint`.
    pub sink_state: Option<Vec<u8>>,
    /// From `Sink::committed_seq` at the moment of the checkpoint.
    pub committed_seq: Option<Seq>,
    /// Where the source drive is: the index into the plan and the next row within
    /// that split, plus the next sequence number to assign.
    pub source_cursor: SourceCursor,
}

/// Where the source drive is in the plan.
#[derive(Copy, Clone, Debug, Default)]
pub struct SourceCursor {
    /// Index into the plan.
    pub split_index: u32,
    /// Next row within that split.
    pub row_offset: u64,
    /// Next sequence number to assign.
    pub next_seq: Seq,
}

/// What `Placement::restore` hands back so the scheduler can continue the run.
#[derive(Clone, Debug, Default)]
pub struct ResumePoint {
    /// The pieces the scheduler put in the manifest.
    pub extras: CheckpointExtras,
    /// Morsels the manifest knew about that have no disk copy and are not
    /// committed: the scheduler re-reads each from its origin and pushes it to
    /// Q0 with its original `seq` before restarting the source drive.
    pub to_recompute: Vec<(Seq, Origin)>,
}

/// The queues between stages, and the movement of bytes between tiers (component 9).
pub trait Placement: Send + Sync {
    /// Never blocks. Ok(()) even if the queue is above high water; the caller
    /// consults `is_full` for admission.
    fn push(&self, stage: StageId, morsel: Morsel) -> crate::Result<()>;
    /// Returns the head if it is resident in a tier satisfying `want` and `locality`;
    /// `Ok(None)` if the queue is empty or the head is not yet resident. Never blocks.
    fn pop(
        &self,
        stage: StageId,
        want: PayloadSpec,
        locality: Locality,
    ) -> crate::Result<Option<Morsel>>;
    /// Blocks until the head is resident in a tier satisfying `want` and `locality`,
    /// or the queue is closed (returns Ok(None)). The only blocking call in the contract (CT-I7).
    /// The second element is the microseconds the caller waited on a move (0 when none);
    /// the scheduler writes it to the trace as `placement_miss_wait_us` (PL-I9).
    fn pop_blocking(
        &self,
        stage: StageId,
        want: PayloadSpec,
        locality: Locality,
    ) -> crate::Result<Option<(Morsel, u64)>>;
    /// True if the head of `stage` is resident in a tier satisfying `want` (non-consuming;
    /// the scheduler's pick uses it so a worker never pops what it cannot run).
    fn peek_resident(&self, stage: StageId, want: PayloadSpec, locality: Locality) -> bool;
    /// Entries in state `Evicted` for `stage`, oldest first; the scheduler re-reads each
    /// from its origin and pushes the replacement with `replace`.
    fn evicted(&self, stage: StageId) -> Vec<(Seq, Origin)>;
    /// Replace an `Evicted` entry's bytes (same `seq`) in its original position.
    fn replace(&self, stage: StageId, morsel: Morsel) -> crate::Result<()>;
    /// Cancel every in-flight move, stop planning, release reservations (preamble 4.3). Idempotent.
    fn shutdown(&self);
    /// The sink has committed every morsel with a sequence number at or below `seq`.
    /// Lets the engine forget the lineage of committed morsels (placement f.11).
    fn set_committed(&self, seq: Seq);
    /// Write the run manifest atomically to the staging directory (placement e.5).
    /// Returns the manifest path. An engine without a staging directory returns
    /// `Err(Resume("no staging directory"))`.
    fn checkpoint(&self, extras: &CheckpointExtras) -> crate::Result<PathBuf>;
    /// Rebuild queues from a manifest written by `checkpoint`: entries with a disk
    /// copy come back `OnDisk`; the rest are listed in `ResumePoint::to_recompute`.
    /// Must be called before any `push`. Validates the manifest against the plan
    /// and kernel fingerprints it is given.
    fn restore(
        &self,
        manifest: &Path,
        plan: &[Split],
        fingerprints: &[Fingerprint],
    ) -> crate::Result<ResumePoint>;
    /// True when the stage may not take more work.
    fn is_full(&self, stage: StageId) -> bool;
    /// Declare the consumer of a queue so promotion targets the right tier.
    fn set_consumer(&self, stage: StageId, want: PayloadSpec);
    /// Replace the global tier budgets.
    fn set_budgets(&self, budgets: TierBudgets);
    /// Replace one queue's water marks for one tier.
    fn set_water(&self, stage: StageId, tier: TierKind, low: u64, high: u64);
    /// Turn demotion to disk on or off for one queue.
    fn set_staging(&self, stage: StageId, enabled: bool);
    /// How many morsels ahead of the head are promoted.
    fn set_promotion_window(&self, stage: StageId, morsels: u16);
    /// No more pushes will arrive for this stage; pops drain then return None.
    fn close(&self, stage: StageId);
    /// Every queue's live state.
    fn stats(&self) -> PlacementStats;
}
