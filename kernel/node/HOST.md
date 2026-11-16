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
  - a sink on this host is written to the VM's output root (`/tmp/out`), and its directory is
    the run's output directory unless `--output` names another.

  A document may name its devices itself (`disk://NAME`) and bind them in `JOB.inputs.json`
  beside it (`{"NAME": "PATH or URL"}`), or with `--input NAME=URL`.
- **The node** is `--image`, or one built for the run from the node kit
  (`kernel/node/tools/node-for-job.py`): its CPUs the job's, its memory the job's budget, the
  engine's scratch (`--scratch-gib`, 1 GiB by default; the outputs wait there for the output
  disk) and the node's services, or exactly `--memory-gib` when given (a job that does not fit is
  refused, naming the least it needs).
- **The node kit** is `--node-kit`, else `MORUNA_NODE_KIT`, else `share/moruna/node-kit` beside
  the executable's `bin`: `kernel/tools/build.sh`'s output, with `kernel/node/tools/node-for-job.py`
  and `configure.py` in its `node/tools/` and `kernel/node/standalone.image` in its `node/`.
  `--image-stack` attaches other image layers (a `moruna-layer stack` directory) than the kit's.
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

See the table below (stream D, 2026-11-16).
