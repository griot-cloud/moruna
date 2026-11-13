# SDD 33 : Standalone node

Status: declared bounded source implementation complete; integrated compile/link and SIMD fixture gates passed; production node/KVM ownership validation remains pending.

## Boundary

The [0.6.6 delivery plan](../release-0.6.6.md) maps the deferred launcher and
isolation work onto these native node services, capability grants and owned
process lifetimes. Its acceptance gates are tracked in `BOARD.md`.

The node is a boot image of the existing microkernel and separate user processes.
`node-supervisor` is the only initial process. It holds service images, explicit
boot capabilities, and the root process resource. It carves one bounded account
per service and uses KB5 `Process::create_in`. Drivers, device directory, imagefs,
Linux personality, and network remain processes. The node does not implement
storage authentication, TCP/IP, TLS, or Linux syscalls inside the kernel.

Prerequisite merges are KE1 drivers, KE2 network, KB5 accounts and Linux/imagefs
protocol 3.0, and KD2 engine. KS1 owns the sole storage-format wire codec and
capability verification. These interfaces are dependencies, not local shims.

## Configuration

`moruna-node 1` is an explicit bounded UTF-8 boot configuration, at most one
zero-padded 4096-byte data page. Unknown records, names longer than ABI start
slots, duplicate services/images/destination slots, unaligned/zero budgets,
overflow, and aggregate budgets beyond the supervisor's available account fail
before launching a child.

```
moruna-node 1
service devices image.devices 67108864
service net image.net 67108864
grant devices drivers device.table
link devices directory.net net directory
```

`service NAME IMAGE_SLOT ACCOUNT_BYTES` declares launch order and a child account.
`grant SERVICE CHILD_SLOT BOOT_SLOT` moves one boot capability exactly once.
`link SERVICE SLOT SERVICE SLOT` makes a fresh channel pair and moves each end
once. Every service receives its own account handle and a write-only console.
No service implicitly receives the process root or another service's account.
Trusted roots/time for KE2 TLS are explicit boot capabilities; no wall clock,
certificate bypass, or generated production trust anchor is synthesized here.
The boot packager must provide these from its operator's trusted inputs.

## Lifecycle

The supervisor prepares all accounts, ELF images, grants and links before any
process starts. It retains process/account handles, never duplicate channel
endpoints. Start handles are atomically moved using the existing SDK. Any start
failure kills all prepared children. An unexpected termination of any service
stops the node; transparent restart would leave stale capability endpoints and
is not claimed. The devices process retains its existing bounded driver restart
policy. Shutdown kills children and waits against one shared five-second bound.
Process start is reported only as `started`; it is not service readiness. Current
ABI exposes termination signals, not a process exit-status query.

## Remaining integration evidence

The source now assembles a single supervisor and uses the actual device/Linux
root convention through one strict codec. Admission starts the explicitly granted
initial per-run engine through its ordinary launch/account path, retains delegated
backend listeners, and offers scoped subsequent job admission. Compiler, protocol,
actual kernel account/alias, shutdown, UEFI and KVM boot proofs remain required.
No cloud hardware or TLS bare-metal handshake result is implied by source.

## Verification

Source tests cover budget refusal, ambiguous/unversioned config, unexpected death
preventing later launches, and commanded shutdown state. They have not yet been
executed on this branch. Parent owns compiler, runtime, image and boot proof
stages and the shared resource reservation.

## Assembly implementation

`kernel/node/tools/package.py` takes an explicit moruna-image description and a
local asset directory, builds the kernel and chosen service packages offline and
locked, and uses the existing image writer. It never creates OCI fixture layers
or test services. It then builds a GRUB x86_64 EFI binary with `multiboot2` plus
`module2`, writes a FAT32 ESP with mtools, and places it in a GPT disk at sector
2048. No host loop mount or root device access is used. Required tools are checked
before building; existing output directories are refused. Kernel ELF machine type
is preserved (OSTD 0.19.0 cached source has x86 multiboot2 entry/header support).
QEMU direct-loader EM_386 patching is not applied. Inputs and resulting artifacts
are hashed. The image is not boot-validated yet; the packaging path requires an
actual UEFI boot proof before any standalone claim.

## Canonical root bootstrap

`moruna_node::Bootstrap` defines an exact eight-byte `MNBT` version-1 header,
kind and handle count. Devices receives exactly process/device/ECAM/MSI/DMA;
Linux receives exactly its process resource. The actual device server, Linux
server and old test supervisor message senders/receivers use this codec; old raw
`roots`/`process` message payloads are refused. This does not change `moruna.linux`
or `moruna.imagefs` service protocol layout; their current assets-capability major
is 3.0. Explicit `bootstrap devices NAME`
and `bootstrap linux NAME` config records create one queued message and move the
receiving endpoint to the service's supervisor slot before launch. The node
retains no sender. Linux's existing direct root.process start slot remains an
explicit direct capability bootstrap rather than an alternative message decoder.

