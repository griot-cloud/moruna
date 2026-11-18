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
  | 3 | Adds `imagefs`, `linux`, `node-admission`, `engine` (no `storage`: cloud jobs reach data through volumes, 3C, and object stores, 3D) |

- **Layout:** a GPT disk with one partition, the EFI system partition (FAT32). It holds
  `EFI/BOOT/BOOTX64.EFI` (GRUB), `boot/moruna-kernel.elf` and `boot/initramfs.cpio`. A second
  partition, `moruna-system`, holds step 3's engine layer (and later cached function images): the
  boot disk's driver registers it in `devices`' directory as the read-only block device `system`,
  a slice at the partition's bounds, and `imagefs` takes its layers from it.
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
  moruna: network wan <address>/<prefix> gateway <address|none>
  moruna: cloud <openstack|azure|none> instance <id>
  moruna: tls sha256=<fingerprint>
  moruna: ready api=<address>:443 boot_ms=<n>
  ```

- **`boot_ms`** counts from the core's first instruction to ready. Firmware and GRUB time is
  measured from outside, at bring-up, as create-to-ready.
- **Limits:** services started within 5 s; an address within 30 s; instance data within 90 s.
- **Failure:** the line `moruna: fatal <stage>: <reason>`, then the machine stays halted for 60 s and powers off. The
  60 s hold is so the console can still be read: OpenStack serves a guest's console log only while it runs and shows
  SHUTOFF 15 s late, so a node that powers off at once leaves no readable cause (1.6b). The first
  fatal line is the cause, and tools read only it; consequence lines may follow. The stage
  is `core`, or the name of the service that can't go on, unless this document names another. A node
  never hangs silently, never restarts itself in a loop, and never offers a prompt. A service that
  dies stops the node (SDD 33), except `api`, which is restarted (2B).
- **Power-off is ACPI S5 on every machine:** SLP_TYPa from the DSDT's `\_S5` object (found by
  scanning the DSDT for `_S5_` and reading the package that follows, as small kernels do),
  written with SLP_EN to the FADT's PM1a control register. OSTD today powers off only through
  QEMU's test exit device, which no cloud has. That device stays for CI's exit codes alone.
- **No wait without a deadline in boot code.** OSTD's PIT calibration waits forever on a machine
  with no PIT. Every such wait gets a deadline and, when it passes, a `fatal` line: the PIT and APIC
  timer calibrations, and the wait for the other CPUs to start.

### 1C. Network

- **One interface, `wan`,** on the first network card, set by DHCP: address, mask, gateway, DNS.
  DHCP keeps RFC 2131 4.1's MUST, a randomized exponential backoff: a DISCOVER with no answer
  is sent again after 1 s, then after 2, 4, 8, 16 and 32 s, then every 64 s, each delay moved by
  up to a tenth either way (RFC 8415's way); the schedule starts over when the link comes up or
  a lease is taken. A REQUEST, which follows an OFFER over a path that works, is sent again
  after 4 s. Why not RFC 2131's 4 s, which is its example for a 10 Mb/s Ethernet: a cloud may
  drop what the guest sends for a while after its card starts, and the guest has no signal
  when the path opens. On Azure (item 1.11c's trace) the host completed the first DISCOVER with
  success and dropped it, sent no indication, and the first frame came in 2.7 s later; its
  DHCP answers in about 15 ms. 1 s is DHCPv6's first Solicit timeout and the shortest first
  interval dhcpcd allows. smoltcp's own DISCOVER retry, one fixed delay, kept no backoff and
  no randomization. The console's lease line says how many ms after the network server's
  start it came.
  IPv4 only for now. The network server's config is:

  ```
  moruna-netstack 1
  interface wan card=net0 dhcp
  ```

- **The network server faces the internet, so its limits hold against anyone** (item 1.3b):
  - smoltcp 0.13.1, which fixes a window-underflow panic (upstream issue 1048);
  - a grant's `sockets=` counts every socket, closing ones included, and refuses at accept;
  - `rx=` and `tx=` set its buffers, carried on the wire so a minted grant gets no more than its
    parent's;
  - `per-peer=` caps what one remote address holds;
  - `linger=` shortens abandoned closes;
  - `handshake-timeout-ms` (10 s) frees half-open connections;
  - a line is printed, at most once a minute per grant, when a bound is hit.

  The cloud grant is `grant listen-api rights=listen local=0.0.0.0/0@443 sockets=81 rx=65536
  tx=65536 per-peer=8 linger=5000`.
- **One port listens: TCP 443,** owned by `api`. Nothing else accepts connections. `api` listens on
  `wan`'s leased address, and moves its listener when the lease's address changes.
- **`devices` lives as long as the node.** Started by the node's supervisor, it serves until the
  supervisor stops it, not until its last client leaves. Clients come and go: the network server
  at start, `node-admission` for each job's volumes (3C).
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
4. **Hands `api` its config** (token keys, TLS identity, wall clock, instance ID, the machine's
   memory) over a channel the supervisor links. The node's memory is the machine account's limit,
   which every service and job is carved from: the supervisor reads it with `account_info` and
   passes it to `platform` at start. It is less than the RAM the core's `memory=` line counts, by
   what the kernel keeps. `api` answers `ready` within 10 s, or
   it is fatal.
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
| 1.3b | The network server hardened for a public port: the smoltcp underflow, socket memory bounded per grant | 10,000 abandoned connections never exceed the grant; the suite passes | 1.3 |
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
- **A node needs no nested virtualization** (Brackly, 2026-11-17): Moruna is the machine, so KVM
  support no longer limits the choice. A size qualifies when it boots UEFI and presents disks and
  a network card Moruna drives, and among those the cheapest that holds the job is used. In
  Central India (public prices, 2026-11-17): `D2s_v4` $0.10 an hour, `D4s_v4` $0.20, `D8s_v4` $0.40,
  against `D4ls_v5` $0.17 and `D8ds_v6` $0.52.
- **The first bring-up runs on `Standard_D2s_v4`** (or `D4s_v4`), Generation 2, in subscription
  `griot-dev`, Central India, zone 3, with accelerated networking off. `griot-dev` has no quota for
  any v5 family there (checked 2026-11-17). The v4 size presents the same devices: SCSI disks over
  VMBus and the synthetic NIC, and no virtual function when accelerated networking is off. The
  final proof on `Standard_D4ls_v5` needs DLSv5 quota in `griot-dev`, which is Brackly's to ask
  Azure for.
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
- **No PCI bus:** Generation 2 has no ACPI MCFG. Today the core refuses to boot without it
  (`kernel/core/src/boot.rs`). Instead, `root.ecam` then covers an empty range, `devices` finds no
  PCI function and says so in one line, and the boot goes on. The same holds on any machine
  without MCFG.
- **Guest OS ID:** written before the hypercall page, in the TLFS's open-source encoding. Item 1.8
  proposes the exact value with its TLFS citation; the PM approves it.
- **Power-off:** ACPI S5 (1B).

### VMBus, split by the microkernel rule

- **In the core, one mechanism.** When Hyper-V is found, the core sets up the hypercall page and
  CPU 0's SynIC, and makes one boot capability, `root.hyperv`, which the supervisor grants to the
  `vmbus` process only. The calls on it:

  | Call | What it does |
  |---|---|
  | post a message | `HvPostMessage` to a connection ID: a type and up to 240 bytes. Answers ok, busy, or Hyper-V's status |
  | take a message | Copies out the message waiting on SINT 2, marks the slot empty, and writes end-of-message when one more is pending |
  | map the event flags | Maps CPU 0's event-flags page into the caller, once |
  | the interrupt | Gives an interrupt object for SINT 2, waited on as a driver waits on its device's |
  | a signal handle | Makes a handle that can signal one connection ID and nothing else, to give to that channel's driver |
  | signal | `HvSignalEvent`, through a signal handle |

  - The SynIC is enabled on CPU 0 only, and every channel targets it, by the VP index read from
    `HV_X64_MSR_VP_INDEX` on CPU 0, never assumed to be 0.
  - A GPADL's guest page numbers come from the DMA grant a driver already uses to pin memory.
  - **DMA on Hyper-V:** a VMBus process that pins memory itself (`vmbus`, `storvsc`) gets a whole
    copy of `root.dma`; one that doesn't (`netvsc`, whose buffers `vmbus` shares) gets none.
    With no IOMMU, a process that can write a VMBus ring can already name any guest page to the
    host, so a ring holder is trusted with all guest memory and the grant adds nothing. The
    narrowing is by process: only VMBus processes get it, and only on Hyper-V. With an IOMMU, page
    numbers become I/O addresses and this rule is revisited.
  - Added kernel-mode lines and calls are counted in `data-os.md` section 0.
- **In the `vmbus` process:** `moruna-vmbus` wired to those calls. It serves drivers a small
  protocol, `moruna.driver.vmbus`: list the offers of an interface, open a channel, share memory
  with the host, close.
  - Opening a channel gives the driver the ring memory, the channel's signal handle, and a wake
    channel: `vmbus` writes an empty message to it when the host has data (a full channel means one
    is already pending) and closes it when the channel is gone, so a driver waits only on handles,
    in its dispatcher, beside its clients.
  - **Out:** a driver writes its ring and signals the host itself, through its signal handle.
  - **In:** the host's interrupt reaches `vmbus`, which reads the event flags and wakes the
    drivers whose channels have data, through their wake channels. That is one hop on the way in, accepted for the first
    bring-up.
- **`storvsc` and `netvsc`** are driver processes that serve today's block and network driver
  protocols, so nothing above them changes. netvsc speaks NVSP and RNDIS.
  - **Disks are found in one place:** storvsc registers each disk in `devices`' directory (a
    `register` request, a right only driver links carry), with its serial, size and read-only flag,
    so 3C's `disk://<serial>` works the same on every cloud. Serials follow Microsoft's Azure udev
    rules (WALinuxAgent's `66-azure-storage.rules`, azure-vm-utils' `80-azure-disk.rules`): on the
    root controller `{f8b3781a-1e82-4818-a1c3-63d806ec15bb}` (or a Gen 1 `00000000-0000-` one), LUN 0
    is the OS disk (registered as `boot`, read-only, never a volume) and LUN 1 the resource disk
    (`resource`); on `{f8b3781b/c/d-…}`, LUN n is `lun-<n>`. A disk no rule names is printed and not
    registered; if no disk matches the OS-disk rule, every disk is registered read-only. Disks are
    found at start by REPORT LUNS; disks attached later are a known limit.
  - **vmbus serves one listener slot per interface class,** `scsi` and `net`; a connection lists
    and opens only that class's offers.
  - **A client of a service that isn't on this machine isn't on it either** (the supervisor drops
    it, transitively, along client links only; a server never goes for losing a client), so storvsc
    and netvsc never start, with their DMA grants, off Hyper-V.
