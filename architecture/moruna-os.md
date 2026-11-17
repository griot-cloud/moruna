# Moruna OS: design

Approved by Brackly on 2026-11-17 as the plan. The three decisions at the end are still open. It
replaces the benchmark-first plan in `data-os-streams.md`; the microkernel rule and the contracts
in `data-os.md` stand.

**Moruna OS is a machine you boot from one image and drive only through an API. It runs jobs made of
sources, functions and sinks over Arrow batches, on whatever hardware the machine has.**

## The shape

```
   agents · SDKs · Griot
          │   Job API: HTTPS and MCP                      user-facing interface
   ┌──────▼──────────────────────────────────┐
   │ api     one process, one port            │
   │ engine  plans, places, moves batches,    │
   │         keeps the memory budget          │
   └──────┬──────────────────────────────────┘
          │   Plane: units on queues, Arrow buffers in shared memory
          │                                               hardware-facing interface
   ┌──────▼──────┬──────────────┬─────────────┐
   │ CPU cores   │ GPU          │ FPGA, ours  │           executors
   └─────────────┴──────────────┴─────────────┘
   core (OSTD + Moruna's microkernel) · drivers and servers as processes
```

Two interfaces with the engine between them. What sits above the Job API and what sits below the
plane can change without touching the engine.

| Word | Meaning |
|---|---|
| Node | One machine booted from the Moruna image, for one client. |
| Core | Moruna's microkernel. ("Kernel" below never means the OS.) |
| Batch | An Arrow record batch. The only data that moves. |
| Function | A named, pure step with declared schemas: batches in, batches out. A source has no input; a sink has no output. |
| Kernel | One implementation of a function for one kind of executor. |
| Unit | One kernel applied to one batch. The atom the engine schedules. |
| Executor | Anything that runs units: a CPU core, a GPU, an FPGA, a card of ours. |

## 1. Boot as a node

One image for every cloud: a UEFI disk holding the core, the drivers and servers, the engine and
the API. Cloud differences live in driver processes and one `platform` process, never in the
image's layout.

| | OpenStack (Servercore, KVM) | Azure |
|---|---|---|
| Image | UEFI; raw or qcow2 | UEFI (Generation 2); fixed-size VHD |
| Bus | PCI: have | VMBus: build (a small core mechanism and a bus driver process) |
| Disk | virtio-blk: have | storvsc over VMBus: build |
| Network | virtio-net: have | netvsc over VMBus: build |
| Clock | kvmclock: have | Hyper-V's clock: build |
| Handshake | none | report "ready" after DHCP, or Azure reboots the machine: build |
| Node config | metadata service or config drive: build | instance metadata: build |

- **Have:** the image builder (`kernel/node/tools/package.py`: a UEFI GPT disk, GRUB loading the
  core), the node's manifest (`standalone.image`), DHCP and TLS in the network server. The image has
  never booted on a cloud.
- **Build for both:** boot the image under UEFI firmware on a development machine (QEMU with KVM is
  what Servercore runs); the `platform` process, which finds the cloud and reads the node's config
  from instance data; the boot log on the serial console.
- **OpenStack first.** It needs no new driver, so it proves the image, the boot, the network and
  the config.
