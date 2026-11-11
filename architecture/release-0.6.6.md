# Moruna 0.6.6 native execution and isolation

Status is planned. Brackly moved this scope from 0.6.5 on November 11, 2026.
Implementation, compilation and runtime evidence are tracked individually in
`BOARD.md`. The release gate requires evidence through the published entry points.

## Goal

A host supplies a canonical MJS document, checked native boot assets and explicit
run authority. Moruna executes the job on its native microkernel with bounded
resources, task-scoped services and durable recovery. The embedding platform can
attach data and services while each run retains its own authority and lifetime.

The Linux compute host runs QEMU/KVM and the host process jails. The guest runs
the Moruna microkernel, its user-process services and the job engine. Linux task
programs use the existing Linux personality. The native descriptor carries the
launch contract. Guest authority uses capability handles.

## Existing implementation

These are source capabilities. Their release acceptance remains subject to the
relevant host, native-boot and failure-path tests.

| Area | Current source | Work for 0.6.6 |
| --- | --- | --- |
| Native boot | `crates/moruna-vmm/src/cli.rs` and `microkernel.rs` inspect an explicit descriptor and supervise QEMU/backend lifetimes | Extend the native host contract for the capabilities below |
| Host isolation | `crates/moruna-vmm/src/jail/` implements namespaces, restricted mounts, dropped capabilities, syscall filters and delegated cgroups | Apply equivalent run ownership to host-side service processes |
| Disks | The descriptor supports digest-bound read-only layers and explicitly sized, exclusively locked writable NVMe files | Complete run-authorized attachment, access mode, recovery and host integration |
| Resources | `Resources::admit` checks a job against fixed physical and engine account budgets | Add acknowledged resource changes across the host, guest and runtime |
| Guest services | SDD 33 and `kernel/node/` define capability-based admission, carved accounts, service grants and owned child lifetimes | Bind additional per-run authority and prove teardown under races |
| Network and storage | SDDs 31 and 32 define KE2 networking and exact-action storage authority. Runtime host ports already carry named calls over the run connection | Complete policy-bound access through those existing service boundaries |
| Image identity | OCI records, sealed layer files and native boot descriptors carry content identities | Complete reusable content caches and the published tool bundle |

The jail maps the launching service's host identity into each isolated user
namespace. Its isolation evidence must cover that actual model. The native
network service and card drivers remain user processes with explicit capabilities.

## Execution and authority

1. The host reads and validates MJS with Moruna's existing reader. The job identity
   is SHA-256 of the canonical MJS bytes. Native boot-asset identities are checked
   independently and bound to the launch.
2. The host supplies explicit disk, account, service and policy grants for that
   execution. Grants bind to the job digest, a fresh launch identity, permitted
   operations, deadlines and the intended task/service boundary.
3. Moruna admits the job inside the actual provisioned guest resources. The
   native supervisor and admission service move the required handles into the
   engine and tasks using the current ownership rules.
4. Task `needs` resolve through existing ports or native service capabilities.
   Every privileged operation is checked at the service holding the actual I/O
   authority. Expiry, cancellation and loss of the run's authority close that path.
5. Completion records the real task and process outcomes. Cancellation and
   shutdown retain ownership until process termination and outstanding operations
   have been accounted for.

MJS continues to describe tasks, edges, declarations, image identities and named
needs. Secret values, deployment credentials and live connection authority enter
through runtime bindings. The boot descriptor describes native assets and
resources. The embedding platform supplies its own tenant and policy decisions.

## Run-owned host services

Each run owns the host processes serving its vsock/port requests, their bounded
queues, sockets, file handles, cancellation and cgroup allocation. Their filesystem
view contains only their executable dependencies and explicitly granted run assets.
Mount access modes and socket access are checked before executing a helper.

The host may prepare a service directory with explicit owner/group access. The
launcher validates directory identity, permissions, symlinks and existing contents
before binding sockets. Authority to access a directory and authority to perform
a run operation are checked at their respective boundaries. Reusing a pathname
requires fresh launch-bound authority.

Helper creation, partial startup, control-connection loss and termination of a
critical service use the existing owned-process and supervisor teardown model.
Shutdown stops creators, observes their termination, collects descendant escrow
handles, then observes descendant termination. An expired teardown deadline is
reported as incomplete cleanup.

## Policy-bound networking

Networking follows the current execution profile. Hosted ports use the existing
run connection and policy-bound host service. Native networking uses KE2's
user-process service and explicitly granted network authority. The task receives
only the named port or scoped service capability. The native descriptor's
`user_network` setting selects virtual hardware. The current jailed network
namespace still limits its reachability.

0.6.6 must bind external reachability to run policy at the actual dial/service
boundary. The existing restricted hosted-VM profile remains restricted. Native
network-driver and KE2 work retains its capability-based service architecture.
Each enabled production network path must have its own end-to-end acceptance.

The policy carries allowed names, hosts, ports, protocols and explicit private
destination authority. Its enforcement must cover the following properties.

- DNS resolution and every selected IPv4/IPv6 address are checked before connect.
  Connections pin approved addresses. Re-resolution and redirects require a new
  policy check. IPv4-mapped IPv6 and private/link-local destinations use the same
  address rules.
