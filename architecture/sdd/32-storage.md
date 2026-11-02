# Moruna SDD 32: Governed storage (`moruna-storage`)

**Document type:** software design document, component KS1
**Status:** implementation in progress, 2026-10-30; no execution proof yet
**Parent:** KS1 storage decision, 2026-10-30
**Preamble:** 00, G-I1, G-I2, G-I7, G-I12; **Contracts:** 01 d.3, d.9, d.17; **Related:** 06 reactor, 09 placement, 30 engine
**Component location:** `crates/moruna-storage-format` (no_std + alloc identities and the single durable codec), `crates/moruna-storage` (host adapters); integration points in reactor, placement and scheduler

## a. Purpose and boundary

Buckets are the durable home. Each node's local NVMe is encrypted workspace-scoped cache and scratch. A separately selected single-node raw-device home is an explicit estate choice, never an automatic fallback from a bucket. This component owns the versioned storage contract, conditional publication, authorization boundary, cache envelope, durable local publication and portable checkpoint publication. It does not invent an identity service, a credential format, keys, cloud access, a retention policy or a network device.

## b. Vocabulary

An object identity names its resource-owning workspace, bucket and key. Requester workspace is distinct: an exact verifier grant may authorize foreign-workspace resources, and a bucket target may be delegated. There is no generic same-workspace denial. An object version preserves both the backend's entity tag and version identifier. A read pins that version. A run access context carries an opaque credential, requesting workspace and run identity; credentials and keys have no printable Debug representation. An authorization provider verifies each operation against the exact identity and action, including sharing/delegation. A cache hit is an object read and requires fresh authorization too. A checkpoint is recovery metadata and immutable durable dependencies, not a promise of retained historical task rows.

## c. Invariants

- ST-I1: A range read fills exactly the supplied host arena buffer and returns that allocation. Short reads, changed versions and unsupported protocol majors fail; a device buffer is refused before slice access.
- ST-I2: A conditional replace is one backend compare-and-swap using the observed version, or create-if-absent. No read-then-unconditional-write emulation is permitted. A conflict leaves the previous object intact.
- ST-I3: Every object read, including cache, checkpoint and retained rows, verifies the run's credential for that object. A previously granted read is not a reusable authorization cache. Listing authorization does not grant access to each returned object.
- ST-I4: Pinned-source audit (2026-10-30): aes-gcm 0.10.3's zeroize feature clears its temporary GHASH key but does not enable downstream GHASH/POLYVAL state erasure. Direct-use ghash 0.5.1 and polyval 0.6.2 zeroize features are enabled under the approved E2 rows. POLYVAL's autodetect union uses ManuallyDrop without Drop, and its ARMv8 PMULL backend explicitly lacks zeroization. The selected upstream `polyval_force_soft` cfg bypasses those backends and uses soft32/soft64, whose Drop wipes keyed state. Root `.cargo/config.toml` passes the cfg globally to dependencies; storage-format's crypto build guard verifies exact CARGO_ENCODED_RUSTFLAGS, refusing overrides that omit it. This is an explicit security/performance choice: no hardware GHASH/offload performance is claimed, and KR must measure the cost. Native/cross builds overriding RUSTFLAGS must retain the same global cfg; a flag on storage-format alone is insufficient. Plaintext Volume refuses CacheScratch role; encrypted native raw cache must bind this same Cipher/envelope before any payload publication.

