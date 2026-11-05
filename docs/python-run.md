# Jobs and `moruna.run`

A run is a job: sources, functions and sinks joined by name. Build one with `moruna.Job`, look at it, write its spec, and run it.

```python
import moruna
import pyarrow as pa
import pyarrow.compute as pc


class Enrich(moruna.Function):
    takes = pa.schema([("id", pa.int64()), ("text", pa.string())])
    gives = pa.schema([("id", pa.int64()), ("text", pa.string()), ("orders", pa.int64())])
    needs = ("warehouse",)

    def __init__(self, table: str) -> None:
        self.table = table

    def __call__(self, batch: pa.RecordBatch) -> pa.RecordBatch:
        figure = self.ports["warehouse"].count(self.table)
        return batch.append_column(self.table, pa.array([figure] * batch.num_rows, pa.int64()))


job = moruna.Job("orders")
rows = job.add(moruna.ParquetSource("orders.parquet", columns=["id", "text"]), name="rows")
enriched = job.add(Enrich(table="orders"), inputs=rows)
job.add(moruna.ParquetSink("enriched"), inputs=enriched, name="out")
print(job)                          # one line per task, with its image's state
report = moruna.run(job, ports={"warehouse": warehouse}, budget="2GiB")   # in this process
job.build(out="images")             # one image per task of your code
job.write("orders.mjob")            # the runnable spec
print(report.exit, report.sinks)
```

## `moruna.Job`

```python
job = moruna.Job(name, *, platform=None, runtime=None, lock=None, files=())
job.add(task, inputs=(), name=None) -> task
job.arg(name, type) -> None
job.tasks -> tuple[(name, role, task, inputs), ...]
job.spec() -> moruna.JobSpec
job.write(target) -> moruna.JobSpec
moruna.Job.read(source) -> moruna.Job
job.build(out="moruna-images", *, registry=None, platform=None, ports=None, files=None) -> list
job.status() -> list[dict]
job.check() -> None
job.explain(planner=None) -> dict
moruna.image(reference, *, role="function", entry=None, params=None, needs=(), takes=None, gives=None)
print(job)
```

`add` adds a task fed by `inputs` (tasks of the job, or their names, in port order) and returns it. A source takes no input; a function or a sink takes at least one. A name defaults to the task's (`enrich` for `Enrich`, the function's name, the standard function's name), numbered when it is taken. A plain callable is made a function with the defaults of `moruna.function` first, and that function is returned. One task's output may feed several tasks (fan-out), and a function may take several inputs (fan-in), each on a port named after the task that feeds it. Branches that are ready at the same time run at the same time. `moruna.std.join()` puts the columns of branches of one upstream side by side, morsel by morsel; `moruna.std.union()` passes on every batch of every input.

Adding a name twice, a name that is not 1 to 128 letters, digits, `_`, `-`, `.` or `:`, the same task twice, or an input that is not in the job raises `ValueError`. Unknown input names, cycles and outputs no task reads are refused by `check()` and by `moruna.run`, before anything starts, with one `ConfigError` that names every one of them.

`print(job)` shows one line per task: its name, its role, its kind or its image's entry, its params, its inputs, its needs, the schemas it takes and gives and, for an image task, its image's state (`not built`, `built`, `pushed` or `external`), its build key and, once built, its digest and location.

`job.arg(name, type)` declares a run argument (`string`, `int`, `float`, `bool`, `date` or `timestamp`): the spec carries the declaration, and every run supplies its value (`moruna.run(job, args={"start": "2026-11-01T00:00:00Z"})`), so one spec serves every run, a time window per run for example. A task reads the values as `self.args`.

### The spec

`job.spec()` returns the job's spec at any step of building it: the job document, version 2 (see [The job document](job-document.md)). `spec.bytes()` are the document's FlatBuffers; `spec.digest()` is `"sha256:"` and the hex of SHA-256 over exactly those bytes; `spec.to_json()` renders it for people and is never read back. A spec holds the job's name, tasks and edges, and nothing else: the run's settings are `moruna.run`'s arguments.

