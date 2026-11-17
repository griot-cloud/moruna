# Moruna OS: system design

The detailed design under `moruna-os.md` (the OS design). It follows the critical path, component by
component, and fixes every decision an agent would otherwise make. Written by the PM, 2026-11-17.

- **Final** sections are ready to build. **Draft** sections state the decisions so far and what is
  still open; no work is issued from them.
- The last section shows how each part traces to the OS design.

## How work is issued

- An agent gets two documents: this one, and its work item (`architecture/work/<id>-<name>.md`).
- The work item names the sections it implements, exactly what to build, the checks that prove it,
  and what is out of scope.
- Where the two documents leave a choice open, the agent stops and asks the PM. It does not choose.
- An item is done when its checks pass and the PM has run them again. The PM merges.
- Every item: a branch from `feature/arrow-core-0.7`, local commits, nothing pushed, tests with the
  code, no speed work.

## 0. A base that boots (Final)

The branch head carries a new Linux layer that was merged without being booted. Nothing is built on
a tree that has not booted.

| Item | Build | Success |
|---|---|---|
| 0.1 | Boot the branch head as CI does. If the Linux-layer swap breaks it, move the swap to its own branch. | Done, 2026-11-17: see below. |

**The base, from item 0.1:**
- The head failed every Linux-layer check: the new `linux` server duplicates a process resource
  handed to it without the duplicate right.
- Stream B2's merge (`803e984c`) and stream A's wake queue (`16cf745c`) are reverted
  (`a095f5d3`, `078e8087`). With them out, the kernel is the tag's, which booted under KVM.
- Each returns as its own work item, proven under KVM: the Linux layer before section 5 needs
  it, stream A's wake queue with the speed work of item 3.9.
- Until then the engine and Linux programs run on `kernel/linux`, as at the tag.

**Judging boots under the emulator.** QEMU's emulator on this Mac can't hold the timing checks.
At the tag they fail on 2 CPUs (DS-A share) and 4 CPUs (DS-P steady), and they pass under KVM.
Under the emulator, an item's boot check is `check-boot.sh` with those timing lines removed from
`expect`, and the report says so. Under KVM, every line counts.

## 1. The node on OpenStack (Final)

Implements the OS design, section 1, OpenStack column.

### 1A. The image

- **One disk image for every cloud,** built by `kernel/node/tools/package.py` from a profile. The
  `cloud` profile grows by step:

  | Step | Processes in the image |
  |---|---|
  | 1 | `node-supervisor`, `devices`, `netstack`, `platform`, `api`; drivers `virtio-blk`, `virtio-net`, `nvme`, `gve`. Item 1.1 boots the supervisor, `devices` and the drivers alone; `netstack` joins with `api` in 1.3, because the network server refuses a config with no listener grant |
  | 2 | The same; `api` gains TLS and its operations |
  | 3 | Adds `storage`, `imagefs`, `linux`, `node-admission`, `engine` |

- **Layout:** a GPT disk with one partition, the EFI system partition (FAT32). It holds
  `EFI/BOOT/BOOTX64.EFI` (GRUB), `boot/moruna-kernel.elf` and `boot/initramfs.cpio`. A second
  partition, `moruna-system`, is reserved for step 3's function images.
- **Boot chain:** UEFI firmware, GRUB with no menu and no wait, the core by multiboot2 with the
  initramfs as its module, then `node-supervisor`. This is what `package.py` builds today.
- **UEFI only.** Secure Boot comes later. A BIOS boot is added only if a cloud cannot do UEFI.
- **The disk is never written while the node runs.**
- **Formats** are made from the one raw image: raw and qcow2 for OpenStack, a fixed-size VHD
  rounded up to a whole MiB for Azure.
- **No vsock, no host socket, no direct kernel load.** Those belong to the Linux-node launcher.

### 1B. Boot and console

- **Console:** the first serial port (COM1, 115200 8N1). Clouds show it as the machine's console
  log. The node reads nothing from it: there is no login and no prompt.
- **Boot lines,** one per stage, exactly:

  ```
  moruna: core <version> cpus=<n> memory=<bytes>
  moruna: services started
  moruna: network wan <address>/<prefix> gateway <address>
  moruna: cloud <openstack|azure|none> instance <id>
  moruna: tls sha256=<fingerprint>
  moruna: ready api=<address>:443 boot_ms=<n>
  ```