The single portable cache codec/cipher authority is `moruna-storage-format::cache` (optional crypto feature with the approved pinned primitives); Linux cache and native service adapters share canonical binary identity, prefix, requester-derived key schedule, authenticated encryption and wipe-on-failure. No second JSON/native cache envelope is maintained. Native keys arrive only in explicitly granted read-only secret Memory: the canonical `WorkspaceKey::encode_secret` / `decode_secret` layout has MORUKEY1, major, exact used length, bounded requester/key-id lengths, 32 key bytes, requester identity and key-id, with zero trailing page padding. The requester identity must match the actual cache key scope. Maximum secret page is 4096 bytes and each identity at most 1024 bytes (owner: storage key port). Missing key Memory/verifier refuses; no key bytes are sent through another network RPC or ambient provider. Local cache and scratch plaintext never reaches disk. Scratch uses the same encryption/key/epoch authority as cache, with an explicit write grant and immutable versioned handle; a missing scratch entry is an error, never a bucket fallback. Scratch is explicitly ephemeral and cannot satisfy retained historical rows or portable checkpoint dependencies. Authenticated encryption and cache path/key derivation bind requester workspace plus resource owner/object, immutable version, range and key identity. Another workspace, key, version or range cannot reuse an entry.
- ST-I5: Node recycle revokes the in-memory node cache secret and removes cache files. Cryptographic erasure is stated separately from physical NVMe sanitization; overwrite/unlink is not a physical erasure guarantee.
- ST-I6: A local publication acknowledges only after immutable data and its small commit record are durable. A bucket checkpoint acknowledges portability only after its immutable dependencies and conditional checkpoint pointer are committed in the bucket. Local success alone is not portable success.
- ST-I7: Spill, logs and checkpoints are not historical row artifacts. Retained task rows require an explicit governed artifact decision and publication boundary; fusion and transient buffer release remain intact.
- ST-I8: Concurrency is bounded; prefetched ranges use caller-reserved arena pages. There is no unbounded heap allocation of payload size and no API-data prefetch hidden in a metadata operation.

## d. Interfaces

`Storage` protocol major 1 exposes range-read into `Buffer`, whole-object write from `BufferView`, recursive prefix listing, and atomic conditional replace. An explicit object version is required for a range read so parallel reads cannot mix generations. Metadata and version identities are transport-neutral. `Backend` supplies operations; `Governed` validates identities and invokes the injected exact-action authorizer before every operation; it does not substitute workspace equality for authorization. There is no permissive production authorizer.

A bucket backend is supplied an established `object_store` client, a configured bucket/workspace and bounded range/part concurrency. It preserves SDK entity tags and generation versions; `PutMode::Create` and `PutMode::Update` implement conditional replace. Multipart completion publishes the whole object atomically; failed parts or completion trigger abort, and abort failure is returned with the primary failure. Conditional pointer replacement uses a single conditional write, not multipart emulation. Objects with no usable conditional version cannot be used as update targets.

Linux node bucket requests go through the host proxy over the existing HTTP-over-vsock connector, with an independently configured policy/key provider. A Moruna node uses KE2's network once integrated. Neither a trait implementation nor an SDK in-memory conformance run proves cloud access, proxy deployment, shared-page ingress or KE2 networking.

### d.2 Consumed

The format core uses `alloc`, pinned BLAKE3 1.8.7 without default std, and optional serde 1.0.229 for host envelopes. Host storage consumes contracts Buffer/BufferView/Allocator/Completion, object_store 0.13.2, futures/bytes/Tokio, the existing HTTP/socket connector dependencies, serde_json/thiserror/getrandom/libc, and AES-GCM 0.10.3/AES 0.8.4/zeroize 1.9.0 with explicit zeroization features. Direct-use E2 rows approved by the integrating PM on 2026-10-30 are recorded in preamble 6.2; no old lockfile package version/source/checksum changed.

## e. Data formats

`moruna-storage-format` is the one codec authority: no_std + alloc, without a filesystem, Tokio, SDK or credentials. It owns Object/Version/Metadata/Role, a bounded header (magic, major, payload length, BLAKE3 checksum), typed superblock/commit records, and file/contiguous-extent locations with block-digest-tree metadata. `Commit::validate_in` is the shared geometry/bounds reader for host and native-node adapters. Host JSON API envelopes use an optional serde feature on those same identity types, not a duplicated definition. Unknown versions, oversized lengths, invalid tags, checksum failures and nonzero trailing padding fail. FileStore now persists that binary codec, replacing its unproved draft JSON log rather than maintaining compatibility readers. This codec is not itself a native-node storage service. Cache entries use AES-256-GCM with a fresh OS-random nonce. The entry's authenticated additional data is a deterministic serialization of the complete identity/version/range/key binding. The encryption key is derived from the requesting workspace's key and an ephemeral node secret; the node secret is never persisted. Cache filenames are hashes, never object path concatenations. Key identifiers are not key bytes.

