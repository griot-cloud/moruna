# Command line

The `moruna` command is installed with the Python package. Run `moruna --help` for a summary and `moruna --version` for the installed version.

## moruna run

```text
moruna run JOB.json [--plan PLAN.mplan] [--strict]
moruna run --vm JOB.json --image IMAGE [--disk PATH[:ro|:rw]]...
```

Runs the job described in `JOB.json` and exits when it ends, printing one line with the exit code and the report's location.

- `--plan` runs the job by a plan `moruna explain --out` wrote, instead of the plan Moruna makes for it. A plan of another job, or one that is not a plan of this job, is refused before anything runs.
- `--strict` refuses a document that would take any setting from an environment variable, naming the setting.
- `--vm` runs the job in a virtual machine with no network; Linux with KVM only. `--image` is the guest image directory or OCI layout, and each `--disk` attaches a disk image, read-only with `:ro`. See [An isolated virtual machine](hosting.md#an-isolated-virtual-machine).

## moruna explain

```text
moruna explain JOB.json [--planner NAME] [--out PLAN.mplan]
```

Prints the plan Moruna would run the job by, as JSON, without running it: its steps in the order they start, each with the tasks it runs, the steps it waits for, its group (the steps that may run beside it), the resources it is given and its estimated cost, then the job's totals. The same job planned twice has the same step ids, and the events of its run name them, so a screen can show the plan and mark each step as it starts and ends. `--planner` chooses the planner (`levels` by default); `--out` also writes the plan for `moruna run --plan`. Exits 2 when the document is refused or the planner is unknown.

## moruna serve

```text
moruna serve --listen ADDRESS [--no-strict]
```

Waits on `ADDRESS`, a `unix:///path` or `vsock://CID:PORT` address, for one job document, runs it while reporting on the same connection, and exits. Serve is strict by default; `--no-strict` allows settings from environment variables.

## moruna check

```text
moruna check TARGET [--kernel NAME] [--json] [--seed N] [--store DIR] [--budget SECONDS]
                    [--statistics FILE] [--cost-model NAME] [--no-warm-up]
```

Checks every kernel in `TARGET`, a module name, a path to a `.py` file or a package, against its declared schemas, using generated batches. Each kernel that agrees is then warmed up: run on generated data at 1,000 to 1,000,000 rows, at 1, 2 and 4 workers, and, for a stateful kernel, at increasing numbers of distinct keys. Every measurement is kept in an observation store and the cost model is fitted from it, so the report gives what the kernel costs per 10,000 rows and what the warm-up itself used.

- `--kernel NAME` checks only the named kernel.
- `--json` prints the report as JSON.
- `--seed N` changes the seed used to generate batches; the default is 0, so repeated checks use the same batches.
- `--store DIR` is where measurements are kept; the default is `~/.moruna/observations`.
- `--budget SECONDS` is how long the warm-up may take; the default is 60. A warm-up that runs out of time says how far it got.
- `--statistics FILE` is a JSON file describing the input table's columns (null fraction, distinct count, string and list lengths, which columns are keys), which the generated data follows.
- `--cost-model NAME` is the cost model to fit; the default is `loglinear`.
- `--no-warm-up` checks the schemas only.

## moruna build

```text
moruna build MODULE:FUNCTION [--out DIR] [--tag NAME] [--platform linux/aarch64|linux/x86_64]
             [--lock PATH] [--file PATH]... [--json]
```

Builds a Python kernel as an OCI image the `image` kind runs. `MODULE` is a module name or a path to a `.py` file, and `FUNCTION` is the kernel in it. The image is written to the OCI layout directory `--out` (default `moruna-image`) under `--tag` (default `FUNCTION:latest`) and has four layers: the Python 3.14 runtime, the dependencies the project's lock pins, Moruna's shim, and the function. Layers 2 to 4 carry compiled bytecode, so a start compiles nothing; the build needs a standard (not free threaded) CPython 3.14 on the path to write it, and installs only wheels. The command prints the image's digest and each layer's digest and size.

- A lock is required: a `uv.lock` or a `requirements.lock` with a hash on every pin, found next to the module or above it, or `--lock PATH`. Nothing is resolved without one.
- `--platform` defaults to this machine's on Linux and to `linux/aarch64` on a Mac.
- `--file PATH` adds a file the function reads, below the directory the module is imported from; repeat it for each.
- `--index-url`, `--find-links` and `--no-index` go to `uv`, which installs the wheels.

The same function built twice is the same image, and functions with one lock share every layer but their own.

`python -m moruna` accepts the same commands.

## Exit codes

| Code | `moruna run` and `moruna serve` | `moruna check` |
| --- | --- | --- |
| 0 | The job completed. | Every kernel agrees with its declaration and was warmed up. |
| 1 | The job failed reading or writing data. | |
| 2 | The document was refused; the message names the field. | A kernel was refused, or its warm-up could not run. |
| 3 | The budget cannot hold the job. | |
| 4 | A kernel failed and the error policy stopped the job. | |
| 5 | Resuming was refused. | |
| 130 | The job was cancelled. | |

## Environment variables

These fill settings a job does not set. In strict mode, a job that would use one is refused.

| Variable | Setting |
| --- | --- |
| `MORUNA_BUDGET` | The memory budget, in bytes or as `8GiB`. |
| `MORUNA_CPU` | The number of cores. |
| `MORUNA_SPILL_DIR` | The staging directory. |
| `MORUNA_SPILL_LIMIT` | The most disk the staging directory may use. |
| `MORUNA_HOST_PROFILE` | System features to assume instead of detecting them. |

## The host protocol

`moruna serve`, and `moruna run` with `report.socket` set in the document, exchange newline-delimited JSON messages with one peer. Each message has a `type`.

Moruna sends:

| Type | When | Fields |
| --- | --- | --- |
| `hello` | On connecting | `moruna_version`, `spec_digest`, `limits` |
| `heartbeat` | At least every 2 seconds | `t_ms`, `committed_seq`, `rows_out`, `bytes_out`, `active_workers`, `ceiling_bytes`, `anon_bytes`, `bottleneck`, `staging_bytes` |
| `limits_changed` | When the machine's memory or cores change | `old`, `new`, `reason` (`memory` or `cpu`) |
| `report` | Before exiting, if the job produced a report | `report`, the full run report |
| `exit` | Last | `code`, and `diagnostic` when the code is not 0 |

The peer sends:

| Type | Effect |
| --- | --- |
| `spec` | The job document, in its `spec` field. `moruna serve` only, and once. |
| `cancel` | Stops the job after writing a checkpoint; Moruna exits with 130. |
| `checkpoint` | Writes a checkpoint immediately. |

A `moruna serve` session starts with Moruna's greeting, then the peer's job document:

```text
<- {"type": "hello", "moruna_version": "0.3.1", "spec_digest": null, "limits": {...}}
-> {"type": "spec", "spec": {"moruna_spec": 1, "source": {...}, "sink": {...}}}
<- {"type": "heartbeat", "t_ms": 2000, "rows_out": 120000, "active_workers": 8, ...}
<- {"type": "report", "report": {...}}
<- {"type": "exit", "code": 0, "diagnostic": null}
```

If the peer disconnects, the job continues and its report is still written to the report file.
