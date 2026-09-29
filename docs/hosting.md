# Where Moruna runs

Moruna runs inside one process on one machine, and reads that machine's limits when a job starts. Where it runs changes what those limits are, where it spills to disk and how you set the budget. This page covers each place in turn.

## A laptop or a virtual machine

With no container limit, Moruna uses 90% of the machine's memory as the budget and every core it can see. That suits a machine given over to the job. If other programs need memory at the same time, set `budget` so Moruna leaves room for them.

Moruna spills to a directory under the system's temporary directory and uses at most 20% of the free space there. On a machine with a small system disk, point it at a larger one with `staging_dir`, as described in [The staging directory](#the-staging-directory).

## A container or Kubernetes pod

In a container, Moruna reads the memory and CPU limits from the container's control group, so you set the limit once, on the container, and do not repeat it in the job. It uses `memory.high` if the container sets one, otherwise 90% of `memory.max`, and the CPU quota for the number of workers.

In Kubernetes, set the pod's memory and CPU limits and leave `budget` unset. Give the staging directory its own volume, such as an `emptyDir`, so that spilling does not fill the container's writable layer:

```yaml
resources:
  limits:
    memory: 8Gi
    cpu: "4"
volumeMounts:
  - name: staging
    mountPath: /staging
env:
  - name: MORUNA_SPILL_DIR
    value: /staging
```

## The staging directory

When data waiting between steps does not fit in memory, Moruna writes it to the **staging directory** and reads it back later. The same directory holds the job's checkpoints.

Set it with `staging_dir` in `moruna.run`, `staging.dir` in a job document, or the `MORUNA_SPILL_DIR` environment variable. `staging_limit` (or `staging.limit_bytes`, or `MORUNA_SPILL_LIMIT`) caps how much of it a job may use. Choose a local disk: Moruna reads spilled data back while the job runs, so a slow network drive slows the job.

If the staging directory is on a disk that outlives the machine, a job can resume on a different machine after the first one is lost. Mark the disk as durable with `"staging": {"dir": "/staging", "durable": true}` in the job document. Learn more in [Checkpoints and resume](resume.md).

## Machines that change size

Some platforms add memory or cores to a running machine, or take them away. A job document can allow Moruna to follow those changes with `budget.elastic`:

```json
"budget": {
  "elastic": {"memory_max_bytes": 34359738368, "cpu_max": 32}
}
```

Moruna checks the machine's limits several times a second. When memory or cores are added, it uses them, up to the maximums in `elastic`. When they are removed, it reduces what it uses. Reducing memory is gradual, because Moruna waits for data in use to be released rather than discarding it, so a host should wait for the job to settle before removing more. The run report records each change. A job without `elastic` keeps the limits it started with.

## An isolated virtual machine

:::{note}
**Preview.** Running a job in a virtual machine requires Linux with KVM (`/dev/kvm`). It is new, and its interface may change.
:::

`moruna run --vm` starts a small virtual machine, runs the job inside it, and returns its report. The virtual machine has no network interface: the job reads and writes only the disks you attach, which keeps a job that runs someone else's code away from your network and your files.

```text
moruna run --vm job.json --image moruna-guest/ --disk /data/reviews.img:ro --disk /data/scratch.img
```

`--image` is the guest image, as a directory or an OCI image layout. Each Moruna release on GitHub includes a `moruna-guest-<architecture>.ref` file naming its image, `ghcr.io/griot-cloud/moruna-guest` pinned by digest, with a signature and a software bill of materials. `--disk` attaches a disk image, read-only with `:ro`. Inside the virtual machine, a job can reach object storage only through a socket the host provides, named as the `endpoint` in the job document's `object_store` section.

Next: [How it works](execution.md).