Raw publication uses fixed aligned commit cells (16 KiB rounded to device alignment, followed by an equally sized receipt cell). A checksummed receipt in the shared codec binds sequence and encoded commit hash after data/tree and commit sync; recovery ignores a zero receipt (unpublished tail), refuses malformed/nonzero receipts or mismatched committed bytes, and replays only receipt-backed records. Log/extent exhaustion is an explicit refusal; no implicit overwrite, retention period or garbage collection is invented. The regular-file block fixture uses exactly these cells but is labelled FormatProof.

Local immutable data uses contiguous extents; data has no redo journal. A bounded, checksummed commit record names the versioned metadata and immutable extent/object identities. Record ordering is data sync, commit sync, then visibility. An incomplete tail is not a committed record; corruption of a committed record is an error. Raw-device home uses the same format as cache/scratch with an explicit durability role. Regular-file fixture results do not prove raw-device O_DIRECT, io_uring or DMA.

Portable checkpoints export actual local dependencies via verified source read into an explicitly supplied charged buffer and immutable bucket write; restore verifies complete object metadata and full content before returning the same destination. The engine must rewrite/partition its local dependency references, not relabel local paths. Portable checkpoints name immutable bucket objects and hashes, preserve the existing run manifest's source cursors, issued reads, kernel/sink states and watermarks, and publish their pointer conditionally. A stale writer receives a conflict. Resume must authorize each referenced object and validate its identity. Node-local spill references cannot silently become portable: either publish their dependencies or re-read lineage from durable sources under the existing recovery policy.

Native control framing is `moruna_storage_format::wire`, version 1, with a 64 KiB maximum header and 16 KiB opaque capability maximum (owner: storage port). The exact-length binary header carries request id, requester workspace/run, owner object, pinned read version/range or conditional write/list operation. Successful read replies bind metadata and the exact range/body length; create/replace acknowledgements carry committed metadata. Refusals are typed codes without arbitrary provider text or credentials. Body bytes follow separately, with an exact-length tracker; a native server validates control and verifies the exact action before accepting body/object/cache IO. The network service never interprets the capability. `encode_into` writes directly to caller-owned header space, and decode borrows the capability so no secret clone/Debug/serialization is introduced. The host socket object port sends the same codec and reads exact bodies directly into charged caller buffers using bounded-deadline owned blocking sockets. Its host service uses the same Governed authority before accepting write bodies and before object reads, with requester-specific arena admission. This is direct socket-to-arena application landing, not network DMA or elimination of host bucket SDK staging. Native charged `Memory` receive-into is a dependent KE2 service extension; bounded TCP/TLS staging still exists and is not called DMA.

## f. Algorithms and policies

Read: validate context/object/version/range, authorize, resolve an immutable cache entry if present, otherwise issue bounded bucket ranges into disjoint reserved buffers. Network-library chunk copies are counted ingress, not described as zero-copy DMA. A transport that can fill arena pages directly implements the same port without changing consumers.

Write: authorize, retain an owned publication worker and its source view through all original SDK operations. Consumer cancellation stops scheduling new multipart parts, drains every admitted part, then awaits abort; a completed temporary object is deleted if cancellation arrives before atomic publication. Single-put/CAS requests already in flight finish and retain their actual result. Detached completion/cleanup errors are retained by requester scope and surfaced by drain, never discarded with a dropped join handle. The runtime must drain before releasing its provider/budget or recycling. Write: authorize, keep the source view alive, use a whole-object put for small objects or parallel fixed-size multipart parts for large ones, and publish metadata only after completion. No cloud persistent disk is selected implicitly.

Sources use the real cache read-through path: a miss performs a fresh governed pinned home read into the same destination, then encrypts the admitted range; a hit still verifies current authority. Corruption/key/admission failures propagate explicitly. `prefetch::fetch_cached` integrates that path with the known-future range plan. Known-future prefetch accepts the scheduler/source plan's exact column ranges, orders by next use, reserves bytes before issuing and returns the same buffers. It does not infer columns from URLs or extend a buffer's lifetime without a reservation. Hooking this port into the existing scheduler requires a source range identity beyond the placement catalog's decoded-batch next-use values.