A spec names Moruna's own tasks by their kind and settings, and every task of your code (a `Source`, `Function` or `Sink` subclass, or a `@moruna.function`) as an `image` task: its entry (`module:Class`), its params, its needs and its image, `{build_key, digest, location}`. Every task carries its definition inline: its title and description (its docstring), the JSON Schema of its params (from its constructor's annotations), the schemas it takes and gives, its needs with their descriptions, and where its code came from (see [The job document](job-document.md#a-tasks-definition)). `spec()` refuses, naming the task, what a spec cannot hold: an `IteratorSource` (a stream this process holds), code defined inside a function, a param that is not data (a JSON value), a param whose name names a secret (`password`, `token`, `api_key`, ...), which is a need, supplied when the job runs, and an import a build cannot follow. Such a job still runs in this process; its report's `job_digest` is `None`.

`moruna.Job.read(source)` reads a spec back (a `JobSpec`, its bytes, or a path; a version 1 JSON document is read too) and writing the job read gives the same bytes. A document that also sets run settings is refused, naming each. An image task of a job read from a spec runs its image, so its spec must be runnable (every image built). `Job.read(path).explain()` plans a document without importing anything.

`job.write(target)` writes the spec through a writer: a file path, a callable taking bytes (`moruna.BytesWriter`), or any object with `write_spec(data)` and `write_report(data)` (`moruna.Writer`, which a writer of another package implements). An object-store URL is refused in this release.

### Checking and explaining

`job.check()` checks the job without running it: names, ports and edges, each task's kind and settings against its definition, and, along every edge whose two ends declare one, that the schema one task `gives` is the schema the next `takes`. It raises `moruna.ConfigError` naming every refusal.

### Building images

`job.build(out, registry=None, platform=None, ports=None, files=None)` builds one image per task of your code into the OCI layout `out`, and returns `job.status()`: for each image task its `task`, `role`, `state`, `build_key`, `digest` and `location`. The spec then names each image's digest and location: the runnable spec. Tasks share layers (the same content is the same blob, stored once), and an image already in the layout for a build key is reused. `registry` (`HOST/REPOSITORY`) also pushes every image, with credentials the resolver `ports` supplies for the need `registry:<HOST>`. `platform`, `files` (data files every task reads) and `lock` retarget the job first: they are part of every build key. What an image holds, and how it is found and checked, is in [Images and ports](images.md) and [`moruna build`](cli.md#moruna-build).

A build key is known before anything is built: `sha256:` over each file of the task's import closure (its syntax tree with docstrings removed, so a docstring, comment or formatting change keeps it), its data files' bytes, the lock, the runtime image, the shim, the platform and the entry. `moruna.image(reference, ...)` adds a task of an image Moruna did not build, named `HOST/REPOSITORY@sha256:...`; its state is `external`.

`job.explain(planner=None)` returns the plan Moruna would run the job by, as a dict, without running it: `id`, `job_digest`, `planner`, `resources`, `steps` (each with `id`, `tasks`, `depends_on`, `group`, `resources` and `cost`), `groups`, `totals` and `images`, each image task's image as `job.status()` gives it. It works for a job of in-process objects and for a job read from a spec alike, and is the same function `moruna explain JOB` plans a document with. A refused job raises `moruna.ConfigError`; an unknown planner raises `moruna.PlanError`.

## Tasks

`moruna.Source`, `moruna.Function` and `moruna.Sink` are `moruna.Task`s. A task the user writes subclasses one of them and declares:

| Attribute | Meaning |
| --- | --- |
| `takes` | The `pyarrow.Schema` of every batch it takes; `None` declares nothing. |
| `gives` | The `pyarrow.Schema` of every batch it gives; for a source, also its schema. |
| `needs` | The names of what the runner must supply, each a string Moruna does not read (`"warehouse"`, `"warehouse.read"`, `"model"`), or a mapping of each name to what it is. |
| `files` | Data files its code reads, relative to its module's directory; they travel in its image and are part of its build key. |

Its constructor's arguments are its params, and a spec holds them only when they are data. Its docstring is its title (the first line) and description. What `needs` names is supplied before the run starts and read as `self.ports[name]`: in this process, the object the resolver supplied; inside its image, a client of the port (`call(bytes) -> bytes`). The run's arguments are `self.args`. Declared schemas are checked by `job.check()` and on every batch at run time: a batch of another schema fails the task, naming it and both schemas.

A `Function` subclass defines `__call__(self, batch) -> batch` and optionally `finish(self)`, called once when its input has ended, whose batch (or `None`) goes downstream. Moruna calls the one instance added to the job, one batch at a time. A `Source` subclass and a `Sink` subclass follow [their own contracts](python-sources.md); a sink's `finish` may return the version it committed, `{"snapshot_id": ..., "version": ...}`.

`@moruna.function` makes a function of a callable (see [Functions](python-functions.md)), and `moruna.std` builds Moruna's own; each is a `moruna.Function`.

## Needs and the resolver

```python
class moruna.Resolver:
    def resolve(self, need: str) -> object      # raises LookupError when it cannot supply it

moruna.Ports(mapping)                           # the in-process resolver
```

A resolver is the port that supplies needs. `moruna.run(job, ports={"warehouse": warehouse})` wraps the mapping in `moruna.Ports`; a host that runs Moruna passes its own `moruna.Resolver` as `ports=`, or names it on the command line (`moruna run JOB --resolver module:attribute`). Every need of every task is resolved before the run starts, and each one the resolver cannot supply is refused by name in one `ConfigError`. A task this process runs gets the object (each call through it measured for the report); an image task's need is a port, served by what the resolver supplies: a callable taking the request's bytes and returning the answer's, or an object with `call(request)` ([the port protocol](images.md#the-port-protocol)).

## `moruna.run`

```python
moruna.run(
    job, functions=None, sink=None, *, ports=None, args=None, spec_to=None, explain=False,
    budget=None, cpu=None, trace=None,
    staging_dir=None, staging_limit=None, on_error="terminate",
    ordered=False, sizer="rule", profiles_dir=None, storage=None,
    host_profile=None, allow_gil=False, checkpoint=True,
    checkpoint_interval=5.0, keep_checkpoint=False, resume=None,
) -> moruna.RunReport
```

Runs the job `job`, or the job of one path `moruna.run(source, functions, sink)`, whose tasks are `source`, `function-1` .. `function-n` and `sink`; `functions` may be one function or a list, and an empty list copies the source.

| Parameter | Type and default | Meaning |
| --- | --- | --- |
| `job` | `Job`, or a source | The job, or the source of a job of one path. |
| `functions`, `sink` | function or list, sink | The functions and sink of a job of one path. |
| `ports` | mapping or `Resolver`, `None` | What the tasks need, by name. |
| `args` | mapping, `None` | The run's arguments, every one the job declares; refused by name when one is missing, of another type or undeclared. |
| `spec_to` | path, callable or `Writer`, `None` | Gets the spec before the run starts and the report's bytes after it ends. |
| `explain` | `bool = False` | Put the plan the run started from in `report.plan`. |
| `budget` | `int \| str \| None = None` | Whole-process memory ceiling in bytes or a size string such as `"8GiB"`; `None` uses discovery. |
| `cpu` | `float \| None = None` | CPU quota; `None` uses discovery. |
| `trace` | `str \| None = None` | File or directory for a per-morsel trace. |
| `staging_dir` | `str \| None = None` | Directory for staged data and checkpoints. |
| `staging_limit` | `int \| str \| None = None` | Maximum staging bytes; default is 20% of available space. |
| `on_error` | `str \| tuple[str, int] = "terminate"` | `"terminate"`, `"skip"`, or `("budget", n)` to allow at most `n` failed morsels. |
| `ordered` | `bool = False` | Deliver results to each sink in source order. |
| `sizer` | `str = "rule"` | `"rule"` or `"learned"` morsel sizing. |
| `profiles_dir` | `str \| None = None` | Profile directory; defaults to `~/.moruna/profiles`. |
| `storage` | `dict \| None = None` | The object store: `endpoint`, `region`, `bucket`, `account`, `container`, `local_root`, `allow_http`, and its credentials, `access_key_id`, `secret_access_key`, `session_token`, `service_account_path`, `service_account_json`, `access_key`. Credentials reach the run, never a spec; settings not given are read from the provider's usual environment variables. |
| `host_profile` | `None` | Reserved. A non-`None` value raises `ConfigError`; use `MORUNA_HOST_PROFILE`. |
| `allow_gil` | `bool = False` | Permit Python functions on a GIL-enabled interpreter. |
| `checkpoint` | `bool = True` | Write checkpoints while running. |
| `checkpoint_interval` | `float = 5.0` | Seconds between checkpoints. |
| `keep_checkpoint` | `bool = False` | Keep checkpoint files after success. |
| `resume` | `str \| None = None` | `"auto"`, a 32-character run id, or path to `manifest.json`. |

**Returns:** [`RunReport`](python-results.md). **Raises:** `ConfigError`, `PlanError`, `BudgetError`, `FunctionError`, `IoError`, `ResumeError`, or `Cancelled` as appropriate. Runtime exceptions may carry a partial report, which `spec_to` also gets. Size strings accept binary units (`KiB` through `TiB`) and decimal units (`KB` through `TB`). Numeric values outside supported ranges may be clamped; `report.notes` records the change.

```python
import moruna

report = moruna.run(
    moruna.ParquetSource("orders.parquet", columns=["order_id", "amount"]),
    [moruna.std.filter("amount > 0")],
    moruna.ParquetSink("clean-orders"),
    budget="2GiB",
)
print(report.run_id, report.tasks[1]["rows_out"])
```