## Admission and native object service (required next)

The node owns a single long-lived admission protocol. Its only admission authority
is an explicit granted channel capability; it accepts no ambient identity or
anonymous network credentials. Jobs must carry an explicit carved account,
engine image/config/capability grants and bounded launch request. Per-job engine
process handles remain owned by admission until termination and shutdown. The
service must retain its authorized backend listeners, not manufacture keepalive
calls or label process start as backend health. The exact svc contract and
implementation are part of this node package, not a future generic bootstrap API.

Native object transport/service lives under kernel/storage and consumes the sole
`moruna-storage-format::wire` codec and shared Journal publication authority from
KS1. It must verify an injected exact-action authority before object/cache/body I/O;
absence of a verifier or required device is a typed refusal, never permissive
acceptance. Read payload ingress uses KE2's caller-owned charged Memory receive
contract. Ready(length) admits a bounded write body and is not a durable receipt;
only shared Journal publication rules can acknowledge durability.

### Admission contract and implementation

`moruna.node.admission` 1.0 is declared once in `kernel/node/protocol`. The
`node-admission` process accepts only connections to its explicitly boot-granted
listener; capability possession authorizes this exact service, never arbitrary
network bearer strings. Launch transfers an Account capability, an explicit child
budget, the KD2 config memory and corresponding named grant handles. It carves a
new child account from the transferred account, creates a process in it, loads
only the boot-configured engine image, and starts with console/account/config plus
those explicit grants. Reserved/duplicate/overlong slot names and unaligned/zero
budgets are refused. Account object type is checked before carving. The engine
receives no implicit process resource, network or filesystem grant.

Job ids and queries are scoped to their originating connection. Status and wait
reflect actual TERMINATED signals, with a separate cancellation marker; no success
exit status is fabricated. Cancel issues process_kill and status remains running
until termination is observed. Release refuses running jobs. There are at most 64
retained jobs and eight pending waits per job. Disconnect kills that connection's
jobs. The dispatcher watches each real process handle, and process-watch setup or
dispatch failure kills remaining jobs rather than orphaning them. A parent node shutdown first uses the private control channel and then owns
escrow process handles for actual descendant kill/drain; it does not assume
creator death or account deletion terminates children.

Source remains uncompiled pending prerequisite merges and the parent's granted
verification stage. Protocol conformance and actual engine launch/teardown proof
are still required. No remote admission listener is published by this channel-only
contract; an eventual remote ingress must use an injected exact-action verifier,
not reinterpret channel possession as anonymous remote permission.

## Fixed initial-run resource contract

The immutable descriptor now declares `resources` with `node_memory_bytes`,
`job_memory_bytes`, `job_account_bytes`, `scratch_bytes`, and `cpus`. The initial
account strictly covers runtime memory, scratch and native supervisor overhead;
the admission account also covers its own service overhead. Node RAM is distinct
from a document's job memory. Launcher admission accepts ordinary smaller job
budgets within the fixed engine contract and refuses larger memory/elastic/CPU
requests. Actual QEMU RAM/CPUs come from the descriptor. Changing `-m` alone is not
an implementation of dynamic boot configuration. Dynamic run-config injection is
not implemented.

`moruna-node-image` validates the actual single-supervisor description, exact data
slot bindings, node config, initial-job config and KD2 engine config using their
canonical Rust parsers. The engine must run once and listen at the explicit
`agent_port`; its parsed memory/scratch values generate `node-resources.json`.
`package.py` consumes this output into `node-image.json`, with exact artifact
names/sizes/digests, rather than guessing a directory layout. The shipped
`kernel/node/standalone.image` assembles actual production component ELFs and
explicit input assets, with no test processes or fabricated bucket layers.

## Native host launcher and duplex session

`moruna_vmm::microkernel::launch` is the Linux/x86_64 production path: explicit
QEMU KVM/q35 PCI plus the existing owned moruna-vhost-vsock helper. There is no TCG
fallback and no Linux zero-page loader on this path. It reads one checked
Multiboot2 descriptor, snapshots and hashes boot artifacts, validates the actual
ELF/header, and adapts only the owned QEMU copy's e_machine exactly as the existing
OSTD/QEMU harness requires. Original and UEFI-packaged ELF remain unmodified.
Helper/emulator processes and private socket paths belong to this invocation;
cleanup signals only its Child handles. KVM/tool/architecture/artifact failure is
an explicit refusal. No cloud or Mac hypervisor acceptance is implied.