Historical rows: the orchestrator supplies a retention decision per task and run, including an immutable policy identity, destination and authority. Materialization is a governed sink/tap at the logical task boundary and is independent of executable fusion. No artifact is published by merely observing stdout, stderr, spill or an in-memory batch. Policy details remain an integration decision until supplied by the owner.

## g. Concurrency

Async backend futures own their input view or output buffer through completion. No mutex spans a network await. Cache recycle serializes against local cache IO and destroys the ephemeral node secret even if file removal fails. Local commit serialization requires an exclusive writer lock; cross-process writers cannot use an in-process mutex as compare-and-swap. Within an exclusively opened raw device, a non-cloneable Writer admits one transaction. Each submitted read, write and sync retains its shared writer lease through the original operation, even when the transaction or consumer future is dropped. Only the final original completion releases admission; this is ownership protection, not automatic recovery of a partially written transaction. Raw-volume recovery must still validate the durable commit log before a subsequent publication.

## h. Failure behavior

Authorization failure precedes cache/file/network access. Authentication failure wipes the supplied destination before returning. A missing cache entry is distinct from corruption; corruption is not silently treated as a miss. Version conflict is a typed result. Unsupported major versions, unconfigured bucket, missing key, unavailable transport and unspecified raw-device home fail explicitly. Credential/key material is excluded from diagnostics.

## i. Configuration

Bucket name and workspace are required, without defaults. Range and upload concurrency default to 4 and must be positive, owner `moruna-storage`. Range size defaults to 8 MiB; upload part size defaults to 16 MiB (minimum 5 MiB except the final part); single-put ceiling defaults to 64 MiB. Metadata size ceiling defaults to 1 MiB, owner checkpoint/local log readers. Node cache root, nonzero logical ciphertext byte budget, and scratch capacity are explicit. Ciphertext-file byte admission includes the complete replacement temporary while the old file still exists. Readers close the header descriptor before awaiting keys and reopen/validate under the node lock, so they cannot pin unlinked old ciphertext across replacement. Cleanup failure poisons further insertion until recycle. This bounds regular-file contents, not filesystem metadata/allocated-block overhead or a measured physical NVMe capacity. Raw-device home requires an explicit device, role and capacity; no default device is opened.

## j. Observability

Report bucket ingress bytes, cache hit/miss/encrypted bytes, conditional conflicts, uploads/aborts, durability role, pending portable checkpoint, and selected transport/direct IO path. Never report credentials or keys. No latency or throughput claim precedes a measured proof.

## k. Tests

ST-T1 runs the same storage conformance against SDK memory and local durable adapters: exact range bytes and allocation identity, empty object, recursive list, create and stale-version conflict, atomic concurrent replacement and unknown version refusal. ST-T2 revokes a credential between reads and requires denial on a cache hit. ST-T3 proves cross-workspace/key/version/range isolation, nonce uniqueness, corruption refusal and destination wiping. ST-T4 interrupts local publication before and after commit sync and verifies only committed data is visible. ST-T5 publishes a portable checkpoint, restores through another node's store instance, and refuses stale pointers or missing/unauthorized dependencies. ST-T6 holds parallel range/part operations at a controlled barrier to prove concurrency bounds and cancellation cleanup. ST-T7 requires an explicit governed retained-row decision and proves no implicit artifact from spill/logs. Hardware proofs separately exercise the raw device, O_DIRECT/io_uring, arena landing, real bucket conditional semantics and node recycle.

## l. Implementation notes