- TCP reaches only a permitted endpoint. TLS and HTTPS bind the declared server
  name, certificate validation and HTTPS authority to that endpoint. ECH,
  conflicting names and attempts to change authority on a retained connection
  are refused by the relevant protocol handler.
- Trusted roots and time use the existing explicit service inputs. A failed TLS
  check ends the operation. Task-level UDP, raw-packet and inbound authority
  require explicit policy. The existing trusted network infrastructure services
  retain their separately scoped DNS/DHCP/device capabilities.
- Flow records contain run identity, policy decision, destination, timing and
  bounded byte counters. Request bodies, credentials and secret values remain
  outside operational flow records.

The macOS restricted-process path must enforce the same granted-port boundary
through its platform adapter. Linux/KVM VM acceptance and macOS process acceptance
are recorded separately with their supported capabilities.

## Scoped secrets

The embedder supplies the existing resolver with only the secret authority needed
by the run. A service that performs an authenticated operation can retain the
credential and expose the scoped operation through a port. A task that explicitly
requires secret bytes receives them through its private runtime channel after
the run/task/deadline checks.

Secret values stay outside MJS, OCI labels, image layers, launch arguments,
ambient host environments and shared operational diagnostics. Secret-bearing task
execution needs a log policy that suppresses raw task output and free-form
exceptions from shared logs. Structured lifecycle/error records carry bounded
codes and identifiers. Tests must deliberately print and throw a secret canary.
Authorized job data outputs follow the embedder's data policy.

Secret-bearing buffers and temporary capabilities have explicit ownership and
bounded lifetime. Cancellation closes service access and releases the associated
resources. Any zeroization claim must identify the buffers the implementation
owns and the verification that covers their reuse.

## Data and resources

Mutable data disks retain the existing explicit path, capacity, serial identity
and lifetime lock. The host grants the required read/write authority for that run.
The native storage service validates its actual superblock, role and publication
state through SDD 32. Read-only image layers retain their immutable identities.
Restart acceptance reopens the same authorized data and checkpoint generation.

Resource control distinguishes physical VM capacity, guest service accounts,
the engine account, scratch and the runtime budget. An accepted change must reach
every affected layer before its acknowledgement is emitted. Growth establishes
capacity before increasing admission. Shrinkage first establishes a safe usage
bound and accounts for in-flight work and retained pages. Unsafe or unsupported
requests return a typed refusal with the actual supported limits.

Live VM resizing requires guest online/offline support and host accounting proof.
Changing a cgroup or a document field alone supplies only that layer's evidence.
The resource controller must observe the committed limits during active work.
Conflicting changes, retries, cancellation and interrupted changes must converge
on one reported resource state.

## Output, identity and recovery

An image task's declared `gives` is checked at the runtime boundary for every
produced batch, including Source output and Function completion output. The shared
type grammar and existing Arrow validation supply the comparison rules. A refusal
names the task and mismatched field before downstream publication. Supported
undeclared schemas retain their documented behavior.

Content-addressed caches cover immutable native assets, tools, dependency inputs
and image layers. Digest verification establishes trust on admission. Reuse
requires continuing immutable-object identity. Mutable data has its own version
and publication authority. Concurrent population, corruption and interrupted
population must yield a verified entry or an explicit failure. Existing layer
page-integrity checks remain part of use-time validation.

The published launcher bundle binds the platform's monitor, QEMU/backend tools,
native boot assets, manifests and checksums. The same published inputs must boot
and run a job through the public CLI. Job digest, launch identity, policy and
checkpoint identity must remain distinguishable in lifecycle records.

Kill/resume acceptance runs under CPU/memory pressure, concurrent spill and
checkpoint activity, and service interruption. It verifies output equivalence,
publication ordering and the original failure cause. Teardown checks creators
and descendants, including children created during shutdown.

## Acceptance and delivery order

| Order | Gate | Required evidence |
| --- | --- | --- |
| 1 | Current-code audit | Name each reused API, implementation gap, protocol change and existing proof in the board |
| 2 | Native launch and ownership | Published descriptor/CLI path, real delegated limits, private services, failed startup and complete teardown |
| 3 | Disk and identity binding | Correct data reopen, access modes, exclusive writer, wrong-run/digest refusal and interrupted attachment cleanup |
| 4 | Service authority | Cross-run, expired, revoked and cancelled calls refused before I/O. Secret canaries absent from shared logs and metadata |
| 5 | Network policy | Controlled DNS/TCP/TLS peers, IPv4/IPv6, rebinding, name/certificate failures, redirects, connection reuse and inbound refusal |
| 6 | Resource changes | Live workload during accepted growth/shrink, unsafe requests, concurrent retries and failure during each transition |
| 7 | Output and caches | Wrong first/later/final schemas, valid declared/undeclared jobs, corrupted cache entries and concurrent publication |
| 8 | Recovery and release | Loaded kill/resume, shutdown races, installed published tools and the required Rust/Python/native CI checks |

The native descriptor, MJS, service protocols and existing storage authority each
retain one canonical codec. Changes follow their existing version/refusal rules.
Implementation extends those owners. Acceptance records the precise execution
profile, platform and resource limits exercised.

Release 0.6.6 requires completed evidence for its declared supported paths.
Source completion, host tests, native boot and platform integration each retain
their individual status. Griot performs its own policy translation and adapter
acceptance against the upstream native interfaces.