`moruna_runtime::host::native::NativeRun` uses the existing host protocol schema,
with typed Outbound reads and a cloneable Control for canonical Inbound
Spec/PortReply/Cancel/Checkpoint messages. Start accepts the caller's exact Spec,
including its optional plan. Canonical MAX_SPEC_LINE/MAX_CONTROL_LINE/MAX_PORT_LINE
bounds apply; valid large port/report frames are not reduced to one MiB. Deadlines
bound reader and writer/lock wait, poisoned control locks refuse, partial frames
survive socket timeouts, and EOF/emulator exit never substitutes for a real job
Exit. Guest readiness is an actual connection at the descriptor's declared engine
port. Runtime --vm uses this path; extra --disk attachments are refused in favor
of the descriptor. The CLI refuses unconfigured host ports; embedding hosts use
Control to provide real replies. Existing low-level Linux-monitor source is not a
fallback for this native runtime entrypoint.

## Descendant ownership and private scratch

Every production process creator (devices, Linux personality, admission) uses the
single Guardian/ChildRecord escrow codec. It transfers a narrowed WRITE|WAIT
process handle to the node owner before launch; failed starts retract it. Queue
pressure waits only to a fixed deadline. Devices/Linux reuse the reverse direction
of their existing root-bootstrap channel, and the actual test supervisor drains
that same codec through service lifetime. The node owner retains bounded handles
and watches actual termination, so a failed creator cannot silently strand its
children. Shutdown stops creators, drains every published child handle, then kills
and waits actual descendants; private control first asks admission to stop jobs
and complete only after their termination. The creator-death invariant is not
inferred from Rust handle Drop or account deletion.

PrivateMemory lives only in the native moruna-sys memory boundary. It allocates
fresh non-DMA memory under the supplied KB5 account, creates its private mapping
before secrets are accepted, exposes no handle/map/duplicate constructor, permits
only closure-scoped exclusive bytes, and unmaps before consuming export. Drop
volatile-wipes the complete private backing without another fallible allocation.
It is never constructed from received/DMA/shared handles. The existing trusted
process-control boundary still applies: authorized remote Process::write may not
target a live private Rust borrow; guardian WRITE rights are used solely to kill,
never to mutate descendant memory. Native alias/account/lifetime proof remains
required; host modeling is not that proof.


### Explicit backend delegation

The initial-job codec distinguishes `grant SLOT SOURCE` (move one explicitly
boot-granted capability) from `service SLOT SOURCE` (retain the actual authorized
listener in admission and delegate a narrowed duplicate). Subsequent launch
requests explicitly name configured services. Possession of that admission
listener authorizes only these declared backend delegations; unknown service names,
slot collisions and excessive grants are refused. No ambient backend, credential,
workspace or anonymous authority is synthesized. KD2 `Source::Service` opens a
fresh connection from that listener for each actual run. The node's retained
listener is an actual capability owner for subsequent admissions, not a periodic
keepalive or readiness substitute.

### Mutable NVMe backing and host stop

A descriptor's `writable_disks` explicitly names preprovisioned relative regular
files, exact capacity and bounded serial. They are mutable device state, not
immutable boot blobs. The launcher never formats/truncates them, holds a real
exclusive backing-file lock, and exposes actual QEMU NVMe controllers. Native
storage still validates the real superblock/role and block-2.0 namespace lease;
VMM path metadata grants none of that authority. Owned QMP quit precedes emulator
reap and backing-lock release. Forced kill remains an error/fallback, not a claim
of graceful durable drain. Rust's safe file-lock API is the platform boundary:
[File::try_lock](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock).

Since 0.6.6 a writable disk may also carry `access` and `grant`. `access` is
`read_write` (the default) or `read_only`. A read-only disk is opened read-only
under a shared lock that still excludes any writer, and is attached with
`readonly=on`. `grant` is `{job_digest, run_id, attempt}`: the public MJS digest,
the 32-hex run id and the launch attempt it is granted to. `launch_for_run`
checks every grant against the launch's `RunAuthority` before any disk is opened,
and refuses a malformed grant, a launch without run authority, another job, or
another run or attempt. Reusing the path needs a fresh grant. A disk without a
grant keeps the earlier unbound behaviour until the packaging tools emit grants.

`configure.py` renders actual node/initial/KD2 configs only from explicit resource
profiles, image records, layer lists, network policy and trusted TLS root input.
It fetches nothing, generates no production clock/key/credential, and writes a new
private output directory. Canonical Rust validation follows during packaging.
The runtime CLI now preserves explicit --plan content, refuses retired --disk,
and admits/forwards the same captured spec bytes without a mutable-file reread.


### Exact charged-origin admission (ABI 2.5)

`Account::matches(&Handle)` is the sole object-origin check. Syscall
`ACCOUNT_MATCHES = BASE + 52` requires READ on both capabilities, accepts an
actual Account as the first object, and accepts only Account or Memory as the
second. Account equality uses kernel object identity; Memory equality uses its
immutable charge owner. Equal names, limits, ancestry, current usage, numeric
wire identities and Process aliases cannot establish equality. Foreign accounts
return false; wrong types or missing rights are errors. The primitive exposes no
account identifier and grants no additional authority.

