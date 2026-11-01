# Moruna SDD 09: Placement engine (`moruna-placement`)

**Document type:** software design document, component 9 of 12, the load-bearing component
**Status:** DRAFT · 2026-09-15, amended 2026-10-29 by KA5 (a DAG's queues: PL-I16, PL-I17, f.17, manifest version 2, PL-T27, PL-T28) and by KD7a (the buffer catalog with known-future eviction: PL-I16 restated, PL-I18 to PL-I22, f.18, PL-T29 to PL-T37, PL-B1; adaptive-runtime 9.1) (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/moruna-runtime-design.md` sections 5.6, 5.6a; decisions D5, D10, D11 (preconditions); criteria S10, S14, S15; global invariants G-I1, G-I2, G-I3
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.10 (`Placement`, `TierBudgets`, `QueueStats`, `PlacementStats`, `CheckpointExtras`, `ResumePoint`), d.2 (`Tier`, `TierKind`, `SegmentRef`), d.3 (`Buffer`, `BufferView`, `Allocator`), d.9 (`Reactor`, `Completion::then`, `CopySrc`, `CopyDst`), e.1 (tier transitions), e.7 (`moruna_kernel::ipc`)
**Component location:** `crates/moruna-placement`, Rust; features `cuda`, `gds`
**Consumes:** contracts (1); the arena (2) as `Arc<dyn Allocator>` and the reactor (6) as `Arc<dyn Reactor>`, neither as a crate; discovery's (3) host profile as values in `PlacementConfig`. **Consumed by:** scheduler (10), controller (11), facade (12, `find_manifest` and `read_manifest_header`)

**Decisions worth your eye:** (1) a queue is a FIFO of entries with a per-entry state machine and the engine plans moves from queue position, not from time: promote the next `k` entries toward the consumer's tier, demote from the tail, and never both for one entry at once; (2) source morsels (Q0) may be demoted to disk as recomputable entries that are dropped rather than written when the disk tier is under pressure, and re-read from the source through the scheduler; (3) the staging log is append-only with fixed-size segment files numbered globally across queues, and a segment is deleted only when every entry in it has been promoted or consumed and the current run manifest no longer references it, which trades some disk for no compaction; (4) the engine writes the run manifest (e.5) because it is the one component that already knows where every uncommitted morsel is; (5) the engine has no thread: every move completes through `Completion::then` on the reactor thread that resolved it, and every reactor call is issued from the worker inside `push` or `pop`, which never blocks (RE-I6).

---

## a. Purpose and boundary

The placement engine owns every morsel between the moment a producer pushes it and the moment a consumer pops it. Its job is to have each morsel's bytes in the tier its consumer declared, before the consumer asks, using only DMA to move them, within per-tier budgets the controller sets. It keeps the head of every queue resident, demotes from the tail when a tier is over its high-water mark, uses local disk as a deliberate tier rather than an emergency, and reports every miss so the controller can widen the promotion window.

It owns: queue order; entry states; tier accounting and in-flight reservations; the move planner; the staging log (segment files, their format, their lifecycle); the disk budget; promotion and demotion policy; miss statistics; the run manifest (the record of every morsel not yet committed by the sink, where its bytes are if they are on disk, and where the source drive is) and the lineage index behind it; the resume path that rebuilds queues from a manifest.

It refuses to know: morsel contents; why a budget is what it is (controller); which worker will pop (scheduler); how a byte is moved (reactor); anything about kernels beyond their declared `PayloadSpec`; how to re-read a morsel from its origin (it lists what needs re-reading; the scheduler does it).

The engine is also the seam for the multi-node extension (architecture section 11): a `Tier::Remote` entry is an entry whose bytes are in another node's registered memory, and the move table has reserved rows for it. No v1 code path produces such an entry; every v1 path that could meet one returns `Unsupported("rdma")` (CT-I11).

## b. Vocabulary

**Entry.** One queued morsel: its position in its queue and the batch it names; identified by `(stage, seq)`. Where the bytes are is the batch's (f.18), never the entry's.

**Batch.** One payload the engine holds, recorded once in the buffer catalog however many queues it waits in (PL-I18). e.1's states are a batch's states. A batch has a size, a state (where its bytes are and what move is in flight), a disk copy when it has one, an owner (the producer: a source or a kernel stage), its remaining consumers, its holders and its reload cost.

**Buffer catalog.** The record of every batch the engine holds, from its push to the release of its last holder (f.18; adaptive-runtime 9.1). It is the one source of truth for residency: the tier totals, each queue's figure and the manifest's disk copies are all read from it.

**Remaining consumers.** The entries still to take a batch, one per queue it waits in. A push fixes them from the DAG's edges, the producer's queues (contracts d.16); each take removes one.

**Holder.** A consumer that took a batch and has not yet called `release` (contracts d.10): a running unit, or a sink write in flight. A held batch counts as resident and never leaves memory (PL-I19).

**Next use.** The fewest takes before a remaining consumer reaches a batch: the smallest position, head 0, of its entries across the queues that hold it. A queue loses entries only at its head (PL-I5), so a position is the number the entry was pushed as on its queue less the takes the queue has had, and nothing is walked to find it.

**Hot.** A batch with an entry inside its queue's promotion window: a consumer is about to take it. A hot batch never leaves memory for pressure (PL-I1, PL-I20).

**Reload cost.** The disk bytes bringing a batch back once it has left memory: its size, read from its record or from its source. A spill also writes the bytes, so it moves twice the size.

**Consumer spec.** The `PayloadSpec` the queue's consumer declared through `set_consumer`; the target tier for promotion is derived from it: `Device(cfg.devices[0])` when `tier == Device` and a device exists (v1 uses the first device for every queue and collapses per-device budgets into the one `Device` slot; several devices are a v1 limit noted in the report), else the host tier.

**Host tier.** The run's one host tier (contracts e.1): `PinnedHost` when `alloc.is_pinned()`, else `Host`; read once at `new`. No entry is ever in the other one, and no move between them exists.

**Target tier.** The tier a promoted entry should reach; per queue.

**Promotion window (`k`).** The number of entries from the head, inclusive, that the engine keeps moving toward the target tier ahead of the consumer.

**High water / low water.** Per queue per tier, in bytes: above high, demotion starts; demotion stops at low. Set by the controller.

**Reservation.** Bytes claimed against a destination tier's budget for a move in flight, released when the move completes or fails; or for an admission, a unit's or a source's batch claimed before it is made and pushed with `push_reserved` (f.18, PL-I22).

**Recomputable.** An entry whose bytes can be re-obtained from its origin (CT-I12). `recomputable = (stage == 0)`: in v1 only Q0 entries are treated as recomputable (D5); the lineage index makes every entry recomputable in principle.

**How a batch leaves memory.** One of four ways (f.18), which the catalog decides per batch from its state and the queues it waits in: `DropCopy` (a batch already `Resident + OnDisk` drops its resident copy, no IO, f.7), `Spill` (a record is written and the resident copy dropped when it lands, f.5), `Recompute` (a source's batch is dropped, `Evicted`, to be read again through `replace`, PL-I6) and `CopyDown` (a device batch is copied to the host tier, e.4). What decides between `Spill` and `Recompute` is each queue's `staging_enabled` flag (`set_staging(stage, on)`): a recomputable batch is written only when every queue it waits in stages and the run has a disk tier, and dropped otherwise; a batch that is not recomputable is written when some queue it waits in stages and the run has a disk tier, and cannot leave otherwise (PL-I6, f.2). The default is off for a queue a source gives and on for every other, so a chain's Q0 drops and its other queues write, as before. KD7a's `Shed` supersedes the reserved `DemotionPolicy` enum, which is gone.

**Segment.** A staging file, `staging.segment_bytes` in size, append-only, holding whole payload records; numbered by one counter per engine across all queues (`seg-<n:06>.seg`); one active segment per queue at a time, so records of one queue are contiguous within a segment and a segment holds records of one stage (the record header carries the stage regardless).

**Record.** One payload written into a segment: a record header in its own page, followed by the payload's pieces, each at a page-aligned offset (e.3).

**Lineage index.** A map from `seq` to `(origin, stage, disk copy if any)` for every morsel the engine has seen that the sink has not yet committed. It is what the manifest is written from and it is small: one 64-byte record per uncommitted morsel.

**Committed watermark.** The highest `seq` such that every morsel at or below it is committed by the sink, as told to the engine through `set_committed`. Lineage below the watermark is dropped.

**Manifest.** The file the engine writes at `checkpoint` and reads at `restore`: the lineage index, the source cursor, the sink and kernel checkpoints the scheduler hands over, and enough identity (run id, plan digest, kernel fingerprints) to refuse a resume against a different run.

## c. Invariants

**PL-I1. Head stays hot.** If the head entry of a queue is not resident in a tier satisfying the consumer spec, a move toward the target tier is in flight for it, and no demotion is in flight for it. Upholds G-I3.

**PL-I2. An entry has at most one move in flight.** Promotion and demotion of the same entry never overlap; a demotion request for an entry with a promotion in flight is dropped (the promotion wins because position beat pressure).

**PL-I3. Tier accounting is exact and includes reservations.** For every tier, `resident_bytes + reserved_bytes ≤ budget` at all times; a move is issued only after its destination reservation succeeds. Upholds G-I1.

**PL-I4. Bytes move only by the reactor.** The engine issues `Reactor::copy` for every tier change; it never reads or writes payload bytes itself. Upholds G-I2, S14.

**PL-I5. FIFO order is never violated.** `pop` returns entries in push order per queue; demotion changes where an entry's bytes are, never its position.

**PL-I6. Q0 never writes source morsels to disk unless told to.** With `set_staging(0, false)` (the default), Q0 demotion drops recomputable entries (`Evicted`) rather than writing; with `true` (weight-major and deep read-ahead, E8), Q0 writes like any other queue. Upholds D5.

**PL-I7. Disk is bounded, and a budget of zero means there is no disk tier.** Staging bytes across all queues never exceed `budget.disk`, and when `budget.disk` is 0 the planner never plans a demotion to disk at all: the queue's `staging_enabled` is read as `staging_enabled && disk_budget > 0`, so a run on a host with no writable staging directory runs without spill rather than dying mid pass with `Staging` on the first demotion (PM, 2026-09-22, on the first wheel; preamble section 5 already said 0 disables the disk tier). Disk is charged per segment: opening a segment charges `segment_bytes` against `disk_bytes` (`disk_bytes + segment_bytes ≤ disk_budget`, else no segment opens), and unlinking one releases it; a record never charges separately, because the segment is preallocated at its full size. When a demotion needs a new segment and none can open, the engine evicts a recomputable entry or, for a non-recomputable one, fails the run with `MorunaError::Staging` carrying the totals, rather than growing. Upholds S10.

**PL-I8. A segment is deleted only when empty and unreferenced.** A segment file is unlinked only when every record in it is in state `Consumed` or has been promoted and its disk copy released, and the current manifest does not reference it (PL-I12); no live entry and no manifest on disk ever references a deleted segment.

**PL-I9. Misses are counted, never hidden.** Every `pop_blocking` that waits on a move increments `misses`, adds the wait to `miss_wait_us`, and returns the wait as the second element of its result (contracts d.10); the scheduler writes it into the trace as `placement_miss_wait_us`. There is no side channel.

**PL-I10. Close drains.** After `close(stage)`, pushes error, pops return the remaining entries in order then `None`; no entry is lost.

**PL-I11. Lineage is kept until commit.** Every morsel that has been pushed to any queue and not yet covered by the committed watermark has a lineage record (origin, current stage, disk copy if any), including morsels that have been popped and are inside a kernel or the sink. Upholds CT-I12. Rationale: the manifest must be able to name every morsel whose output the sink does not yet have, or resume produces gaps.

**PL-I12. The manifest is atomic and self-sufficient.** A manifest is written to a temporary name and renamed into place, so a reader sees either the previous manifest or the new one, never a partial one; and it references only segment files that exist at the moment of the rename (a segment is never unlinked while the current manifest references a record in it: unlink waits for the next manifest write). Rationale: a crash between two writes must leave a resumable state.

**PL-I13. Resume is by lineage, never by replication.** The engine never writes a morsel to disk for the sake of recovery; the manifest records what is on disk because pressure put it there, and lists the rest for recomputation. Upholds CT-I12, S17. Rationale: the normal path pays nothing for the ability to resume.

**PL-I14. The engine has no thread and never blocks a caller on IO.** Every reactor operation is issued from the thread that called `push`, `pop`, `replace`, a setter or `restore`, and returns at once (RE-I6); its outcome is observed through `Completion::then`, whose callback runs on the reactor thread that resolved it (contracts d.9). The engine never calls `Completion::wait`, never spawns a thread, and the only blocking call it offers is `pop_blocking`, which parks on a queue's unparker (CT-I7). Rationale: preamble 4.1; a placement thread would be a third party in every lock order.

**PL-I16. A fanned-out batch is one batch (KA5, 2026-10-29; restated by KD7a).** `push_to(stages, morsel)` records one batch in the catalog with one remaining consumer per queue and puts in each queue an entry naming it (contracts d.16, CT-I15); every consumer is handed a share of the one payload. The batch is resident, spilled, reloaded and freed once; each queue's figure counts it while it waits there (its water marks and `is_full` see what it holds), and the tier's total counts it once (PL-I18). KA5's per-entry groups, `shared_bytes` and `PlacementStats::shared_by_tier` are gone: the catalog's `resident_by_tier` is the total.

**PL-I17. Lineage is by source morsel, at every position (KA5).** A lineage record is keyed by its root, the source and the number it read the morsel as (`Topology::root`), and holds a position for every queue a morsel derived from it is in, or was taken from by a consumer that has not produced from it yet. A push removes the position the morsel was taken from (`Topology::parent`) and adds one at each queue, in one step under the lineage lock, so a manifest sees the morsel before or after the move and never half way. A record is released when every sink its source reaches has committed what every path makes of it (f.11). In a chain a record has one position, which moves from queue to queue: the chain's lineage exactly.

**PL-I18. The catalog is the one record of residency (KD7a, 2026-10-29).** Every batch the engine holds, waiting or held, has exactly one record in the catalog, and nothing else records where its bytes are: an entry names its batch; the tier totals (PL-I3), each queue's figure, the held bytes and the manifest's disk copies (e.5) are read from the catalog. A batch's bytes are counted once in its tier however many queues hold it, and in the `Disk` slot while a segment record is its only copy. Rationale: adaptive-runtime 9.1; two records of one residency drift, and a fan-out spilled per entry wrote its bytes once per queue.

**PL-I19. What a running unit holds stays; what nothing needs is freed, never spilled (KD7a).** A batch with a holder never leaves memory, by any shed or by a policy's `spill`, and its bytes count as resident until the holder releases them. A batch whose last consumer has taken it and whose last holder has released it is freed at once: its bytes leave every total, its record is released (f.7), and `Freed` is emitted; no path writes it. `release` of a take that does not exist is refused with `Staging`. Rationale: spilling bytes a unit still reads frees nothing and lies to the budget.

**PL-I20. Known-future eviction (KD7a).** When memory must be freed, the batches that leave are chosen by the engine's `EvictionOrder` from the candidates of f.18, and the order shipped is `KnownFuture`: the candidate with the largest `(next_use + 1) × bytes / reload` leaves first, where `reload` is the disk bytes leaving and coming back moves (`bytes` for `DropCopy`, `Recompute` and `CopyDown`, `2 × bytes` for `Spill`); ties go to the larger batch, then to the lower id; candidates leave in that order until the bytes freed reach the need. A hot batch, a held batch, a batch with a move in flight and a batch no consumer waits for are never candidates. Rationale: adaptive-runtime 2 (G7), Belady's rule over the known future, weighed by reload cost against size.

**PL-I21. The totals follow the records exactly (KD7a).** Every change of a batch goes through one function that takes its bytes out of every total before the change and puts them back after it: the tier totals, each queue's figure, the held bytes, the bytes with a demotion in flight and the set of batches per tier. The peak per tier is raised at every such change, so it sees every value the total took, not only those an observer sampled.

**PL-I22. Never past the budget (KD7a, adaptive-runtime 12.5).** A reservation, for a move or an admission, is granted only when the tier's resident bytes, its reservations and the new claim fit its budget; a claim larger than the budget is refused for ever. A batch pushed with `push_reserved` turns its claim into its residency in one step under the catalog lock, so a run whose every push is admitted under a claim never holds more than the budget; a batch larger than its claim is pushed, the excess counted as `overrun_bytes`, and the tier is shed back within its budget. A push made without a claim (the contract's `push`, which the scheduler uses until KD7b's admission reserves) is accounted and then shed back within the budget. When memory is short the catalog sheds, or answers `Pending` (room is on its way) or `Refused`; it never grants what does not fit.

**PL-I15. Segment files are touched only through the reactor, and a descriptor never outlives its file.** The engine writes and reads segment bytes through `write_file`, `read_file` and `copy`, names each segment to the reactor with `register_segment` before its first write, and calls `unregister_segment` before `unlink`, so the reactor's cached descriptor is closed and the space returns to the filesystem at unlink (RE-I8). The manifest is the one file the engine writes itself, with `std::fs` (f.12), because it is small and needs `fsync` and `rename` on the calling thread.

## d. Interfaces

### d.1 Exposed

```rust
pub struct PlacementConfig {
    pub run_id: RunId,                            // names the staging subdirectory and the manifest
    pub node: NodeId,                             // LOCAL_NODE in v1
    pub stages: u16,                              // number of queues = kernels + 1 (Q0 .. Qn)
    pub budgets: TierBudgets,                     // initial; controller updates
    pub staging_dir: Option<std::path::PathBuf>,  // None = no disk tier and no manifest
    pub durable_staging: bool,                    // profile.durable_staging.is_guaranteed(); recorded in the manifest
    pub disk_budget: u64,                         // budget.disk
    pub segment_bytes: u64,                       // staging.segment_bytes
    pub codec: StagingCodec,                      // Raw in v1 (contracts d.2); recorded in every segment record header
    pub page_bytes: usize,
    pub gds: bool,                                // profile.gds.is_available() && feature gds && reactor.paths().gds
    pub devices: Vec<DeviceId>,                   // devices[0] is the promotion target for Device consumers (b)
    // Manifest identity (e.5): what `checkpoint` writes and `restore` checks.
    pub plan_digest: [u8; 32],                    // BLAKE3 over the source plan (e.5); computed by the facade from `Source::plan`
    pub fingerprints: Vec<Fingerprint>,           // per stage 1..n, from `Kernel::fingerprint`
    pub resume_policy: Vec<ResumePolicy>,         // per stage 1..n, from `KernelHints::resume`
    pub config: serde_json::Value,                // the resolved configuration table (preamble section 5)
    pub checkpoint_enabled: bool,                 // checkpoint.enabled; false unlinks reclaimable segments at once (f.7)
    pub eviction: Arc<dyn EvictionOrder>,         // the order batches leave memory in (f.18); KnownFuture
}

pub struct PlacementEngine { /* private */ }
impl PlacementEngine {
    /// Reads `alloc.is_pinned()` once for the host tier; creates the run directory
    /// when `staging_dir` is `Some`; issues nothing to the reactor.
    pub fn new(cfg: PlacementConfig, alloc: Arc<dyn Allocator>, reactor: Arc<dyn Reactor>) -> Result<Arc<PlacementEngine>>;
    pub fn detailed_stats(&self) -> PlacementDetail;   // per-queue tier histograms, segments, evictions
    /// `staging_dir/moruna-<run_id>/manifest.json`, or None without a staging directory.
    pub fn manifest_path(&self) -> Option<std::path::PathBuf>;
    /// Locate the newest manifest (by `written_ns`) under `staging_dir` for `run_id`, or,
    /// with `None`, the newest manifest of any run in that directory (Python's `resume="auto"`).
    pub fn find_manifest(staging_dir: &std::path::Path, run_id: Option<RunId>) -> Result<Option<std::path::PathBuf>>;
    /// Read only the identity fields of a manifest (`run_id`, `node`, `hostname`, `version`)
    /// so the facade can build `PlacementConfig` with the manifest's run id before `restore`.
    pub fn read_manifest_header(path: &std::path::Path) -> Result<ManifestHeader>;
}
/// Contracts d.10, every method: `push`, `pop`, `pop_blocking`, `peek_resident`,
/// `evicted`, `replace`, `shutdown`, `set_committed`, `checkpoint`, `restore`, `is_full`,
/// `set_consumer`, `set_budgets`, `set_water`, `set_staging`, `set_promotion_window`,
/// `close`, `stats`. The scheduler and the controller hold `Arc<dyn Placement>`; only
/// the facade names the concrete type, for the two manifest helpers above.
impl Placement for PlacementEngine { /* contracts d.10 */ }

/// The buffer catalog (f.18): what a policy reads and drives, and what it is told.
pub struct BufferId(pub u64);
pub trait EvictionOrder: Send + Sync + Debug { fn name(&self) -> &'static str; fn order(&self, candidates: &mut [Candidate]); }
pub struct KnownFuture;                                   // PL-I20; the order `PlacementConfig::eviction` defaults to
pub struct Candidate { pub id: BufferId, pub bytes: u64, pub next_use: u64, pub reload: u64, pub arrived: u64, pub touched: u64, pub how: Shed }
pub enum Shed { DropCopy, Recompute, Spill, CopyDown }
pub enum CatalogEvent { Released { batch, stage, seq }, Freed { batch, bytes }, Spilled { batch, bytes }, Reloaded { batch, bytes }, Dropped { batch, bytes } }
pub trait CatalogObserver: Send + Sync { fn event(&self, event: &CatalogEvent); }
pub trait Residency: Send + Sync {                        // the port KD7b's policy drives (adaptive-runtime 9.2)
    fn snapshot(&self) -> CatalogSnapshot;                // every batch (size, state, disk copy, owner, consumers, holders, reload cost, future) and the figures
    fn reserve(&self, tier: Tier, bytes: u64) -> Result<Reserve>;   // Granted(Reservation) | Pending { need } | Refused { need, budget }
    fn shed(&self, tier: TierKind, bytes: u64) -> bool;   // free at least `bytes` by the order
    fn spill(&self, batch: BufferId) -> Result<()>;       // write one batch; refused for a held batch or one not resident in the host tier alone
    fn reload(&self, batch: BufferId) -> Result<()>;      // read one batch back into the host tier; refused without room
    fn observe(&self, observer: Arc<dyn CatalogObserver>);
}
impl Residency for PlacementEngine { /* f.18 */ }
impl PlacementEngine {
    pub fn push_reserved(&self, stages: &[StageId], morsel: Morsel, claim: Reservation) -> Result<()>;
    pub fn replace_reserved(&self, stage: StageId, morsel: Morsel, claim: Reservation) -> Result<()>;
    pub fn buffer_of(&self, stage: StageId, seq: Seq) -> Option<BufferId>;
    pub fn catalog_stats(&self) -> CatalogStats;          // what `stats().catalog` carries (contracts d.10)
    pub fn eviction_order(&self) -> &'static str;
}

pub struct ManifestHeader { pub version: u32, pub run_id: RunId, pub node: NodeId, pub hostname: String, pub written_ns: u64 }
```

`set_water(stage, tier: TierKind, low, high)` keys a queue's marks by `TierKind` (contracts d.2); `Device` means the one device slot of b. `pop_blocking` returns `Some((morsel, wait_us))` (f.4). `find_manifest` orders candidates by the header's `written_ns`, not by file modification time.

### d.2 Consumed

`moruna_kernel::{Placement, Morsel, Payload, PayloadSpec, PayloadKind, TierPref, Tier, TierKind, TIER_COUNT, StagingCodec, SegmentRef, RemoteRef, DeviceId, NodeId, LOCAL_NODE, RunId, Locality, Origin, Split, Fingerprint, ResumePolicy, CheckpointExtras, SourceCursor, ResumePoint, TierBudgets, QueueStats, PlacementStats, Buffer, BufferView, ManagedTensor, Allocator, Reactor, Completion, CopySrc, CopyDst, MorunaError, ipc, mrb1}` (`ipc::encode_framing` and `ipc::decode`, contracts e.7; `ManagedTensor::from_buffer`, d.4; `Buffer::into_arrow_buffer` and `BufferView::{of_arrow, of_tensor}`, d.3; `copy`, `write_file`, `read_file`, `read_file_opt`, `register_segment`, `unregister_segment`, `paths` of d.9); `crossbeam` (`Parker`/`Unparker` for `pop_blocking`); `serde_json` (the manifest, e.5; the only text format in this crate); `base64`; `blake3` (verifying `plan_digest`); `hostname` (the manifest's `hostname`); `std::fs` (the manifest write and the run directory only). No dependency on `moruna-arena`, `moruna-reactor` or `moruna-sinks`.

## e. Data model, formats and state machines

### e.1 Entry state machine

```mermaid
stateDiagram-v2
    [*] --> Resident : push / replace
    Resident --> Promoting : promote issued (toward target tier)
    Promoting --> Resident : done, no disk copy
    Promoting --> ResidentOnDisk : done, promoted from disk (copy kept)
    Promoting --> OnDisk : promotion failed (disk copy kept)
    Promoting --> Resident : promotion failed (source tier kept)
    Resident --> Demoting : demote issued (staging on, or Device to host tier)
    Demoting --> Resident : done, landed one tier down (Device to host tier); or failed with no disk copy, stays where it was
    Demoting --> ResidentOnDisk : failed, a disk copy already existed (stays where it was, copy kept)
    Demoting --> OnDisk : done, record written
    Resident --> Evicted : demote with staging off and recomputable (Q0)
    Evicted --> Resident : replace
    OnDisk --> Promoting : promote issued (read_file or GDS copy)
    ResidentOnDisk --> OnDisk : demote again (drop resident copy only, no IO)
    ResidentOnDisk --> Consumed : pop (disk record released)
    Resident --> Consumed : pop
    Consumed --> [*]
    state "Resident + OnDisk" as ResidentOnDisk
    state "OnRemote (reserved, rdma)" as OnRemote
    OnRemote --> Promoting : reserved
```

This diagram answers "given where an entry's bytes are now, which moves may the planner issue for it and which state does each completion land it in", with the disk copy as its own dimension so that a second demotion of a promoted entry is visibly free.

States: `Resident(tier)` (bytes in one resident tier, no disk copy), `Resident + OnDisk` (bytes resident and a valid segment record; `disk: Some`), `Promoting(from, to)`, `Demoting(from, to)`, `OnDisk(SegmentRef)` (no arena buffer), `Evicted` (no bytes anywhere; recomputable), `Consumed`. `OnDisk` is `Resident(Tier::Disk(_))` in the contract's terms; the engine keeps them distinct internally because an `OnDisk` entry has no arena buffer, and it keeps `Resident + OnDisk` distinct from `Resident` because the second demotion of such an entry costs no IO (f.7). Illegal: `Consumed` → anything; `Promoting` → `Demoting`; `Evicted` → `Promoting` (must be replaced first); any transition into or out of the other host tier (there is none, contracts e.1). Attempting one is a bug (`debug_assert`) and, in release, a `Staging` error.

Reserved (feature `rdma`, not v1): `OnRemote(NodeId, RemoteRef)`, the counterpart of `OnDisk` for bytes in another node's memory, entered by a remote demotion and left by a remote promotion. The `State` enum carries the variant now so that every `match` on it is written once (CT-I11); in a v1 build the variant is never constructed and its arms return `Unsupported("rdma")`.

`Consumed` is a terminal state for the queue, not for the lineage index: a consumed entry's lineage record stays until the committed watermark passes its `seq` (PL-I11), at which point it is dropped. Every transition marked "done" or "failed" happens inside a `Completion::then` callback on a reactor thread (PL-I14, g).

### e.2 Queue structure

```rust
struct Queue {
    stage: StageId,
    order: VecDeque<Entry>,                 // FIFO; head at front
    consumer: PayloadSpec, target: Tier,
    water: [(u64, u64); TIER_COUNT],        // (low, high) per TierKind::index: Device, PinnedHost, Host, Disk, Remote
    pushed: u64, taken: u64,                // entries pushed and taken so far: an entry's position is its number less `taken` (f.18)
    staging_enabled: bool,                  // set_staging; default false for stage 0, true otherwise (b)
    promotion_window: u16,
    active_segment: Option<u32>,            // the segment this queue appends to; segments themselves are global
    head_error: Option<MorunaError>,         // a failed move of the head, surfaced by the next pop (f.10) and by is_full (f.14)
    misses: AtomicU64, miss_wait_us: AtomicU64, promotions: AtomicU64, demotions: AtomicU64, evictions: AtomicU64,
    closed: bool, waiters: Vec<Unparker>,
}
struct Entry { seq: Seq, stage: StageId, origin: Origin, features: MorselFeatures, buffer: BufferId }   // KD7a: the rest is the batch's
struct Buffer { id: BufferId, bytes: u64, kind: PayloadKind, owner: Producer, payload: Option<EntryPayload>, state: State, disk: Option<SegmentRef>, move_id: Option<u64>, recomputable: bool, consumers: Vec<(StageId, Seq)>, holders: Vec<(StageId, Seq)>, arrived: u64, touched: u64, missed: bool }   // f.18
```

Global: `host_tier: Tier` (from `alloc.is_pinned()` at `new`), `catalog: Mutex<Catalog>` (f.18: every batch's record, the tier totals, the per-queue figures, the reservations and the figures of `CatalogStats`), `disk_bytes: AtomicU64`, `next_segment: AtomicU32` (the one segment counter), `segments: Mutex<BTreeMap<u32, Segment { stage, path, bytes_written, records_total, records_released, active }>>`, `budgets: RwLock<TierBudgets>`, `moves: Mutex<HashMap<u64, MoveInFlight { stage, seq, pieces_left, failed }>>`, `shutting_down: AtomicBool`, and the lineage index:

```rust
struct Position { stage: StageId, seq: Seq, kind: PayloadKind, bytes: u64, consumed: bool }   // KD7a: a disk copy is the batch's, read from the catalog (PL-I18)
struct Lineage { origin: Origin, positions: Vec<Position>, committed: bool }
lineage: Mutex<BTreeMap<Seq, Lineage>>,     // every uncommitted seq; lock order position 2b (after any queue lock, before tier accounting)
committed: AtomicU64,                        // the watermark; u64::MAX means "nothing committed yet"
manifest_refs: Mutex<HashSet<u32>>,          // segments the current manifest references (PL-I12)
```

A lineage record is created at the first `push` of a source morsel and holds its positions (PL-I17): a push moves a position from the queue it was taken from to each queue it is pushed into, a `pop` sets `consumed`, and `set_committed` drops it. Whether a position's bytes have a disk copy is not the lineage's: the manifest reads it from the catalog when it is written (f.12, PL-I18). Its size is bounded by the number of morsels between the source cursor and the committed watermark, which the queue budgets already bound.

### e.3 Segment file format

Segment files live at `staging_dir/moruna-<run_id>/seg-<segment:06>.seg`, numbered from one engine-wide counter across all queues, created at `segment_bytes` (`fallocate` through `std::fs`, the one file operation besides the manifest that does not go through the reactor, because it moves no bytes) and then registered with the reactor (`register_segment`) before the first write; written sequentially by direct IO. **Staging and spill stay uncompressed** (KA10): a segment record is the page-aligned record of contracts e.7, written by direct IO and read back into an arena buffer to be used where it lands, so a compressed record would be one more copy for a few bytes of disk, and `ipc::decode` refuses a compressed record (a ceiling of zero, contracts e.8) rather than inflate one. The same directory holds `manifest.json` (e.5). `<run_id>` is the 32-character hex form of `RunId`. Each record begins at a page boundary with a header page:

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic `MORUNSEG` |
| 8 | 8 | seq |
| 16 | 2 | stage |
| 18 | 1 | kind (0 table, 1 tensor) |
| 19 | 1 | codec (`StagingCodec`: 0 raw; a reader rejects an unknown value with `Unsupported("codec")`) |
| 20 | 4 | reserved zero |
| 24 | 8 | payload_len (bytes of the payload body, from body_offset to the end of its last piece, before tail padding) |
| 32 | 8 | body_offset (absolute in file; page-aligned; equals the header's offset plus one page) |
| 40 | 24 | reserved zero |
| 64 | to page end | zero padding (the header occupies one page) |
| body_offset | payload_len | body, in page-padded pieces |

The header is written from one class-0 arena buffer as a single page. Body for a table: the page-aligned Arrow IPC record of contracts e.7, produced by `moruna_kernel::ipc::encode_framing(batch, page_bytes, body_offset, &*alloc)`: the framing (Schema and RecordBatch messages, one page-rounded arena buffer) first, then each Arrow buffer of the batch at the page-aligned offset the encoder assigned, written with its length as is (a final piece that is not a page multiple lands buffered in the reactor, 06 e.3, which is correct and counted; the next record's header begins at the page boundary after it). The schema message is included per record so a segment is self-describing; the schema is small. Body for a tensor: the `MRB1` header (contracts e.4) in one page-rounded arena buffer, then the tensor bytes at `MRB1`'s `data_offset`. A segment is full when the next record's header page plus its page-padded pieces would not fit in `segment_bytes`; the engine then rolls (f.5). `SegmentRef { segment, offset: body_offset, len: payload_len }` is what the entry carries. Reading a record back needs only the `SegmentRef`: `ipc::decode` over the body finds the framing and the pieces by the offsets inside the framing.

Portable checkpoint export (KS1, coordinated 2026-10-30) uses `portable_record::encode` to lay a resident host payload out through these same RecordDraft/RecordHeader/Piece authorities, with caller-supplied real queue/sequence, unique synthetic segment id and charged metadata allocator. It creates a one-record segment with body offset one page; no new staging format or source replay is introduced. Before quiescing, the engine admits its snapshot/IO/encoding pool. During the barrier, encode uses only the supplied pre-reserved allocator adapter with no underlying fallback or wait; encoder validity/framing allocation exhaustion propagates, the engine releases the barrier, and checkpoint refusal remains explicit. An inaccurate fixed header-only reservation is not claimed. Sorted existing piece views retain their source allocations. A bounded `materialize` fills only the caller's charged chunk, explicitly zeros all holes/tail and reports actual copied versus zero-filled bytes. This export boundary copy is visible; it is not described as DMA or a payload-sized heap allocation. Device payloads refuse until the engine explicitly moves them into the admitted host tier. Restore reads the exact native record with the existing placement reader. Synthetic id allocation, checkpoint barrier and durable dependency publication remain the engine's authority.

### e.4 Move planning table

The legal transitions are contracts e.1, the single authority; this table repeats its rows and adds only which reactor calls the engine issues for each, in what order. "Host tier" is the run's one host tier (b). Every source of a `copy` or `write_file` is a `BufferView` over the entry's own buffers (`BufferView::of_arrow` per Arrow buffer, `BufferView::of_tensor` for a tensor), so a failed move loses nothing (RE-I1).

| From | To | Reactor calls the engine issues |
|---|---|---|
| host tier | Device(d) | one `copy(View(piece), Buffer(device_buf))` per Arrow buffer of a table (each into its own device buffer, the batch rebuilt over them) or one for a tensor; the reactor bounces when the arena is unpinned (06 f.5), which the engine does not see |
| Device(d) | host tier | one `copy(View, Buffer(host_buf))` per buffer; same |
| host tier | Disk | `write_file` per piece of the record into the active segment (f.5); no encode for a table beyond the framing, the pieces are the batch's own buffers |
| Disk | host tier | `read_file(segment path, body_offset, buf)` of `[body_offset, body_offset + len)` into one arena buffer, then `ipc::decode` over `buf.into_arrow_buffer()` (arrays point into it, no copy) or `ManagedTensor::from_buffer` after the `MRB1` header (f.6) |
| Disk | Device(d) | `copy(Disk(seg), Buffer(device_buf))` when `cfg.gds` (one call; the reactor uses the registered segment's cuFile handle); else the two rows Disk → host tier, host tier → Device issued in sequence by the engine, the second from the first's `then` |
| Device(d) | Disk | the two rows Device → host tier, host tier → Disk in sequence (GDS write is not used in v1) |
| host tier | Remote(n) | reserved, feature `rdma`, not v1: one `copy` whose destination is a `Remote`-tier endpoint the reactor obtained from node `n`'s lease (one-sided RDMA write); v1 returns `Unsupported("rdma")` |
| Remote(n) | host tier | reserved, feature `rdma`, not v1: one `copy` from a `Remote`-tier source (one-sided RDMA read into an arena buffer); v1 returns `Unsupported("rdma")` |

Pairs contracts e.1 calls illegal are never planned: there is no `Host ↔ PinnedHost` row because the two never coexist; `Remote ↔ Disk` is never issued (a remote copy is demoted by the node that owns the bytes); `Device ↔ Device` across devices does not arise with one target device. A planner that computes one of them has a bug, and the move issuer returns `Staging` naming the pair before calling the reactor.

Demotion target: one tier down from the current (`Device → host tier`, `host tier → Disk`), never two at once, so a device-heavy queue demotes to host first and to disk only if host is also over its high water. With the `rdma` feature, `host tier → Remote` sits between the host tier and `Disk` in that ladder when a peer node has budget to lend (contracts f.6 rank order); without it, the ladder is as stated.

### e.5 Run manifest

`staging_dir/moruna-<run_id>/manifest.json`, UTF-8 JSON, one object, written by f.12 and read by f.13. Version 2 (KA5, 2026-10-29; version 1, the chain's, is refused by its version, and differs in naming one source, one sink and one position per morsel):

| Key | Type | Meaning |
|---|---|---|
| `version` | integer | 2; an unknown version is refused, not skipped |
| `run_id` | string | 32 hex characters |
| `written_ns` | integer | wall clock at the write, nanoseconds since the epoch |
| `node` | integer | `NodeId` of the writer; `nodes` below lists every node of the run |
| `hostname` | string | the writer's hostname from the OS (`hostname` crate); in v1 the only way to tell "same node" from "another node" (f.13) |
| `nodes` | array of integer | `[0]` in v1 |
| `durable_staging` | boolean | from `PlacementConfig`; a resume on a different node checks this and refuses with `Resume` when false, because the segments it references are on the dead node's local disk |
| `plan_digests` | array of string | per source, BLAKE3 hex over its plan: for each split in order, `id` and `rows` as little-endian u64s (`uncompressed_bytes` is excluded because it may be an estimate that varies between plans); `PlacementConfig::plan_digests`, computed by the facade with the same rule, which `restore` recomputes from the `plans` it is given |
| `kernels` | array of string | kernel fingerprints per stage 1..n, hex; `PlacementConfig::fingerprints` |
| `resume_policy` | array of string | `reinit`, `checkpoint` or `forbid` per stage; `PlacementConfig::resume_policy` |
| `queues` | integer | number of queues; `restore` refuses a manifest of another number of queues, sources or sinks |
| `committed` | array of integer or null | each sink's watermark at the write |
| `source_cursors` | array of `{split_index, row_offset, next_seq}` | each source's, from `CheckpointExtras` |
| `issued` | array of `{source, seq, split, row_start, row_end, node}` | reads issued behind a cursor that had not reached their source's queues |
| `sink_states` | array of string or null | each sink's `Sink::checkpoint`, base64 |
| `kernel_states` | array of `{stage, instance, state}` | base64 of `KernelState::checkpoint`, `Checkpoint` kernels only |
| `lineage` | array of `{source, root, seq, split, row_start, row_end, node, stage, kind, bytes, disk}` | one per position of each source morsel some sink has not committed (PL-I17), by source and root; `stage` is the queue and `seq` the morsel's number on it; `disk` is `{segment, offset, len}` or null; `kind` is `table` or `tensor` |
| `segments` | array of `{stage, segment, bytes}` | every segment file the lineage references; `bytes` is the bytes written so far (`bytes_written`), which `restore` checks against the file (f.13) |
| `config` | object | `PlacementConfig::config`, the resolved configuration table (preamble section 5) at the write, for the report and for refusing a resume with a different `staging.segment_bytes` or `page.bytes` |

The manifest is small by construction: `lineage` holds only morsels between the committed watermark and the source cursor, which the queue budgets bound (a 32 GiB budget of 64 MiB morsels is about 500 entries, under 100 KiB of JSON). It is written to `manifest.json.tmp` and renamed (PL-I12). A run that ends normally leaves a final manifest whose watermarks are each sink's last number and an empty `lineage`; `Runtime::run` deletes the run directory after `finish` unless `checkpoint.keep` is set (preamble section 5).

## f. Algorithms and policies

**f.1 `push(stage, morsel)`.** After `shutdown`, `Err(Cancelled)`. Check the tier (below) and that the queue is open; record the batch in the catalog, `Resident(morsel.payload.tier())`, with the queue as its one remaining consumer (f.18 rule 1); lock the queue and append an entry naming it, telling the catalog its ordinal; unlock; record the lineage position (e.2); unpark one waiter; call `plan(stage)`; shed the tier back within its budget if the push put it over (f.18 rule 6b). `push_to(stages, morsel)` does the same with one consumer per queue (f.17). A push whose payload tier is `Remote` is `Unsupported("rdma")` in v1; a push whose payload tier is a host tier other than the run's is `Staging("wrong host tier")`, which cannot happen through the arena (AR-I6) and is a bug. The reactor calls `plan` issues are made on the pushing worker's thread and return at once (PL-I14).

**f.2 `plan(stage)`.** Runs after every push, pop, move completion and knob change, under the queue lock, in this order:

1. *Head first.* If the head is not resident in a tier satisfying the consumer spec and has no move in flight, issue a promotion for it (PL-I1). If the head is `Evicted`, record it for `evicted()` and stop planning this queue (the scheduler must replace it).
2. *Window.* For entries 2..=k from the head that are not at the target tier and have no move in flight, issue promotions in position order, each only if the destination reservation succeeds (PL-I3); stop at the first reservation failure (the tier is full; demotion will make room).
3. *Pressure.* For each tier from `Device` down to the host tier: if the queue's figure in the tier, less the bytes a demotion in flight is already taking out of it, is above `high[tier]`, the catalog is asked to shed the excess down to `low[tier]` from the batches this queue holds (f.18): which ones leave, and how, is the eviction order's, over every queue's knowledge of each batch's future, not this queue's tail. When the shed finds no candidate in the target tier the queue records it and reports full (f.14), and admission stops (the scheduler's rule). The planner reads the head and the window and nothing behind them, so a plan costs the window, not the queue.
4. *Disk bound.* A disk write needs room in the queue's active segment; when the record does not fit, a new segment must open, which charges `segment_bytes` (PL-I7): if `disk_bytes + segment_bytes > disk_budget`, the write is not issued; if the entry is recomputable, evict; else fail the run with `Staging` carrying `disk_bytes`, `disk_budget` and the segment size (PL-I7): the error is stored as the queue's `head_error`, `is_full` becomes true (f.14), and the next `pop` on the queue returns it (f.10).

**f.3 `pop(stage, want, locality)`.** Lock queue; if `head_error` is set, take it and return `Err` (f.10); if empty or closed-and-empty, return `Ok(None)`; if the head's batch is `Resident` (or `Resident + OnDisk`) in a tier satisfying `want`, take it (f.18 rule 2: the consumer's share, the consumer now a holder; the disk record released when no consumer is left, f.7), remove the entry, unlock, set `consumed` on its lineage position, `plan`, return it; the consumer calls `release` when it is done with it (contracts d.10, PL-I19); else return `Ok(None)` (not resident yet). `peek_resident(stage, want, locality)` is the same test without the removal, under the same lock, and is what the scheduler's pick calls so a worker never pops what it cannot run. `locality` is accepted and, in v1, ignored after a `debug_assert` that the head is not `OnRemote`; with the `rdma` feature, `Local` refuses a head whose bytes are remote and lets the scheduler pick another stage while the promotion lands. `want` satisfaction: `TierPref::Any` accepts any resident tier; `Host` accepts `Host` or `PinnedHost`; `Device` accepts `Device(_)`. A resident head in the wrong tier (for example on `Host` when the consumer wants `Device`) is not returned; f.2 step 1 will have a promotion in flight for it.

**f.4 `pop_blocking`.** Loop: `pop`; on `Ok(None)` and not closed-empty, record the wait start, park on the queue's unparker with a 10 ms timeout (to observe `close` and shutdown), and on wake retry; on success, if a wait was recorded, `misses += 1`, `miss_wait_us += elapsed` (PL-I9) and return `Some((morsel, elapsed_us))`, else `Some((morsel, 0))`; on closed-empty, `None`. After `shutdown`, `Err(Cancelled)` on the next iteration. No thread-local, no side channel: the wait is in the return value (contracts d.10).

**f.5 Demotion to disk (write).** On the issuing thread, under the queue lock: compute the record's pieces (header page; for a table `encode_framing(batch, page_bytes, body_offset, &*alloc)` gives the framing buffer and each body buffer's offset; for a tensor the `MRB1` header buffer and the tensor bytes) and the record's page-padded length; if the active segment has no room, roll: take `n = next_segment.fetch_add(1)`, check `disk_bytes + segment_bytes ≤ disk_budget` (else f.2 step 4), `fallocate` `seg-<n:06>.seg` at `segment_bytes` with `std::fs`, charge `disk_bytes += segment_bytes`, `reactor.register_segment(n, path)`, mark the previous active segment inactive and this one active, `placement.segment_roll`; set the entry `Demoting(host tier, Disk)` with a move id whose `pieces_left` is the piece count; unlock. Then issue one `write_file(path, offset, view)` per piece: the header and framing from their arena buffers' views, each body from `BufferView::of_arrow(buf, alloc)` (table) or `BufferView::of_tensor` (tensor), with lengths as is (e.3); each returns at once, and each `then` callback decrements `pieces_left` under the moves lock (g). When the last piece resolves without error: lock the queue, drop the entry's `Morsel` (releasing the arena buffers; the views are gone with the completions), set `OnDisk(SegmentRef { segment: n, offset: body_offset, len })`, subtract from the host tier, `records_total += 1` on the segment, `bytes_written` advanced to the padded end, `demotions += 1`, set `disk` on the lineage record; unlock; `plan`. Any failed piece: f.10. The write is a DMA from the arena (direct IO for every page-multiple piece), so no CPU copy (PL-I4).

**f.6 Promotion from disk (read).** On the issuing thread: reserve `len` in the destination tier (f.9); `alloc(len rounded up to a page, destination tier)`; for a host-tier destination `read_file(segment path, body_offset, buf)`, for a device destination with `cfg.gds` `copy(CopySrc::Disk(seg), CopyDst::Buffer(buf))`, and without GDS the host-tier read followed, in its `then`, by `copy(View(host_buf.view()), Buffer(device_buf))` (e.4); set `Promoting(Disk, dest)`; return. In the final `then`: for a table, `ipc::decode(buf.into_arrow_buffer()?, page_bytes)` gives a `RecordBatch` whose arrays point into the buffer (contracts e.7; the arena token survives, so `Payload::table` infers the tier); for a tensor, `ManagedTensor::from_buffer(buf, amb1_data_offset, dtype, shape)` after parsing the `MRB1` header out of the same buffer; set `Resident + OnDisk` (the disk record stays valid, f.7), `promotions += 1`, release the reservation, unpark waiters, `plan`. A device-resident table is rebuilt over the device buffers by the same decode applied to the framing read into a small host buffer (the framing is read separately with a second `read_file` of its page-rounded length, because the metadata must be parsed on the CPU while the bodies land on the device).

**f.7 Segment lifecycle.** Each segment tracks `records_total` and `records_released`; a record is released when the last remaining consumer of its batch takes it (f.18 rule 2), whether or not it was promoted first; a batch fanned out to several queues has one record, released once (KD7a). The engine keeps the disk copy of a promoted batch until then so a later re-demotion of it is a no-op: `Resident + OnDisk` becomes `OnDisk` by dropping the resident copy, which is the cheapest demotion. When `records_released == records_total` and the segment is not active, it becomes *reclaimable*; it is unlinked at the first manifest write that no longer references it (PL-I8, PL-I12), or at once when `cfg.checkpoint_enabled` is false. Unlinking is: `reactor.unregister_segment(n)` first (so the reactor closes its descriptor and the space is really released, RE-I8, PL-I15), then `std::fs::remove_file`, then `disk_bytes -= segment_bytes`. Without a staging directory there are no segments and this case does not arise.

**f.8 Budgets and water marks.** `set_budgets` replaces the global budgets; `set_water` replaces a queue's marks; both trigger `plan` for every queue. Default water marks when the controller has not set them: high = tier budget / number of queues, low = high / 2.

**f.9 Reservation arithmetic.** `reserve(slot, bytes)`, under the catalog lock (f.18): granted when `resident[slot] + reserved[slot] + bytes ≤ budget[slot]`, where `resident[slot]` counts every batch once, held ones included (PL-I18, PL-I19); a refused reservation changes nothing. Released when its move completes or fails, or turned into residency by the push it admitted (PL-I22).

**f.10 Failure of a move.** A move fails when any of its completions resolves `Err`. The reactor has already applied its own per-operation fallback where one exists (06 f.9); the engine then re-issues the identical operation (same source view, same destination, same offsets) exactly once, from the `then` callback, with a fresh move id; the reactor chooses the path again. On the second failure: for a promotion, the entry stays where it was (`Resident`, `OnDisk`, or `Resident + OnDisk`, its reservation released) and the failure is recorded (`placement.move_failed`); if it was the head, the error is stored as `head_error` and the next `pop` returns `Err(Staging)` naming the morsel, which the scheduler turns into a run termination. For a demotion, the entry stays resident (the views were dropped; the bytes are intact, RE-I1), the partial record's pages are abandoned in the segment (`bytes_written` still advances past them, so the segment stays append-only; they are never referenced), pressure remains, the controller sees it through `bytes` above high water and reduces morsel targets; if the tier is at budget and no demotion succeeds, `is_full` holds (f.14), the scheduler stops admitting and, if the head cannot be produced, the run terminates (G-I8's diagnostic path).

**f.11 `set_committed(seq)`.** Store the watermark (monotonic: a lower value is ignored with a `debug_assert`). The engine does not serialise its callers and does not take the maximum for them: the watermark has one owner (10 f.11, the sink drive) and the calls arrive in increasing order, which is a requirement on the caller, not an arrangement the engine makes. Then: lock the lineage index (2b); remove every record with `seq` at or below the watermark, collecting the `SegmentRef`s of the removed records that had a disk copy into a local list; unlock the lineage index; then lock the segments map (3) and release each collected record in its segment (f.7), noting which segments became reclaimable; unlock; unlink the reclaimable ones now when `checkpoint_enabled` is false (f.7). The lineage lock and a queue lock are never held together, and the lineage lock is dropped before the segments map is taken (preamble 4.2: 2b before 3, and never 2 inside 2b). O(removed) per call; the scheduler calls it once per sink commit, not per morsel.

**f.12 `checkpoint(extras)`.** Without a staging directory, `Err(Resume("no staging directory"))`. Otherwise: lock the lineage index; snapshot it into the e.5 shape together with `extras`, the watermark, `cfg.plan_digest`, `cfg.fingerprints`, `cfg.resume_policy` and `cfg.config`; collect the set of segments referenced; unlock; lock the segments map, copy each referenced segment's `bytes_written`, unlock; serialise; with `std::fs` on the calling thread, write `manifest.json.tmp`, `fsync` it, rename over `manifest.json`, `fsync` the directory (the manifest does not go through the reactor: it is small, it needs the two `fsync`s and the rename in order on one thread, and the reactor's completions run elsewhere); replace `manifest_refs` with the collected set; then lock the segments map, take every reclaimable segment not in the set, unlock, and unlink each (f.7). Runs on the caller's thread (the scheduler's checkpoint thread, SC f.12; preamble 4.1), never on a worker, and never under a queue lock; the lineage lock is held only for the snapshot and is released before any other lock is taken. Duration is dominated by the two `fsync`s; `tracing` records it (`placement.checkpoint`).

**f.13 `restore(manifest, plan, fingerprints)`.** Must precede any `push`. Read and parse with `std::fs`; refuse (`Resume` with the path and the first mismatch) on: unknown `version`; `run_id` different from `PlacementConfig::run_id`; `plan_digest` different from the digest recomputed over `plan` (and from `cfg.plan_digest`, which must agree); `kernels` different from `fingerprints`; any `resume_policy == forbid`; `durable_staging == false` when the manifest's `hostname` differs from this host's (in v1 the hostname is the node identity; the multi-node extension replaces this with `NodeId`); `config.staging.segment_bytes` or `config.page.bytes` different from the current ones; a referenced segment whose file is missing (`std::fs::metadata` on the path) or which, once registered with the reactor, is shorter than the manifest claims: `read_file_opt(path, page_floor(max_end − 1), one page, allow_short = true)` must return at least `max_end − page_floor(max_end − 1)` bytes, where `max_end` is the largest `offset + len` in the lineage referencing it (the bytes are checked through the reactor, PL-I15, so the check sees what a promotion would see). Then: set the watermark; rebuild the lineage index from `lineage`; rebuild the segments map from `segments` with `bytes_written = bytes`, `records_total := the number of lineage records referencing the segment`, `records_released := 0`, `active = false`, and `disk_bytes = segment_bytes × count`; `next_segment := max(segment) + 1`; for each record with `disk` set, create an `OnDisk` entry in queue `stage` (the record's own stage); the queues are rebuilt in ascending `seq`, which restores FIFO order per queue (PL-I5) because sequence numbers were assigned in push order; every record without `disk` (resident bytes lost, or in flight inside a kernel or the sink at the crash) goes to `ResumePoint::to_recompute` as `(seq, origin)`, in ascending `seq`. Segments in `segments` that no lineage record references become reclaimable at once and are reclaimed by the next manifest write; each queue opens a fresh active segment on its next demotion (an old segment is never appended to after a restore, so `bytes_written` beyond the manifest's figure is never trusted). Return `ResumePoint { extras, to_recompute }`. The engine does not re-read anything: the scheduler re-reads each `to_recompute` origin and pushes it to Q0 with its original `seq` (SC f.13), and the kernels re-run it from stage 1; a morsel that had reached stage 3 in RAM is recomputed from the source, which is the price of not replicating (PL-I13).

**f.14 `is_full(stage)`.** True when any of: the queue's `head_error` is set; the queue's figure in the target tier is above `high[target]` and the last shed of its pressure found no candidate (f.18 rule 4: every batch it holds is hot, held, moving, or cannot leave because staging is off and it is not recomputable), or the disk bound of f.2 step 4 was reached for a batch that is not recomputable; the queue is closed. Otherwise false, including when the queue is above high water but demotion is making room (the scheduler's SC-I3 reads `is_full(0)` and the controller's `HighWater` knob sets the marks; a queue above high water with demotions in flight is not full, it is draining). Evaluated under the queue lock from the last `plan`'s outcome, which records whether it found a candidate; O(1).

**f.15 `shutdown()`.** Set `shutting_down`; under each queue lock, mark every `Promoting` and `Demoting` entry as its source state (a promotion in flight stays where its bytes were; a demotion in flight stays resident), release every reservation as if the move had failed (f.9) and drop the moves map's entries; set every queue closed and unpark every waiter (so `pop_blocking` returns `Err(Cancelled)`, PL-I10 covers `close`, this is the cancellation exit of preamble 4.3); do not unlink segments (a manifest may reference them; the facade removes the run directory when the run ended normally, e.5). Late completions arriving after `shutdown` find no move id and return without touching a queue. Idempotent. The reactor's own `shutdown` (RE-I7) is the facade's to call, after this.

**f.17 A DAG's queues (KA5, 2026-10-29).** `PlacementConfig::topology` (contracts d.16) replaces the number of queues: one queue per edge, a queue a source gives being recomputable (staging off by default, b) and every other staged. `push_to(stages, morsel)` with one queue is `push`; with several it checks every queue first (a closed one refuses the whole push, so nothing is pushed anywhere), then puts an entry holding a share of the payload in each, counted into each queue and joined to one group (PL-I16), then records the lineage at every queue in one step (PL-I17). `set_committed(sink, w)` records sink `sink`'s watermark and, for each source, computes the highest number `R` such that every path from it to every sink it reaches maps every number at or below `R` to one that sink has committed (`Affine::floor_inverse`, the least over the paths); the source's records at or below `R` are committed as f.11 commits a chain's (kept, marked, until a manifest no longer needs them when checkpointing is on). A chain's `R` is its sink's watermark. **Restore (f.13 for a DAG).** Rows are grouped by root, and roots a source's `R` covers are dropped. A root whose every position has a disk copy comes back at each of them, `OnDisk`; any other is listed in `to_recompute` for its source, and its disk copies are not restored (their records count as released, so the segment is reclaimed). The scheduler re-reads a listed root and replays it into every queue its source gives; a sink that had committed what it becomes there drops it (10 f.16), so every sink gets each morsel once. In a chain a root has one position, so this is the chain's rule exactly.

**f.16 `evicted` and `replace`.** `evicted(stage)`: under the queue lock, the `(seq, origin)` of every entry whose batch is `Evicted`, in position order; a batch fanned out to several queues is listed once, under the lowest of them, and one `replace` from any of its queues brings it back for all (KD7a). `replace(stage, morsel)`: under the queue lock, find the `Evicted` entry with `morsel.seq` (else `Staging("replace: no evicted entry")`), set it `Resident(morsel.payload.tier())` with the new bytes in its original position, add bytes to the tier counter, `evictions_replaced += 1`; unlock; update the lineage record (stage, no disk); unpark; `plan`.

**f.18 The buffer catalog (KD7a, 2026-10-29; adaptive-runtime 9.1, 9.2).** The catalog holds one record per batch (b) and the totals derived from them: per tier slot the resident bytes, the reserved bytes, the bytes a demotion in flight is taking out, the peak and the set of batches whose bytes count there; per queue the same bytes for the batches waiting in it; the held bytes; and the figures of `CatalogStats` (contracts d.10). It does no IO and takes no lock of its own: the engine holds it behind one mutex, position 2c of g. Its rules:

1. *Push.* `push` and `push_to` record one batch, `Resident(tier)`, with one remaining consumer `(queue, seq)` per queue (PL-I16), then append an entry naming it to each queue under that queue's lock, telling the catalog the number the entry was pushed as (its ordinal). A `(queue, seq)` the catalog already holds is refused. `push_reserved` does the same and turns its claim into the batch's residency in the same locked step (PL-I22). After a push the tier is shed back within its budget if it is over (rule 6).
2. *Take.* `pop` checks, under the queue lock and then the catalog lock, that the head's batch is resident in a satisfying tier with no move in flight; the consumer's `(queue, seq)` moves from the batch's consumers to its holders, and the consumer is handed a share of the payload (never a copy, CT-I15). When no consumer is left, the catalog drops its own copy of the payload and releases the batch's disk record (f.7); the bytes stay resident, held.
3. *Release.* `release(queue, seq)` removes the holder and emits `Released`. When the batch has neither a consumer nor a holder left it is freed: removed, its bytes out of every total, `Freed` emitted, and every queue waiting for room is planned again (PL-I19).
4. *Candidates.* A batch may leave tier slot `t` when its bytes are in `t` in state `Resident` or `Resident + OnDisk`, the catalog holds its payload, it has no holder, it has a remaining consumer, every one of its entries is placed in its queue, none of them is inside its queue's promotion window, and, for a queue's own pressure, it waits in that queue. How it leaves is b's: `DropCopy` with a disk copy; `CopyDown` from a device; else `Recompute` for a recomputable batch unless every queue it waits in stages and the run has a disk tier; else `Spill` when some queue it waits in stages and the run has a disk tier; else it cannot leave.
5. *Order.* The candidates are sorted by the engine's `EvictionOrder` (PL-I20, `KnownFuture`) and taken from the front until their bytes reach the need. `DropCopy` and `Recompute` happen under the catalog lock (`Dropped`); `Spill` and `CopyDown` mark the batch `Demoting` and are issued with no lock held (f.5, e.4), and free their bytes when they land (`Spilled`). A record that lands while the batch has become hot keeps its resident copy, `Resident + OnDisk` (f.5).
6. *When memory is freed.* (a) A queue's own pressure (f.2 step 3), from its batches. (b) A tier over its budget after bytes arrived (a push or a replace without a claim, a lowered budget), from any batch, by the excess less what is already leaving. (c) A promotion whose reservation failed (a head reloaded on demand, or the window prefetching), from any batch, by what the promotion needs less what is already leaving; the queue waits for room and is planned again when a landing, a drop, a release or a given-back claim frees some. (d) An admission: `reserve(tier, bytes)` grants a claim that fits (PL-I22); one that does not asks for room as in (c) and answers `Pending { need }` when room is on its way (a shed found something, or demotions in flight cover it) and `Refused { need, budget }` when nothing can leave; a claim larger than the budget is refused at once. A `Reservation` dropped unused gives its bytes back.
7. *Misses.* A consumer reaching a batch that eviction took out of memory (a head promoted from disk, or a head `Evicted`) is one miss, weighted by the batch's reload cost; counted once until the bytes leave memory again.
8. *Events.* `Released`, `Freed`, `Spilled`, `Reloaded` and `Dropped`, in the order they happened, delivered to the observer (`Residency::observe`) on the thread that caused them, with no engine lock held. `Released` and `Freed` are the data plane's events of the same names (SDD 29): a consumer is done with its share; a batch has no holder left and its memory is back.
9. *Restore.* f.13 brings a root whose every position has a disk copy back as one batch per distinct disk record, with one consumer per position naming that record, so a fan-out spilled once comes back as one batch with all its consumers; each segment's `records_total` is its distinct records.
10. *The port.* `Residency` (contracts d.17; KD7b moved it from this crate so the scheduler can hold it, and `Placement` requires it) exposes the state of adaptive-runtime 9.1 (`snapshot`: every batch's size, state, disk copy, owner, remaining consumers, holders, reload cost and future, and the figures) and the actions of 9.2 (`reserve`, `shed`, `spill`, `reload`), each keeping PL-I19 and PL-I22. It is independent of any one policy: the order is `PlacementConfig::eviction`, and KD7b's policy port drives the rest.
11. *Report.* `PlacementStats::catalog` carries the figures; the run report's `buffers` (04 d.1) carries the order's name, the peak and budget per tier, the bytes and batches spilled and reloaded, the evictions, the misses and their reload bytes, the releases, the frees and the overrun bytes, for KD7b's benchmark matrix.
12. *Straight to disk (KD7b).* `push_spilled(stages, morsel)` records a batch memory cannot admit as `Demoting` with `outside` set and its write issued once its entries are in place: its bytes count in no tier (`CatalogStats::outside_bytes`, `overflows`, `overflow_bytes`) until the record lands, when it is `OnDisk` whatever its position, and its consumers read it back under a claim. A write that fails twice blocks every queue it waits in with the error (f.10). Without a disk tier the push is refused with `Staging`. `head(stage)` gives the head's batch, number, bytes, whether the take is its last and when it arrived. A queue's `blocked` finding (f.14) is recorded only when no later planning pass has run since (each pass takes an epoch), so a stale finding never outlives the queue's emptying. A claim of nothing is always granted.
13. *A head on disk always comes back (KD7b).* The run's floor is every queue's promotion window and one unit's input and output, `(Σ windows + 2) × batch` at the smallest batch the run can be made to use (`Residency::floor`, contracts `CatalogFloor`); the scheduler refuses a budget under it at `new` and the facade's N-7 floor counts it at `morsel.min_bytes` and the default window, naming it in the refusal. Above the floor, a head whose reload finds no room asks for room on demand: its candidates are every batch of the tier rule 4 allows except that hotness no longer protects them, since a head a consumer waits for is used before any of them, but the head itself and every head already read back on demand and not yet taken (`missed`) are protected, so two heads never take each other's room in turn. The order is the same known-future order, so the batch used last leaves first. The scheduler's stall detector (10 f.21) is the backstop.

## g. Concurrency within the component

The engine has no thread (PL-I14). One mutex per queue (preamble 4.2 position 2), the lineage mutex (2b), the catalog mutex (2c, f.18: every batch's record, the tier totals and the reservations), the moves map and the segments map mutexes (3, the "moves map and tier accounting" position). A queue lock may be held while the catalog's is taken (planning, `pop`, `peek_resident`, `evicted`); the catalog's may be held while the segments map's is taken (a record landing); no queue lock and no lineage lock is ever taken while the catalog's is held, and no two queue locks are ever held at once: a shed reads each queue's view in turn, releases it, and only then takes the catalog's. Move completions arrive as `Completion::then` callbacks on reactor threads: the callback looks up the queue id from the move id in a lock-free side table, locks the queue (2), then the moves map (3), updates state, releases reservations, unparks waiters, runs `plan`, and returns; it is short and never blocks, as contracts d.9 requires of a `then` callback. No lock is held across a reactor call: `plan` collects the moves to issue into a local list, unlocks, issues them (each call returns at once, RE-I6), and re-locks only to record their move ids. `pop_blocking` parks without holding the lock. The lineage mutex (2b) is taken after a queue lock is released, never inside one; a queue lock is never taken while the lineage mutex is held (f.11, f.12 collect first, drop it, then lock queues or segments); `checkpoint` holds it only while snapshotting, never during serialisation or IO. A reactor thread running a `then` callback may therefore take a queue lock that a worker holds briefly inside `push` or `pop`; the worker never waits for the reactor while holding it, so there is no cycle.

## h. Behaviour

**Normal path (host only, compute-bound).** Consumer specs all `Host`; the arena is unpinned so the host tier is `Host`; every push is `Resident(Host)`; `plan` finds heads resident and nothing over high water; pops return immediately; no moves ever issue. The engine's overhead is one lock per push and pop.

**Normal path (GPU kernel).** The arena is pinned, so the host tier is `PinnedHost`. Q1's consumer is `Tensor/Device`; target `Device(cfg.devices[0])`; pushes arrive `Resident(PinnedHost)`; `plan` promotes the head and the next `k−1` entries by `copy(View, Buffer(device))` on the H2D stream, issued from the pushing worker's thread and completed in `then` callbacks on the reactor's event-polling task; the kernel pops device-resident morsels; its outputs are pushed to Q2 `Resident(Device(0))`; Q2's consumer (the sink) wants `Host`, so the target is `PinnedHost` and `plan` issues a `copy(View, Buffer(pinned))` toward it, which the engine calls a promotion regardless of rank direction ("toward target"). Device bytes are bounded by the device budget through reservations.

**Normal path (slow sink, S10 and S15).** Qn (before the sink) grows; `bytes[Host] > high`; demotion from the tail writes records to segments at NVMe sequential bandwidth; the head stays resident; the sink drains at its pace; as it consumes, promotions from disk refill the window; segments are released when their records are consumed. Throughput after engagement is bounded by the slower of sink rate and disk write rate, and S15's 70% is met when disk bandwidth exceeds the sink's.

**Weight-major precondition (E8).** With `set_staging(0, true)`, Q0 behaves like any queue and may hold the whole dataset on disk with a resident window; this is the mode a future weight-major controller uses and it is tested here (PL-T12) even though no v1 controller enables it.

**Edge cases.** A morsel larger than the destination tier's whole budget: reservation fails forever; f.2 step 1 detects a head that cannot be promoted and reports it through `pop` as `Staging("head cannot fit in <tier>: need X, budget Y")`. A queue with `k` larger than its length: promote all. Two consumers popping the same queue (stateful instances): the queue lock serialises; FIFO holds. `set_consumer` after entries exist: retarget; existing promotions complete to the old target and are re-planned. Shutdown with moves in flight: f.15, then the reactor's RE-I7; entries left in `Promoting` are treated as their source tier for accounting release, and a completion that arrives afterwards is ignored. Several devices in `cfg.devices`: only `devices[0]` is a promotion target and the `Device` budget slot is the sum the controller gives; the report notes "v1 uses one device for placement". A `set_water` for `TierKind::Remote`: accepted and stored, never consulted in v1.

**Resume (S17).** The process died (OOM kill, node loss, operator cancel) with 4,000 morsels committed, 300 uncommitted of which 180 were on disk under pressure, 100 resident and 20 inside kernels or the sink. A new process, on the same node or on another node with the staging volume attached, calls `restore`: the 180 come back `OnDisk` in their stages, the 120 others are listed for recomputation, the source cursor says where to continue, the sink discards its one uncommitted file and continues numbering, `Reinit` kernels run `init`, and the controller starts from the profile store rather than probing. Output is byte-identical to an uninterrupted run for an ordered sink, and set-identical for an unordered one (PL-T17). The cost of the crash is the 120 recomputed morsels plus the sink's discarded file; nothing else is redone.

**Failures.** See f.10. Disk full below the budget (filesystem lied about free space, or `fallocate` fails at a roll): the roll fails; treat as a demotion failure; also lower the effective disk budget to `disk_bytes` and note it. A manifest write failure (`Resume` from f.12) is reported to the scheduler, which logs it at `warn` and continues; the run is still correct, only its resumability is stale; three consecutive failures terminate the run with the error, because a platform that promised a durable staging directory and cannot write to it is misconfigured (G-I7 is about performance fallbacks; this is not one).

## i. Configuration

`budget.disk`, `staging.dir`, `staging.segment_bytes`, `queue.high_water`, `queue.low_water`, `queue.promotion_window`, `checkpoint.enabled`, `checkpoint.interval_ms`, `checkpoint.keep` (preamble section 5).

## j. Observability

`PlacementStats` and `QueueStats` (contracts) plus `PlacementDetail { per_queue: Vec<QueueDetail { bytes_by_tier, entries_by_state: [u64; 8] /* the seven states of e.1 plus OnRemote, always 0 in v1 */, active_segment, evictions, evictions_replaced, move_retries, head_error: bool }>, segments_live, segments_reclaimable, next_segment, moves_in_flight, reservations: [u64; TIER_COUNT], disk_bytes, disk_budget_effective, lineage_len, committed_seq, manifests_written, last_manifest_us, host_tier }`. `tracing`: `placement.demote` and `placement.promote` (debug: stage, seq, from, to, bytes), `placement.evict` (debug), `placement.segment_roll` (info), `placement.miss` (trace), `placement.move_failed` (warn), `placement.disk_bound` (error), `placement.checkpoint` (debug: lineage_len, segments, duration_us), `placement.checkpoint_failed` (warn), `placement.restore` (info: on_disk, to_recompute, committed_seq, next_seq).

## k. Tests

Unit tests use the testkit's `FakeReactor` (contracts d.15: `with_latency(Duration)` for delayed moves, `fail_next(op, n)` for failure injection, `cancel_on_shutdown(bool)`, in-memory files keyed by path so segment writes and reads round-trip, `ops()` as the record of every operation with tiers and offsets, `in_flight()`) and `FakeAllocator` (`with_limit(tier, bytes)`, `pinned(bool)`, `page_bytes(n)`, `in_use(tier)`, `AllocStats`); the manifest is a real file in a temp directory (f.12 writes it with `std::fs`) and the segment files the fake keeps in memory, so a test that checks disk usage with `du` or kills the process needs the real reactor and is tagged "(integration, closes in wave 3)" or, for PL-T17, "(integration, closes in wave 4)"; S15 is "(reference host, E1)". Tests that need a pinned arena use `FakeAllocator::pinned(true)`; the host tier is then `PinnedHost` throughout the test, never mixed.

**PL-T1 head_hot.** Property test: random push/pop/knob sequences with `with_latency` moves; at every observable point, the head is resident or has exactly one promotion in flight and no demotion. Over the pure planner: only the head is promoted as the head, promotions stay inside the window and aim at the target, no entry gets two, and the pressure reported is the figure above a high mark down to the low one (KD7a). PL-I1, PL-I2.

**PL-T2 accounting_exact.** Model-based test: resident + reserved per tier, read in one catalog snapshot, never exceeds budget after every push, take and release; at quiescent points the queue's figure and the catalog's totals equal the model of its batches, the `Disk` slot counting the batches only on disk. PL-I3, PL-I21.

**PL-T3 no_cpu_bytes.** `payload_copies_total` unchanged over a run with demotions and promotions; every byte movement is an entry in `FakeReactor::ops()` whose `src_tier`/`dst_tier` match e.4; every `write_file` and `copy` source is a view over the entry's own buffers (the fake records the pointer, which equals the buffer's). PL-I4.

**PL-T4 fifo.** 10,000 entries with random demotion pressure; pops are in push order. PL-I5.

**PL-T5 q0_evicts.** Staging off: Q0 pressure produces `Evicted` entries and no segment writes (`ops()` has no `write_file`); `evicted()` lists them oldest first; `replace` restores order and the replaced entry pops in its original position; `set_staging(0, true)` then makes the same pressure write instead. PL-I6, f.16.

**PL-T27 fan_out_is_counted_once** (`tests/pl_t27_dag_queues.rs`, KA5, KD7a). A batch pushed into two queues is one batch in the catalog, counted in each queue's figure and once in the tier's (`catalog.resident_by_tier`); a take leaves it resident once, held by the consumer that took it and waiting in the other queue; both consumers are handed the same bytes; it is freed when the second consumer releases it, and a share is released once; a fan-out into a closed queue pushes nothing anywhere; an empty fan-out and a batch in no tier of the run are refused; the contract's default `push_to` pushes a share into each. PL-I16, PL-I18, PL-I19, f.17.

**PL-T28 lineage_by_root.** Four source morsels fanned out to two queues are four records; one taken through both stages to both sinks' queues moves; sink 0's commit alone releases nothing and both sinks' release it (`source_committed`); the manifest has a row per position by root; a restore brings back at each position a morsel whose every position is on disk and lists for its source one with a position off disk; a manifest of another shape is refused naming the queues. PL-I17, f.17, e.5.

**PL-T29 known_future_order** (`src/catalog.rs`, KD7a). Candidates are ordered by `(next_use + 1) × bytes / reload` descending, then by size, then by id; of two used equally soon the one with a disk copy goes first; a zero reload cost does not divide by zero; `pick` takes the leading candidates until their bytes reach the need. PL-I20.

**PL-T30 who_may_leave** (`src/catalog.rs`). A hot batch, an entry its queue has not placed, a held batch, a batch with a move in flight, one with no payload in the catalog and one outside the scope are never candidates; a shared batch is hot when any of its entries is; once released, a batch's next use is its smallest position across its queues; how it leaves follows b: `DropCopy` with a disk copy, `Spill` (reload twice its size) when a queue stages and there is a disk, `Recompute` for a source's batch unless every queue it waits in stages and there is a disk, `CopyDown` from a device, and not at all for a written batch with no staging or no disk. PL-I19, PL-I20, PL-I6.

**PL-T31 totals_follow_the_records** (`src/catalog.rs`). Insert, spill, reload, two takes and two releases of a fanned-out batch move the tier totals, each queue's figure, the held bytes and the peaks exactly; the last take releases the disk record; the batch is freed once, after its last holder; the events come in order (`Spilled`, `Reloaded`, `Released`, `Released`, `Freed`); a miss counts once with its reload cost; a reservation never passes the budget; a duplicate `(queue, seq)`, a second take, a release of nothing and a second release are refused. PL-I3, PL-I18, PL-I19, PL-I21, PL-I22.

**PL-T32 a_fanned_out_batch_moves_once** (`tests/pl_t32_buffer_catalog.rs`). Twelve batches fanned out to two staging queues under per-queue pressure are twelve batches; both entries of each name one batch in one state; no batch is written twice (the `Spilled` events name each batch at most once); each queue drains in order with each batch's own bytes; at the end every batch is freed once, every take released once, nothing is resident or reserved, and every segment is reclaimable once its queues close. PL-I16, PL-I18, PL-I19, f.7.

**PL-T33 known_future_chooses_what_leaves.** Five batches wait in queue 0 and a sixth is queue 0's tail and queue 1's head. With the fourth given a disk copy by the port (`spill`, then `reload`), shedding one batch from queue 0 drops the fourth's copy with no IO before writing the fifth; the next shed writes the fifth; the next writes the second and third; the shared tail, hot in queue 1, never leaves, and with only heads left the queue reports full. The snapshot gives the shared batch two consumers and a hot future at position 0. PL-I20, f.14, f.18.

**PL-T34 held_batches_stay.** A batch taken by one queue's consumer and still waiting in another, deep in it, does not leave under that queue's pressure, which then reports full, and the port's `spill` of it is refused naming the rule; released, the same pressure writes it. Releases of what was released, and of a queue the run does not have, are refused. PL-I19.

**PL-T35 events_and_reservations.** A claim that fits is granted, one that does not with nothing to shed is refused with its need, one larger than the budget is refused, and one that a spill in flight will make room for is pending until the spill lands, then granted; a dropped claim and the claim of a refused push are given back; a batch pushed under its claim turns the claim into residency, and one larger than its claim counts the overrun; a reservation outside a memory tier or in the other host tier is an error. The observer hears `Spilled`, `Released`, `Freed`, `Reloaded`, `Released`, `Freed` in order. **pl_t35_the_port_refuses_by_name:** spill and reload of an unknown batch, a claim of one tier for a batch of another, a reserved replace of a queue outside the run or of nothing evicted, a spill of a batch on disk, a reload without room and a spill with no disk tier are each refused by name and change nothing; `shed` answers whether anything could leave. PL-I22, f.18.

**PL-T36 evicted_once_and_resumed_once.** A source's fanned-out batch dropped under pressure with staging off is `Evicted` in both queues, listed by `evicted` under the lower queue only, and one `replace` brings it back for both; a second is refused. Six batches fanned out and spilled, checkpointed and restored into a fresh engine, come back as five batches (the resident head is read again), each one batch for both queues, and drain and free once. f.16, f.18 rule 9, PL-I16.

**PL-T37 never_past_the_budget_every_batch_once** (`tests/pl_t37_catalog_property.rs`, driver `tests/common/dag.rs`). Property test over random DAGs (one or two sources, one to four stages, fan-out, fan-in by two ports, sinks on every producer nothing else takes and on some others), random batch sizes, a working set two to four times the host budget, one to three units at once, a promotion window of one to four per case (the budget leaves room for every queue's window), random staging of the source queues, the queues' default shares in half the cases and the whole budget in the other half, sources running ahead in some cases, and a random arrival order and interleaving of source reads, unit starts and finishes, sink takes and replaces. Every push and replace is admitted under a claim. After every step resident plus reserved is within the budget, and so is the catalog's peak, which sees every change between steps; every take carries its producer's bytes for its number, reaches its consumer once, in push order; at the end every queue's takes are its pushes, every batch is freed once, every take released once, and nothing is resident, reserved or overrun. At least half the cases must evict, and spills and drops must both occur. `MORUNA_PL_T37_CASES` (default 48) and `MORUNA_PL_T37_SEED` set the run; a value that does not parse fails the test. PL-I18 to PL-I22, adaptive-runtime 12.5.

**PL-B1 known_future** (`benches/known_future.rs`, `cargo bench -p moruna-placement --bench known_future`). Not a test. A source fans out to stages 1 to 3; stages 1 and 2 meet in stage 4; stages 3 and 4 give sinks. The source runs ahead, so its data, three budgets, presses memory at once; every IO resolves after `MORUNA_PL_B1_IO_US` microseconds (default 100) on the fake reactor. It runs with `KnownFuture` and with a least-recently-used order that lives in the bench alone, the same seed and drive, once with only the catalog's budget holding memory back and once with the queues' default shares, and prints for each the bytes and batches spilled and reloaded, the evictions, the misses with their reload bytes and the finish time, and the two orders' ratios. Every run is checked as PL-T37 checks it.

**PL-T38 what_memory_cannot_admit** (`tests/pl_t32_buffer_catalog.rs`, KD7b). A batch fanned out to two queues and pushed straight to disk counts in no tier while its record is written (`outside_bytes`, one overflow); `head` names it and says the take is not its last; the record lands and, as queue 0's head, it is read back under a claim with its disk copy kept; each queue drains it with its own bytes and it is freed once; `head` of a queue outside the run is `None`; a push into a queue outside the run, and one on a run with no disk tier, are refused by name. f.18 rule 12.

**PL-T39 a_head_on_disk_comes_back** (`tests/pl_t32_buffer_catalog.rs`, KD7b). A budget of three batches is full of queue 1's window, every batch hot; queue 0's batch goes straight to disk and, as its head, comes back by taking the room of the batch queue 1 uses last, which is written out, while the other two stay; the peak never passes the budget, the reload is one miss, and everything drains and is freed. PL-T30 also asserts the demand rule over the catalog alone. f.18 rule 13.

**PL-T25 a_zero_disk_budget_has_no_disk_tier.** With `disk_budget = 0` and `staging_enabled` on for a non-recomputable queue under sustained pressure, no `Demote { to: Disk }` is ever planned and no `Staging` error reaches a `pop`; the queue reports `is_full` and the run stalls on admission instead. PL-I7, f.2.

**PL-T6 disk_bounded.** `segment_bytes = 64 MiB`, `disk_budget = 128 MiB`, one non-recomputable queue (stage 1) under sustained pressure: two segments open, `disk_bytes == 128 MiB`, the third roll is refused, `is_full(1)` becomes true, the next `pop` returns `Staging` carrying `disk_bytes`, `disk_budget` and the segment size; `disk_bytes` never exceeded 128 MiB at any observable point; (integration, closes in wave 3) with the real reactor the same run's `du` on the staging directory never exceeds 128 MiB. PL-I7, f.14.

**PL-T7 segment_deleted_when_empty.** With `checkpoint_enabled = false`, segments are unlinked exactly when their last record is released (the preallocated file disappears from the temp directory), and `unregister_segment` precedes the unlink (a test-local `Reactor` wrapper around the fake timestamps the registry call and the test compares it with the file's disappearance); with it on, at the first manifest write after that, and never while a manifest on disk references them (the test parses each manifest and checks every referenced segment file still exists); a live entry never references a missing file (fuzz with random consumption order across two queues); `disk_bytes` drops by `segment_bytes` per unlink. PL-I8, PL-I12, PL-I15.

**PL-T8 misses_counted.** `with_latency` moves; every waiting `pop_blocking` increments `misses`, returns a non-zero `wait_us` as its second element, and the sum of returned waits equals `miss_wait_us` and is within 5% of measured; a pop that did not wait returns 0. PL-I9.

**PL-T9 close_drains.** Close with 100 entries, some on disk; pops return all 100 in order, then `None`. PL-I10.

**PL-T10 record_format.** Written segments (read back from the fake's in-memory file) parse by an independent reader in the test: header page fields, `body_offset` one page after the header, every piece at a page-aligned offset, `payload_len` as e.3 defines it, the IPC body readable by `moruna_kernel::ipc::decode` with arrays pointing into the read buffer and by `arrow`'s stream reader, the MRB1 body by the contracts reader; segment numbers are global (two queues writing alternately produce `seg-000000`, `seg-000001`, ... with each segment holding one stage); a record whose last piece is not a page multiple is followed by the next record at the next page boundary. e.3.

**PL-T11 move_table.** For each row of e.4, with `FakeReactor::ops()` recording operations, the issued sequence (kinds, tiers, offsets, and for two-step rows the second issued from the first's `then`) matches the table; with `FakeAllocator::pinned(true)` the host tier is `PinnedHost` in every row and with `pinned(false)` it is `Host`, and no operation ever names the other; there is no row and no issued operation joining `Host` and `PinnedHost`. GPU rows are exercised against the fake (the fake accepts any tier pair) and tagged "(reference host, E1)" for the real reactor. e.4, contracts e.1.

**PL-T12 q0_staging_on.** `set_staging(0, true)`: a 10× RAM-sized synthetic Q0 holds the dataset on disk with a resident window of `k`; pops proceed in order; no eviction. E8 precondition.

**PL-T13 slow_sink_throughput.** (reference host, E1; real reactor, NVMe) Sink throttled to 10% of producer rate; memory stays under budget; throughput after staging engages ≥ 70% of before. S10, S15.

**PL-T14 move_failure_paths.** `FakeReactor::fail_next(Copy, 1)` on a promotion: `ops()` shows the identical operation issued a second time (same source pointer, destination and offsets), the second succeeds and the entry lands; `fail_next(Copy, 2)`: after the second failure the head error surfaces through `pop` as `Staging` naming the morsel and `is_full` is true; `fail_next(WriteFile, 2)` on a demotion: the entry stays resident with its bytes intact (`in_use` unchanged), pressure persists, `bytes_written` of the segment advanced past the abandoned pages, no crash. f.10.

**PL-T15 concurrency.** 8 producers, 8 consumers, `with_latency` completions resolving on the fake's threads, 1 M entries; no deadlock (lock order test with a detector), stats consistent; no thread other than the test's producers, consumers and the fake's belongs to the engine (thread enumeration before and after). g, PL-I14.

**PL-T16 manifest_roundtrip.** Drive an engine through pushes, demotions, pops and `set_committed`; `checkpoint`; parse the file with an independent reader in the test against e.5 (every key, ascending `lineage`, every referenced segment file present in the temp directory with `bytes` no greater than its length and no smaller than the largest `offset + len` referencing it, temporary file absent, `hostname` equal to the OS's); a second engine over the same fake reactor (its in-memory file contents survive; the preallocated files are on disk) and a `PlacementConfig` with the same identity fields: `restore` yields queues whose `OnDisk` entries equal the first engine's, a `to_recompute` list equal to the first engine's uncommitted entries without a disk copy, segments with `records_total` equal to the lineage references and `records_released == 0`, `disk_bytes` equal to `segment_bytes` times the segment count, and `next_segment` above every restored number; refusal cases: wrong `run_id`, wrong `plan_digest` (a plan with one row count changed), wrong fingerprint, a `forbid` stage, a missing segment (the manifest edited by the test to name a number never written), a shortened segment (a `lineage` entry edited to reference an offset beyond the segment's `bytes`), `version: 2`, a different `page.bytes` in `config`, each named in the `Resume` message; `find_manifest` over two run directories returns the one with the larger `written_ns`. PL-I11, PL-I12, e.5, f.13.

**PL-T17 kill_and_resume.** (integration, closes in wave 4; real reactor, temp directory; driven by the scheduler and `FakeSource` from the testkit; the Python variant is PY-T12 in wave 5) A 3-stage run with a throttled sink and a 10× RAM-sized source; the test process is killed with `SIGKILL` at ten random points including mid-checkpoint (the `.tmp` file exists); a fresh process resumes from the manifest through the scheduler's resume path; the final output is byte-equal to an uninterrupted run for an ordered sink and row-set-equal for an unordered one; the number of source rows re-read equals the recomputed lineage, never more (the test counts `Source::read` calls). PL-I13, S17.

**PL-T18 lineage_bounded.** With a committed watermark advancing at the sink's rate, `lineage_len` never exceeds the number of morsels admitted above the watermark, and drops to zero at the end of a run; with the watermark held, it grows exactly as fast as admission. PL-I11.

**PL-T19 remote_unsupported.** A segment record with codec byte 1 is rejected with `Unsupported("codec")`; a `push` of a `Remote`-tier payload, a `pop` on a queue whose head is (unsafely constructed in the test, E9 exempts tests) `OnRemote`, and every move-table row that names `Remote` return `Unsupported("rdma")` in a build without the `rdma` feature; the `no_tier_wildcard` lint (CT-T14) passes on this crate. CT-I11.

**PL-T20 completions_through_then.** With `with_latency(20 ms)`: after a `push` that triggers a promotion, the pushing thread's time inside `push` is under 1 ms (it did not wait), `FakeReactor::in_flight() == 1`, and the state change to `Resident` happens on the fake's resolving thread (thread id recorded in the entry by a test hook) before any other call into the engine; the engine spawns no thread (PL-T15's enumeration); `Completion::wait` is never called (a `FakeReactor` built with `cancel_on_shutdown(true)` and shut down mid-move leaves the engine responsive: `shutdown` then `pop` return at once). PL-I14, g.

**PL-T21 is_full.** A queue above high water with staging on and demotions in flight: `is_full` false; with staging off and non-recomputable entries: true; with staging off on Q0 (recomputable): false, evictions happen; after a head error: true until the error is taken by `pop`; at the disk bound of PL-T6: true; below high water: false; after `close`: true. f.14.

**PL-T22 shutdown.** With 50 moves in flight under `with_latency(50 ms)`: `shutdown` returns within 1 ms; every reservation is released (`reservations` all zero) and `resident + reserved` per tier equals the bytes of entries in their source states; `pop_blocking` on every queue returns `Err(Cancelled)`; `push` returns `Err(Cancelled)`; the late completions that arrive afterwards change nothing (`detailed_stats` equal before and after they resolve); no segment was unlinked (`ops()` has no `unregister_segment` after the shutdown); a second `shutdown` is a no-op. f.15, preamble 4.3.

**PL-T23 checkpoint_lock_order.** `checkpoint` and `set_committed` run concurrently with 8 producers and consumers and a `with_latency` reactor for 10 s under a lock-order detector, `set_committed` from the one thread that owns the watermark (f.11), the consumers only recording what they consumed: the lineage lock is never held while a queue lock or the segments lock is requested, the manifest write happens with no engine lock held, and every manifest on disk is well-formed at every instant (a reader thread parses `manifest.json` in a loop and never sees a partial file). f.11, f.12, PL-I12.

**PL-T24 water_by_tier_kind.** `set_water(stage, TierKind::Host, low, high)` on an unpinned arena and `set_water(stage, TierKind::PinnedHost, ..)` on a pinned one govern demotion of the host tier; setting the mark for the other host tier is stored and has no effect; `set_water(stage, TierKind::Device, ..)` governs `Device(devices[0])`; the `stats().queues[stage].bytes_by_tier` index the marks refer to equals `TierKind::index()`. d.1, e.2.

## l. Implementation notes for the agent

Decisions the PM took on 2026-09-22, on the component 9 agent's report, each because the document could not be followed as written:

Preamble 4.2 beats f.5. f.5 lays a staging record out under the queue lock, which calls the allocator with a lock held, and no lock is held across a call into another component. The engine drafts the record outside the lock, encoding at base offset 0 (the layout is offset-invariant), and takes the lock again only to record the move.

PL-I14 beats f.13. f.13's segment-length check "through the reactor" needs `Completion::wait`, which only the scheduler's drives may call; the engine uses the manifest's own arithmetic and the file's length, and both refusal cases of PL-T16 still fire.

Arrow 59.3.0's IPC encoder allocates its own validity bitmap even for a non-null column whose bitmap is arena-backed, so e.3's "the pieces are the batch's own buffers" is false for that one buffer: the engine copies it into the arena per record and declares it through `note_payload_copy`, and the data buffers still move by DMA from their own bytes. The body is also not readable by arrow's own stream reader, because `bodyLength` is not rewritten; `moruna_kernel::ipc::decode` is the reader, which is what e.7 provides.

A record that lands while its entry has moved into the promotion window keeps its resident copy rather than becoming `OnDisk`, so the head never waits on a read of bytes that are already in memory (PL-I1, G-I3); and `close(stage)` retires the queue's active segment so PL-I8 can reclaim it.

A device move of a table is refused with `Staging` naming the item: rebuilding a `RecordBatch` over engine-allocated buffers needs either `unsafe` in this crate, which section l forbids, or a safe constructor the contracts do not have. Tensors take every device row. This is an E10 item left open deliberately, because no host in the project has a device to exercise it on (E1), and the contracts change should be made when there is one.

Files: `src/lib.rs`, `src/queue.rs` (e.2, f.1, f.3, f.4, f.14, f.16), `src/plan.rs` (f.2, f.8, f.9), `src/moves.rs` (e.4, issue and `then` completion, f.10), `src/staging/{mod.rs, segment.rs (e.3, f.7, the global counter and the segments map), write.rs (f.5), read.rs (f.6)}`, `src/lineage.rs` (e.2 lineage index, f.11), `src/manifest.rs` (e.5, f.12, f.13, `find_manifest`, `read_manifest_header`; `serde` derive structs mirror the table exactly and nothing else is serialised; `std::fs` lives here and in `segment.rs` for `fallocate` and `remove_file` only), `src/stats.rs`, `src/shutdown.rs` (f.15). `unsafe` is not permitted in this crate; the IPC-over-buffer decode in f.6 is `moruna_kernel::ipc::decode` over `Buffer::into_arrow_buffer` (contracts d.3, e.7), the tensor view is `ManagedTensor::from_buffer` (d.4), and every DMA source is a `BufferView` from a safe constructor (d.3).

The planner must be written as a pure function from a queue snapshot to a list of moves, and tested as such (PL-T1 drives it); issuing is a thin layer over it. The reactor is reached only through `Arc<dyn Reactor>` and the contract's methods (`copy`, `write_file`, `read_file`, `read_file_opt`, `register_segment`, `unregister_segment`, `paths`); the arena only through `Arc<dyn Allocator>` (`alloc`, `page_bytes`, `is_pinned`, `contains`, `tier_of`). Nothing of `moruna-reactor`, `moruna-arena` or `moruna-sinks` is named in this crate.

Anti-patterns: no per-entry heap allocation on the pop path (entries live in the `VecDeque`; moving a `Morsel` out is a move, not an allocation); no holding a queue lock across a reactor call; no thread of the engine's own and no `Completion::wait`; no compaction of segments; no "helpful" CPU copy when a move fails; no writing a morsel to disk because a checkpoint is due (PL-I13: the manifest describes, it never causes IO of payload size); no `_ =>` arm on `Tier`, `TierKind` or `State` (CT-I11); no `Host`/`PinnedHost` pair anywhere (contracts e.1); no unlink before `unregister_segment`.

Verify before starting: `moruna_kernel::ipc::decode` zero-copy over `into_arrow_buffer` in the pinned `arrow` version (CT-T18 proves it in the contracts crate; PL-T10 re-checks by pointer comparison here); `fallocate` on the staging filesystem; the `hostname` crate returns the same string the manifest of a previous run on this node recorded (a container's hostname changes across restarts unless pinned, which is what `durable_staging` exists for).

## m. Open items

None. (`Buffer::into_arrow_buffer`, `BufferView`, `ManagedTensor::from_buffer` and `moruna_kernel::ipc` are in `01-contracts.md` d.3, d.4 and e.7; `peek_resident`, `evicted`, `replace` and `shutdown` are in the contract's `Placement` trait, d.10; `register_segment` and `unregister_segment` are in the `Reactor` trait, d.9.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| G-I3, D10 | PL-I1, PL-I2, PL-I5 | PL-T1, PL-T4 |
| G-I1 | PL-I3 | PL-T2 |
| G-I2, S14 | PL-I4, e.4 | PL-T3, PL-T11 |
| D5 | PL-I6 | PL-T5 |
| S10 | PL-I7, f.10 | PL-T6, PL-T13, PL-T14 |
| S15 | f.5, f.6 | PL-T13 |
| S17, D13, CT-I12 | PL-I11, PL-I12, PL-I13, e.5 | PL-T16, PL-T17, PL-T18 |
| S16, D12, CT-I11 | e.1 reserved state, e.4 reserved rows | PL-T19 |
| D11 / E8 | h (weight-major precondition) | PL-T12 |
| preamble 4.3 | PL-I10, f.15 | PL-T9, PL-T22 |
| preamble 4.1, contracts d.9 (`then`, non-blocking submission) | PL-I14 | PL-T15, PL-T20 |
| preamble 4.2 (lock order) | f.11, f.12, g | PL-T23 |
| contracts d.9 (`register_segment`), RE-I8 | PL-I15, f.7 | PL-T7 |
| contracts e.1 (one host tier), d.2 `TierKind` | e.4, d.1 | PL-T11, PL-T24 |
| SC-I3 (admission) | f.14 | PL-T21 |
| S10 (disk per segment) | PL-I7 | PL-T6 |
| adaptive-runtime 9.1 (the buffer catalog), contracts d.16 | PL-I16, PL-I18, PL-I21, f.18 | PL-T2, PL-T27, PL-T31, PL-T32, PL-T36 |
| adaptive-runtime 9.2 (release, spill, reload), SDD 29 `Released`/`Freed` | PL-I19, f.18 | PL-T30, PL-T31, PL-T34, PL-T35 |
| adaptive-runtime 2 G7 (known-future eviction) | PL-I20, f.18 | PL-T29, PL-T30, PL-T33, PL-B1 |
| adaptive-runtime 12.5 (the hard rule) | PL-I22, f.18 | PL-T35, PL-T37 |

## o. Deferred (post-v1)

None.

### Portable checkpoint byte inventory

A local manifest remains a local resume contract. Portability requires actual acknowledged immutable bytes for every live lineage position, including resident positions. `checkpoint_payload` pins an existing payload under its queue lock and validates the manifest identity; it never re-reads a source. Missing, evicted, unsupported device, or mismatched positions refuse publication. The existing staging codec encodes these pinned payloads into snapshot-unique native segments; the governed checkpoint publisher owns their immutable acknowledgements and full content hashes. Existing segment files are uploaded as bounded actual byte chunks. Restoring materializes and syncs every exact segment before atomically exposing the native manifest in a new run directory. Missing dependency or lineage mappings refuse rather than trigger recomputation.

Portable writers enable staging on every queue before work begins, so source eviction cannot silently replace a required immutable byte dependency with a source query. Snapshot encoding uses only a caller-reserved allocator with no live allocation fallback. Explicit host materialization copies and zero-filled holes are reported separately by the codec. No metadata-only copy is considered portable and `staging.url` raw manifest copying is retired.

Required historical output is part of checkpoint consistency. While the actual worker/drive barrier is held, a FIFO capture command drains every preceding batch to an acknowledged append and publishes immutable Capturing prefix snapshots through the sole artifact authority. The native/portable manifest carries exact snapshot Metadata, never a mutable latest pointer or a complete label. Ordinary node-local checkpoints use the same barrier and prefix rule; their metadata buffer is admitted before quiescence. Portable materialization preserves these references. Resume requires all original output identities/attempts and prepares the same artifact publisher before verified prefix continuation; missing authority, missing prefixes, malformed snapshots, or chunk verification failure refuses before execution. Snapshot or drain deadlines never imply successful publication.
