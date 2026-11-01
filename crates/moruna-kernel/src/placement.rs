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

/// Every queue's state, the bytes in flight, and the buffer catalog's figures.
#[derive(Clone, Debug, Default)]
pub struct PlacementStats {
    /// One entry per queue, by queue id. A batch fanned out to `m` queues is in each of their
    /// `bytes_by_tier`; [`CatalogStats::resident_by_tier`] counts it once.
    pub queues: Vec<QueueStats>,
    /// Bytes in moves the engine has issued and not yet seen complete.
    pub in_flight_bytes: u64,
    /// The buffer catalog (placement f.18): the one record of where every batch the engine
    /// holds is, and what moving them has cost.
    pub catalog: CatalogStats,
}

/// The buffer catalog's figures (placement f.18, adaptive-runtime 9.1). Every batch the engine
/// holds is counted once, whether it waits in one queue, in several (a fan-out, d.16), or in the
/// hands of a running unit that took it and has not released it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatalogStats {
    /// Bytes per tier now, indexed by `TierKind::index`; the `Disk` slot holds the batches whose
    /// only copy is a segment record.
    pub resident_by_tier: [u64; TIER_COUNT],
    /// The highest `resident_by_tier` reached in each tier over the run.
    pub peak_by_tier: [u64; TIER_COUNT],
    /// Bytes claimed against each tier and not yet resident: moves in flight and admissions.
    pub reserved_by_tier: [u64; TIER_COUNT],
    /// The budget in force per tier, beside which the two figures above are read.
    pub budget_by_tier: [u64; TIER_COUNT],
    /// Bytes of batches a running unit has taken and not yet released.
    pub held_bytes: u64,
    /// Batches the catalog records now.
    pub buffers: u64,
    /// Bytes written to disk to free memory.
    pub spilled_bytes: u64,
    /// Batches written to disk to free memory.
    pub spills: u64,
    /// Bytes read back from disk into memory.
    pub reloaded_bytes: u64,
    /// Batches read back from disk into memory.
    pub reloads: u64,
    /// Batches sent out of memory to free it, by the eviction order or by a policy's spill:
    /// spilled, a resident copy dropped beside a disk copy, or dropped to be read again from the
    /// source.
    pub evictions: u64,
    /// Times a consumer reached a batch that eviction had taken out of memory.
    pub misses: u64,
    /// The misses weighted by each one's reload cost: the disk bytes bringing it back moves.
    pub miss_reload_bytes: u64,
    /// Shares a consumer took and then released.
    pub released: u64,
    /// Batches freed: every consumer took and released them, and their bytes left memory.
    pub freed: u64,
    /// Bytes a batch pushed beyond the reservation made for it.
    pub overrun_bytes: u64,
    /// Batches memory could not admit, pushed straight to disk (`Residency::push_spilled`).
    pub overflows: u64,
    /// Their bytes.
    pub overflow_bytes: u64,
    /// Bytes of such batches whose records have not landed yet: admitted to no tier.
    pub outside_bytes: u64,
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
/// manifest on behalf of the scheduler (placement e.5). Sources and sinks are indexed by their
/// numbers in the run's topology (d.16).
#[derive(Clone, Debug, Default)]
pub struct CheckpointExtras {
    /// `(stage, instance, bytes)` from `KernelState::checkpoint` for `Checkpoint` kernels.
    pub kernel_states: Vec<(StageId, usize, Vec<u8>)>,
    /// From each sink's `Sink::checkpoint`.
    pub sink_states: Vec<Option<Vec<u8>>>,
    /// From each sink's `Sink::committed_seq` at the moment of the checkpoint.
    pub committed: Vec<Option<Seq>>,
    /// Where each source's drive is: the index into its plan and the next row within that
    /// split, plus the next sequence number to assign.
    pub source_cursors: Vec<SourceCursor>,
    /// Per source, the reads its drive has issued and not yet pushed to its queues, ascending
    /// `seq`: behind the cursor, so the cursor does not name them, and in no queue, so the
    /// lineage does not either. `restore` hands each back in `ResumePoint::to_recompute` unless
    /// the lineage already holds it (placement e.5, MH 4.7). Re-reading one is safe because
    /// `read` is deterministic for a repeatable source (CT-I12).
    pub issued: Vec<Vec<(Seq, Origin)>>,
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
    /// Per source, the morsels it read that the manifest could not restore from disk and that
    /// some sink had not committed, and the reads `CheckpointExtras::issued` named that the
    /// lineage does not hold: the scheduler re-reads each from its origin and pushes it to the
    /// source's queues with its original `seq` before restarting the source's drive, and each
    /// sink drops what its own watermark already covers (placement f.13). Ascending `seq`, no
    /// duplicates.
    pub to_recompute: Vec<Vec<(Seq, Origin)>>,
}