Private storage admission retains the actual narrowed child Account capability.
It must compare the supplied Account and every incoming control, reply and payload
Memory before mapping, parsing or IO. The native client also compares the actual
Account capability returned by its bound session. A service-owned registered-loan
pool could avoid a syscall, but would require another pool/generation/CQE lease
protocol for all three Memory roles. The exact identity primitive preserves the
single caller-owned direct ingress contract without copying or a parallel payload
path.

Canonical integration retains KD1 ABI 2.1, KD2 2.2, KB5 2.3, KE1 contiguous DMA
flags at 2.4, and this primitive at 2.5. Contiguous allocations retain KB5 Account
charges before physical allocation, including failure rollback and pin/quarantine
ownership. `node-memory-proof` exercises actual native syscalls for exact aliases,
foreign accounts with identical names/limits, Memory loans, denied rights, wrong
types, charged contiguous allocation and quota rollback. This is proof source;
compilation and native execution remain required.

Process termination alone is not a DMA quiescence acknowledgement. On a block
peer failure, a consumer must retain loan owners until original completion or an
actual device reset/drain acknowledgement; guardian Process tracking must not
fabricate safe caller reuse or durable completion.

### Explicit native storage authority and assembly

The node admission process is the designated native authority only for an explicit
trusted `storage.grants` readonly boot Memory artifact. Its sole parser and grant
semantics are `moruna_storage_format::grants::GrantSet`: exact pre-admitted Scope
and backend role, explicit objects/versions or bounded literal directory prefixes,
explicit Read/Write/List/Publish bits, nonzero revocation identities and finite
monotonic validity intervals. The node does not derive rights from owner equality,
request metadata or an engine descriptor. The application must supply this artifact
from its real policy; packaging neither issues tokens nor manufactures grants.
Opaque application proofs remain the external application's verifier's concern;
this native provider accepts only the canonical NativeSession mode under a bound
actual capability context.

A private connected `storage.authority` channel joins the storage service to node
admission's canonical delegate handler. Bind checks the actual Account type/rights
and immutable charged origin of readonly control Memory before decoding Admission.
The returned raw verifier channel retains that actual Account and its exact grant
lease. Every operation uses the sole Authorization/Verdict codec and reevaluates
live grant rules, expiry, revocation and owner lifetime. The configured initial
engine's actual Job/Process is the owner: no verification succeeds before it runs
or after it terminates, node shutdown/fault, or master authority disconnect. Boot
grants do not automatically authorize later public admissions. Private node control
ordinal 2 `revoke(grant_id)` only removes rights; there is no public install-policy
operation or default allow. Session count and frame allocation are bounded.

Storage's own service Account uses the common `account` start slot. Other required
slots are readonly `storage.config`, connected `storage.admission`, connected
`storage.authority`, and console. Bucket mode additionally requires the explicit
KE2 `storage.network-listener` and issued `storage.credential` Memory; raw home mode
instead requires the device-directory **listener** `storage.block-directory` and a
literal configured block service. No provider fallback is allowed. The sole
BootConfig parser checks timeout, body/session limits and provider-specific inputs.

`configure.py` now requires the real storage config, trusted grant artifact and
canonical private Admission alongside the canonical binary EngineBindings envelope.
The initial KD2 account receives only the private admission endpoint and its exact
control record. KD2 compares the sole envelope Scope before binding freshly carved
runtime Account/control Memory; the runtime validates the actual returned session
Scope and home role against the same envelope. The optional local component requires its actual multi-volume backend and explicit
role grants; the initial home is never relabeled as checkpoint metadata or cache.

The packaging validator consumes these shared codecs, checks actual channel links,
provider role, matching binding identity and exact credential length before image
assembly. Host validator builds retain dependency-wide `--cfg polyval_force_soft`;
no bare-metal link flags leak to host tools. The launcher/host reader uses canonical
KA8 events7: persistent partial framing for NUL+record events and JSON controls,
canonical frame/decode validation, typed `native::Message::{Control,Event}`, and
bounded producer/run correlation. There is no JSON Event transport fallback.
All new provider/assembly/reader changes remain source-only until integrated
compiler, native account/admission refusal, real artifact boot and device gates run.

The native origin fixture is `kernel/node/proof/origin.image`, selecting only the
real `node-memory-proof` executable and console. It needs no mock device, network
or authority service. Build/package it with the actual merged KB5/KE1/ABI2.5
kernel and user linker script, then boot through the existing native kernel boot
harness. Its marker follows successful real Account/Memory/contiguous syscalls;
it does not prove actual device DMA completion, storage durability or node boot.

### Production launcher supersession

The public Linux-guest `boot`/`boot_and_run` API and image/OCI/resize/status/stop
CLI are retired in 1.0. Their low-level emulated-device/kernel-loader code remains
only under private `cfg(test)` modules for focused component tests; it cannot be
selected as a production fallback. Related dependencies are test dependencies,
with their original workspace pins. The production binary accepts only:

