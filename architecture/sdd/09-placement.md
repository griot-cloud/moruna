# Amoru SDD 09: Placement engine (`amoru-placement`)

**Document type:** software design document, component 9 of 12, the load-bearing component
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` sections 5.6, 5.6a; decisions D5, D10, D11 (preconditions); criteria S10, S14, S15; global invariants G-I1, G-I2, G-I3
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.10 (`Placement`, `TierBudgets`, `QueueStats`, `PlacementStats`), d.2 (`Tier`, `SegmentRef`), d.9 (`Reactor::copy`), e.1 (tier transitions)
**Component location:** `crates/amoru-placement`, Rust; features `cuda`, `gds`
**Consumes:** contracts (1), arena (2), discovery (3, host profile), reactor (6). **Consumed by:** scheduler (10), controller (11)

**Decisions worth your eye:** (1) a queue is a FIFO of entries with a per-entry state machine and the engine plans moves from queue position, not from time: promote the next `k` entries toward the consumer's tier, demote from the tail, and never both for one entry at once; (2) source morsels (Q0) may be demoted to disk as recomputable entries that are dropped rather than written when the disk tier is under pressure, and re-read from the source through the scheduler; (3) the staging log is append-only per queue with fixed-size segment files, and a segment is deleted only when every entry in it has been promoted or consumed and the current run manifest no longer references it, which trades some disk for no compaction; (4) the engine writes the run manifest (e.5) because it is the one component that already knows where every uncommitted morsel is.

---

## a. Purpose and boundary

The placement engine owns every morsel between the moment a producer pushes it and the moment a consumer pops it. Its job is to have each morsel's bytes in the tier its consumer declared, before the consumer asks, using only DMA to move them, within per-tier budgets the controller sets. It keeps the head of every queue resident, demotes from the tail when a tier is over its high-water mark, uses local disk as a deliberate tier rather than an emergency, and reports every miss so the controller can widen the promotion window.

It owns: queue order; entry states; tier accounting and in-flight reservations; the move planner; the staging log (segment files, their format, their lifecycle); the disk budget; promotion and demotion policy; miss statistics; the run manifest (the record of every morsel not yet committed by the sink, where its bytes are if they are on disk, and where the source drive is) and the lineage index behind it; the resume path that rebuilds queues from a manifest.

It refuses to know: morsel contents; why a budget is what it is (controller); which worker will pop (scheduler); how a byte is moved (reactor); anything about kernels beyond their declared `PayloadSpec`; how to re-read a morsel from its origin (it lists what needs re-reading; the scheduler does it).

The engine is also the seam for the multi-node extension (architecture section 11): a `Tier::Remote` entry is an entry whose bytes are in another node's registered memory, and the move table has reserved rows for it. No v1 code path produces such an entry; every v1 path that could meet one returns `Unsupported("rdma")` (CT-I11).

## b. Vocabulary

**Entry.** One queued morsel with its placement state; identified by `(stage, seq)`.

**Consumer spec.** The `PayloadSpec` the queue's consumer declared through `set_consumer`; the target tier for promotion is derived from it: `Device(d)` when `tier == Device` and a device exists, else `PinnedHost` when the arena is pinned, else `Host`.

**Target tier.** The tier a promoted entry should reach; per queue.

**Promotion window (`k`).** The number of entries from the head, inclusive, that the engine keeps moving toward the target tier ahead of the consumer.

**High water / low water.** Per queue per tier, in bytes: above high, demotion starts; demotion stops at low. Set by the controller.

**Reservation.** Bytes claimed against a destination tier's budget for a move in flight, released when the move completes or fails.

**Recomputable.** An entry whose bytes can be re-obtained from the source (Q0 entries); demoting it may drop the bytes instead of writing them, marking the entry `Evicted`, which the scheduler resolves by re-reading.

**Segment.** A staging file, `staging.segment_bytes` in size, append-only, holding whole payload records; one active segment per queue.

**Record.** One payload written into a segment: a 64-byte record header followed by the payload in its in-memory layout, page-aligned.

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

**PL-I7. Disk is bounded.** Staging bytes across all queues never exceed `budget.disk`; when a write would exceed it, the engine fails the run with `AmoruError::Staging` carrying the totals, rather than growing. Upholds S10.

**PL-I8. A segment is deleted only when empty and unreferenced.** A segment file is unlinked only when every record in it is in state `Consumed` or has been promoted and its disk copy released, and the current manifest does not reference it (PL-I12); no live entry and no manifest on disk ever references a deleted segment.

**PL-I9. Misses are counted, never hidden.** Every `pop_blocking` that waits on a move increments `misses` and adds the wait to `miss_wait_us`; the scheduler writes the wait into the trace.

**PL-I10. Close drains.** After `close(stage)`, pushes error, pops return the remaining entries in order then `None`; no entry is lost.

**PL-I11. Lineage is kept until commit.** Every morsel that has been pushed to any queue and not yet covered by the committed watermark has a lineage record (origin, current stage, disk copy if any), including morsels that have been popped and are inside a kernel or the sink. Upholds CT-I12. Rationale: the manifest must be able to name every morsel whose output the sink does not yet have, or resume produces gaps.

**PL-I12. The manifest is atomic and self-sufficient.** A manifest is written to a temporary name and renamed into place, so a reader sees either the previous manifest or the new one, never a partial one; and it references only segment files that exist at the moment of the rename (a segment is never unlinked while the current manifest references a record in it: unlink waits for the next manifest write). Rationale: a crash between two writes must leave a resumable state.

**PL-I13. Resume is by lineage, never by replication.** The engine never writes a morsel to disk for the sake of recovery; the manifest records what is on disk because pressure put it there, and lists the rest for recomputation. Upholds CT-I12, S17. Rationale: the normal path pays nothing for the ability to resume.

## d. Interfaces

### d.1 Exposed

```rust
pub struct PlacementConfig {
    pub run_id: RunId,                            // names the staging subdirectory and the manifest
    pub node: NodeId,                             // LOCAL_NODE in v1
    pub stages: u16,                              // number of queues = kernels + 1 (Q0 .. Qn)
    pub budgets: TierBudgets,                     // initial; controller updates
    pub staging_dir: Option<std::path::PathBuf>,  // None = no disk tier and no manifest
    pub durable_staging: bool,                    // profile.durable_staging == Present; recorded in the manifest
    pub disk_budget: u64,                         // budget.disk
    pub segment_bytes: u64,                       // staging.segment_bytes
    pub page_bytes: usize,
    pub gds: bool,                                // profile.gds == Present && feature gds
    pub pinned: bool,                             // arena.is_pinned()
    pub devices: Vec<DeviceId>,
}

