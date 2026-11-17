# The node's host

What a Linux x86-64 host needs to run `moruna run --vm` (stream D of the data OS), and how its
CPUs are kept for the VM's workers (contract C5's host half).

## What the launcher needs

- **KVM:** `/dev/kvm` readable and writable by the launching user.
- **QEMU** built with seccomp and vhost-user, found by name in `PATH` (`qemu-system-x86_64`).
  QEMU stays the hypervisor in 0.7.0.
- **Unprivileged user namespaces:** the launcher starts QEMU and the vsock back end each in its own
  jail (`crates/moruna-vmm/src/jail`): user, mount, PID, network, IPC, UTS and cgroup namespaces,
  a tmpfs root holding only the run's files, no capabilities, a system-call filter. Ubuntu 24.04
  and later restrict unprivileged user namespaces through AppArmor: give the launcher an
  AppArmor profile that allows `userns`, or set `kernel.apparmor_restrict_unprivileged_userns=0`
  (the jail then fails at `deny setgroups` with `Permission denied` until one is done).
- **`moruna-vhost-vsock`** beside the `moruna` executable.
- **Disk:** the run's host directory (`$TMPDIR/moruna-data-*`) holds remote inputs fetched before
  the boot and the output disk (sparse, the size the image declares). It is removed when the
  run ends, whatever the end (C7: nothing is kept between runs).

The host is managed like Talos: there is no shell, console login or SSH into the guest. The VM
is driven only by the job protocol over the vsock.

## A run

```
moruna run JOB.json --output OUT_DIR [--report REPORT.json] [--input NAME=URL]...
    [--memory-gib N | --memory BYTES] [--scratch-gib N] [--node-kit DIR] [--image-stack DIR]
    [--launch-witness DIR] [--vcpu-cpus LIST]
moruna run --vm JOB.json [--image node-image.json] [the options above]
```

- **A run that names its output directory runs in a VM.** One job per VM, nothing kept between
  runs (C7).
- **The document is adapted to the VM** before it is sent (a version 1 document; the one sent
  is what the run's identity and report name):
  - a `run_id` is minted when it has none, as a host run mints one at its start;
  - each input of its source that is a local file, a local directory (its `*.parquet` files) or
    an `s3://`, `gs://` or `az://` object becomes a data disk, `disk://NAME`, its name made from
    the file's. Remote objects are fetched to the run's host directory first (the VM has no
    network);
  - a sink on this host writes onto the VM's output disk, and its directory is the run's output
    directory unless `--output` names another. A Parquet sink writes there as it commits each
    file (`disk://out/`); another sink writes under the VM's output root (`/tmp/out`, in the
    engine's scratch), and the run's end puts its files on the disk. Either way the outputs are
    one layer on the output disk, which the host unpacks.

  A document may name its devices itself (`disk://NAME`) and bind them in `JOB.inputs.json`
  beside it (`{"NAME": "PATH or URL"}`), or with `--input NAME=URL`.
- **The node** is `--image`, or one built for the run from the node kit
  (`kernel/node/tools/node-for-job.py`): its CPUs the job's, its memory the job's budget, the
  engine's scratch (`--scratch-gib`, 1 GiB by default; the output disk has room for that much
  output, the layer's 16 MiB of tables before it and its hash tree after it) and the node's
  services, or exactly `--memory-gib` when given (a job that does not fit is refused, naming the
  least it needs). The node's processes get about 95% of its memory, the rest being the kernel's:
  on 24 GiB a job's budget and scratch together may be about 21 GiB.
- **The node kit** is `--node-kit`, else `MORUNA_NODE_KIT`, else `share/moruna/node-kit` beside
  the executable's `bin`: `kernel/tools/build.sh`'s output, with `kernel/node/tools/node-for-job.py`
  and `configure.py` in its `node/tools/` and `kernel/node/standalone.image` in its `node/`.
  `--image-stack` attaches other image layers (a `moruna-layer stack` directory) than the kit's.
  A stack whose lowest layer is on `block0` is attached alone; the kit's own (`node-linux`, from
  `block2`) has the kit's two image filesystem fixtures below it, which no job reads.
- **The launch witness** is `--launch-witness`, else `$XDG_STATE_HOME/moruna/launches` (or
  `~/.local/state/moruna/launches`).
- **The output directory** is new or empty; when the job completes, the output layer on the
  output disk is unpacked there (C3).
- **Standard output** carries the job updates as lines of JSON: the protocol's messages and every
  event, the task logs among them (`"kind":"log"`, with the task, the level and the line).