```
moruna-vmm boot --descriptor node-image.json --runtime-dir NEW_PRIVATE_DIR \
  --qemu /exact/qemu-system-x86_64 --vsock-backend /exact/moruna-vhost-vsock \
  --lifetime-seconds 3600
```

All options are explicit, unique and required; finite lifetime is 1..86400 seconds.
It launches the same checked native artifact API, supervises actual emulator exit,
and performs owned QMP shutdown/reap at lifetime expiry or SIGINT/SIGTERM. CLI-only
signal handlers live inside the existing OS unsafe boundary, do only atomic stores,
and restore their previous handlers; embedding APIs install none. Process exit or
scheduled stop is reported as a node process outcome, never a completed engine job.
Actual job/control/events are the separate NativeRun duplex API. CONNECT handshake
reads are bounded and respect the caller's remaining deadline. The old ignored
Linux guest/OCI integration gates are replaced by an explicit ignored **native**
artifact KVM gate requiring supplied descriptor/tools/new proof directory; component
unit tests remain test-only. No new gate has been executed by this source change.

Trusted boot grants use one explicit time namespace: finite elapsed monotonic
**nanoseconds since the authority's single successful policy load for this boot**.
The authority captures real `moruna_svc::now()` once, uses checked subtraction,
permanently denies backward elapsed time, and refuses anchor-plus-expiry overflow.
Neither reconnect nor a policy reload renews this epoch (there is no reload API).
A new boot is a distinct explicit host admission, not lease continuation. Opaque
application expiry/revocation is not reinterpreted by this boot policy. Before each
allow, actual Process TERMINATED and master PEER_CLOSED signals are polled, avoiding
callback ordering races. An allow queued behind transport pressure is rechecked
before sending, so it cannot outlive expiry/revoke/owner or master termination.


### One physical local device, distinct real roles

The production template uses ONE optional `local` storage process/Account and one
actual block connection/writer lease for checkpoint metadata and encrypted cache.
`--local-config`, `--checkpoint-admission`, `--cache-admission` and `--cache-key`
are supplied together. The sole storage provider validates explicit nonoverlapping
physical byte regions, actual capacity, and each region's real Superblock/Journal
role. It checks physical base-plus-relative bounds on every IO. The node does not
infer physical separation from service names or retry a second lease. It passes one
connected `local.admission` endpoint to KD2; the retained client binds two explicit
controls into separate scope/role-bound channels using the same runtime Account.

The local authority master is restricted to those exact trusted checkpoint/cache
bindings and cannot request a home grant; the home master cannot request local
roles. Each master's actual peer lifetime independently gates its verifier channels.
One shared grant set and policy epoch remain immutable except explicit revocation.
Multiple genuinely separate service instances can reuse the readonly program ELF,
but each has its own actual Process/Account/channel graph. That generic ability is
not evidence of separate physical namespaces. The single-device local backend and
its encrypted cache are owned by the sole storage implementation, not duplicated
in node code; compilation, real role/alias refusal and NVMe boot proof are pending.

### Admission reserves the actual dispatcher wait budget

The SDK's `WatchCapacity` and opaque non-cloneable `WatchPermit` reserve from the
same real 64-item dispatcher budget as channels/listeners/watches. A pending
termination or verifier watch cannot be displaced by new connections. Node launch
reserves BEFORE child Account carve/process start; authority bind reserves BEFORE
publishing a verifier endpoint. Publication requires the original dispatcher and
consumes that reservation, not another budget. All failure paths drop/roll back the
permit; closed watches release actual used capacity; a dropped dispatcher denies
new reservations. Normal exhaustion is a typed pre-admission refusal, not a
start-then-kill-all failure. Host SDK/channel-model fixtures cover exhaustion,
connection flood, rollback, foreign dispatcher and peer-close retirement; they do
not substitute for actual native Process/Account/termination proof.

### Explicit private storage maintenance

The optional renderer inputs `--maintenance-target storage|local` and
`--maintenance-config FILE` must appear together. Selecting maintenance requires
an explicit carved `housekeeping` service Account in the operator profile and
packages the real `storage-maintenance` resident executable. The node creates one
connected `storage.maintenance` pair between that process and the selected actual
storage instance. Neither initial-job/runtime grants nor a tenant listener receive
this endpoint. The housekeeping process receives only its actual Account, console,
private endpoint and immutable maintenance configuration; it is supervised as a
resident service, not a one-shot process disguised by a keepalive.

Both participants receive the same exact readonly `maintenance.conf` asset under
`storage.maintenance-config`. The sole storage-format `MaintenanceConfig` parser
validates finite interval, objects, roots and page-IO bounds, with metadata bytes fixed to the canonical 65536-byte control workspace; the
node defines no parallel maintenance codec and supplies no defaults. Absence emits
no endpoint. The image validator refuses dangling configuration, multiple workers,
non-storage targets, extra worker capabilities and different input assets. Storage
uses its own real service Account and sealed LiveSet; callers do not supply root
lists or substitute tenant accounts. Packaging/source validation is not execution
or media-GC proof; the resident client and parser are coordinated storage-owner
prerequisites still requiring integrated compilation and native execution.