- **Azure second, on `Standard_D4ls_v5`** (production's size). Its disks are SCSI over VMBus, so
  the first bring-up needs no PCI-over-VMBus. NVMe sizes and the fast NIC (MANA) come later; both
  also sit behind VMBus. Before writing VMBus drivers, read Microsoft's OpenVMM (Rust): it may hold
  guest-side VMBus code we can use. Not verified.

**Done when** a machine created from our uploaded image, on each cloud, gets an address, answers on
its API port, and shows as running in the cloud's console. Nobody logs in to make it work.

Size: OpenStack small, Azure large (rough guesses; nothing here is estimated in detail).

## 2. The API

One process on the node, `api`, built on `moruna-svc` like the other servers. It does not need the
Linux layer.

- **Transport:** HTTPS (TLS 1.3, HTTP/1.1, JSON), with Server-Sent Events for live streams. MCP's
  Streamable HTTP maps onto the same operations one to one. One listening port.
- **Node config** arrives as instance data: the key that signs API tokens, and the node's TLS
  identity. Tokens are short-lived and scoped: observe, submit, admin.
- **No shell and no arbitrary command.** Every call is logged.

| Operation | What it does |
|---|---|
| `node` | Version, cores, memory, executors, health |
| `functions`: register, list | Signed function images and the kernels they carry |
| `jobs`: validate, submit, get, list, cancel | A job document in, a job ID out |
| `jobs/{id}/events` | Progress, logs and each stage's time split, live |
| `jobs/{id}/report` | The final report |

Step 2 serves `node` only; the job operations arrive with step 3.

**Done when** `node` answers over HTTPS and over MCP from outside the cloud, on both clouds; a call
with a bad token is refused; and killing `api` leaves the node running.

Size: medium.

## 3. Wiring

`api` hands a job to the engine with the job messages the node already uses (submit, events, logs,
report), which today cross a socket to a Linux host. In order:

1. **Start from something that boots.** The branch head swapped in the new Linux layer without
   booting it. Boot it, or go back to tag `gate-verified-2026-11-17`. Fix the full-year hang.
2. **Data.** Attached volumes first (`disk://` works today). Then object storage: the S3 API, then
   Azure Blob. That puts the network on the data path, so measure its throughput early.
3. **Functions** registered through the API as signed images. Admission exists.
4. **Proof.** The taxi jobs submitted through the API give 0.6.6's rows. The benchmark returns
   here: operational time against the other engines on the same machine size.

Size: medium to large.

## 4. The two interfaces and the unit of work

### Why not one function per GPU core

- A GPU's thousands of cores are lanes of one program. They run the same instructions in step, so
  ten thousand different functions cannot run at once.
- A lane has no OS under it: no system calls, no allocation, no waiting.
- Starting work on a GPU and moving data to it cost per start, not per row. Small pieces lose.
- Inputs and outputs must fit in the device's memory.

### The unit: one kernel applied to one batch

- **Parallel inside a unit, independent across units.** The device spreads one batch's rows over
  its lanes. The engine keeps many units in flight on many executors at once: different kernels on
  different batches. That is the MIMD level, and it is the same on every kind of hardware.
- **Batch size belongs to the executor:** a few MiB for a CPU core, hundreds of MiB for a GPU.
- **An executor states its slots:** how many units it runs at once. A CPU core has 1, a GPU a few
  (bounded by its memory), an FPGA one per loaded pipeline.
- **A kernel declares two things** so no executor allocates mid-run: the most output a given input
  can produce, and any state it keeps between batches. Units of a stateful kernel stay on the
  executor that holds the state.

### What the user sees

The user writes sources, functions and sinks. A function carries one or more kernels:

| Kernel | Written as | Runs on |
|---|---|---|
| Opaque | Any code: Rust, or any language in an image | CPU cores |
| Library | A named kernel that Moruna or a package ships with an implementation per device: matrix multiply, group and aggregate, join, sort, decode, encode | Every executor that implements it |
| Expression (later) | Typed expressions over columns, which Moruna compiles | CPU, GPU, FPGA |

- **Every function has a CPU kernel.** A device kernel is an acceleration, and it must produce the
  CPU kernel's rows. The benchmark's row check enforces that.
- **The job does not name hardware.** The engine places each stage from measured cost; the user can
  pin a stage.
- **Moruna stays a runtime.** It defines the kernel contract and ships a small library. Engines and
  vendor libraries (cuDF, cuBLAS) arrive as packages that provide kernels.

### The plane: the hardware-facing interface

Every executor speaks the plane, and the engine speaks nothing else to hardware.

| Verb | Meaning |
|---|---|
| describe | The kernels it implements, its slots, its memory, its preferred batch size |
| load | Make a kernel ready: native code, a shader, a bitstream |
| submit | Run this kernel on these input buffers into these output buffers |
| complete | Done: status, rows out, timings |
| cancel | Stop a unit; unload a kernel |

- **A batch is described the same way wherever it lives,** by Arrow's C Device Data Interface. A
  batch already in a device's memory stays there for the next unit on that device.
- **Software form, built today:** rings and shared memory between the engine and a task process.
  A driver process for a device speaks the same rings.
- **Hardware form, to design:** the same queues as a device's own queues, with batches moved by
  DMA, in the shape of NVMe. A card of ours implements it directly, with no translation.

### How it runs

A device needs a driver where it runs. On a Moruna node, drivers are processes we write.

| Executor | Driver | When |
|---|---|---|
| CPU cores | None | Now |
| FPGA, or a card of ours | A small driver process; we define the device's side | First accelerator on a Moruna node |
| NVIDIA GPU | The vendor's closed Linux driver, which cannot load on Moruna's core | First on a Linux node: an executor process on the host uses CUDA, and the plane reaches the Moruna VM over shared memory. On a Moruna node it needs a native driver, which does not exist. |

## 5. Rust first, every language after

Three seams, each neutral to language by construction:

1. **The Job API** is defined once as Rust types. The OpenAPI document, the MCP tools and the
   client SDKs are generated from them. A job is a JSON document; an SDK's builders only write it.
2. **The function ABI** is the task side of the plane: the Rust crate `moruna_sys::plane`, and a C
   header over it of about six calls that pass batches by the Arrow C Data Interface. Any language
   with an Arrow library binds it: Python, Go, Java, C++, R, Julia.
3. **Packaging.** A function ships as a signed image. Compiled languages run directly on the core.
   Languages with a runtime (Python, the JVM) bring it and run on the Linux layer.

Rust is the reference SDK and Python the second. `@moruna.function` builds the image and registers
it.

## What changes from today

- **Keep:** the core, drivers, network server, image filesystem, plane, the engine's memory and
  disk work, the image builder, and the benchmark harness as the row check.
- **Park until step 3:** the speed work and the 1.5x bar; the Linux-node launcher and the QEMU
  question; the usage build for pruning; Linux-layer work beyond what step 3 needs.
- **Carried debts:** the full-year hang; 0.7 slower than 0.6.6 because tasks wait for batches; the
  branch head unbooted.

## Decisions needed

1. **Scope of 0.7.0:** steps 1 to 3 on both clouds with the Rust and Python function SDKs, and
   accelerators after it? The interfaces in section 4 are fixed now either way.
2. **Azure's first size:** `Standard_D4ls_v5`, which means VMBus disk and network drivers?
3. **First accelerator:** an FPGA or a card of ours on a Moruna node, or an NVIDIA GPU on a Linux
   node?