Use existing pinned `object_store` 0.13.2, `aes-gcm` 0.10.3, `aes` 0.8.4 and `zeroize` 1.9.0 already in the lockfile (AES/key-derivation zeroize features enabled), with OS entropy from `getrandom` 0.4.3. No custom cipher or credential parsing. Ordinary storage modules and the format crate forbid unsafe. The integrating PM approved `moruna-storage::raw` on 2026-10-30 only for Linux block geometry ioctls: a live exclusively opened block FD and exact-size local u64/int outputs. O_DIRECT|O_EXCL, no-follow, block-device type, real capacity, logical alignment, positive role and checked ranges are mandatory. The direct path retains Device and the charged buffer/view on the blocking operation through completion. The uring path binds a retained O_DIRECT FD through `Reactor::strict_storage`, which requires actual guaranteed Present paths and bypasses every buffered/engine fallback. It retains Device through the original operation completion even if the consumer future drops; a cancellation CQE alone cannot release it. SDD06 owns the required single-owner ring repair and controlled tests. Raw home must additionally hold its exclusive writer lease through every outstanding original IO when a transaction future is dropped; a poisoned/partial transaction cannot admit another writer before drain and recovery. Tests may construct controlled fixtures but do not claim a real cloud, DMA or kernel result.

The portable `journal::Journal` owns raw log cell geometry, receipt/commit binding, monotonic replay, contiguous extent placement, current version checks and exhaustion decisions. Both Linux Volume and native service adapters call it; OS/device/network code supplies bytes and durability barriers, never another parser or publication decision. `auth::Verifier` is the no_std injected exact-action authority contract over validated requester Scope, owner Object and borrowed opaque capability. It has no permissive/default implementation. The host asynchronous Authorizer and native authority service adapt this same action vocabulary; neither transport invents token formats or owner-equality policy.

Authority proofs have two explicit representations: Application borrows actual issuer-provided opaque bytes, while NativeSession is only a request marker whose authority comes from the actual SDK session/context bound by node admission to Scope+Account and a designated live verifier. A marker/wire scope grants nothing. Socket endpoints always return Denied for NativeSession; sealed host NativeConnection credentials are minted only from a consumed startup channel and actual storage handshake. Combining explicit local/home roles retains the actual connection set under one actual account; every backend verifies membership and calls its own live context authority. No dummy token, owner-equality allowance or cross-process Arc assumption is introduced.

Native service transport is `moruna.storage` 1.0 inside kernel/storage, not another object codec. Its execute call transfers bounded control, caller payload and caller reply Memory loans; the control/reply bytes are exactly storage-format::wire. Node control grants bind one requester Scope to its real KB5 Account and an explicit object listener; arbitrary wire scope cannot select another account. A boot-granted authority channel receives the canonical auth request and returns a correlated typed Verdict, with missing authority failing closed. Existing moruna.driver.block and moruna.net protocols move actual bytes; native Mapping is accessed only through its safe volatile API or fresh SDK PrivateMemory, never an invented mutable slice over untrusted shared Memory. Native direct-range landing and bounded crypto/checksum copies are separate counters. All provider grants, home role, TLS target and empty-format provisioning are explicit. Driver namespace exclusivity must be enforced by a real lease or proved capability routing, not a caller boolean.

## m. Open integration items

Credential and workspace-key provider integrations, raw-device proof environment, KE2 native network and KA2/shared-page proxy integration require the integrating owner. The inherited retained-row requirement is now explicit: saved fused jobs must expose each logical task's historical rows without rerunning data or disabling fusion. Capture requires an explicit request and reservation; required unsupported/over-budget capture fails loudly, no truncation/partial-as-complete. Identity includes workspace, run, attempt, logical task and output port; manifests include policy, schema/content identities, row/byte counts and capturing/complete/refused state. The implemented publisher/reader is a real governed object path, but existing standard fusion currently merges projections (`moruna-kernels::fuse`) and the job builder replaces stages with the merged kernel. Those kinds need actual intermediate logical-boundary taps plus asynchronous, charged publication admission in the scheduler; the standalone publisher does not instrument them. Raw Device now supplies strict owned block IO to Volume, which implements formatting, mount/replay, receipt-backed CAS publication, authenticated direct range reads and charged unaligned halos through the shared no_std Journal. FileBlocks runs that identical format path as an explicit regular-file proof; raw physical-device/uring acceptance has not run. Encrypted disk cache and Scratch share one key/epoch/envelope/recycle authority; native raw encrypted-cache binding remains the native service adapter integration, not a second parser or cipher design. The host socket object Client/Service implements exact codec framing, verification-before-body admission and direct application landing into caller arena; host SDK/TCP staging remains independently counted. Native receive_into and NVMe service integration remain owned by the native adapter, with the shared Journal/auth contracts. Full adapter conformance, raw home, node service, physical recycle/cloud performance and historical fused-job acceptance have not run. Existing reactor object reads currently accept URLs without a run credential, and placement checkpoints retain node-local segment references; those APIs must be deliberately migrated, not silently presented as governed or portable. Existing scheduler fusion must be retained when adding the task artifact tap. No section here marks these integrations complete.