### Native user-process SIMD target

Native userland uses the sole custom target
`kernel/targets/x86_64-moruna-user.json`, derived from pinned
`nightly-2026-07-21`'s actual `x86_64-unknown-none` target specification. It selects
the normal x86_64 SysV floating-point ABI (the builtin's `rustc-abi: softfloat`
selection is absent), baseline SSE/SSE2, no red zone and the existing freestanding
ELF/no_std properties. No `target-feature=-soft-float` ABI override remains. Both
Cargo's userland invocation explicitly enables `-Zjson-target-spec`; both Cargo
and userland rustc also retain `-Zunstable-options`. The pinned Cargo gate is
separate from rustc's metadata/target-loading gate. Core/alloc are rebuilt for this distinct target. The kernel
boot build keeps the unmodified builtin target and original flags.

This is supported by actual existing thread context handling: `core/boot.rs`
registers `process::deactivate_current` and `activate_current` as OSTD schedule
hooks; every Thread owns an OSTD `FpuContext`, saved before scheduling out and
loaded before scheduling in. Pinned OSTD 0.19.0 enables OSFXSR/OSXMMEXCPT and SSE
XCR0, and saves/restores through FXSAVE/FXRSTOR or XSAVE/XRSTOR. Ring's real
SSE/SSE2 assertions remain intact. This neither changes kernel code generation nor
introduces a crypto fallback. Actual native crypto execution and cross-thread
SIMD isolation still require the integration owner's boot tests.

Both `tools/build.sh` and node `package.py` select this JSON only for userland.
User executables live under `target/x86_64-moruna-user/release`; the kernel remains
under `target/x86_64-unknown-none/release`. Image assembly consumes the user path,
while kernel packaging explicitly reads the kernel path. Compiler metadata-only
`--print cfg` accepts the custom JSON and reports `target_os=none`, empty
`target_abi`, and SSE/SSE2 without the incompatible feature-override warnings.
This metadata result is not compilation/link/boot evidence.

### Native SIMD/FPU isolation fixture

`kernel/programs` supplies `fpu-a` and `fpu-b`, linked by an actual boot-image
channel. Each starts eight native threads with independent patterns and four distinct
rounding controls; both processes reach the channel barrier before releasing workers.
A single narrowly allowed test assembly region holds XMM0–12 patterns, a live SSE2
floating accumulator, MXCSR, and an x87 accumulator/control across repeated native
CLOCK_GET entries and timer preemption for at least 250 ms per worker. Assembly
compares each pattern and both controls on every iteration and checks both floating
accumulators against the exact integer count. Rust cannot spill and reload these
values around scheduling calls. The region restores controls and empties its x87
stack; it accesses only a live private State and issues no pointer-bearing syscall.
The finite iteration ceiling and ten-second join deadline fail loudly, with seed,
worker and diagnostic (1 state corruption, 2 iteration ceiling, 3 x87 count mismatch,
4 insufficient iterations). The manifest requires BOTH independently seeded success
lines and zero exits. Sixteen runnable workers exceed each required 1/2/4-CPU boot;
real boot logs on all three CPU counts remain mandatory. Source inclusion and
formatting are not an executed isolation or crypto acceptance proof.

### Durable launch witness and uncertainty

`host::launch_witness::Admission` takes the canonical RunId, a caller-supplied
node-boot fencing RunId, attempt and an explicit existing absolute durable witness
directory. `NativeRun::start(admission, descriptor, budget, runtime_dir, tools,
Spec, deadline)` returns `Result<NativeRun, StartError>`. `StartError.disposition`
is `NotDispatched` only for the original call's validation before dispatch, or
`MayHaveDispatched` from exclusive claim creation onward. This says nothing about
an older attempt. No later probe reports proven absence.

The canonical key is run_id + `moruna_plan::job_digest(checked_job)` +
`Plan::content_id()` + caller node_boot/attempt. The host-plan digest is specifically
the existing canonical 32-hex content ID (creation time/id normalized by Plan), not
a second hashing convention. Explicit plans are checked against the actual job;
the existing default planner supplies the digest when no plan was supplied. The
caller fencing node_boot is distinct from the optionally observed actual Linux
host boot identity. Numeric PIDs are never used to recover ownership or cancel a
process after owner loss.