- **Standard error** ends with the run's timing:
  `moruna run --vm: timing {"stage_ms":..,"launch_ms":..,"boot_ms":..,"job_ms":..,"outputs_ms":..}`.
- **`--report`** writes the engine's own report with the VM's figures: `exit` (`completed` when
  it completed), `code`, `diagnostic`, the engine's `exit` as `engine_exit`, and `vm`:
  `boot_s` (QEMU running to the guest's hello), `launch_s`, `stage_s`, `node_s`, `job_s`,
  `outputs_s`, `total_s`, `memory_bytes`, `cpus`, `vcpu_cpus`, `inputs` and `outputs`.

## Tick isolation

A guest worker CPU pinned by the engine (`thread_set_cpu`) on a tickless vCPU (stream A's C5)
still loses time to the host: the host's own 1 kHz tick and its other work interrupt whichever
host core runs that vCPU. The fix is on the host: give each vCPU a host core of its own that
the host keeps free of its tick, its scheduler's balancing, its managed interrupts and its RCU
callbacks, and keep everything else of the run off those cores.

**1. The host's kernel command line** (a reboot):

```
nohz_full=W isolcpus=domain,managed_irq,W rcu_nocbs=W irqaffinity=H
```

`W` is the worker cores, `H` the rest. Keep CPU 0 in `H`, isolate whole cores (both SMT
siblings, `/sys/devices/system/cpu/cpuN/topology/thread_siblings_list`), and check after the
boot that `/sys/devices/system/cpu/nohz_full` and `/sys/devices/system/cpu/isolated` read `W`.
`nohz_full` needs `CONFIG_NO_HZ_FULL=y`, which Ubuntu's Azure kernels have.

**2. The run:** `moruna run --vm ... --vcpu-cpus W`, one host CPU per vCPU of the node, vCPU 0
first. The launcher starts QEMU paused, pins each vCPU thread to its CPU, moves every other
thread of QEMU and of the vsock back end to the CPUs left, and only then lets the guest run.

**Without a reboot,** a cgroup v2 cpuset partition (`cpuset.cpus.partition=isolated`) keeps other
tasks and load balancing off the cores, but not the tick: `nohz_full` is set at boot only.

**Griot's node** (Standard_D4ls_v5, 4 vCPUs as 2 cores of 2 threads): `H` = 0-1, `W` = 2-3, a node
of 2 vCPUs with `--vcpu-cpus 2,3`.

### Measured on moruna-bench

Stream D, 2026-11-16, Standard_D8ds_v6 (8 CPUs, cores 4-5 and 6-7 isolated): stream A's quiet
boot (only the `datapath` test), whose pinned guest CPU spins 300 ms and counts what interrupts
it for less than 100 us. QEMU 10.2.1 under KVM, its vCPUs placed by the launcher's own placement
code (`crates/moruna-vmm/examples/pin-vcpus.rs`); 3 runs each, on 2 and 4 vCPUs.

| Host | vCPUs | Interruptions in 300 ms | Time lost |
| --- | --- | --- | --- |
| Host tick on, vCPUs where the scheduler puts them | unplaced | 342 to 799 | 1.44% to 2.73% |
| Host tick on, vCPUs placed | 4,6 and 4-7 | 335 to 360 | 1.45% to 1.47% |
| `nohz_full`, `isolcpus`, `rcu_nocbs` on 4-7, vCPUs placed there | 4,6 and 4-7 | 62 to 72 | 0.058% to 0.073% |
| The same host, vCPUs unplaced | unplaced | 360 to 431 | 1.62% to 2.48% |

Placing the vCPUs alone takes out the spread (other host work landing on the vCPU's core);
the isolated cores take out the host's tick: about 25 times less time lost. What is left (about
0.23 interruptions a millisecond) is the guest's own stretched tick and the Azure hypervisor
under this VM.

## Building and booting the cloud image on the Mac (items 1.1 to 1.5)

The cloud profile (system design 1A) is built by `kernel/node/tools/package.py --profile cloud`
and booted from UEFI by `kernel/node/tools/uefi-boot.sh`. Both need Linux tools, so on the
project's arm64 Mac they run in `moruna-arm`, the project's Linux container, where the x86_64
machine runs under QEMU's emulator (TCG). Under KVM, on a Linux x86_64 host with the same
packages, the commands are the same.

### The VM lock

Building an image and every boot need `~/.vm-lock` (protocol: `tools/proof-lab/run.py`;
`architecture/work/_rules.md`). Never take it while `~/.vm-lock-next` names someone else, never
steal it, and release it, and stop `moruna-arm`, whenever you aren't building or booting:

```
mkdir -m 700 ~/.vm-lock && echo "<who> <item> $(date -u +%FT%TZ)" > ~/.vm-lock/owner
container start moruna-arm
# ... build, boot ...
container stop moruna-arm && rm ~/.vm-lock/owner && rmdir ~/.vm-lock
```

Host tests of single crates (`cargo test -p ...`) need no lock: run them on the Mac with `-j 4`.

### `moruna-arm`

Apple `container` (`/opt/homebrew/bin/container`), image `ubuntu:24.04`, arm64, 4 CPUs, 6 GiB:
keep it at or under 6 GiB. It mounts `/Users/brackly/Desktop/Projects/moruna` at `/src`, so a
worktree is `/src/.claude/worktrees/<name>`. Run commands with
`container exec moruna-arm bash -lc 'cd /src/.claude/worktrees/<name> && ...'`. `git` does not
work inside it (a worktree's `.git` names a Mac path): commit on the Mac.

What it holds (set up for items 0.1 and 1.1, 2026-11-17):

| What | Where | For |
| --- | --- | --- |
| QEMU 10.2.1 (CI's pin), vhost-user | `/opt/qemu/bin`, **not on `PATH`**: `export PATH=/opt/qemu/bin:$PATH` | every boot |
| QEMU's UEFI firmware (edk2) | `/opt/qemu/share/qemu/edk2-x86_64-code.fd`, vars template `edk2-i386-vars.fd` | `uefi-boot.sh` finds them beside `qemu-system-x86_64` |
| GRUB for x86_64 EFI | package `grub-efi-amd64-bin:amd64`, modules in `/usr/lib/grub/x86_64-efi`; `grub-mkstandalone` from `grub-common` | `package.py` |
| `sgdisk`, mtools | packages `gdisk`, `mtools` | `package.py` |
| x86_64 Linux C compilers | packages `gcc-x86-64-linux-gnu`, `libc6-dev-amd64-cross`, `musl-dev:amd64` | `kernel/tools/build.sh` (CI's boot image) |
| Rust | rustup: `1.98.1` with target `x86_64-unknown-linux-musl`; `nightly-2026-07-21` with `rust-src` and `x86_64-unknown-none` (`rustup toolchain install` in `kernel/`) | both builds |

The amd64 packages come through multiarch: `dpkg --add-architecture amd64`,
`/etc/apt/sources.list.d/ubuntu.sources` restricted to `Architectures: arm64`, and
`/etc/apt/sources.list.d/amd64.sources` naming `http://archive.ubuntu.com/ubuntu/` (noble,
noble-updates, noble-security; main universe; `Architectures: amd64`). To set up a fresh
container the same way:

```
apt-get update
apt-get install -y --no-install-recommends gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
  musl-dev:amd64 grub-efi-amd64-bin:amd64 gdisk mtools
```

CI's boot image (`kernel/tools/build.sh`) also needs python:3.14-slim's OCI layout:
`kernel/tools/python-layout.sh` on the Mac (Apple `container` pulls it) writes
`kernel/target/oci/python`.

### Build the cloud image

Inside `moruna-arm`, from the worktree root, with the lock held (about 2 minutes once Cargo's
caches are warm):

```
export PATH=/opt/qemu/bin:$HOME/.cargo/bin:$PATH CARGO_INCREMENTAL=0
python3 kernel/node/tools/package.py --profile cloud --description kernel/node/cloud.image \
  --assets kernel/node/cloud --out OUT --node-memory 2147483648
```

`OUT` must not exist yet. It gets `moruna-node.raw` (the GPT disk: GRUB as
`EFI/BOOT/BOOTX64.EFI`, `boot/moruna-kernel.elf`, `boot/initramfs.cpio`), the payload it was
made from, and `inputs-and-artifacts.json` with every input's and output's digest.

### Boot it

```
kernel/node/tools/uefi-boot.sh OUT CPUS
```

It runs item 1.1's machine (q35, 2 GiB, edk2, the disk read-only on virtio-blk, one virtio-net
card on QEMU's user network, COM1 to `OUT/console.log`) and exits 0 when
`moruna: services started` is on the console: within 120 s under KVM, 900 s under the emulator.
It prints the seconds from QEMU's start to the line. Exit 124 is the timeout, 1 is QEMU ending
first. The console log holds the firmware's screen output (terminal escapes) before GRUB, then
the kernel's lines. `MACHINE=pc kernel/node/tools/uefi-boot.sh OUT CPUS` boots the same disk on
QEMU's i440FX machine instead, which has no MCFG (item 1.8).
