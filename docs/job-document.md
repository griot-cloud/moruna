# Job document

A job document is a JSON object describing one job. Only `moruna_spec`, `source` and `sink` are required; every other field has the same default as the matching `moruna.run` argument. Fields that are absent or `null` are filled from the environment, then from the machine, then from the default. Unknown fields are refused.

```json
{
  "moruna_spec": 1,
  "source": {"kind": "parquet", "url": "reviews/"},
  "kernels": [{"kind": "python", "module": "kernels.py", "callable": "score"}],
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
| `kernels` | list | `[]` | The kernels, in order; empty copies the source to the sink. |
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
| `profiles_dir` | string | `~/.moruna/profiles` | Where kernel profiles are stored. |
| `allow_gil` | boolean | `false` | Run Python kernels on the standard build of Python. |
| `report` | object | report file only | Where the report goes. |

## source

`kind` selects the source; the other fields depend on it.

**`parquet`**: `url` is a path, a URL or a list of them. `options.columns` lists the columns to read, and `options.filters` lists row-group filters as `[column, op, value]`, with op `>`, `<` or `==`.

```json
{"kind": "parquet", "url": "s3://acme-data/reviews/", "options": {"columns": ["review_id", "text"]}}
```

**`tensor`**: `url` is a path or a list of paths to safetensors or aligned binary tensor files; `options.tensors` lists the tensors to read.

**`datafusion`**: a query planned by peQL for a caller. `root` is the peQL workspace; exactly one of `contract`, to read one contract, or `sql`, a query over contracts; and `caller`, the caller as peQL reads it, with `id`, `tenant` and `purpose` and optionally `tier`, `clearance`, `classification`, `roles`, `now` and `other`. See the [peQL documentation](https://griot-cloud.github.io/peQL/).

```json
{"kind": "datafusion", "root": "/data/workspace", "contract": "purchasing/orders",
 "caller": {"id": "svc-scoring", "tenant": "acme", "purpose": "reporting"}}
```

An `iterator` source exists only in Python and is refused in a document.

## kernels

Each entry has a `kind`.

**`python`**: `module` is a module name or a path to a `.py` file, and `callable` is the name of the kernel in it, dotted for a nested name. The decorator's options can be given here as well: `stateful`, `instances`, `accepts`, `tier`, `device_memory`, `releases_gil`, `expected_amplification`, `preferred_rows`, `resume` and `state_bytes`.

**`std`**: `name` is a standard kernel, such as `"filter"`, and `args` is an object of its arguments.

```json
{"kind": "std", "name": "select", "args": {"columns": ["review_id", "score"]}}
```

Any entry can set `fingerprint`, as printed by `moruna check`. Moruna then refuses to run the job if the loaded kernel's fingerprint differs, so a document cannot run a kernel that has changed since it was checked.

## sink

**`parquet`**: `url` is a directory or prefix. `options.row_group_bytes` (default 128 MiB), `options.file_bytes` (default 1 GiB) and `options.compression`: `zstd` (default), `snappy`, `gzip`, `lz4` or `none`.

**`tensor`**: `url` is a directory. `options.format` is `mrb1` (default) or `safetensors`; `options.one_file_per_morsel` and `options.name` (default `tensor`).

**`arrow_ipc`**: `url` is a directory; `options.file_bytes` (default 1 GiB).

**`peql`**: writes under a peQL contract. `root` is the peQL workspace, `contract` the contract, `caller` the writer, who must own the contract, and `mode` either `append` (default) or `overwrite`. peQL writes the data in the contract's layout and updates the contract's manifest when the job finishes.

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

`s3` takes `endpoint`, `region`, `access_key_id`, `secret_access_key`, `session_token` and `bucket`. `gcs` takes `service_account_path`, `service_account_json` and `bucket`. `azure` takes `account`, `access_key` and `container`. `allow_http` permits endpoints without TLS, and `local_root` sets the directory `file://` URLs are relative to. An `endpoint` can be a `unix://` or `vsock://` socket, for a host that provides object storage without a network.

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