Before any helper/QEMU spawn, an exclusive 0600 run claim appends
PotentialDispatch, syncs the file and syncs its containing directory. The bounded
append journal uses the existing pinned SHA256 implementation for chained record
checksums, preserving partial/truncated/mutated records as Unknown. Only a live
owner appends Launched after actual launcher success, Hello after real guest
protocol validation, and GuestExit after an explicit valid guest exit. Final
Exited/Cancelled requires actual owned QMP/child shutdown and both process reaps,
then a successful durable append. Failed durable writes poison the witness;
failed cleanup, timeout, transport loss and process disappearance never fabricate
a final result. Once this live owner observes guest transport failure or a deadline,
reconcile remains Unknown even if QEMU/helper processes still exist; a stale Hello
cannot erase that observation. The live owner retains the actual directory/file descriptors and
checks named inode/link identity plus full journal integrity before and after
publication; unlink/replacement/corruption poisons ownership even if a stale open
file descriptor remains writable. Expired deadlines refuse before the claim or
retain MayHaveDispatched after the claim, without starting a new process. Cancellation does not undo IO or prove that execution never began.

`NativeRun::reconcile()` returns actual live-owner evidence or Unknown;
`NativeRun::cancel()` returns Exited, Cancelled or Unknown with evidence/error.
`launch_witness::probe(directory, expected_key)` returns only terminal durable
Exited/Cancelled or Unknown (possibly carrying last evidence). A missing, corrupt,
wrong-key or partial claim is Unknown. Existing claims never automatically spawn
again, including after owner loss. Regional queue/fencing and any explicitly
authorized new attempt remain controller responsibilities; this local API neither
releases inputs nor grants redispatch. The CLI requires `--launch-witness` and an
explicit canonical job id; it cannot replace this typed controller contract with
an inference from a process exit code.

Actual-file source fixtures cover exclusive claim collision, owner-loss windows,
wrong fencing key, missing/partial/corrupt journals and unearned terminal states.
They do not claim VM execution, durable storage hardware or real guest readiness
proof. Compiler and native process/boot gates remain integration-owner work.

### Generic CI boot RAM recipe

The generic full-test boot uses `boot.sh`'s single fixed `memory=3G` decision for
both QEMU `-m` and the description's shared memfd backend. The strict image parser
accepts only the additional literal `{memory}` placeholder alongside `{out}` and
`{cpus}`; the boot script substitutes it before splitting the device arguments.
Unknown braces and shell expansions remain refused. The image writer preserves
this explicit template and has no independent RAM default. This is generic CI
machine assembly only: production node descriptor Resources/admission remain
unchanged, and no dynamic tenant memory or boot-injection authority is added.

### Mandatory five-case native engine successor fixture

The generic bare KD2 engine configuration has no production Guest authority and
cannot serve as a substitute for real bindings/storage admission. The successor
uses fifteen dedicated production-node fixture assemblies: each of the original
five peer cases at each supported fixed CPU profile 1/2/4. Each has one initial
run, a distinct canonical job/run id, real freshly carved Accounts, the production
node authority and an actual preprovisioned SingleNodeHome device. No production
per-run configuration sequence, anonymous default or test authorization parser is
introduced. The existing generic kernel/device checks remain separately required.

The sole job templates/checks remain in `kd3-peer`; its `--emit-job` supplies the
exact document, and `--case/--job` retains each original assertion. The host example
`node-ci-assets` parses that exact document and uses only production
EngineBindings/Admission/GrantSet codecs. It emits a finite exact READ-marker grant
for the controlled `moruna-ci` scope, no write/list/publish/prefix authority and no
application proof. Actual private admission/session Scope/role/Account checks are
still performed by the real service. These cases request no capture, checkpoint,
cache or cost authority; this does not claim those separately tested features.

`kernel/node/tools/proof.py` accepts explicit prebuilt tools and immutable image
inputs, one bounded `--cpus/--case` invocation, and a new private output directory.
It invokes production configure/validator/image assembly, creates only its own new
64MiB fixture image, and uses the actual storage provisioner before boot. Dedicated
PCI ordering gives home block0, readonly fixture layers block1/2 and the existing
engine stack block3 onward. Existing immutable layer files are reused readonly.
No destructive generic NVMe test device is reused as the home namespace.

The harness deliberately selects TCG for native execution tests, starts the actual
vsock backend and QEMU, invokes the canonical peer, checks actual CPU-online and
failure logs, and records actual natural debug-exit33 separately from acknowledged
QMP quit0. It reaps only owned subprocesses and records actual status/cleanup,
input hashes, job and initramfs identity. The common immutable census covers actual
QEMU/backend executables, every selected service ELF, every actual stack layer,
original kernel, template/configure/proof sources and explicit trust/image inputs.
It is rehashed after owned process cleanup; mutation or a changed layer census
refuses the outcome. Per-case profile, time-anchor/network config, provision Root,
canonical grants/bindings/Admission, rendered assembly, job and initramfs are
retained and hash-bound separately rather than forced equal across fifteen boots.
`node-ci-assets` obtains `Job.id` through the existing JobSpec conversion and
emits `moruna_plan::job_digest` as `engine_spec_digest` (`sha256:` identity digest),
separate from the whole document `job_sha256`. The runner checks the assigned run
against that receipt and compares every validated fixed resource field. A failed/missing case or cleanup cannot
produce a pass. Overall CI must require all fifteen same-input outcomes alongside
generic checks; generic engine assertions cannot simply be deleted. No fixture
source, successful host compilation or TCG boot establishes production KVM
ownership or cloud acceptance. These new fixture sources still require compilation
and execution by the integration owner.

