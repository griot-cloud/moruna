# Images and ports

Every task of a job that is not one of Moruna's own kinds is an **image task**: a source, a function or a sink whose code is an OCI image's command. Moruna builds an image for each task of your Python code (`job.build()`, `moruna build`), and you can bring an image Moruna did not build, written in any language. This page is the contract an image follows, and the protocol its needs are served by.

## In a spec

An image task is the kind `image`, of any role, with these settings:

```json
{
  "image": {"build_key": "sha256:6f1c...", "digest": "sha256:9a0e...", "location": "/data/images"},
  "entry": "pipeline.tasks:Enrich",
  "params": {"table": "alerts"},
  "needs": ["warehouse.read"]
}
```

| Setting | Meaning |
| --- | --- |
| `image.build_key` | For an image Moruna builds: `sha256:` over the task's code (each file's syntax tree with its docstrings removed, or a data file's bytes), the lock, the runtime image, the shim, the platform and the entry. Known before anything is built. Absent for an image you bring. |
| `image.digest` | The image's manifest digest, once it is built; `null` until then. |
| `image.location` | Where the image is, once it is built: an OCI layout's absolute path, or a registry reference `HOST/REPOSITORY@sha256:...` whose digest is `digest`. |
| `entry` | What the image's entry point runs, given to the process as `MORUNA_ENTRY`; Moruna does not read it. |
| `params` | The task's params, data only, given to the process as `MORUNA_PARAMS`. |
| `needs` | What the runner supplies the task: each a port, called as below. |

A spec whose image tasks have no digest is a **draft**: complete for drawing and checking, and refused for running, naming every task whose image is not built. Running a built spec runs exactly the images its digests name: an image Moruna built must carry its build key as its manifest's `org.moruna.build.key`, and every blob is checked against its digest. `command`, `env`, `cache`, `plane` and `ports` are the kind's other settings (see [The job document](job-document.md)).

## Bring your own image

An image you bring is named by a registry reference with its digest and no build key:

```python
scorer = job.add(moruna.image("registry.example.com/team/scorer@sha256:9a0e...", entry="score"), inputs=rows)
```

Its status is `external`. Moruna pulls it through the OCI distribution API before the run starts (anonymously, or with the token or the username and password the resolver supplies for the need `registry:<HOST>`), checks every digest, and keeps it in `~/.cache/moruna/images` by digest. Any image that follows the contract below runs as a task.

## The contract

Moruna starts the image's entrypoint and command, as its config names them, in a confined process with no network. The process gets the image's environment and these variables:

| Variable | Value |
| --- | --- |
| `MORUNA_ENTRY` | The task's entry, as its spec names it. |
| `MORUNA_ROLE` | `source`, `function` or `sink`. |
| `MORUNA_PARAMS` | The task's params, a JSON object (`{}` when it has none). |
| `MORUNA_ARGS` | The run's arguments, a JSON object. |
| `MORUNA_PORTS` | The task's ports, a JSON array of names in name order; absent when it has none. |
| `MORUNA_PORT_SOCKET` | The socket its ports are called on: `fd:N`, a descriptor the process inherits, or `unix:PATH`; absent when it has no port. |

What it reads and writes depends on its role. Every stream is one Arrow IPC stream (the streaming format, not the file format), its schema first.

- **A function** reads batches on its standard input and writes on its standard output exactly one batch for each batch it read, in order. When its input ends it may write one more batch, its last, and then ends its stream and exits with status 0. An input with no byte at all has no batch. A function may instead make the data plane's calls on its descriptor 3 (`plane: true`), with no stream.
- **A source** reads nothing and writes every batch it gives on its standard output, then ends its stream and exits 0. A source with no rows writes a stream with its schema and no batch.
- **A sink** reads batches on its standard input until the stream ends, then writes on its standard output either nothing or one JSON object naming the version it committed, `{"snapshot_id": "...", "version": "..."}`, and exits 0. Every commit names its snapshot.

What a process writes on its standard error is the task's log. Any other exit status, a stream that is not Arrow IPC, or a function that writes more or fewer batches than it read, fails the task, and the error quotes the end of the standard error. A run on a Linux node starts the process in its own namespaces and cgroup, with a seccomp filter and no network; on the Moruna kernel a function runs as a task of the data plane.

An image Moruna builds has four kinds of layer: the runtime (`python:3.14-slim`, by digest), one layer per Python distribution its code imports (so tasks that import the same distribution share its layer), the shim, and the task's own code with its bytecode. Its entrypoint is the shim, `python3 /opt/moruna/shim.py --workdir /app`, and its config sets `MORUNA_ENTRY`, so the image also runs by itself under any OCI runtime, Arrow IPC in and out: Inside it, the `moruna` module holds only `Task`, `Source`, `Function`, `Sink`, `Split`, `function` and `polars`, not the engine, so a task's module must not build its job when it is imported: write the job in a module of its own that imports the tasks (`jobs.py`), and build that (`moruna build jobs:job`). Each image's process gets `HOME=/tmp`, the one place it may write, and no user site-packages (`PYTHONNOUSERSITE=1`), so nothing at its start looks the user up.

```sh
podman run -i --rm -e MORUNA_PARAMS='{"table": "alerts"}' oci:images:sha256:6f1c... < in.arrows > out.arrows
```

Its manifest carries its provenance as annotations: `org.opencontainers.image.source` and `org.opencontainers.image.revision` (from git, when the code is in a repository), `org.moruna.source.dirty` (whether a bundled file differed from that revision), `org.moruna.entry`, `org.moruna.build.key`, `org.moruna.lock.digest` and `org.moruna.files`, each bundled file with its sha256.

## The port protocol

A need of an image task is a port: a named request and answer that the runner serves. `moruna.run(job, ports=..., sandbox_cgroup=...)` serves each need of an image task with what the resolver supplies for it, a callable taking the request's bytes and returning the answer's (or an object with `call(request)`); `moruna run JOB --resolver MODULE:ATTRIBUTE` does the same. Under the host protocol a need with no handler in the program goes to the host's peer.

The process calls every port over the one connected stream socket `MORUNA_PORT_SOCKET` names, one call at a time: it writes a request and reads its answer before it writes the next.

```text
request   "MRP1"   u16 name length   name (UTF-8)   u64 body length   body
answer    u8 status   u64 length   bytes
```

Integers are little-endian. Status 0 is the port's answer, its bytes as the port gave them; status 1 is a refusal, its bytes the reason in UTF-8: the port refused, the name is not one of the task's ports, the request is larger than 64 MiB, or the call's deadline passed. A port that answers with batches answers with an Arrow IPC stream. The process ends the conversation by closing its end, and anything that is not a request ends it too.

Inside an image Moruna builds, `self.ports[name]` is a client of that port: `call(request: bytes) -> bytes`, `batches(request)` for an answer that is an Arrow IPC stream, and `table(request)`. A refusal raises an error naming the port. In a run of this process the task is handed what the resolver supplies, as it was in 0.6.2, so a task that runs both ways asks its port by `call`.

The report counts, for each task and each need, the calls, the bytes in and out, and the time they took (see [The run report](run-report.md)).