## n. Traceability

KS1 protocol and bucket backend: ST-I1/I2/I8, d/f, ST-T1/T6. Cache governance/recycle: ST-I3/I4/I5, e/h, ST-T2/T3. Local commits and bucket checkpoints: ST-I6, e/f, ST-T4/T5. Retained task rows: ST-I7, f, ST-T7.

## o. Deferred

No part of the decided KS1 programme is silently deferred by a port declaration. Hardware and dependent integrations are explicit open items until implemented and proved.

### Native session admission decision (2026-10-30)

Application-issued opaque credentials and native capability-bound session proofs are
explicit different auth modes in the sole core codec. A native marker grants nothing.
Socket transports always refuse it. Native consumers consume an actual startup Channel,
perform the real SDK handshake, and obtain server-owned Session scope/role facts before
minting an unforgeable in-process connection credential. Scope equality alone is never
an authorization result. Each exact object/cache action still delegates to the designated
live verifier bound at admission to the actual child Account and scope.

The private `moruna.storage.admission` 1.0 `bind` operation accepts canonical Admission
control plus the actual narrowed child Account capability and returns an already bound
connected object Channel. This endpoint is itself a boot/application authority, separate
from the public object port. No public object operation can bind or relabel an account.
Absent authority, account, key, explicit backend, or credential refuses. Actual SessionRole
facts distinguish BucketHome, CheckpointMetadata, CacheScratch and SingleNodeHome;
separate slot names alone do not establish portable checkpoint roles.

The std-native consumer requires actual KD2 startup/sys/svc native ABI, KB5 charged
Account/Memory, and the canonical storage service protocol. Those dependency source
merges remain required in this standalone checkout; no substitute ABI/model is supplied.
Control/reply Memory and DMA payload Memory are charged to the actual granted child
Account. Current arena-to-SDK staging copies are counted, not called direct arena ingress.
A timed-out connection closes and cannot retry an uncertain publication or recycle its
mapping; the service/driver retains transferred original loans through original completion.
Native service boot, authority admission, raw device and bucket integration require their
real implementations and tests; protocol/client source alone does not establish acceptance.

### Native local-role and cache-consumer decision (2026-10-30)

A local process owns ONE real block connection and exclusive writer lease. Explicit
nonoverlapping byte regions hold independently validated Superblocks/Journals for
CheckpointMetadata and CacheScratch; an optional third region may explicitly hold
SingleNodeHome. Checked base+length translation uses actual driver capacity and alignment
before any view/IO. Different strings are not device identity, and no second SSD is
mandatory. There is no automatic partition allocation or format-on-mount fallback.
Actual flush remains the physical device's barrier shared by those disjoint views.
Cache-only plaintext backend execution is prohibited; the encrypted wrapper is required.

Native range cache requests carry the ORIGINAL home Object, pinned Version and exact
range. Trusted engine consumers freshly authorize/head home before every lookup, preserve
the charged destination on genuine miss, read actual home bytes, then fill under an
explicit CacheScratch WRITE grant. Untrusted task processes are not delegated that
trusted-writer endpoint. Cache grants never grant home mutation. Original identity is
authenticated by the sole Binding/Prefix/Cipher codec, not a second hashed-resource policy.
A different home version/epoch selects another entry; corruption/key/origin failures are
errors, never misses. Conflicting bytes under an already filled pinned range refuse.
The workspace key scopes cache reuse within that requester workspace; resource ownership
is distinct, and full per-run authority is freshly checked for every access.