- **`boot_ms`** counts from the core's first instruction to ready. Firmware and GRUB time is
  measured from outside, at bring-up, as create-to-ready.
- **Limits:** services started within 5 s; an address within 30 s; instance data within 90 s.
- **Failure:** one line, `moruna: fatal <stage>: <reason>`, then the machine powers off. A node
  never hangs silently, never restarts itself in a loop, and never offers a prompt. A service that
  dies stops the node (SDD 33), except `api`, which is restarted (2B).
- **Power-off is ACPI S5 on every machine:** SLP_TYPa from the DSDT's `\_S5` object (found by
  scanning the DSDT for `_S5_` and reading the package that follows, as small kernels do),
  written with SLP_EN to the FADT's PM1a control register. OSTD today powers off only through
  QEMU's test exit device, which no cloud has. That device stays for CI's exit codes alone.
- **No wait without a deadline in boot code.** OSTD's PIT calibration waits forever on a machine
  with no PIT. Every such wait gets a deadline and, when it passes, a `fatal` line.

### 1C. Network

- **One interface, `wan`,** on the first network card, set by DHCP: address, mask, gateway, DNS.
  IPv4 only for now. The network server's config is:

  ```
  moruna-netstack 1
  interface wan card=net0 dhcp
  ```

- **One port listens: TCP 443,** owned by `api`. Nothing else accepts connections.
- **Outbound is denied unless granted.** In steps 1 and 2 only `platform` has grants:
  `169.254.169.254` port 80 (instance data), and on Azure `168.63.129.16` port 80 (the "ready"
  report).
- **In step 1, `api` accepts a connection on 443 and closes it.** That proves the address, the
  route and the cloud's firewall rule. Step 2 gives it TLS and operations.

### 1D. `platform` and node config

One process, `platform`. In order, it:

