# The node's host

What a Linux x86-64 host needs to run `moruna run --vm` (stream D of the data OS), and how its
CPUs are kept for the VM's workers (contract C5's host half).

## What the launcher needs

- **KVM:** `/dev/kvm` readable and writable by the launching user.
- **QEMU** built with seccomp and vhost-user, found by name in `PATH` (`qemu-system-x86_64`).
  QEMU stays the hypervisor in 0.7.0.
- **Unprivileged user namespaces:** the launcher starts QEMU and the vsock back end each in its own
  jail (`crates/moruna-vmm/src/jail`): user, mount, PID, network, IPC, UTS and cgroup namespaces,
  a tmpfs root holding only the run's files, no capabilities, a system-call filter.
- **`moruna-vhost-vsock`** beside the `moruna` executable.
- **Disk:** the run's host directory (`$TMPDIR/moruna-data-*`) holds remote inputs fetched before
  the boot and the output disk (sparse, the size the image declares). It is removed when the
  run ends, whatever the end (C7: nothing is kept between runs).

The host is managed like Talos: there is no shell, console login or SSH into the guest. The VM
is driven only by the job protocol over the vsock.

## A run

```
moruna run --vm JOB.json --image node-image.json --launch-witness DIR \
    --input NAME=PATH|file://PATH|s3://B/K|gs://B/K|az://C/K ... \
    --output OUT_DIR [--vcpu-cpus LIST]
```

- `node-image.json` declares the engine's data disks (`data_disks`): its inputs by name, in
  device order, and its output disk's size (`kernel/node/tools/configure.py`, `package.py`,
  `node-for-job.py`).
- Each `--input` binds one declared name to a host file, used in place, or to a remote object,
  fetched to the run's host directory first (the VM has no network). The job reads it as
  `disk://NAME`.
- `--output` is a new or empty directory. When the job completes, the output layer on the output
  disk is unpacked there (C3).
- Standard output carries the job updates as lines of JSON: the protocol's messages and every
  event, the task logs among them (`"kind":"log"`, with the task, the level and the line).
- Standard error ends with the run's timing, the VM's boot apart from the job:
  `moruna run --vm: timing {"stage_ms":..,"launch_ms":..,"boot_ms":..,"job_ms":..,"outputs_ms":..}`.

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