For native PEX sampling, `run_plane` supplies the SAME actual Account used by
`Process::create_in` in startup slot `account`. Metadata authority is READ plus the
TRANSFER right required for that one startup move; WRITE/DUPLICATE are absent.
The existing grant path is unchanged, and no syscall/ABI/account issuer is added.
Native observer fixtures check actual account identity/usage and refuse allocation
or duplication through this narrowed capability; actual child PEX execution must
prove the end-to-end startup path.


### Per-task readonly asset capability (imagefs3 / Linux3)

A granted runtime receives `filesystem`, an actual unconnected imagefs3 readonly
root-view channel. It is reserved against caller shadowing. The runtime connects
once using the generated imagefs handshake and exports a fresh directory view for
each task's requested source directory. `LinuxPlane.assets` carries that actual
channel, not a path, connection-local file number or copied directory. Linux3 and
imagefs3 reject older service majors at hello; there is no alternate old decoder.

An imagefs view pins the resolved image directory or scratch directory inode and
shares the original Scratch store, including its actual charged Account, until
all source/view clients close. Source disconnect does not invalidate the view.
Every mutating view operation and write-open is refused. Absolute symbolic links
restart at the capability root and parent traversal above that root is refused.
The endpoint reserves one of the existing dispatcher's 64 wait slots before
publication; connection floods cannot consume its reserved slot, and failures
return that capacity through the existing reservation destructor.

The Linux actor holds separate root and asset clients and local file-number
routing; `/assets` is a mount of that real view. Root-image symbolic links can
enter the mount, but links and parent traversal originating inside it cannot
escape to the source's ambient tree. Its original private `/tmp` is unchanged.
Asset writable mappings are refused. Readonly mappings of scratch-backed assets
use the existing bounded file-to-private-memory mapping path charged to the child;
this is not an end-to-end zero-copy claim. Immutable image mappings retain the
imagefs read-only Memory rights. The filesystem server owns path resolution;
there is no host-directory alias, new kernel syscall, or authority inferred from
a pathname. Fixtures exercise real view updates, mutation/escape refusals and
continued reads after the original source disconnect; host reader tests separately
exercise rooted links and pinned unlinked inode lifetime. New source awaits root's
combined compilation and actual native boot execution.


### Bounded native storage/network acceptance fixture

`kernel/node/tools/io-proof.py` uses explicit prebuilt `proof.py` inputs and the
existing KE2 `net-proof.py` controlled local TLS peer/config (single-card `net0`
fixture option). It assembles the normal node supervisor, designated authority,
network and native storage process, replacing only the test initial ELF with
`node-storage-proof` and adding the existing one-shot `netstack-test` as an
auxiliary initramfs test actor. An actual image-builder link reaches the network
listener through the supervisor's existing grant seam; it is not a resident critical
service and its normal exit does not weaken production guardian semantics. Production
`standalone.image`, config codecs and protocols remain unchanged. The explicit
Rust asset-emitter test option authorizes READ|WRITE of exactly one controlled
`moruna-ci/jobs/native-io-proof` object; another object is denied, and no prefix,
List, Publish or application proof is admitted. Every boot has its own canonical
run/Admission and freshly carved actual Account; the object identity is the same.

The orchestrator provisions one newly owned 64MiB SingleNodeHome NVMe image ONCE.
Boot one must acknowledge Written, deny the ungranted object, close/rebind the
actual session and read the same acknowledged immutable version/bytes. After real
owned QMP cleanup/reap, boot two uses the SAME image inode, exact after/before byte
hash and unchanged Root asset, without format. Its existing-key conflict and
pinned read establish native journal recovery; the read-only second pass must not
change device bytes. Actual kernel-reported zero exits of both the storage job
(`engine`) and the network fixture are required before QMP cleanup. Each boot also requires the genuine KE2 DHCP, loopback DNS,
verified TLS byte/EOF and conformance success markers. Real failures/timeouts and
actual cleanup statuses remain recorded; source compilation is not an acceptance
claim. This two-boot IO fixture does not replace any of the fifteen canonical
engine/task boots or production KVM ownership evidence.

Invocation after the integration owner has built the exact binaries is
`python3 kernel/node/tools/io-proof.py --out NEW_ABSOLUTE_DIR --net-proof
ABSOLUTE_NET_PROOF_PY --cpus 1 --seconds 600 --` followed by the same explicit
immutable tool/image arguments as `proof.py`, omitting its owned out/case/cpus/
seconds options. Existing OpenSSL in the owned proof environment is used only by
the KE2 fixture; no installation, public network or production credential is used.