pub struct PlacementEngine { /* private */ }
impl PlacementEngine {
    pub fn new(cfg: PlacementConfig, arena: Arc<Arena>, reactor: Arc<Reactor>) -> Result<Arc<PlacementEngine>>;
    /// Entries in state Evicted for `stage`, oldest first; the scheduler re-reads them and pushes replacements with `replace`.
    pub fn evicted(&self, stage: StageId) -> Vec<(Seq, Origin)>;
    /// Replace an Evicted entry's bytes (same seq) after the scheduler re-read it.
    pub fn replace(&self, stage: StageId, morsel: Morsel) -> Result<()>;
    /// True if the head of `stage`'s queue is resident in a tier satisfying `want` (non-consuming; used by the scheduler's pick).
    pub fn peek_resident(&self, stage: StageId, want: PayloadSpec) -> bool;
    /// Cancel every in-flight move and stop planning (preamble 4.3).
    pub fn shutdown(&self);
    pub fn detailed_stats(&self) -> PlacementDetail;   // per-queue tier histograms, segments, evictions
    /// `staging_dir/amoru-<run_id>/manifest.json`, or None without a staging directory.
    pub fn manifest_path(&self) -> Option<std::path::PathBuf>;
    /// Locate the newest manifest under `staging_dir` for `run_id`, or, with `None`,
    /// the newest manifest of any run in that directory (Python's `resume="auto"`).
    pub fn find_manifest(staging_dir: &std::path::Path, run_id: Option<RunId>) -> Result<Option<std::path::PathBuf>>;
    /// Read only the identity fields of a manifest (`run_id`, `node`, `hostname`, `version`)
    /// so the facade can build `PlacementConfig` with the manifest's run id before `restore`.
    pub fn read_manifest_header(path: &std::path::Path) -> Result<ManifestHeader>;
}
impl Placement for PlacementEngine { /* contracts d.10, including set_committed, checkpoint, restore */ }

pub struct ManifestHeader { pub version: u32, pub run_id: RunId, pub node: NodeId, pub hostname: String, pub written_ns: u64 }
```

### d.2 Consumed

`amoru_kernel::{Placement, Morsel, Payload, PayloadSpec, PayloadKind, TierPref, Tier, TIER_COUNT, SegmentRef, RemoteRef, DeviceId, NodeId, LOCAL_NODE, RunId, Locality, Origin, Split, Fingerprint, CheckpointExtras, SourceCursor, ResumePoint, TierBudgets, QueueStats, PlacementStats, Buffer, Allocator, Reactor as ReactorTrait, Completion, AmoruError}`; `amoru_arena::Arena` (`alloc`, `contains`, `is_pinned`); `amoru_reactor::Reactor` (`copy`, `write_file`, `read_file`, `register_segment`); `amoru_sinks::ipc` (the page-aligned IPC record encoder for table payloads; see e.3); `crossbeam` (`Parker`/`Unparker` for `pop_blocking`); `serde_json` (the manifest, e.5; the only text format in this crate).

## e. Data model, formats and state machines

### e.1 Entry state machine

```
                push                      promote (issued)           promote (done)
   ──────────────────▶ Resident(tier) ────────────────────▶ Promoting(from,to) ────────────▶ Resident(to)
                            │                                                                    │
                            │ demote (issued)                                                    │ pop
                            ▼                                                                    ▼
                      Demoting(from,to) ──done──▶ Resident(to) | OnDisk(seg)              Consumed
                            │
                            │ (Q0, staging off)
                            ▼
                         Evicted ──replace──▶ Resident(tier)