- **Libraries first, as with the API server.** The VMBus client, storvsc and netvsc are each a
  library without std (`moruna-vmbus`, `moruna-storvsc`, `moruna-netvsc`), tested on the host
  against a fake of the host's side. The processes only wire them to the core and to each other.
  - `moruna-vmbus` talks to Hyper-V through a trait, `Hv`: post a message, signal an event, take the
    next message, read and clear event flags, and give the guest-physical page numbers of a
    memory object.
  - The drivers talk to a channel through a trait, `Channel`, defined in `moruna-vmbus`: the
    channel's two rings (`vmbus_ring`), signal the host, wait for the host's signal, and share more
    memory with the host (a GPADL).
- **Versions:** the client offers the newest VMBus version `vmbus_core` defines, then steps down,
  and refuses anything older than Windows 10's (4.0).
- **Reused from Microsoft's OpenVMM** (MIT, `no_std`, at e7e7df77): `hvdef`, `x86defs`,
  `vmbus_core`, `vmbus_ring` and `netvsp_protocol` as written, and `storvsp_protocol` with a small
  fork. They are vendored under `kernel/vendor/openvmm` with the licence, the commit and file hashes,
  as OSTD is. OpenVMM's std and async drivers are read as references only.

### `platform` on Azure

- Instance data as 1D says.
- **The "ready" report,** once node config is valid: `GET http://168.63.129.16/machine?comp=goalstate`,
  then `POST /machine?comp=health` with the `Health` XML document, both with the header
  `x-ms-version: 2012-11-30`. The POST also sends `Content-Type: text/xml;charset=utf-8` and
  `x-ms-agent-name: moruna`. Without the report, Azure reboots the machine.