Local cache plaintext uses fresh actual-account PrivateMemory; received peer mappings
are accessed only through bounded volatile copies. Issued ciphers remain owned through
private plaintext lifetime, and their schedules are dropped before the epoch owner count
falls. Recycle destroys the old epoch before clearing/syncing only the cache region log;
failed entropy/IO poisons use, and no physical erase or already delivered user-data erase
is claimed. Neither scratch nor cached rows substitute for historical row artifacts.

Provider descriptions report actual backend classes without resource permission. Only
typed established GoogleCloudStorage or AmazonS3 clients construct a production bucket
home; arbitrary ObjectStore/InMemory/format fixtures remain unclassified. Native proxy
checks that fact at startup AND every forwarded operation requires that same role, so
miswiring or endpoint replacement cannot silently downgrade to local/fixture storage.
This classification is not a physical/cloud durability proof.

Native grants use finite elapsed monotonic nanoseconds from one successful authority
load this boot. Reconnect/redecode does not renew it. Actual node authority checks live
job/Account context and revocation per request; application proof/token expiry remains
separate and arbitrary bytes never become native authority.

### Native asynchronous originals and compaction decision (2026-10-30)

Native volume operations submit actual block tickets and suspend until the original replies, retaining the transaction and charged DMA owners. An abandoned operation poisons further publication; Controller ownership survives its receiver and cancellation intent. The service executor must watch the same driver channel and continue draining orphaned originals. Sequential publication dependencies remain data/Merkle completion, flush, commit completion, flush, receipt completion, flush; a queued blocking adapter is not asynchronous IO. Namespace mutation and collection cannot overlap an active volume transaction.

The approved reclaim design uses a new disk-format major, two bounded banks and alternating checksummed generation roots. Usable capacity excludes the other-bank copy reserve. Only internally frozen namespace roots/open-inode references and all nonnamespace objects define liveness. Copy preserves public versions and ciphertext; the cache AAD contains logical identity, not physical location. The old bank remains authoritative until copied data/journal and the new root have actually synced, and cannot be reused while original IO or pinned reads remain. Generation overflow and unavailable pre-reserved copy resources refuse before admission is blocked. Implementation and real crash/reclaim acceptance remain required.

The native bank-copy path now prepares `CompactionResources` before the registry freeze: checked next root/generation, bounded commit capacity, index charge, private data/proof/record/receipt pages, encoded root, and one reusable `device_reserved::Prepared` DMA span with its persistent volatile mapping/control charge. Reuse follows only the original validated completion; uncertain completion or dropped transaction closes admission and retains original Controller owners. Per-request SDK handle transfer and bounded frame bookkeeping still occur; this is a reserved backing/mapping contract, not a claim of allocation-free kernel execution. The frozen liveness set is sealed to the actual mounted-owner token and preserves all nonnamespace objects unconditionally. Explicit `Encrypted::format` provisions zero-root cache media through the same v2 authority, but normal boot/mount never invokes provisioning. New v2 source and source fixtures require fresh combined compile/runtime/crash gates; the earlier v1 fixture passes do not prove them.

### Explicit operator provisioning and private maintenance (2026-10-30)

The supported host executable is `moruna-storage-provision` in package `moruna-storage`. `plan --output ABS_ROOT --capacity BYTES --alignment BYTES --log-bytes BYTES --role checkpoint-metadata|cache-scratch|single-node-home --workspace OWNER --bucket BUCKET` creates a new synced canonical initial Root artifact. `format --image ABS_EXISTING_IMAGE --root ABS_ROOT` opens an existing owned regular disk image without resizing/truncating, takes an exclusive nonblocking lock and validates exact capacity/geometry. Linux `format --raw ABS_BLOCK_DEVICE --root ABS_ROOT` uses the existing strict O_DIRECT|O_EXCL/no-follow block-device opener, actual capacity/sector queries and unique original-operation ownership. Fixed aligned scratch is at most 64 KiB. Both paths prove BOTH root cells zero before mutation, clear/sync the initial journal, write/sync the root and verify actual readback. Failures carry exact phase and target-mutation uncertainty. No raw or plaintext CacheScratch store is returned; an empty encrypted-cache region needs keys only when encrypted runtime begins. Neither normal mount nor node startup invokes this tool. These are actual source entrypoints awaiting combined gates, not claimed physical raw-device acceptance.