/// The queues between stages, and the movement of bytes between tiers (component 9).
/// Every placement keeps a buffer catalog and offers its port (d.17), so the scheduler claims
/// what it pushes and its policy reads and drives memory through the same object.
pub trait Placement: crate::catalog::Residency + Send + Sync {
    /// Never blocks. Ok(()) even if the queue is above high water; the caller
    /// consults `is_full` for admission.
    fn push(&self, stage: StageId, morsel: Morsel) -> crate::Result<()>;
    /// Push one morsel into every queue of `stages`, its producer's fan-out (d.16): the
    /// payload is shared by each, never copied, and its bytes are resident once until the last
    /// queue's consumer has taken it. The default pushes a share of the payload into each in
    /// order; the engine overrides it to count the bytes once and to record the morsel's lineage
    /// at every queue in one step (placement f.1).
    fn push_to(&self, stages: &[StageId], morsel: Morsel) -> crate::Result<()> {
        let Some((last, first)) = stages.split_last() else {
            return Err(crate::MorunaError::Plan(
                "a morsel is pushed into at least one queue".into(),
            ));
        };
        for stage in first {
            let share = Morsel {
                seq: morsel.seq,
                stage: morsel.stage,
                payload: morsel.payload.share(),
                bytes: morsel.bytes,
                origin: morsel.origin.clone(),
                features: morsel.features.clone(),
            };
            self.push(*stage, share)?;
        }
        self.push(*last, morsel)
    }
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
    /// The consumer of `stage` is done with the batch it took as `seq`: the unit that ran on it
    /// has finished, or the sink's write of it has resolved. Until then the batch counts as
    /// resident and nothing evicts it; once every consumer has taken and released it, its bytes
    /// leave the catalog (placement f.18). `Staging` when `stage` holds no such take.
    fn release(&self, stage: StageId, seq: Seq) -> crate::Result<()>;
    /// Entries in state `Evicted` for `stage`, oldest first; the scheduler re-reads each
    /// from its origin and pushes the replacement with `replace`. A batch fanned out to several
    /// queues is one batch, listed once, under the lowest of them, and one `replace` brings it
    /// back for every queue (placement f.18).
    fn evicted(&self, stage: StageId) -> Vec<(Seq, Origin)>;
    /// Replace an `Evicted` entry's bytes (same `seq`) in its original position.
    fn replace(&self, stage: StageId, morsel: Morsel) -> crate::Result<()>;
    /// Cancel every in-flight move, stop planning, release reservations (preamble 4.3). Idempotent.
    fn shutdown(&self);
    /// Sink `sink` has committed every morsel with a sequence number at or below `seq`. The
    /// engine forgets the lineage of a source's morsel once every sink it reaches has committed
    /// what it became there (placement f.11).
    fn set_committed(&self, sink: u16, seq: Seq);
    /// Write the run manifest atomically to the staging directory (placement e.5).
    /// Returns the manifest path. An engine without a staging directory returns
    /// `Err(Resume("no staging directory"))`.
    fn checkpoint(&self, extras: &CheckpointExtras) -> crate::Result<PathBuf>;
    /// Rebuild queues from a manifest written by `checkpoint`: entries with a disk
    /// copy come back `OnDisk`; the rest are listed in `ResumePoint::to_recompute`.
    /// Must be called before any `push`. Validates the manifest against each source's plan
    /// (`plans[i]`) and the kernel fingerprints it is given.
    fn restore(
        &self,
        manifest: &Path,
        plans: &[Vec<Split>],
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