- **`platform`'s HTTP client reads chunked answers too,** within the same 1 MiB: an HTTP/1.1 server
  may send them, and Microsoft doesn't say which the WireServer uses.

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
| 1.9a | The vendored OpenVMM crates | They build without std for the user target; `cargo deny` passes | small | none |
| 1.9b | `moruna-vmbus`: the client library and the `Channel` trait | Host tests against a fake host: contact, offers, GPADLs, open, close, rescind | large | 1.9a |
| 1.9c | The core mechanism and the `vmbus` process | A booted test lists the devices Azure offers; added kernel-mode lines are counted | large | 1.8, 1.9b |
| 1.10a | `moruna-storvsc`: the library | Host tests against a fake storvsp: initialisation, read, write, flush, errors | medium | 1.9a |
| 1.10b | The `storvsc` driver process | The node reads its Azure disk; the block driver suite passes | medium | 1.9c, 1.10a |
| 1.11a | `moruna-netvsc`: the library | Host tests against a fake netvsp: NVSP and RNDIS initialisation, send, receive, the VF offer ignored | large | 1.9a |
| 1.11b | The `netvsc` driver process | DHCP and a connection to 443 on Azure; the network driver suite passes | medium | 1.9c, 1.11a |
| 1.12 | `platform` for Azure, with the "ready" report | Azure shows provisioning succeeded | small | 1.4, 1.11b |
| 1.13 | Bring-up on Azure | 1E's success, on Azure | small | 1.10b, 1.12 |

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
  - **its listener:** the supervisor holds a network-server client whose grant is
    `rights=grant,listen local=0.0.0.0/0@443`. A grant can hand out only what it holds, so it holds
    `listen`, but the supervisor never listens itself. At each start it mints `api`'s listener from
    it (`Client::grant`). `node.conf`'s one record says it all:
    `restart api network network listen-supervisor rights=listen local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000`,
    and a fresh connection to `node-admission` at every start (the supervisor holds a client of
    admission's listener and connects anew; a dead api's connection closes, and admission ends its
    jobs),
    giving the service, its listener slot, the network server's service, its granting slot, and
    the policy. `network.conf`
    has no `listen-api` grant.
  - **its hand-over, the same at every start, the first included:** the supervisor makes a fresh
    `platform`-`api` channel pair and sends `platform` its end, as `api-started`, over `platform`'s
    bootstrap channel, which the supervisor keeps. `platform` keeps the validated config and sends
    the hand-over on each new end. Every start has 1D's 10 s to answer `ready`.
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
  answer is `invalid` or `busy`, never a crash. Idle is the client's: the idle clock runs only
  while the server waits on the client for a request or for bytes. A connection whose request is
  being answered is not idle; that request's own bounds end it, such as admission's 5-minute fetch
  and the API's 10-minute launch backstop (3E). Found by 3.6's boot: a submit waiting on a fetch
  was closed at 60 s.