1. **Finds the cloud** by asking for instance data over HTTP at `169.254.169.254`, in this order:
   - Azure: `GET /metadata/instance?api-version=2021-02-01` with the header `Metadata: true`;
     user-data is its `compute.userData`, base64-encoded.
   - OpenStack: `GET /openstack/latest/meta_data.json`, then `/openstack/latest/user_data`.

   A cloud is named only by a `200` whose body parses as that cloud's document; any other answer
   (Azure answers `400` or `404` to OpenStack's paths) moves to the next. A refused or timed-out
   connection is retried every second, at most 5 requests a second; a `410` is retried. Nothing by
   90 s is fatal. No config drive: it would need a filesystem reader.
2. **Sets the wall clock** from the `Date` header of that answer. The node has no other clock
   source that works the same on every cloud. The monotonic clock carries it forward.
3. **Validates node config,** which is the instance's user-data, version 1:

   ```json
   {
     "moruna": 1,
     "api": {
       "token_keys": ["<Ed25519 public key, base64>"],
       "tls": { "certificate": "<PEM chain>", "key": "<PEM>" }
     },
     "functions": {
       "keys": ["<Ed25519 public key, base64>"],
       "cache_bytes": 1073741824
     }
   }
   ```

   - `token_keys` is required, 1 to 8 keys. `tls` is optional.
   - `functions` is optional: `keys` holds 0 to 8 keys that may sign function images (3E), and
     `cache_bytes` defaults to 1 GiB.
   - Unknown fields are refused. A missing or invalid config is fatal.
4. **Hands `api` its config** (token keys, TLS identity, wall clock, instance ID) over a channel
   the supervisor links.
5. **Prints** the `cloud` and `ready` lines.

- **By default node config holds no secrets.** Without `tls`, `api` makes an ECDSA P-256 key and a
  self-signed certificate at each boot, and the console shows its fingerprint for clients to pin.
  An operator who wants a certificate from their own authority supplies `tls`, and accepts that the
  key then sits in instance data.
- **Cloud `none`** is for development only: when no instance data answers and the image was built
  with a development config file, that file is the node config. A release image has no such file,
  so there it is fatal.
- **Tests** serve fake instance data to QEMU at the same address.

### 1E. Bring-up on Servercore

- Upload the image with the properties `hw_firmware_type=uefi`, `hw_machine_type=q35`,
  `hw_disk_bus=virtio` and `hw_vif_model=virtio`; create one machine with node config as
  user-data and a firewall rule for 443 only; check; delete.
- **q35 matters:** the core finds PCI through ACPI's MCFG table, which only q35 has. If Servercore
  ignores `hw_machine_type`, item 1.6 reports it, and PCI configuration through I/O ports is added
  as its own item.
- **Success is the OS design's:** the machine gets an address, answers on 443, and shows as
  running, with nobody logging in. Create-to-ready time and `boot_ms` are recorded.
- Creating cloud resources needs the PM's go-ahead each time.

### Work items

| Item | Build | Success | Needs |
|---|---|---|---|
| 1.1 | The `cloud` profile for step 1; boot its image in QEMU under UEFI firmware with one virtio disk and one virtio network card (1A) | A script boots it to `services started` on 1 and 2 CPUs | 0.1 |
| 1.2 | The boot lines, the limits, deadlines on boot waits, and ACPI S5 power-off on a fatal error (1B) | Each line appears in order; each limit, when forced, gives its `fatal` line and the machine powers off by S5 | 1.1 |
| 1.3 | DHCP on `wan`; `api` owning 443 and closing each connection; the outbound grant (1C) | A connection from the host reaches 443; one to any other port is refused; `api` cannot connect out | 1.1 |
| 1.4 | `platform`: instance data, wall clock, node config v1, the hand-over to `api` (1D) | Booted against fake OpenStack data: the `cloud` and `ready` lines appear; each invalid config is refused with its reason | 1.2, 1.3 |
| 1.5 | The qcow2 and VHD formats and the upload recipe, `kernel/node/OPENSTACK.md` (1A, 1E) | Each format boots in QEMU; the recipe's commands are complete | 1.1 |
| 1.6 | Bring-up on Servercore (1E) | 1E's success, on Servercore | 1.4, 1.5, credentials |

## 1F. The node on Azure (Final)

Implements the OS design, section 1, Azure column. Made final from item 1.7a's findings
(`architecture/work/1.7-azure-findings.md`). The booted half, 1.7b, may still change details here;
if it does, this section changes first.

### The machine

- **`Standard_D4ls_v5`, Generation 2, security type `Standard`.** Azure defaults to Trusted Launch,
  whose Secure Boot refuses our unsigned GRUB. Signing comes later.
- **Accelerated networking cannot be turned off on this size,** so Azure offers a virtual function
  (ConnectX or MANA) beside the synthetic NIC. The node uses the synthetic NIC (netvsc) only. It
  ignores the virtual-PCI offer and never asks Azure to move traffic to the virtual function.
  Networking still works, at the synthetic NIC's speed. MANA comes later.
- **The image** is 1A's fixed-size VHD.

### The core on Hyper-V

- **Finding Hyper-V:** CPUID leaf `0x40000000` returns `Hv#1`.
- **Clock rates:** OSTD tries, in order, kvmclock, CPUID leaf `0x15`, then Hyper-V's MSRs
  `0x40000022` (TSC) and `0x40000023` (local APIC), and the PIT last, with a deadline (1B).
- **Timer:** the local APIC timer at the rate Hyper-V gives. Nothing from the old PC (PIT, PIC,
  HPET, CMOS clock) is assumed.
- **No PCI bus:** Generation 2 has no ACPI MCFG. The core boots without `root.ecam`, and `devices`
  runs with no PCI functions. Today the core refuses to boot without MCFG (`kernel/core/src/boot.rs`).
- **Guest OS ID:** written before the hypercall page, in the TLFS's open-source encoding. Item 1.8
  proposes the exact value with its TLFS citation; the PM approves it.
- **Power-off:** ACPI S5 (1B).

### VMBus, split by the microkernel rule

- **In the core, one mechanism,** granted to the `vmbus` process only:
  - the hypercall page, and the hypercalls `HvPostMessage` and `HvSignalEvent`;
  - each CPU's SynIC message and event pages, mapped read-only to `vmbus`;
  - SINT 2's interrupt delivered to `vmbus`, and its end-of-message write.

  Added kernel-mode lines are counted in `data-os.md` section 0.
- **In the `vmbus` process:** the version handshake, offers, opening and closing channels, GPADLs
  (guest memory shared with the host), and each channel's ring pair mapped to its driver. Every
  channel targets CPU 0 at first.
- **`storvsc` and `netvsc`** are driver processes that serve today's block and network driver
  protocols, so nothing above them changes. netvsc speaks NVSP and RNDIS.
- **Reused from Microsoft's OpenVMM** (MIT, `no_std`, at e7e7df77): `hvdef`, `x86defs`,
  `vmbus_core`, `vmbus_ring` and `netvsp_protocol` as written, and `storvsp_protocol` with a small
  fork. They are vendored under `kernel/vendor/openvmm` with the licence, the commit and file hashes,
  as OSTD is. OpenVMM's std and async drivers are read as references only.

### `platform` on Azure

- Instance data as 1D says.
- **The "ready" report,** once node config is valid: `GET http://168.63.129.16/machine?comp=goalstate`,
  then `POST /machine?comp=health` with the `Health` XML document, both with the header
  `x-ms-version: 2012-11-30`. Without it, Azure reboots the machine.

### Testing

- **Azure is the proof,** at each item's end.
- **Between proofs, OpenVMM on a Linux machine with KVM** stands in for Hyper-V: it boots UEFI
  guests and has storvsp and netvsp. The Linux machine is an Azure VM with nested virtualization.
  Creating it, and every Azure resource, needs Brackly's go-ahead.

### Work items

Sizes are 1.7a's estimates.

| Item | Build | Success | Size | Needs |
|---|---|---|---|---|
| 1.7a | Findings from reading | Done: `architecture/work/1.7-azure-findings.md` | | |
| 1.7b | Findings from booting: today's image on Azure, its serial console read | Where the boot stops, and why | small | 1.1, Azure go-ahead |
| 1.8 | The core on Hyper-V: finding it, clock rates, deadlines, no MCFG, guest OS ID | The core reaches `services started` on Azure's console | medium | 1.7b |
| 1.9 | VMBus: the core mechanism, the vendored crates, the `vmbus` process | A booted test lists the devices Azure offers; added kernel-mode lines are counted | large | 1.8 |
| 1.10 | `storvsc` | The node reads its Azure disk; the block driver suite passes | medium | 1.9 |
| 1.11 | `netvsc` | DHCP and a connection to 443 on Azure; the network driver suite passes | large | 1.9 |
| 1.12 | `platform` for Azure, with the "ready" report | Azure shows provisioning succeeded | small | 1.4, 1.11 |
| 1.13 | Bring-up on Azure | 1E's success, on Azure | small | 1.10, 1.12 |

## 2. The API (Final)

Implements the OS design, section 2.

### 2A. Operations and types

- **One crate, `moruna-api`,** holds every request, response and error as Rust types. The OpenAPI
  document and the MCP tool list are generated from it and checked in; a test fails if they drift.
- **Paths start `/v1`.** Bodies are JSON. Unknown request fields are refused.
- **Step 2's operations:**

  | Operation | Scope | Answer |
  |---|---|---|
  | `GET /v1/health` | none | `{"status":"ok"}`. The only call without a token. |
  | `GET /v1/node` | observe | The node document below |
  | `GET /v1/node/audit` | admin | The most recent calls, newest first |

  ```json
  {
    "version": "0.7.0", "instance": "<id>", "cloud": "openstack",
    "cores": 4, "memory_bytes": 8589934592,
    "executors": [{ "kind": "cpu", "slots": 4 }],
    "boot_ms": 2140, "time": 1794931200, "health": "ok"
  }
  ```

- **Errors** are `{"error":{"code":"<code>","message":"<text>"}}` with these codes: `invalid` (400),
  `unauthorized` (401), `forbidden` (403), `not_found` (404), `conflict` (409), `busy` (503),
  `internal` (500).
- Step 3 adds the `functions` and `jobs` operations of the OS design to the same crate.

### 2B. The `api` server

- **A process on `moruna-svc`,** with a 64 MiB account, on no worker's core.
- **If `api` dies, the node keeps running** (the OS design's step 2) and the supervisor starts it
  again, at most 5 times in 10 minutes; a sixth death is fatal (1B). A restarted `api` needs fresh
  channels, so:
  - **its listener:** the supervisor holds a network-server client whose grant has only the `grant`
    right, limited to listening on 443. At each start it mints `api`'s listener channel from it
    (`Client::grant`).
  - **its hand-over:** the supervisor makes a fresh `platform`-`api` channel pair at each start and
    sends `platform` its end over a supervisor-to-`platform` control channel. `platform` keeps the
    validated config and sends the hand-over again.
  - **its identity:** a restarted `api` makes a new key and certificate when node config has no
    `tls`, and prints the new fingerprint line. The audit ring starts empty.
- **Two crates.** The logic (HTTP, TLS, routing, tokens, audit, MCP) is the library
  `moruna-api-server` (`kernel/api/server`, no std, with alloc). It talks to a byte-stream trait, so
  every behaviour is tested on the host. The program `moruna-node-api` (`kernel/node/api`, from item
  1.3) only wires that library to the network server's sockets and to `platform`'s hand-over.
- **The node's own certificate** (1D's default): an ECDSA P-256 key made at boot with `ring` (whose
  randomness comes from `moruna_svc::random`, as the network server's does), and a self-signed
  X.509 v3 certificate written by a minimal DER writer for one fixed template:
  - subject and issuer: `CN=<instance id>`; serial: 16 random bytes;
  - validity: from one hour before the wall clock to 397 days after it;
  - extensions: basicConstraints CA false, keyUsage digitalSignature, extendedKeyUsage serverAuth.

  The console line `moruna: tls sha256=<fingerprint>` is the SHA-256 of the certificate's DER, in
  lowercase hex, printed by `api` before `platform` prints `ready`.
- **TLS ends inside `api`** (rustls), so the node's key stays in the one process that uses it. TLS
  1.3 only.
- **HTTP/1.1 only,** with keep-alive and Server-Sent Events. No HTTP/2, compression, cookies or
  cross-origin headers.
- **Limits:** 16 KiB of headers, 1 MiB of body, 64 open connections, 60 s idle. Over a limit, the
  answer is `invalid` or `busy`, never a crash.
- **New dependencies:** `httparse` and `serde_json`, both without std. Signatures use `ring`,
  already in the tree.

### 2C. Tokens and the audit record

- **A token is a JWT signed with Ed25519,** sent as `Authorization: Bearer <token>`. Its claims:

  ```json
  { "aud": "<instance id>", "exp": 1794934800, "scope": ["observe"] }
  ```

- **A call is allowed when** the signature verifies under one of the node's `token_keys`, `aud` is
  this node's instance ID, `exp` is in the future and at most one hour ahead, and `scope` covers
  the operation. `admin` covers `submit`, and `submit` covers `observe`.
- **Every call leaves an audit record:** time, key, scope, operation, status. `api` keeps the last
  10,000 in memory. Nothing is written to disk or to the console.

### 2D. MCP

- **`POST /mcp`** on the same port, with the same tokens: MCP's Streamable HTTP, stateless.
- **One tool per operation,** generated from `moruna-api`: `moruna_health`, `moruna_node`,
  `moruna_node_audit`. A tool's input and output are the operation's request and response.

### 2E. Clients

- **The CLI:** `moruna node --url <https://…> --token <token> [--pin <fingerprint>]`, a subcommand of
  the existing `moruna` binary (`crates/moruna-py/src/bin/moruna.rs`).
- **The Rust client:** crate `moruna-api-client` (`crates/moruna-api-client`), blocking, over the
  root workspace's `reqwest` with rustls. It uses `moruna-api`'s types and operations table directly,
  so it is generated from the same source as the OpenAPI document.
- **The Python client:** module `moruna.api`, generated from `kernel/api/openapi.json` by an in-repo
  script (`tools/api/gen_python.py`), standard library only: dataclasses and `urllib`.
- **Pinning, in all three:** with `--pin`, the server's certificate is accepted when its DER's
  SHA-256 equals the pin, and nothing else is checked. Without it, normal public verification.

### Work items

| Item | Build | Success | Needs |
|---|---|---|---|
| 2.1 | `moruna-api`: step 2's types, the generated OpenAPI document and MCP tool list (2A) | Host tests pass; the drift test fails when a type changes without regenerating | none |
| 2.2 | The server library `moruna-api-server`: TLS, HTTP/1.1, the three operations, the limits (2B) | Host tests through a real TLS client; each limit holds | 2.1 |
| 2.2b | `moruna-node-api` serves through the library: identity, node facts, tokens from node config | Booted in QEMU: `health`, `node` and `audit` answer over HTTPS; the `tls` line appears | 1.4, 2.3 |
| 2.2c | Restarting `api` (2B) | Booted: `api` killed three times, the node keeps answering after each; a sixth death in 10 minutes powers off | 2.2b |
| 2.3 | Tokens and audit (2C), in the library | A bad signature, a wrong `aud`, an expired or over-long `exp`, and a missing scope are each refused; each call appears in `audit` | 2.2 |
| 2.4 | MCP (2D), in the library | An MCP client lists the three tools and calls `moruna_node`, on the host and then on a booted node | 2.3 |
| 2.5 | The CLI and the generated clients (2E) | All three read `node` from a booted node | 2.1, 2.2 |
| 2.6 | Proof on both clouds | From outside the cloud, `node` answers over HTTPS and over MCP on Servercore and on Azure; a bad token is refused; killing `api` leaves the node answering | 1.6, 1.13, 2.2c, 2.4 |

## 3. Wiring (Final)

Implements the OS design, section 3. It reuses what exists and adds the node's side:
- the job document (SDD 22: version 1 JSON, version 2 FlatBuffers `MJB2`, `moruna-job`'s `check`);
- the host protocol (`moruna-hosted-engine.md` 4.3: `spec`, `cancel`, `hello`, `heartbeat`, event
  records, `report`, `exit`);
- `node-admission`, which starts one engine process per job;
- the engine's object-store connector seam (`moruna-hosted-engine.md` 4.6).

The engine's own design (sources, sinks, scheduler, memory) stands as SDDs 2 to 11 and 30 say.

### 3A. A job's path

```
client ─HTTPS─► api ──(job, cancel)──► node-admission ──starts──► engine ─┬─ volumes (disk://)
           ◄─────── events, report ◄── the host protocol, on a channel ───┘ └─ network (object storage)
```

- **One job at a time.** `submit` while a job runs answers `busy`. Jobs side by side, kept apart by
  the core's accounts, come later.
- **A fresh engine per job,** started by `node-admission` with the job's budget as its account,
  then ended. Nothing is kept between jobs except the function cache (3E).
- **`api` is the engine's host.** The host protocol runs unchanged over a channel between `api` and
  the engine, instead of a socket: `api` sends `spec` and `cancel`, and reads `hello`, `heartbeat`,
  event records, `report` and `exit`.
- **The budget** is the job document's `budget.memory_bytes`, at most the node's memory less 512 MiB
  kept for the node's services. A larger one is refused at `validate`.

### 3B. The job operations

Added to `moruna-api` (2A), so the OpenAPI document, the MCP tools and the clients grow with them:

| Operation | Scope | Body or answer |
|---|---|---|
| `POST /v1/jobs/validate` | submit | Body: a job (below). Answer: `{"ok":true}` or `{"ok":false,"refusals":[…]}`, `moruna-job`'s refusals with their fields |
| `POST /v1/jobs` | submit | Body: a job. Answer `201` with `{"id":"<run id>"}`; `busy` while a job runs; `invalid` with the refusals |
| `GET /v1/jobs` | observe | The last 100 jobs, newest first: id, state, times |
| `GET /v1/jobs/{id}` | observe | State (`starting`, `running`, `completed`, `failed`, `cancelled`), times, the latest heartbeat's fields, exit code |
| `POST /v1/jobs/{id}/cancel` | submit | Sends `cancel`; `conflict` if the job has ended |
| `GET /v1/jobs/{id}/events` | observe | Server-Sent Events: each event record as its JSON line (`moruna_event`'s rendering), heartbeats, log records, then `report` and `exit`. `?from=<seq>` replays from the node's buffer of the last 100,000 records |
| `GET /v1/jobs/{id}/report` | observe | The `RunReport` JSON, once the job has ended |

- **A job is** `{"v1": <job document version 1 as JSON>}` or `{"v2": "<base64 of the MJB2 FlatBuffer>"}`.
  Either is checked by `moruna-job`'s `check` before it is accepted.
- **The node fills in** `report.socket` (the channel), `staging` (none) and the run id when absent.
  A document that names a host socket, a `file://` path outside its volumes, or `resume` is refused
  in this release.
- **Secrets** (object-store credentials) are redacted in every answer, event, log and report. They
  live in the engine's memory for the job and in `api`'s memory until the engine has them.

### 3C. Volumes

- **A job reads and writes attached volumes,** named by the serial the cloud gives the disk: the
  Cinder volume ID on OpenStack, `lun-<n>` on Azure. In the job: `disk://<serial>/<path>`.
- `devices` reports every block device with its serial. `node-admission` grants the engine exactly
  the volumes its job names, read-only for sources and read-write for sinks. Today's `disk://`
  support (stream D) does the rest.
- **The boot disk is never a volume.**

### 3D. The network for jobs

- **The engine reaches object storage through the network server's own client,** not through the
  Linux layer's sockets. It is a new object-store connector, `moruna-net`, beside the `unix://` and
  `vsock://` ones of `moruna-hosted-engine.md` 4.6: it opens a connection, and the network server
  runs TLS on it (`start_tls`, with the node's `tls.roots`).
- **The network server's TLS needs a trusted time,** which it refuses to start without. It gets it
  from `platform` at run time, as the wall clock 1D sets: `platform` sends it once over a new link
  to the network server, and the network server then accepts `start_tls`. Never a build-time clock.
  Until step 3, the cloud profile gives the network server no `tls.roots` and no TLS.
- **Egress per job:** `node-admission` holds a network client with only the `grant` right. For each
  job it mints the engine's channel with `connect` and `resolve` limited to the host names in the
  job's object-store settings, on port 443. Nothing else is reachable from a job.
- **Object stores, in order:** the S3 API (any endpoint), then Azure Blob.
- **Throughput is measured** in item 3.4: one large object read through the connector, MB/s,
  against the same read with `curl` on the same cloud's Linux.

### 3E. Functions

- **Registration:** `POST /v1/functions` (admin) with `{"name", "address", "sha256", "signature"}`.
  - `address` is an `https://` URL or an object-store URL; `sha256` is the image file's.
  - `signature` is an Ed25519 signature over the 32 bytes of that SHA-256, by one of node config's
    `function_keys` (added to 1D's schema, 0 to 8 keys).
  - The node checks the signature and stores the entry. `GET /v1/functions` (observe) lists them.
    Nothing is fetched at registration.
- **At job start,** `node-admission` has the engine fetch each function the job names, through its
  job network (3D). The node refuses the job if a SHA-256 differs. Images are cached in memory, up
  to node config's `function_cache_bytes` (default 1 GiB), until the node stops.
- **A job names a function** by its registered name. An unregistered name is refused at `validate`.

### 3F. The full-year hang

Item 3.1 reproduces it, finds the cause, and proposes a fix for the PM's approval. Nothing in 3A to
3E waits on it except the proof, 3.8.

### Work items

| Item | Build | Success | Needs |
|---|---|---|---|
| 3.1 | The full-year hang (3F) | A host test reproduces it and passes after the fix; one month at 2 GiB and 19 GiB completes | none |
| 3.2 | The job operations, `api` as the engine's host over a channel, one job at a time (3A, 3B) | On a booted node: a `disk://`-free test job (a source that makes rows, a sink that counts) validates, runs, streams events, reports, and can be cancelled | 2.2b |
| 3.3 | Volumes by serial (3C) | On a booted node with two extra virtio disks: a taxi month read from one and written to the other, rows as 0.6.6 | 3.2 |
| 3.4 | The `moruna-net` connector and egress per job (3D) | A job reads an object over HTTPS through the network server; a host not in the job is refused; throughput recorded | 3.2 |
| 3.5 | S3, then Azure Blob, sources and sinks over 3.4 | A taxi month read from and written to S3-compatible storage, and to Blob | 3.4 |
| 3.6 | Functions: registration, fetch, verification, cache (3E) | An unsigned or altered image is refused; a registered native function runs in a job | 3.4 |
| 3.7 | Events and reports over SSE and MCP | An MCP client follows a job to its report; `?from` replays | 3.2, 2.4 |
| 3.8 | Proof | The four taxi jobs through the API give 0.6.6's rows, on a Servercore node and an Azure node | 3.1, 3.3 or 3.5, 3.6, 1.13 |
| 3.9 | The benchmark returns | Operational time on all four jobs against the other engines: first faster than 0.6.6, then the 1.5x bar | 3.8 |

**One open risk:** the engine runs as a Linux program on the Linux layer. Item 0.1 shows whether
the branch head's new Linux layer boots. If it doesn't, the node uses the tag's `kernel/linux`
until it does, and that choice is written here.

## 5. Languages (Draft)

Implements the OS design, section 5.

- **The Rust SDK** is `moruna_sys::plane`, documented, with an example function.
- **The C header** wraps it in six calls: open the task, take the next batch, emit a batch, write a
  log line, report an error, finish. Batches cross by the Arrow C Data Interface.
- **The Python SDK** binds the header with pyarrow: `@moruna.source`, `@moruna.function`,
  `@moruna.sink`. It builds the function's image and registers it. It needs the Linux layer booted
  with CPython.

| Item | Build | Success | Needs |
|---|---|---|---|
| 5.1 | The Rust function SDK | The example builds from the docs alone and runs in a job | 3.6 |
| 5.2 | The C header, with a conformance test | A C function runs in a job and passes the row check | 5.1 |
| 5.3 | The Linux layer boots CPython with pyarrow | A booted test | 0.1 |
| 5.4 | The Python function SDK | A Python function runs in a job on a node and passes the row check | 5.2, 5.3, 3.6 |

## 4. Kernels and accelerators (not started)

The OS design's section 4 stands as written. Its system design is written after 3.8 holds on both
clouds, as Brackly decided. It will cover the kernel contract and registry, the plane's five verbs,
Rust GPU libraries measured per kernel, the first engine adapter (DataFusion, and so peQL), and the
first accelerator.

## Traceability to the OS design

| OS design | Here | State |
|---|---|---|
| 1. Boot as a node, OpenStack | 1A to 1E | Final |
| 1. Boot as a node, Azure | 1F | Final; 1.7b may adjust details |
| 2. The API | 2A to 2E | Final |
| 3. Wiring | 3A to 3F | Final; one open risk (0.1's result) |
| 4. The two interfaces and the unit of work | 4 | Not started, by decision |
| 5. Rust first, every language after | 5 | Draft |
| One image for every cloud | 1A | Final |
| No shell, no arbitrary command | 1B (no console input), 2A (fixed operations) | Final |
| The microkernel rule | 1F (VMBus split), 2B (`api` is a process) | Held |

This document adds detail to the OS design and no behaviour it does not call for. Six details
that go beyond its words:

1. **The node makes its own TLS key by default** (1D). The OS design said node config carries the
   node's TLS identity. Its sentence is changed in the same commit, since this is safer: by default
   no secret sits in instance data.
2. **`api` exists from step 1** and owns port 443 (1C), so "answers on its API port" is true
   before the API has operations.
3. **`GET /v1/health` needs no token** (2A). It says only that the node is up.
4. **The audit record is read at `node/audit`** (2A, 2C).
5. **One job at a time in the first release** (3).
6. **Azure machines boot without Secure Boot** (1F, security type `Standard`) until the boot chain
   is signed. The OS design asks for security as a priority, so signing GRUB and the core is owed
   before production.

If a later detail would contradict the OS design, the OS design is changed first, with Brackly's
approval.

## Order

- **Can start now, on separate files:** 0.1 then 1.1 to 1.5 (one owner, in order); 2.1 (done); the
  reading half of 1.7 (done); 3.1.
- **Then:** 1.6 and 2.2b, 2.2c; 1.7b and then 1.8 to 1.13.
- **Then:** 2.6; 3.2 to 3.7; 3.8; section 5.