```

`OnDisk(SegmentRef)` is `Resident(Tier::Disk(_))` in the contract's terms; the engine keeps them distinct internally because an `OnDisk` entry has no arena buffer. Illegal: `Consumed` → anything; `Promoting` → `Demoting`; `Evicted` → `Promoting` (must be replaced first). Attempting one is a bug (`debug_assert`) and, in release, a `Staging` error.

Reserved (feature `rdma`, not v1): `OnRemote(NodeId, RemoteRef)`, the counterpart of `OnDisk` for bytes in another node's memory, entered by a remote demotion and left by a remote promotion. The `State` enum carries the variant now so that every `match` on it is written once (CT-I11); in a v1 build the variant is never constructed and its arms return `Unsupported("rdma")`.

`Consumed` is a terminal state for the queue, not for the lineage index: a consumed entry's lineage record stays until the committed watermark passes its `seq` (PL-I11), at which point it is dropped.

### e.2 Queue structure

```rust
struct Queue {
    stage: StageId,
    order: VecDeque<Entry>,                 // FIFO; head at front
    consumer: PayloadSpec, target: Tier,
    water: [(u64, u64); TIER_COUNT],        // (low, high) per Tier::index: Device, PinnedHost, Host, Disk, Remote
    bytes: [AtomicU64; TIER_COUNT],         // resident bytes per tier
    staging_enabled: bool,
    promotion_window: u16,
    active_segment: Option<Segment>, segments: Vec<Segment>,
    misses: AtomicU64, miss_wait_us: AtomicU64, promotions: AtomicU64, demotions: AtomicU64, evictions: AtomicU64,
    closed: bool, waiters: Vec<Unparker>,
}
struct Entry { seq: Seq, morsel: Option<Morsel> /* None when OnDisk/Evicted */, origin: Origin, bytes: u64, kind: PayloadKind, state: State, recomputable: bool, disk: Option<SegmentRef>, move_id: Option<u64> }
```

Global: `reserved: [AtomicU64; TIER_COUNT]` per tier, `disk_bytes: AtomicU64`, `budgets: RwLock<TierBudgets>`, `moves: Mutex<HashMap<u64, MoveInFlight>>`, and the lineage index:

```rust
struct Lineage { origin: Origin, stage: StageId, kind: PayloadKind, bytes: u64, disk: Option<SegmentRef>, consumed: bool }
lineage: Mutex<BTreeMap<Seq, Lineage>>,     // every uncommitted seq; lock order position 2b (after any queue lock, before tier accounting)
committed: AtomicU64,                        // the watermark; u64::MAX means "nothing committed yet"
manifest_refs: Mutex<HashSet<u32>>,          // segments the current manifest references (PL-I12)
```

A lineage record is created at the first `push` of a `seq` (Q0, by the source drive, or on `replace`), updated on every `push` of the same `seq` to a later stage (a kernel output: `stage` advances, `disk` clears, `consumed` clears), on demotion to disk (`disk` set), on promotion (`disk` kept; the disk copy stays valid until consumption, f.7), on `pop` (`consumed` set), and dropped by `set_committed`. Its size is bounded by the number of morsels between the source cursor and the committed watermark, which the queue budgets already bound.

### e.3 Segment file format

Segment files live at `staging_dir/amoru-<run_id>/q<stage>-<segment:06>.seg`, created at `segment_bytes` (fallocate), written sequentially by direct IO. The same directory holds `manifest.json` (e.5). `<run_id>` is the 32-character hex form of `RunId`. Each record:

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic `AMORUSEG` |
| 8 | 8 | seq |
| 16 | 2 | stage |
| 18 | 1 | kind (0 table, 1 tensor) |
| 19 | 5 | reserved zero |
| 24 | 8 | payload_len (bytes of the payload body) |
| 32 | 8 | body_offset (absolute in file; page-aligned) |
| 40 | 24 | reserved zero |
| body_offset | payload_len | body |

Body for a table: the page-aligned Arrow IPC encoding of one record batch (schema message included per record so a segment is self-describing; the schema is small). Body for a tensor: `AMB1` (contracts e.4). The next record's header begins at the next page boundary after the body. A segment is full when the next record would not fit; the engine then opens a new segment. `SegmentRef { segment, offset: body_offset, len: payload_len }` is what the entry carries.

### e.4 Move planning table

Given an entry's current tier `c` and the queue's target `t` (or the demotion target `d`), the reactor call sequence, from contracts e.1:

| From | To | Steps |
|---|---|---|
| Host | PinnedHost | one `copy` (arena to arena; on a pinned arena, `Host` does not occur) |
| PinnedHost | Device(d) | one `copy` |
| Host | Device(d) | two: Host → PinnedHost, PinnedHost → Device (bounce inside the reactor when unpinned) |
| Device(d) | PinnedHost | one `copy` |
| PinnedHost / Host | Disk | `write_file` of the encoded body into the active segment (the encode for tables is the IPC layout write, buffer by buffer, no copy; see f.5) |
| Disk | PinnedHost / Host | `read_file` of `[body_offset, body_offset+len)` into an arena buffer, then decode the IPC framing by pointer (no copy: the IPC reader over the buffer yields arrays pointing into it) |
| Disk | Device(d) | `copy` with a Disk-tier source buffer (GDS) when `cfg.gds`; else Disk → PinnedHost → Device |
| Device(d) | Disk | Device → PinnedHost → Disk (GDS write is not used in v1) |
| PinnedHost | Remote(n) | reserved, feature `rdma`, not v1: one `copy` whose destination is a `Remote`-tier buffer the reactor obtained from node `n`'s lease (one-sided RDMA write); v1 returns `Unsupported("rdma")` |
| Remote(n) | PinnedHost | reserved, feature `rdma`, not v1: one `copy` from a `Remote`-tier source (one-sided RDMA read into an arena buffer); v1 returns `Unsupported("rdma")` |
| Remote(n) | Device(d) | reserved: Remote → PinnedHost → Device (GPUDirect RDMA is not planned) |
| Remote(n) | Disk | never: a remote copy is demoted by the node that owns the bytes, not by the node that references them |

Demotion target: one tier down from the current (`Device → PinnedHost`, `PinnedHost/Host → Disk`), never two at once, so a device-heavy queue demotes to host first and to disk only if host is also over its high water. With the `rdma` feature, `PinnedHost → Remote` sits between `PinnedHost` and `Disk` in that ladder when a peer node has budget to lend (contracts f.6 rank order); without it, the ladder is as stated.

### e.5 Run manifest

`staging_dir/amoru-<run_id>/manifest.json`, UTF-8 JSON, one object, written by f.12 and read by f.13. Version 1:

| Key | Type | Meaning |
|---|---|---|
| `version` | integer | 1; an unknown version is refused, not skipped |
| `run_id` | string | 32 hex characters |
| `written_ns` | integer | wall clock at the write, nanoseconds since the epoch |
| `node` | integer | `NodeId` of the writer; `nodes` below lists every node of the run |
| `hostname` | string | the writer's hostname; in v1 the only way to tell "same node" from "another node" (f.13) |
| `nodes` | array of integer | `[0]` in v1 |
| `durable_staging` | boolean | from `PlacementConfig`; a resume on a different node checks this and refuses with `Resume` when false, because the segments it references are on the dead node's local disk |
| `plan_digest` | string | BLAKE3 hex over the source plan: for each split in order, `id` and `rows` as little-endian u64s (`uncompressed_bytes` is excluded because it may be an estimate that varies between plans) |
| `kernels` | array of string | kernel fingerprints per stage 1..n, hex |
| `resume_policy` | array of string | `reinit`, `checkpoint` or `forbid` per stage |
| `stages` | integer | number of queues |
| `committed_seq` | integer or null | the watermark at the write |
| `source_cursor` | object `{split_index, row_offset, next_seq}` | from `CheckpointExtras` |
| `sink_state` | string or null | base64 of `Sink::checkpoint` |
| `kernel_states` | array of `{stage, instance, state}` | base64 of `KernelState::checkpoint`, `Checkpoint` kernels only |
| `lineage` | array of `{seq, split, row_start, row_end, node, stage, kind, bytes, disk}` | one per uncommitted morsel, ascending `seq`; `disk` is `{segment, offset, len}` or null; `kind` is `table` or `tensor` |
| `segments` | array of `{stage, segment, bytes}` | every segment file the lineage references |
| `config` | object | the resolved configuration table (preamble section 5) at the write, for the report and for refusing a resume with a different `staging.segment_bytes` or `page.bytes` |

The manifest is small by construction: `lineage` holds only morsels between the committed watermark and the source cursor, which the queue budgets bound (a 32 GiB budget of 64 MiB morsels is about 500 entries, under 100 KiB of JSON). It is written to `manifest.json.tmp` and renamed (PL-I12). A run that ends normally leaves a final manifest with `committed_seq == next_seq − 1` and an empty `lineage`; `Runtime::run` deletes the run directory after `finish` unless `checkpoint.keep` is set (preamble section 5).

## f. Algorithms and policies

**f.1 `push(stage, morsel)`.** Lock queue; append `Entry { state: Resident(morsel.payload.tier()), recomputable: stage == 0 }`; add bytes to the tier counter; unlock; update the lineage record for `morsel.seq` (create on first sight; else set `stage`, clear `disk` and `consumed`; e.2); unpark one waiter; call `plan(stage)`. A push whose payload tier is `Remote` is `Unsupported("rdma")` in v1.

**f.2 `plan(stage)`.** Runs after every push, pop, move completion and knob change, under the queue lock, in this order:

1. *Head first.* If the head is not resident in a tier satisfying the consumer spec and has no move in flight, issue a promotion for it (PL-I1). If the head is `Evicted`, record it for `evicted()` and stop planning this queue (the scheduler must replace it).
2. *Window.* For entries 2..=k from the head that are not at the target tier and have no move in flight, issue promotions in position order, each only if the destination reservation succeeds (PL-I3); stop at the first reservation failure (the tier is full; demotion will make room).
3. *Pressure.* For each tier from `Device` down to `PinnedHost`/`Host`: if `bytes[tier] > high[tier]`, walk from the tail toward the head, skipping entries within the window, entries with a move in flight, and the head; for each candidate, issue a demotion one tier down until `bytes[tier] ≤ low[tier]` or no candidate remains. For `Host`/`PinnedHost` demotion when `staging_enabled` is false and the entry is recomputable: evict (drop the buffer, `Evicted`, `evictions += 1`) instead of writing. When `staging_enabled` is false and the entry is not recomputable (a kernel output on a queue with staging off): no demotion is possible; the queue reports full and admission stops (the scheduler's rule).
4. *Disk bound.* Before issuing a disk write, check `disk_bytes + len ≤ disk_budget`; if not, and the entry is recomputable, evict; else fail the run with `Staging` (PL-I7).

**f.3 `pop(stage, want, locality)`.** Lock queue; if empty or closed-and-empty, return `Ok(None)`; if the head is `Resident` in a tier satisfying `want`, remove it, subtract bytes, mark `Consumed`, unlock, set `consumed` on its lineage record, `plan`, return it; else return `Ok(None)` (not resident yet). `locality` is accepted and, in v1, ignored after a `debug_assert` that the head is not `OnRemote`; with the `rdma` feature, `Local` refuses a head whose bytes are remote and lets the scheduler pick another stage while the promotion lands. `want` satisfaction: `TierPref::Any` accepts any resident tier; `Host` accepts `Host` or `PinnedHost`; `Device` accepts `Device(_)`. A resident head in the wrong tier (for example on `Host` when the consumer wants `Device`) is not returned; f.2 step 1 will have a promotion in flight for it.

**f.4 `pop_blocking`.** Loop: `pop`; on `Ok(None)` and not closed-empty, record the wait start, park on the queue's unparker with a 10 ms timeout (to observe `close` and shutdown), and on wake retry; on success, if a wait was recorded, `misses += 1`, `miss_wait_us += elapsed` (PL-I9) and return the wait through a thread-local the scheduler reads for the trace.

**f.5 Demotion to disk (write).** Reserve the record size against `disk_bytes`; ensure the active segment has room (else roll: `register_segment` the new file with the reactor); write the record header from a 64-byte arena buffer; for a table, write each Arrow buffer of the batch with `write_file` at successive page-aligned offsets (the IPC framing bytes, schema and record-batch message, are written from a small arena buffer before the body buffers; total framing under 64 KiB); for a tensor, write the `AMB1` header then the tensor bytes. On completion of all writes: release the arena buffers, set `OnDisk(SegmentRef)`, subtract from the source tier, `demotions += 1`. The write is a DMA from the arena (direct IO), so no CPU copy (PL-I4).

**f.6 Promotion from disk (read).** Reserve `len` in the destination tier; `alloc(len rounded to page)`; `read_file(segment path, body_offset, buf)` (or GDS `copy` to a device buffer); on completion, for a table, run the IPC reader over the buffer to produce a `RecordBatch` whose arrays point into the buffer (arrow's IPC reader supports this over an aligned `Buffer`; the segment's page alignment guarantees the 8-byte alignment arrow requires); for a tensor, wrap the body after the `AMB1` header; set `Resident(dest)`; `promotions += 1`; mark the segment record as promoted (for PL-I8).

**f.7 Segment lifecycle.** Each segment tracks `records_total` and `records_released`; a record is released when its entry is `Consumed` after promotion, or when it is promoted and the disk copy is no longer needed (the engine keeps the disk copy until consumption so a later re-demotion of the same entry is a no-op: `OnDisk` remains valid, only the resident copy is dropped, which is the cheapest demotion). When `records_released == records_total` and the segment is not active, it becomes *reclaimable*; it is unlinked, and its bytes subtracted from `disk_bytes`, at the first manifest write that no longer references it (PL-I8, PL-I12). Without a manifest (no staging directory means no segments either, so this case does not arise) or with `checkpoint.enabled = false`, reclaimable segments are unlinked at once.

**f.8 Budgets and water marks.** `set_budgets` replaces the global budgets; `set_water` replaces a queue's marks; both trigger `plan` for every queue. Default water marks when the controller has not set them: high = tier budget / number of queues, low = high / 2.

**f.9 Reservation arithmetic.** `reserve(tier, bytes)`: CAS loop on `reserved[tier]` with the check `resident[tier] + reserved[tier] + bytes ≤ budget[tier]` where `resident[tier]` is the sum over queues; a failed reservation returns false without side effect. Release on completion or failure.

**f.10 Failure of a move.** Promotion failure (reactor error): retry once through the fallback path the reactor offers (bounce for copies; buffered for file ops); on second failure, the entry stays where it was and the failure is recorded; if it was the head, the queue reports the error through the next `pop` as `Err(Staging)`, which the scheduler turns into a run termination with the morsel named. Demotion failure: the entry stays resident; pressure remains; the controller sees it through `bytes` above high water and reduces morsel targets; if the tier is at budget and no demotion succeeds, the scheduler stops admitting and, if the head cannot be produced, the run terminates (G-I8's diagnostic path).

**f.11 `set_committed(seq)`.** Store the watermark (monotonic: a lower value is ignored with a `debug_assert`); lock the lineage index; remove every record with `seq` at or below the watermark; for each removed record with a disk copy, release the record in its segment (f.7); unlock. O(removed) per call; the scheduler calls it once per sink commit, not per morsel.

**f.12 `checkpoint(extras)`.** Without a staging directory, `Err(Resume("no staging directory"))`. Otherwise: lock the lineage index; snapshot it into the e.5 shape together with `extras`, the watermark, the plan digest and kernel fingerprints recorded at `new`, and the current configuration; collect the set of segments referenced; unlock; serialise; write `manifest.json.tmp` through the reactor's file path (buffered is fine: the file is small and the write is `fsync`ed before the rename); rename over `manifest.json`; `fsync` the directory; replace `manifest_refs` with the collected set and unlink every reclaimable segment not in it (f.7). Runs on the caller's thread (the scheduler's checkpoint thread, SC f.12; preamble 4.1), never on a worker, and never under a queue lock. Duration is dominated by the two `fsync`s; `tracing` records it (`placement.checkpoint`).

**f.13 `restore(manifest, plan, fingerprints)`.** Must precede any `push`. Read and parse; refuse (`Resume` with the path and the first mismatch) on: unknown `version`; `run_id` different from `PlacementConfig::run_id`; `plan_digest` different from the digest of `plan`; `kernels` different from `fingerprints`; any `resume_policy == forbid`; `durable_staging == false` when the manifest's `hostname` differs from this host's (in v1 the hostname is the node identity; the multi-node extension replaces this with `NodeId`); `config.staging.segment_bytes` or `config.page.bytes` different from the current ones; a referenced segment file missing or shorter than the largest `offset + len` that references it. Then: set the watermark; rebuild the lineage index from `lineage`; for each record with `disk` set, create an `OnDisk` entry in queue `stage` (the record's own stage) and register the segment with the reactor; the queues are rebuilt in ascending `seq`, which restores FIFO order per queue (PL-I5) because sequence numbers were assigned in push order; every record without `disk` (resident bytes lost, or in flight inside a kernel or the sink at the crash) goes to `ResumePoint::to_recompute` as `(seq, origin)`, in ascending `seq`. Segments with records that are no longer referenced are left alone until the next manifest write reclaims them. Return `ResumePoint { extras, to_recompute }`. The engine does not re-read anything: the scheduler re-reads each `to_recompute` origin and pushes it to Q0 with its original `seq` (SC f.13), and the kernels re-run it from stage 1; a morsel that had reached stage 3 in RAM is recomputed from the source, which is the price of not replicating (PL-I13).

## g. Concurrency within the component

One mutex per queue (lock order position 2), the moves map mutex (position 3, taken only inside a queue lock during issue, and alone during completion), atomics for tier bytes and reservations. Move completions arrive on reactor threads: the completion handler locks the moves map, finds the entry's queue, locks the queue (order: moves map is position 3, queue is position 2, so the handler must take the queue lock first: it looks up the queue id from the move id in a lock-free side table, locks the queue, then the map), updates state, releases reservations, unparks waiters, runs `plan`. No lock is held across a reactor call: `plan` collects the moves to issue into a local list, unlocks, issues them, and re-locks only to record their move ids. `pop_blocking` parks without holding the lock. The lineage mutex (position 2b) is taken after a queue lock is released, never inside one, and `checkpoint` holds it only while snapshotting, never during serialisation or IO.

## h. Behaviour

**Normal path (host only, compute-bound).** Consumer specs all `Host`; the arena is unpinned; every push is `Resident(Host)`; `plan` finds heads resident and nothing over high water; pops return immediately; no moves ever issue. The engine's overhead is one lock per push and pop.

**Normal path (GPU kernel).** Q1's consumer is `Tensor/Device`; target `Device(0)`; pushes arrive `Resident(PinnedHost)`; `plan` promotes the head and the next `k−1` entries by `copy` on the H2D stream; the kernel pops device-resident morsels; its outputs are pushed to Q2 `Resident(Device(0))`; Q2's consumer (the sink) wants `Host`, so `plan` demotes... no: promotion toward `PinnedHost` (the target), which is the same reactor call as a demotion; the engine treats "toward target" as promotion regardless of rank direction. Device bytes are bounded by the device budget through reservations.

**Normal path (slow sink, S10 and S15).** Qn (before the sink) grows; `bytes[Host] > high`; demotion from the tail writes records to segments at NVMe sequential bandwidth; the head stays resident; the sink drains at its pace; as it consumes, promotions from disk refill the window; segments are released when their records are consumed. Throughput after engagement is bounded by the slower of sink rate and disk write rate, and S15's 70% is met when disk bandwidth exceeds the sink's.

**Weight-major precondition (E8).** With `set_staging(0, true)`, Q0 behaves like any queue and may hold the whole dataset on disk with a resident window; this is the mode a future weight-major controller uses and it is tested here (PL-T12) even though no v1 controller enables it.

**Edge cases.** A morsel larger than the destination tier's whole budget: reservation fails forever; f.2 step 1 detects a head that cannot be promoted and reports it through `pop` as `Staging("head cannot fit in <tier>: need X, budget Y")`. A queue with `k` larger than its length: promote all. Two consumers popping the same queue (stateful instances): the queue lock serialises; FIFO holds. `set_consumer` after entries exist: retarget; existing promotions complete to the old target and are re-planned. Shutdown with moves in flight: PL-I10 plus reactor RE-I7; entries left in `Promoting` are treated as their source tier for accounting release.

**Resume (S17).** The process died (OOM kill, node loss, operator cancel) with 4,000 morsels committed, 300 uncommitted of which 180 were on disk under pressure, 100 resident and 20 inside kernels or the sink. A new process, on the same node or on another node with the staging volume attached, calls `restore`: the 180 come back `OnDisk` in their stages, the 120 others are listed for recomputation, the source cursor says where to continue, the sink discards its one uncommitted file and continues numbering, `Reinit` kernels run `init`, and the controller starts from the profile store rather than probing. Output is byte-identical to an uninterrupted run for an ordered sink, and set-identical for an unordered one (PL-T17). The cost of the crash is the 120 recomputed morsels plus the sink's discarded file; nothing else is redone.

**Failures.** See f.10. Disk full below the budget (filesystem lied about free space): the write fails; treat as a demotion failure; also lower the effective disk budget to `disk_bytes` and note it. A manifest write failure (`Resume` from f.12) is reported to the scheduler, which logs it at `warn` and continues; the run is still correct, only its resumability is stale; three consecutive failures terminate the run with the error, because a platform that promised a durable staging directory and cannot write to it is misconfigured (G-I7 is about performance fallbacks; this is not one).

## i. Configuration

`budget.disk`, `staging.dir`, `staging.segment_bytes`, `queue.high_water`, `queue.low_water`, `queue.promotion_window`, `checkpoint.enabled`, `checkpoint.interval_ms`, `checkpoint.keep` (preamble section 5).

## j. Observability

`PlacementStats` and `QueueStats` (contracts) plus `PlacementDetail { per_queue: Vec<QueueDetail { bytes_by_tier, entries_by_state: [u64; 7], segments_live, segment_bytes, evictions }>, moves_in_flight, reservations: [u64; TIER_COUNT], disk_bytes, lineage_len, committed_seq, manifests_written, last_manifest_us }`. `tracing`: `placement.demote` and `placement.promote` (debug: stage, seq, from, to, bytes), `placement.evict` (debug), `placement.segment_roll` (info), `placement.miss` (trace), `placement.move_failed` (warn), `placement.disk_bound` (error), `placement.checkpoint` (debug: lineage_len, segments, duration_us), `placement.checkpoint_failed` (warn), `placement.restore` (info: on_disk, to_recompute, committed_seq, next_seq).

## k. Tests

Unit tests use `FakeReactor` (instantaneous or delayed moves, failure injection) and `FakeAllocator`; integration tests use the real reactor on a temp directory; S15 on the reference host.

**PL-T1 head_hot.** Property test: random push/pop/knob sequences with delayed moves; at every observable point, the head is resident or has exactly one promotion in flight and no demotion. PL-I1, PL-I2.

**PL-T2 accounting_exact.** Model-based test: resident + reserved per tier equals the model after every event and never exceeds budget. PL-I3.

**PL-T3 no_cpu_bytes.** `payload_copies_total` unchanged over a run with demotions and promotions; every byte movement is a recorded reactor operation. PL-I4.

**PL-T4 fifo.** 10,000 entries with random demotion pressure; pops are in push order. PL-I5.

**PL-T5 q0_evicts.** Staging off: Q0 pressure produces `Evicted` entries and no segment writes; `evicted()` lists them; `replace` restores order. PL-I6.

**PL-T6 disk_bounded.** Disk budget 64 MiB; sustained pressure on a non-recomputable queue; the run fails with `Staging` carrying totals; disk usage never exceeded the budget (checked with `du`). PL-I7.

**PL-T7 segment_deleted_when_empty.** With checkpointing off, segments are unlinked exactly when their last record is released; with it on, at the first manifest write after that, and never while a manifest on disk references them (the test parses each manifest and checks every referenced file exists); a live entry never references a missing file (fuzz with random consumption order across two queues). PL-I8, PL-I12.

**PL-T8 misses_counted.** Delayed moves; every waiting pop increments `misses` and the wait sum is within 5% of measured. PL-I9.

**PL-T9 close_drains.** Close with 100 entries, some on disk; pops return all 100 in order, then `None`. PL-I10.

**PL-T10 record_format.** Written segments parse by an independent reader in the test (header fields, alignment, IPC body readable by `arrow`, AMB1 body by contracts' reader). e.3.

**PL-T11 move_table.** For each row of e.4, with `FakeReactor` recording operations, the issued sequence matches the table (GPU rows skippable). e.4.

**PL-T12 q0_staging_on.** `set_staging(0, true)`: a 10× RAM-sized synthetic Q0 holds the dataset on disk with a resident window of `k`; pops proceed in order; no eviction. E8 precondition.

**PL-T13 slow_sink_throughput.** (reference host, real reactor, NVMe) Sink throttled to 10% of producer rate; memory stays under budget; throughput after staging engages ≥ 70% of before. S10, S15.

**PL-T14 move_failure_paths.** Injected copy failure: one retry via fallback, then head error surfaces through `pop`; injected write failure: entry stays resident, pressure persists, no crash. f.10.

**PL-T15 concurrency.** 8 producers, 8 consumers, reactor completions on 4 threads, 1 M entries; no deadlock (lock order test with a detector), stats consistent. g.

**PL-T16 manifest_roundtrip.** Drive an engine through pushes, demotions, pops and `set_committed`; `checkpoint`; parse the file with an independent reader in the test against e.5 (every key, ascending `lineage`, every referenced segment present and long enough, temporary file absent); a second engine's `restore` yields queues whose `OnDisk` entries equal the first engine's and a `to_recompute` list equal to the first engine's uncommitted entries without a disk copy; refusal cases: wrong `run_id`, wrong `plan_digest`, wrong fingerprint, a `forbid` stage, a missing segment, `version: 2`, each named in the `Resume` message. PL-I11, PL-I12, e.5, f.13.

**PL-T17 kill_and_resume.** (integration, real reactor, temp directory; wave 4, driven by the scheduler and `FakeSource` from the testkit; the Python variant is PY-T12 in wave 5) A 3-stage run with a throttled sink and a 10× RAM-sized source; the test process is killed with `SIGKILL` at ten random points including mid-checkpoint (the `.tmp` file exists); a fresh process resumes from the manifest through the scheduler's resume path; the final output is byte-equal to an uninterrupted run for an ordered sink and row-set-equal for an unordered one; the number of source rows re-read equals the recomputed lineage, never more (the test counts `Source::read` calls). PL-I13, S17.

**PL-T18 lineage_bounded.** With a committed watermark advancing at the sink's rate, `lineage_len` never exceeds the number of morsels admitted above the watermark, and drops to zero at the end of a run; with the watermark held, it grows exactly as fast as admission. PL-I11.

**PL-T19 remote_unsupported.** A `push` of a `Remote`-tier payload, a `pop` on a queue whose head is (unsafely constructed in the test) `OnRemote`, and every move-table row that names `Remote` return `Unsupported("rdma")` in a build without the `rdma` feature; the `no_tier_wildcard` lint (CT-T14) passes on this crate. CT-I11.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/queue.rs` (e.2, f.1, f.3, f.4), `src/plan.rs` (f.2, f.8, f.9), `src/moves.rs` (e.4, issue and completion, f.10), `src/staging/{mod.rs, segment.rs (e.3, f.7), write.rs (f.5), read.rs (f.6)}`, `src/lineage.rs` (e.2 lineage index, f.11), `src/manifest.rs` (e.5, f.12, f.13; `serde` derive structs mirror the table exactly and nothing else is serialised), `src/stats.rs`, `src/shutdown.rs`. `unsafe` is not permitted in this crate; the IPC-over-buffer decode in f.6 uses arrow's safe reader over an `arrow::buffer::Buffer` built from the arena `Buffer` through a safe `From` the contracts crate provides (`Buffer::into_arrow_buffer`, d.3), implemented by the arena.

The planner must be written as a pure function from a queue snapshot to a list of moves, and tested as such (PL-T1 drives it); issuing is a thin layer over it.

Anti-patterns: no per-entry heap allocation on the pop path (entries live in the `VecDeque`; moving a `Morsel` out is a move, not an allocation); no holding a queue lock across a reactor call; no compaction of segments; no "helpful" CPU copy when a move fails; no writing a morsel to disk because a checkpoint is due (PL-I13: the manifest describes, it never causes IO of payload size); no `_ =>` arm on `Tier` or `State` (CT-I11).

Verify before starting: arrow IPC reader zero-copy over an owned aligned buffer in the pinned version (`StreamReader` with `Buffer` input yields arrays referencing the input; confirm no copy by pointer comparison in PL-T10); `fallocate` on the staging filesystem.

## m. Open items

None. (`Buffer::into_arrow_buffer` is in `01-contracts.md` d.3; `peek_resident` is in d.1 above.)

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
| preamble 4.3 | PL-I10 | PL-T9 |