- **Memory is bounded before any token is checked:**
  - at most 80 connections open, handshakes included; past that, a connection is closed with no
    answer;
  - a TLS handshake must finish within 10 s;
  - the token is checked before any body is read;
  - all bodies share one 8 MiB budget, and a request that doesn't fit gets `busy`;
  - `node/audit` answers the newest 1,000 records.

  Peak use stays under 32 MiB of the 64 MiB account.
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
- **Every call of an operation leaves an audit record,** refused ones included, except `health`:
  time, key, scope, operation, status. `api` keeps the last 10,000 in memory. Nothing is written to
  disk or to the console.

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
- **The budget** is the job document's `budget.memory_bytes`: the job's whole memory, and its
  account is exactly that. A document that names no budget gets the node's job bound
  (`job_memory_bytes`): a node runs one job, so its memory is the job's. The report says the budget
  the job ran with. (On a 2 GiB node the bound is 590,229,504 bytes, and the engine's 326 MiB arena
  and 108 MiB scratch are this section's split of it.) In node mode the engine sizes itself from it: 128 MiB of engine overhead,
  scratch at 1/4 of the remainder (unless the document names a staging volume), the arena the rest.
  **The arena bounds how far reading runs ahead, never how many workers decode.** A read that does
  not fit beside those in flight is cut to the room left, from a split that can be cut (a Parquet
  row group by its pages), not refused while workers idle. The no-kernel read target lets
  `read_ahead` reads fit at their charge. Found by 6.3a: at 3A's split of 1 GiB, one of two workers
  idled half the run, because each whole row group was charged twice its size.
- **Memory on a node is computed, never a constant.** Each service has a fixed account in node.conf
  except `node-admission`, which takes `rest`: node memory less every other account and the kernel's
  share. Jobs and checks are carved from it. The job bound is admission's account less the 256 MiB
  check account (no scratch) and admission's own 128 MiB; `GET /v1/node` reports it as
  `job_memory_bytes`, and a larger budget is refused. One image serves any node size. The cloud
  profile's step-3 services are imagefs 256 MiB (a cache) and linux 256 MiB. The engine
  receives its channel as it is (`grant <slot> channel <from>`, only with `runs 1`).
- **Validation is the engine's.** `api` does not link `moruna-job`, which needs std. A document is
  checked by the engine itself:
  - `validate`: `node-admission` runs the engine's `check` on the document in a short-lived
    process with a 256 MiB account, also while a job runs, and `api` returns its refusals.
  - `submit`: the engine refuses an invalid document as it does today (exit code 2, with the
    refusals), and `api` answers `invalid` with them.

### 3B. The job operations

Added to `moruna-api` (2A), so the OpenAPI document, the MCP tools and the clients grow with them:

| Operation | Scope | Body or answer |
|---|---|---|
| `POST /v1/jobs/validate` | submit | Body: a job (below). Answer: `{"ok":true}` or `{"ok":false,"refusals":[…]}`, `moruna-job`'s refusals with their fields |
| `POST /v1/jobs` | submit | Body: a job. Answer `201` with `{"id":"<run id>"}`; `busy` while a job runs; `invalid` with the refusals |
| `GET /v1/jobs` | observe | The last 100 jobs, newest first: id, state, times |
| `GET /v1/jobs/{id}` | observe | State (`starting`, `running`, `completed`, `failed`, `cancelled`), times, the latest heartbeat's fields, exit code |
| `POST /v1/jobs/{id}/cancel` | submit | Sends `cancel`; `conflict` if the job has ended |
| `GET /v1/jobs/{id}/events` | observe | Server-Sent Events: each event record as its JSON line (`moruna_event`'s rendering), heartbeats, log records, then `report` and `exit`. `?from=<seq>` replays the records after `<seq>` (as SSE's `Last-Event-ID`) from the node's buffer of the last 100,000 records. MCP's `moruna_jobs_events` (`id`, `from`) answers the same records as pages of at most 1,000 records or 1 MiB, with `next` |
| `GET /v1/jobs/{id}/report` | observe | The `RunReport` JSON, once the job has ended |

- **A job is** `{"v1": <job document version 1 as JSON>}` or `{"v2": "<base64 of the MJB2 FlatBuffer>"}`,
  with an optional `secrets` map.
  Either is checked by the engine (3A) before it runs.
- **The node fills in** `report.socket` (the channel), `staging` (none) and the run id when absent.
  A document that names a host socket, a `file://` path outside its volumes, or `resume` is refused
  in this release.
- **Secrets travel beside the document, by name:**
  - the job body carries `"secrets": {"<name>": "<value>"}`, at most 32;
  - the document refers to one where a credential belongs, as `{"secret": "<name>"}`, and never
    holds a credential itself;
  - `api` passes the map to the engine in the `spec` message's `secrets` member, outside the
    document and its digest;
  - `api` redacts the values in every answer, event, log and report, and drops them when the
    engine exits. They live only in the engine's and `api`'s memory.
- **Node mode:** the engine, serving on a channel, fills in the document's gaps and refuses what a
  node doesn't allow (above). `api` and `node-admission` never read a document.

### 3C. Volumes

- **A job reads and writes attached volumes,** named by the serial the cloud gives the disk: the
  Cinder volume ID on OpenStack (virtio-blk's `GET_ID`), `lun-<n>` on Azure. In the job:
  `disk://<serial>/<path>`, where a volume holds one moruna-image layer (the output disk's format)
  and `<path>` names a file in it; `moruna volume pack <dir> <out>` makes a source volume.
- **A volume is served by its driver** (virtio-blk or storvsc) through `moruna.driver.block`, and
  the engine reads and writes it as that protocol's client. Data moves by the client's page numbers
  with no copy, in large pipelined requests. One path on every cloud, and read-only is enforced
  below the engine: a source's connection gets no write lease. (Changed 2026-11-17 night from
  stream D's engine-driven virtio-blk, because Azure's disks are storvsc's and two paths are one too
  many; 3.3 measures the driver path against the direct one, and a loss is fixed in the protocol.)
- **The engine's slots:** `volume.<n>` (a driver connection each) and a data slot `volumes`, one line
  per volume, `<n> <serial> source|sink`. Admission holds the writer lease on every source volume
  for the job's length, so the engine can read it and never write it. File bytes are as trusted as
  an object store's; the layer's index is checked.
- Every driver registers its disks in `devices`' directory with serial, size and read-only flag.
  The engine's check answers the job's `needs.volumes` (`[{"serial", "write"}]`); api passes them to
  `node-admission`'s `launch`, which grants exactly those connections. An unknown serial or the boot
  disk is refused at `validate`.
- **The boot disk is never a volume.** The image carries its own GPT disk GUID, derived from its
  content hash at packaging; the driver marks the disk with that GUID as `boot` (storvsc also by
  Microsoft's rule, 1F).

### 3D. The network for jobs

- **The engine reaches object storage through the network server's own client,** not through the
  Linux layer's sockets. It is a new object-store connector, `moruna-net`, beside the `unix://` and
  `vsock://` ones of `moruna-hosted-engine.md` 4.6: it opens a connection, and the network server
  runs TLS on it (`start_tls`, with the node's `tls.roots`).
- **The network server's TLS needs a trusted time,** which it refuses to start without. It reads the
  node's one wall clock, the core's (5A0's clock page, `realtime_base_ns` and its set flag), and
  refuses TLS while that clock is unset. `platform` sets it through `clock_set_realtime` on
  `root.clock` from instance data's `Date` before `ready`, and again every hour, which keeps S3 and
  Blob request signing inside their 15-minute skew on a node that runs for months. No second clock,
  no link for it, never a build-time clock. TLS accepts the clock once it is set by the machine's
  CMOS at boot or by `platform`; test machines set it with QEMU's `-rtc`, so `tls-unix-seconds` is
  gone everywhere.
- **`api` reads the same clock** from the clock page for all it times (tokens, the audit record,
  the node document's `time`, its own certificate, a job's times to the millisecond: item 2.4), and
  carries the hand-over's wall clock forward only while the page says it is unset.
- **Trusted roots:** the cloud profile's `tls.roots` is Mozilla's set as curl publishes it
  (`cacert.pem`, MPL-2.0), a pinned dated release checked against curl's SHA-256, converted to the
  server's DER list in `kernel/node/cloud/`.
- **Egress per job:** the engine's check (3B) answers with the job's needs: its object-store host
  names, its volumes (3C) and its functions (3E). `node-admission` grants exactly those. For the
  network it holds a client with `grant,connect` on the name rule `*` at port 443 (every name, held
  by admission alone, which never connects itself), and mints each job's channel with `connect`
  limited to that job's host names, on port 443. Nothing else is reachable from a job; an object-store endpoint on any port but 443 is refused at `validate`, by its field.
- **Connections are reused:** the connector keeps an HTTP/1.1 keep-alive pool per host, bounded by
  the job's socket budget (64), so a range read never pays a new TCP and TLS handshake.
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
- **At job start,** `node-admission` fetches each function the job names: it mints itself a channel
  limited to the address's host on 443, fetches over HTTPS with the network server's TLS, checks the
  SHA-256, and hands the engine the image as a read-only memory object, with no copy. The node
  refuses the job if a SHA-256 differs. `address` is `https://` only (a pre-signed or SAS URL for a
  private store); admission holds no store credentials. **An address's query is a credential:**
  the node keeps it only to fetch, and shows an address everywhere else (`GET /v1/functions`, events,
  diagnostics, logs) as its scheme, host and path with the query replaced by `?…`. Images are cached in memory, up
  to node config's `functions.cache_bytes` (default the smaller of 1 GiB and an eighth of the node's memory; an operator's value at most half of it), until the node stops. The cache is an account `node-admission` carves from its own, so the job bound it reports already leaves it out.
- **A job names a function** by its registered name, as a function task's `"function": "<name>"`;
  the check's `needs.functions` lists them. An unregistered name is refused at `validate`.
- **AWS's own S3** (no endpoint) is reached with path-style requests, so its host is
  `s3.<region>.amazonaws.com`, known without the bucket; the region is required.

### Known costs, scheduled after the OS path

- **Window copies at IPC sinks and spills:** a batch held in a plane window is copied when an Arrow
  IPC sink or a staging spill writes it, because the plane's memory is a second pool the arena does
  not own. The fix is one pool: the plane draws its memory objects from the run's arena, under one
  budget, so windows are arena memory a disk already reaches. Parquet sinks are unaffected.
- **Page per buffer in records** (SDD 01 e.7): under 1% at 64Ki rows; revisited only if a time split
  points at it.

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

## 5. Languages (Final)

Implements the OS design, section 5. A function is a program that the plane feeds Arrow batches.
There are two kinds:
- **native:** a static program for the core, as the gate's tasks are today (`images/gate/tasks`);
- **image:** a Linux program run by the Linux layer, today's `image` kind (SDD 19).

### 5A0. Rust's standard library on Moruna

- **arrow-rs, and most of the Rust ecosystem, need `std`,** and the core's user target has none
  (item 5.1's finding). A native function in Rust must be an ordinary Rust program, so Moruna
  becomes a target `std` supports, `target_os = "moruna"`, rather than each library being forked
  to work without it.
- **How:**
  - `std`'s platform layer for Moruna lives in a patched copy of Rust's `library/`, pinned to the
    kernel's nightly and vendored under `kernel/vendor/rust-std` with the patch marked.
  - Programs build with `-Zbuild-std=std,panic_abort` against it.
  - The target becomes `kernel/targets/x86_64-moruna.json`; the `std`-free target stays for the
    core's own services.
- **What the platform layer provides,** over `moruna_sys`:
  - memory from the process's account;
  - threads, and futex-based locks and condition variables;
  - the monotonic clock from the clock page;
  - randomness from the core;
  - stdout and stderr to the task's log;
  - arguments and environment from the start page: a native process's environment is its `env`
    start slot (item 5.5).
  - Files, sockets and processes answer "unsupported" until a function needs them. A function's
    data comes through the plane, not through files.
- **Decisions from item 5.0:**
  - the layer is modelled on Motor OS's: each `std::sys` file calls `moruna_sys::rt`;
  - `SystemTime` reads the clock page's `realtime_base_ns`, which `platform` sets through a new
    call, `clock_set_realtime`, on `root.clock` (granted to `platform` only). Unset, it reads as
    1970 plus the monotonic clock;
  - `yield_now` is a new core call, `thread_yield`;
  - randomness comes from the core's `random_read` (getrandom's custom backend);
  - builds are offline, from `library/vendor`;
  - output with no log slot goes to the console;
  - `std::os::moruna` re-exports std's own `moruna_sys`;
  - threads default to a 2 MiB stack.
- **Proof:** arrow-rs's arrays, compute and IPC run in a booted native task in CI's own boot image.

### 5A. Rust: `moruna-function`

- **One crate,** `kernel/function/rust`, package `moruna-function`, for the core's user target. It
  wraps `moruna_sys::plane` (rings, windows, log) and arrow-rs:

  ```rust
  moruna_function::serve(|batch: RecordBatch, out: &mut Emit| -> Result<(), FunctionError> { … })
  ```

  - `serve` takes each input batch, read in place from its window (Arrow IPC, no copy);
  - `Emit::batch` writes an output batch into a window, and may be called any number of times;
  - a returned error ends the task with that message;
  - logging goes through `moruna_function::log`.
- **What a function is:** one static ELF, built with `cargo build --target` for the core's user
  target. Registration (3E) takes that file. Nothing else is packaged.
- **The gate's seven functions** move onto this crate. They are the SDK's first users and its
  conformance suite.

### 5B. C, and every language that links C

- **A header,** `moruna_function.h`, and a static library, `libmoruna_function.a`, built from 5A for
  the user target. Batches cross by the Arrow C Data Interface:

  ```c
  int  moruna_open(struct moruna_task **task);
  int  moruna_next(struct moruna_task *task, struct ArrowArray *out, struct ArrowSchema *schema); /* 1 batch, 0 end, <0 error */
  int  moruna_emit(struct moruna_task *task, struct ArrowArray *batch, struct ArrowSchema *schema); /* takes ownership */
  void moruna_log(struct moruna_task *task, const char *line);
  void moruna_error(struct moruna_task *task, const char *message);
  int  moruna_finish(struct moruna_task *task);
  ```

- **A batch from `moruna_next`** stays valid until its `release` is called; its window goes back
  at the next `moruna_next` or `moruna_finish`, so an answer emitted after the release still answers
  it. A failed call returns the negated exit code (`MORUNA_E_…`, as 5A's); `moruna_emit` takes
  ownership of both structs, and may be called with no batch held (an aggregate's answer at the end;
  5A has the same, an end hook).
- **C needs a libc, and it is relibc** (Redox's, MIT, in Rust) with a Moruna platform layer over
  `moruna-sys` and std, vendored at a pinned commit like OSTD. A minimal runtime breaks on the first
  real C library; musl would need an in-process Linux syscall layer. Item 5.2b. **One runtime, not
  two:**
  - std, the SDK and relibc build as one crate graph: one allocator, one `moruna-sys`; relibc's
    `malloc` is std's global allocator, and relibc drops its own panic and alloc-error items.
  - **ELF TLS** (x86_64 variant II, FS:0 the thread control block's self pointer) for std and relibc
    alike; 5.0's key table goes. One control block type holds both runtimes' per-thread state.
  - **One thread path:** `std::thread::spawn` and `pthread_create` both go through it.
  - **One `_start`:** sets up TLS, takes the log slot (std owns it; C's fd 1 and 2 write through
    std's writer), runs relibc's static init when it is linked, then `main`.
  - Unsupported POSIX (fork, exec, signals, sockets, paths) answers `ENOSYS`/`ENOENT`, as 5A0 says.
  - C++ (libc++, libc++abi, libunwind over relibc) follows once relibc runs; exceptions never cross
    a `moruna_*` call.
- **C, C++ and Zig** link the library into a static native function. Go and others follow when
  someone needs them; nothing in the header is tied to C.

### 5C. Python

- **Python functions are the `image` kind,** on the Linux layer (`kernel/linux` until the new layer
  returns, section 0): CPython with pyarrow in a layered image, the batch exchanged as Arrow IPC.
  That is today's 0.6 path (`moruna.function`, the image builder of `python/moruna/_build.py`)
  and needs no new interface.
- **Params (item 5.5):** a Python function reads its params as every function does, native or
  image: `moruna.params()`, the task's `MORUNA_PARAMS`. The shim gives a decorated function the
  job's params like any other and refuses none; a class task is still constructed with them, and
  can read them through `moruna.params()` too.
- **The SDK** keeps `@moruna.source`, `@moruna.function` and `@moruna.sink` (SDD 12). It gains
  `moruna.register(fn, client)`: build the image, sign it with the operator's key, upload it to
  object storage the node can read, and register it through `moruna.api` (2E).
- **What registration takes for an image function:** one file, a **Moruna image archive** that
  `moruna.register` makes on the client: an index (manifest, config, digests, each layer's offset,
  length and seal) and Moruna's own sealed layer files at page-aligned offsets. The node converts
  nothing: `node-admission` hands the engine the archive as a read-only memory object, and `imagefs`
  attaches its layers in place (memory-backed, seals checked at mount); the task runs on that view
  as its root (`run_plane`'s `root`). A native function's file is its ELF, mapped in place. Which
  it is, is decided by content; anything else is refused. The engine's slots are `function.N` plus
  a `functions` index (`N NAME SHA256 SIZE`), at most 16 per job.

### Work items

| Item | Build | Success | Needs |
|---|---|---|---|
| 5.0 | Rust's `std` on Moruna (5A0) | A booted native task builds and runs arrow-rs's arrays, compute and IPC | none |
| 5.1 | `moruna-function` (5A), its guide, and the gate's seven functions moved onto it | Each of the seven gives its rows, as today, in a booted job; an example built from the guide alone runs | 5.0, 3.2 |
| 5.2 | The header and the static library (5B), with a C conformance function | A C function passes the row check in a booted job | 5.1 |
| 5.3 | CPython with pyarrow booted on the Linux layer, as an image function | A Python function's rows match in a booted job | 3.2 |
| 5.4 | `moruna.register` and the end-to-end Python path (5C) | From a laptop: a decorated Python function is registered on a node and runs in a job | 5.3, 3.6 |

## 6. The benchmark (Final)

Implements the OS design's last line of section 3: "The benchmark returns". Brackly's rule
(2026-11-17): a miss points at something broken or inefficient in Moruna; fix that, for every
workload. Nothing is tuned for a benchmark.

### 6A. Where

- **Subscription `griot-dev`,** Central India, one resource group per run, `moruna-bench-<date>`,
  deleted after.
- **Moruna is the node:** our VHD on `Standard_D4s_v4`, zone 3, driven through its API.
- **Every other engine runs on Linux** (Ubuntu 24.04) on the same size in the same zone, one machine
  at a time: 0.6.6 (with Python and with the Rust function), the plain Rust pool, DuckDB, Polars,
  Ray, Spark and Dask.
- **The data** sits in one Azure Blob container in the same region. Moruna reads it through 3.5; the
  others through their own Blob readers or a local copy timed apart.

### 6B. What is measured

- **Per job and per engine,** the median of 5 runs, in interleaved order:
  - operational time: from the first rows to the output written;
  - start-up: Moruna's boot-to-ready, the others' process start and imports;
  - CPU-seconds and peak memory;
  - the output's canonical hash, checked against 0.6.6's.
- **Jobs:** the four NYC taxi jobs on 2024 (41.2M rows) first. FineWeb and Overture follow when
  NYC holds.
- **The bar:** first faster than 0.6.6 on every job, then at least 1.5x the best engine on
  operational time, with CPU-seconds no worse.

### 6C. When Moruna loses

1. The report's time split (read, decode, function, encode, write, waits) names the stage that
   lost.
2. A profile of that stage names the cause in Moruna's code.
3. The fix is written as a work item that states the general defect, why it hurts workloads beyond
   the benchmark, and a test that shows it without the benchmark.
4. **Refused:** constants tuned to a dataset, a path special to a job, anything that changes what
   is measured.
5. Every run and every fix is logged in `architecture/benchmark-log.md`.

| Item | Build | Success | Needs |
|---|---|---|---|
| 6.1 | The harness: provision, run, collect and tear down, from one command on the Mac | A dry run creates and deletes the resource group | 1.13, 3.5 |
| 6.2 | The first full run | The table of 6B, logged | 3.8, 6.1 |
| 6.3 | Each miss, through 6C | One work item per root cause | 6.2 |

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
| 3. Wiring | 3A to 3F | Final; 0.1's risk settled: `kernel/linux` until the new layer returns |
| 3. "The benchmark returns" | 6 | Final |
| 4. The two interfaces and the unit of work | 4 | Not started, by decision |
| 5. Rust first, every language after | 5A to 5C | Final |
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