Private `moruna.storage.maintenance` takes no caller Scope/account/root lists or role selector. Node may grant its connected endpoint only to an explicitly configured resident `storage-maintenance` process. Both server and client consume the same readonly `storage.maintenance-config` page (`moruna-storage-maintenance 1` and all required interval-ns/max-objects/max-roots/metadata-bytes/page-io fields). The canonical metadata workspace is exactly 65536 bytes; fingerprints/index/copy reservations are additional real charges. The service's actual carved Account pays physical-owner housekeeping, never an arbitrary tenant. The server sequences actual local owners, returning only after all actual maintenance result channels acknowledge durability. The resident client uses a real kernel Timer and finite configured interval/RPC wait, reports real Busy/Capacity refusals, and fails on uncertain transport rather than starting overlapping retries. Cache key/epoch recycling remains a distinct private capability. No endpoint/config means no maintenance authority, and a bucket proxy cannot be relabelled local GC.

The connected namespace client now consumes actual object ordinal 3 then validates the real namespace session (Account object type and charged identity, Scope/role/root/limits). Retained SDK worker ownership survives receiver cancellation until original response; replies preserve actual short/failed/cancelled counts. The sealed liveness set pre-reserves its verified full-metadata reader and bounded fingerprint/frame capacity before registry freeze. `ReservedRead` charges its finite ceiling for actual data/proof submissions, including device transfer splits; a configured read-budget refusal before another submission aborts GC without falsely poisoning a healthy owner. No post-freeze DMA/private-map backing creation is used in that walk. These connected native/maintenance/GC source paths and source tests await combined compilation and real GB/repeated-reclaim/crash/lease/cancellation acceptance; no source-only result is presented as deployment proof.

### Root retirement/recovery correction: raw format v3 (2026-10-30)

Raw root magic/major is now MORUBNK3/3. Earlier unreleased v2 media cannot establish the strengthened retirement invariant and is explicitly refused; provisioning artifacts must be regenerated. Inner records and object/namespace transports keep their existing versions. The shared Root recovery policy treats ONLY entirely zero cells as absent. Any invalid nonzero cell is integrity/authority uncertainty, never silently discarded. Two compatible adjacent valid roots select the newer bank only with a typed obligation: replay its validated journal, then zero AND sync the older root before the adapter returns or can acknowledge later mutation. Host replay-per-operation and native mount both enforce that obligation; failed retirement blocks subsequent acknowledged puts until recovery succeeds.

Before any inactive bank overwrite, its previous root cell is zeroed and synced. After copied data/journal and new-root sync, the prior current root is zeroed and synced before compaction succeeds. Thus stable acknowledgement boundaries retain exactly one root authority: corrupting or zeroing it cannot silently fall back to stale acknowledged state. A torn nonzero retirement fails closed; no atomic-sector guarantee is assumed. A provable all-zero inactive cell permits recovery from interrupted pre-publication work while the current bank remains authoritative. Source regressions use real files, several bank reuses, exact acknowledged bytes/Versions, newest-root corruption/zero loss, and retirement refusal followed by attempted mutation/reopen. Their execution belongs to the coordinated root gates.

The operator formatter also supports `format --image ABS_IMAGE --root ABS_ROOT --layout ABS_STORAGE_CONFIG` (or Linux `--raw`). The optional layout is the SAME canonical BootConfig used by native mount. All Local regions are checked together for nonoverlap/alignment/actual capacity; the Root's explicit role selects only its configured metadata/cache/home region. Root physical capacity must equal that region, and IO is checked/rebased under the same single exclusive whole-target owner. No independently supplied/guessed byte offset or implicit cloud/local fallback is accepted. Without a layout, the explicit whole-target behavior remains. This closes supported provisioning for the decided one-NVMe multi-volume design; source tests examine all three real-file region roots and unchanged neighbors after refused overlapping/wrong-capacity plans. Exact checkpoint publication now uses Current rather than a prefix listing, so Read/Publish-only local grants work and genuine denial is never interpreted as absence.
