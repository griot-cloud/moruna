# Job document

A job document describes one job. It has two forms. Version 2 is the job as a DAG of tasks, in FlatBuffers (file identifier `MJB2`, schema `crates/moruna-job/schema/job.fbs`): what `moruna.Job.spec()` writes and `moruna.Job.read` reads (see [Version 2](#version-2-a-dag-of-tasks)). Version 1 is the JSON object described below, a chain from one source through its functions to one sink, which Moruna reads as the DAG of one path.

A version 1 document is a JSON object describing one job. Only `moruna_spec`, `source` and `sink` are required; every other field has the same default as the matching `moruna.run` argument. Fields that are absent or `null` are filled from the environment, then from the machine, then from the default. Unknown fields are refused.

```json
{
  "moruna_spec": 1,
  "source": {"kind": "parquet", "url": "reviews/"},
  "kernels": [{"kind": "std", "name": "filter", "args": {"expr": "is_not_null(text)"}}],
  "sink": {"kind": "parquet", "url": "reviews-scored/"},
  "budget": {"memory_bytes": 8589934592}
}
```

## Top-level fields

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `moruna_spec` | integer | required | The document format version; `1`. |
| `run_id` | string | generated | The job's id, 32 lowercase hexadecimal characters. |
| `source` | object | required | Where the input comes from. |
| `kernels` | list | `[]` | The functions, in order; empty copies the source to the sink. |
| `sink` | object | required | Where the output goes. |
| `budget` | object | discovered | Memory and CPU. |
| `staging` | object | discovered | The directory for spilled data and checkpoints. |
| `object_store` | object | none | Connection settings for object storage URLs. |
| `checkpoint` | object | enabled, every 5 s | Checkpoint settings. |
| `resume` | string | none | `"auto"`, a run id or the path to a `manifest.json`. |
| `error_policy` | string or object | `"terminate"` | `"terminate"`, `"skip"` or `{"budget": n}`. |
| `ordered` | boolean | `false` | Deliver results to the sink in source order. |
| `sizer` | string | `"rule"` | `"rule"` or `"learned"`. |
| `trace` | string | none | Path of a file for the per-morsel trace. |
| `profiles_dir` | string | `~/.moruna/profiles` | Where function profiles are stored. |
| `allow_gil` | boolean | `false` | Run Python functions on the standard build of Python. |
| `report` | object | report file only | Where the report goes. |
| `contracts` | list | `[]` | The contracts a `datafusion` source reads and a `peql` sink writes under, compiled. |

## source

`kind` selects the source; the other fields depend on it.

**`parquet`**: `url` is a path, a URL or a list of them. `options.columns` lists the columns to read, and `options.filters` lists row-group filters as `[column, op, value]`, with op `>`, `<` or `==`.

```json
{"kind": "parquet", "url": "s3://acme-data/reviews/", "options": {"columns": ["review_id", "text"]}}
```

**`arrow_ipc`**: `url` is a local file, a local directory or a list of them; a directory is read for its `.arrow` files, in name order. Only the Arrow IPC file format is read, not the stream format, and every file must have the same schema. A file compressed with `zstd` or `lz4` is read like any other. `options.columns` lists the columns to read, and `options.split_rows` sets the rows each piece of work is cut toward, from whole record batches (by default, one piece per record batch).

```json
{"kind": "arrow_ipc", "url": "/data/features/", "options": {"columns": ["id", "x"]}}
```

**`vortex`**: `url` is a path, a URL or a list of them; a directory or prefix is read for its `.vortex` files. `options.columns` lists the columns to read, and `options.split_bytes` sets the size each piece of work is cut toward (default 128 MiB).

```json
{"kind": "vortex", "url": "s3://acme-data/events/", "options": {"columns": ["event_id", "text"]}}
```

**`tensor`**: `url` is a path or a list of paths to safetensors or aligned binary tensor files; `options.tensors` lists the tensors to read.

**`datafusion`**: a query planned by peQL for a caller. `root` is the peQL workspace, where peQL keeps its manifests, privacy ledger and audit log; the contracts are the ones listed in [`contracts`](#contracts); exactly one of `contract`, to read one contract, or `sql`, a query over contracts; and `caller`, the caller as peQL reads it, with `id`, `tenant` and `purpose` and optionally `tier`, `clearance`, `classification`, `roles`, `now` and `other`. `on_violation` says what the read does with rows that break a contract's row rules: `exclude` (default) leaves them out, and the run report's `read` counts them per rule for each contract; `refuse` refuses the read when the data holds any, naming each rule and how many rows broke it. See the [peQL documentation](https://griot-cloud.github.io/peQL/).

```json
{"kind": "datafusion", "root": "/data/workspace", "contract": "purchasing/orders",
 "caller": {"id": "svc-scoring", "tenant": "acme", "purpose": "reporting"}}
```

**`image`**: an OCI image's command as a source: `image` (`build_key`, `digest`, `location`), `entry`, `params` and `needs`, as for an [`image` function](#kernels).

An `iterator` source exists only in Python and is refused in a document.

## kernels

The functions. Each entry has a `kind`.

**`image`**: an OCI image's command, in any language, for every role: `image` is `{build_key, digest, location}` (a build key for an image Moruna builds, and once built the manifest's digest and where the image is: an OCI layout's absolute path or a registry reference `HOST/REPOSITORY@sha256:...`); `entry` is what its entry point runs; `params` its data; `needs` the ports the runner serves it. A function's other settings are `command`, `env` (added to the image's; a name starting `MORUNA_` is the runner's), `cache` (where unpacked images go: by default Moruna's cache, `$XDG_CACHE_HOME/moruna/unpacked` or `~/.cache/moruna/unpacked`), `plane` (the command makes the data plane's calls on descriptor 3) and `ports` (names routed to the host's peer or a handler of the program). Every task of your own code is an `image` task; its contract is [Images and ports](images.md).

**`std`**: `name` is a standard function, such as `"filter"`, and `args` is an object of its arguments.

```json
{"kind": "std", "name": "select", "args": {"columns": ["review_id", "score"]}}
```

A `package` entry can set `fingerprint`, as printed by `moruna check`. Moruna then refuses to run the job if the loaded function's fingerprint differs, so a document cannot run a function that has changed since it was checked.

## sink

**`parquet`**: `url` is a directory or prefix. `options.row_group_bytes` (default 128 MiB), `options.file_bytes` (default 1 GiB) and `options.compression`: `zstd` (default), `snappy`, `gzip`, `lz4` or `none`.

**`vortex`**: `url` is a directory or prefix. `options.file_bytes` (default 1 GiB) is the most a file may hold.

**`tensor`**: `url` is a directory. `options.format` is `mrb1` (default) or `safetensors`; `options.one_file_per_morsel` and `options.name` (default `tensor`).

**`arrow_ipc`**: `url` is a directory; `options.file_bytes` (default 1 GiB); `options.compression` is `none` (default), `zstd` or `lz4`, and `options.level` the zstd level (default 3). A compressed file is smaller and is opened by any Arrow reader, `pyarrow.ipc.open_file` included.

**`peql`**: writes under a peQL contract listed in [`contracts`](#contracts). `root` is the peQL workspace, `contract` the contract, `caller` the writer, who must own the contract, and `mode` either `append` (default) or `overwrite`. peQL writes the data in the contract's layout and updates the contract's manifest when the job finishes.

`on_violation` says what the write does with rows that break the contract's row rules:

- `refuse` (default): nothing is committed when any row breaks a rule, and the job fails naming each rule and how many rows broke it.
- `exclude`: the rows that pass are committed, and the others are written as Parquet under `excluded_url`, one file per morsel that had any, each row with a `_violations` column listing the rules it broke. `excluded_url` is a local directory, given only with `exclude`, that is empty or absent when the job starts and is apart from the contract's files and from the source.
- `warn`: every row is committed, and peQL's verdict on the write marks it invalid.

The run report's `write` says what was done: `report.violations` (rows by rule), `report.rows_excluded` and, for `exclude`, `excluded` with the directory, the files and their rows.

```json
{"kind": "peql", "root": "/data/workspace", "contract": "purchasing/orders",
 "caller": {"id": "svc-ingest", "tenant": "acme", "purpose": "ingest"},
 "on_violation": "exclude", "excluded_url": "/staging/excluded"}
```

**`image`**: an OCI image's command as a sink: `image`, `entry`, `params` and `needs`, as for an [`image` function](#kernels). What it writes back at its end as a commit (`snapshot_id`, `version`) is in the report's `sinks`.

## contracts

Each entry is one contract, as peQL registers a compiled one: `document`, the contract's document; `compiled`, its compiled form exactly as parcel wrote it; `functions`, the function modules the compiled form pins; and `audiences`, the tenants it is published to for this job besides its owner (`public` is everyone). The job's peQL engine holds these contracts in memory and nothing else: it reads no contract from `root` and writes none there, and a contract the job names that is not listed here is refused by name. A document that lists contracts and has no `datafusion` source or `peql` sink is refused.

## budget

| Field | Meaning |
| --- | --- |
| `memory_bytes` | The memory limit for the whole process, in bytes. |
| `cpu` | The number of cores. |
| `elastic.memory_max_bytes` | The largest memory limit the job may grow to if the machine gains memory. |
| `elastic.cpu_max` | The largest number of cores the job may grow to. |

Without `elastic`, the job keeps the limits it started with. See [Machines that change size](hosting.md#machines-that-change-size).

## staging

| Field | Meaning |
| --- | --- |
| `dir` | An existing directory for spilled data and checkpoints. |
| `limit_bytes` | The most disk the job may use there; default 20% of the free space. |
| `durable` | `true` if the directory is on a disk that outlives the machine. |

## object_store

`s3` takes `endpoint`, `region` and `bucket`. `gcs` takes `bucket`. `azure` takes `account` and `container`. A document holds no credential, and a field that would carry one (`access_key_id`, `secret_access_key`, `session_token`, `service_account_path`, `service_account_json`, `access_key`) is refused: the runner supplies credentials when the job runs, from `moruna.run(..., storage=...)` or from where the job runs (the provider's environment variables, the machine's identity). `allow_http` permits endpoints without TLS, and `local_root` sets the directory `file://` URLs are relative to. An `endpoint` can be a `unix://` or `vsock://` socket, for a host that provides object storage without a network.

## checkpoint

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Write checkpoints while the job runs. |
| `interval_ms` | `5000` | Milliseconds between checkpoints, from 500 to 60,000. |
| `keep` | `false` | Keep the checkpoint directory after the job completes. |

## report

| Field | Meaning |
| --- | --- |
| `file` | Where to write the report; `<run_id>` in the path is replaced with the job's id. By default, `moruna-<run_id>.report.json` in the staging directory, or in the current directory. |
| `socket` | A `unix://` or `vsock://` address to connect to and send progress and the report; see [the host protocol](cli.md#the-host-protocol). |

## Version 2: a DAG of tasks

Version 2 holds the job as tasks and edges. A task has an `id` (1 to 128 letters, digits, `_`, `-`, `.` and `:`, starting with a letter or a digit), a `role` (`Source`, `Function` or `Sink`), a `kind` and its `settings` (the kind's settings as above, a FlexBuffers map), its `definition`, its named input and output ports, and optionally `barrier` and `members`. An edge joins one task's output port to another task's input port. One output may feed many inputs; a function may take several inputs, each fed by exactly one edge. The document also has the job's `name` and every top-level field above (`budget`, `staging`, `object_store`, `checkpoint` and the rest), in the same meaning.

A job spec, what `moruna.Job.spec()` writes, is a version 2 document with the job's name, tasks and edges and nothing else: a task gives on its port `out`, a function or a sink of one input takes it on `in`, and one of several inputs takes each on a port named after the task that feeds it. `moruna.Job.read` refuses a document that also sets run settings, naming them, because they are `moruna.run`'s arguments; `moruna run` runs such a document as it is.

`moruna.Job.spec().digest()` is `"sha256:"` and the hex of SHA-256 over exactly the document's bytes, and the run report names it as `job_digest`.

### A task's definition

Every task of a spec carries its definition inline, so a spec is complete for drawing, checking and running without a catalogue: the declaration of its kind (as `moruna kinds` lists it) for one of Moruna's own kinds, and for an `image` task the declaration its code makes. A definition has the declaration's fields (`name`, `role`, `summary`, `settings`, `inputs`, `outputs`, `needs`, `deterministic`, `preserves_columns`, `isolation`) and:

| Field | Meaning |
| --- | --- |
| `title` | What a person calls the task: its docstring's first line, else its class or function's name. |
| `description` | The rest of its docstring. |
| `settings` | For an image task, the JSON Schema of its params: each of its constructor's params, its type from its annotation (`bool`, `int`, `float`, `str`, `None`, `list`, `dict`, `Literal` or a union of them; an unannotated param takes any data), its default, closed. |
| `takes`, `gives` | The Arrow schemas it declares: `{"fields": [{"name", "type", "nullable"}]}`, each type as Arrow writes it. |
| `requires` | What it needs, each `{"name", "description"}` (a task's `needs` may be a mapping of name to description). |
| `provenance` | For an image Moruna builds: `build_key`, and when its code is in a git repository `source` (the remote, with no credential), `revision` and `dirty` (whether a bundled file differs from the revision). |

The check refuses a definition that is not one, a definition of one of Moruna's kinds that differs from the kind's declaration, params that do not fit an image task's schema, needs that are not what its definition requires, and an edge between two tasks whose declared schemas differ. A docstring is read from the code each time the spec is written, and is not part of a build key.

### Run arguments

A job may declare arguments: `args`, each a `name` (1 to 64 letters, digits and `_`, starting with a letter) and a `type` (`string`, `int`, `float`, `bool`, `date` (`YYYY-MM-DD`) or `timestamp` (RFC 3339)), in name order. The document holds the declaration and never a value. Every run supplies a value of each, and nothing else: `moruna.run(job, args={...})` or `moruna run JOB --arg NAME=VALUE`. A missing argument, one of another type and one the job does not declare are refused by name before anything starts; a task reads them as `self.args` (an image task's process as `MORUNA_ARGS`), and the report records them.

### Draft and runnable specs

An `image` task's image has a build key from the moment it is added: `job.spec()` is instant and complete. Until the job is built its images have no digest, and the spec is a **draft**, refused for running with every unbuilt task named. `job.build()` and `moruna build` fill each digest and location; that spec is **runnable**, and its digest names exactly the images that run. A spec never holds a credential, an argument's value or data.
