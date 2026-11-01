# Moruna SDD 30: The Python image builder (`moruna build`, `moruna.build`; `python/moruna/_build.py`, `_shim.py`, `_standin.py`)

**Document type:** software design document, component 30
**Status:** DRAFT · 2026-10-29 (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** `BOARD.md` item KA3 (the Python builder: `@moruna.kernel` to a four-layer image); `architecture/optimized-runtime.md` section 7 (images: the four layers and the two ways in)
**Preamble:** `00-preamble.md`; **Cited:** 12 (the Python surface: `moruna.kernel`, `KernelSpec`), 16 (the `.pex`, which this generalises), 19 (the `image` kind: the layout it reads, f.6 the stream contract), 27 (the layer files an image converts to)
**Component location:** `python/moruna/_build.py` (the builder and the `moruna build` command), `python/moruna/_shim.py` (layer 3's entry), `python/moruna/_standin.py` (layer 3's `moruna`), `python/moruna/__init__.py` (`moruna.build`), `examples/image_job.rs` and `examples/image_kernel/`, `python/tests/test_shim.py`, `test_build.py`, `test_build_image.py`
**Consumes:** `uv` (the installer), CPython 3.14 and `pyarrow` (the builder's own interpreter), the official `python:3.14-slim` image (a registry, once, by digest). **Consumed by:** the `image` kind (SDD 19), the converter and the microkernel (SDD 27, 28), KD3 (conformance across kinds).

**Decisions worth your eye:** (1) an image is four layers and three of them are tars the builder writes byte for byte the same every time, uncompressed, with the `.pyc` of every source beside it (hash based, written by an interpreter that matches the runtime's, so nothing compiles at a start), so a layer's digest is the sha256 of its tar and is also its diff id; (2) layer 1 is never built: it is the official Python image's own layers, taken by digest from a pinned index, which is what lets any two Python images share it on a node; (3) nothing is resolved: a build installs exactly the wheels a lock pins, with hashes, for the image's platform, and refuses without a lock; (4) the shim is one file that needs only `pyarrow`, and the module a kernel's author wrote, which does `import moruna`, runs in the image with no engine because layer 3 carries a stand-in `moruna` of two names; (5) the builder imports no engine code, so it runs and is tested with no wheel built.

---

## a. Purpose and boundary

Turn a Python function decorated with `@moruna.kernel` (or a plain function) into an OCI image the `image` kind runs on a Linux node (SDD 19) and the Moruna kernel runs after the converter (SDD 27, 28): one command, `moruna build <module>:<function>`, or `moruna.build(fn)`. The user never sees a Dockerfile. This component owns: reading a kernel's declaration; reading a lock; fetching the runtime image by digest; installing the locked wheels for a platform; writing the layers, the config, the manifest and the layout; the shim and the stand-in `moruna` that run in the image.

It refuses to know: any language but Python (a second language is a second builder with its own layer 1 and shim, o), a registry to push to (the output is an OCI layout directory), the engine (`moruna._core` is never imported by the builder, only by the command that loads a user's module), how an image is run or confined (SDD 19), and resolution: no version is ever chosen here.

## b. Vocabulary

**Lock.** A `uv.lock` or a `requirements.lock`: every dependency pinned to an exact version with the hashes of its files. **Runtime.** `python:3.14-slim`, by digest. **Shim.** The program the image runs. **Stand-in.** The `moruna` package inside an image. **Layout.** An OCI image layout directory (SDD 19 e.1). **Function.** What a build needs of a kernel: the module's dotted name, the attribute, the file, the files it names.

## c. Invariants

**BD-I1. A layer is content addressed.** Layers 2 to 4 are uncompressed tars; a layer's digest is the sha256 of its tar and its diff id is the same string. Layer 1 keeps the digests the official image publishes.

**BD-I2. A build is reproducible.** The same lock, platform, runtime, shim and function give bit for bit the same layers, bytecode included, config, manifest and `index.json`, whatever the working directory, the times and modes of the source files, the order they are read in or the machine's own interpreter patch version. Nothing in an image names a time, a user, a path of the build machine or an order the file system chose.

**BD-I3. Layers are shared by what they hold.** Two functions with one lock have layers 1 to 3 in common and differ in layer 4; a change of lock changes layer 2 and no other; a change of function changes layer 4 and no other. A layout that holds both holds each layer once.

**BD-I4. Nothing is resolved.** A build installs the wheels a lock pins and checks each against its hashes; it never chooses a version, never builds a source distribution and never reads an index to decide what to install. A project with no lock is refused, saying a lock is needed, before anything is fetched or written.

**BD-I5. Platform is the image's, never the machine's.** Wheels and environment markers are evaluated for the image's platform (`linux/aarch64` or `linux/x86_64`), whatever machine builds it.

**BD-I6. Every byte is checked against its name.** The runtime's index, manifest, config and layers are hashed as they are fetched or read and refused when they are not what their digest says; a blob already in the layout or the cache is used only when it hashes right.

**BD-I7. The shim keeps the stream.** The shim writes nothing to standard output but the answer stream, and reads nothing from standard input but the request stream: a function that prints or reads cannot corrupt either (SDD 19 f.6).

**BD-I8. A failure is a status and a message.** The shim ends with status 1 and a message on standard error for any failure, and never writes the end-of-stream marker after one; the builder ends with status 2 and a sentence naming the cause.

## d. Interfaces

```python
moruna.build(fn, *, out="moruna-image", tag=None, platform=None, lock=None, files=(),
             runtime_layout=None, runtime_reference=None, cache=None,
             index_url=None, find_links=(), no_index=False) -> Built
```

```text
moruna build <module>:<function> [--out DIR] [--tag NAME] [--platform linux/aarch64|linux/x86_64]
             [--lock PATH] [--file PATH]... [--runtime-layout DIR --runtime-reference REF]
             [--cache DIR] [--index-url URL] [--find-links DIR]... [--no-index] [--json]
```

`<module>` is a dotted name or a path to a `.py` file, as `moruna check` takes it. `Built` has `layout`, `reference`, `digest` (the image's: the manifest's), `layers` (each with `role`, `blob.digest` and `blob.size`), `lock` and `platform`; `report()` is the `--json` document. The command prints the image's digest and one line for each layer of the manifest: its position, its role (`runtime`, `dependencies`, `shim`, `function`), its digest and its size. `moruna.build` raises `moruna.ConfigError` for a refusal; the command exits 2.

`python/moruna/_shim.py` is `shim.py <module>:<name>`; `_standin.py` is the module `moruna` of an image.

## e. Data model, formats and state machines

### e.1 The image

```text
layer 1  runtime       python:3.14-slim's layers (four), gzip, as published
layer 2  dependencies  opt/moruna/site-packages/...        the lock's wheels, installed, with their bytecode
layer 3  shim          opt/moruna/shim.py                  _shim.py
                       opt/moruna/moruna/__init__.py       _standin.py
                       opt/moruna/moruna/_declare.py       _declare.py
layer 4  function      app/<module path>.py                the defining file, with its bytecode
                       app/<package>/__init__.py           each package above it
                       app/<path>                          each file it names
```

Layers 2 to 4 have the media type `application/vnd.oci.image.layer.v1.tar`. The config:

```json
{"architecture": "arm64|amd64", "os": "linux",
 "config": {"Env": ["<the runtime's>", "PYTHONPATH=/app:/opt/moruna/site-packages",
                    "PYTHONDONTWRITEBYTECODE=1"],
            "Cmd": ["/usr/local/bin/python3", "/opt/moruna/shim.py", "<module>:<name>"],
            "WorkingDir": "/app",
            "Labels": {"org.opencontainers.image.base.name": "docker.io/library/python:3.14-slim",
                       "org.opencontainers.image.base.digest": "sha256:<the platform's manifest>",
                       "org.moruna.entry": "<module>:<name>", "org.moruna.stateful": "true|false",
                       "org.moruna.fingerprint": "<the kernel's, when it has one>",
                       "org.moruna.version": "<the building Moruna's>"}},
 "rootfs": {"type": "layers", "diff_ids": ["..."]},
 "history": [{"created_by": "moruna build: <role>", "comment": "<layer digest>"}]}
```

There is no `created` and no entrypoint: the command is `Cmd`, which the `image` kind reads (SDD 19 d.1). The layout is `oci-layout`, `index.json` (the image under `org.opencontainers.image.ref.name`, default `<function>:latest`) and `blobs/sha256/`. A layout that exists is added to: an image of the same name is replaced, the rest are kept, and a blob that is there is not written again.

### e.2 The lock

A `requirements.lock` is pip's format: every requirement `name==version`, optionally `; marker`, with at least one `--hash=sha256:<64 hex>` on each and on every hash; comments and continuations are read; an option line (`--index-url`) is refused. A `uv.lock` is read through `uv export --frozen --no-dev --no-emit-project --no-emit-workspace`, which gives the same format with the project's runtime dependencies. The lock is `--lock`, else the nearest `uv.lock` or `requirements.lock` in the directory a module is imported from or one above it; a directory with both is refused.

## f. Algorithms and policies

**f.1 A build.** In order: find the lock (BD-I4: refused here, before anything is fetched); read it into pins; add the shim's `pyarrow` (f.2); get the runtime (f.5); write the pins as one requirements file in name order and install it (f.2); find the toolchain (f.2, refused here if there is none); write layers 2, 3 and 4 with their bytecode (f.2, f.4); write the config and the manifest; put every blob in the layout, then `index.json`, atomically.

**f.2 Dependencies and bytecode.** `uv pip install --no-config --python <the toolchain's interpreter> --target <dir> --python-platform <aarch64|x86_64>-manylinux_2_28 --python-version 3.<minor> --no-build --require-hashes --no-deps --link-mode copy -r <pins>` with `UV_COMPILE_BYTECODE=0`, plus the build's `--index-url`, `--find-links` and `--no-index`. `--no-build` is `--only-binary :all:`: a source distribution is refused, so no build backend, no `setup.py` and no other code of a package runs on the build machine; nothing else here executes a package either (uv unpacks wheels). Markers are evaluated for the target. Wheels are installed to a directory and not to an environment, so nothing of the build machine is in it. Afterwards the install's own bookkeeping is taken out: `bin/` (scripts that name this machine's interpreter), uv's `.lock`, each package's `uv_cache.json` (a timestamp) with its line of `RECORD`, and any bytecode a wheel shipped. `pyarrow` is in every image: the lock's own pin when it has one (and it must be 17 or newer, the shim's floor), else the pin of this repository's `uv.lock` (`SHIM_PYARROW`: 25.0.1, with the hashes of its two Linux wheels).

The bytecode of layers 2, 3 (the stand-in `moruna` package, not the shim script, which Python never caches) and 4 is written at build time, so no start of the image compiles what it imports. `python -m compileall -f --invalidation-mode unchecked-hash -d <the path the image has>` over the tree writes each `.pyc` beside its source: hash based and not checked, so the runtime uses it whatever the time of the source, the hash is the source's, and `co_filename` is the path in the image, which is also what a traceback shows; it runs with `PYTHONHASHSEED=0` and no other `PYTHON*` or `UV_*` variable. A source that does not compile is left without one, and importing it fails in the image as it would anywhere. The interpreter is the **toolchain**: one that writes the bytecode the runtime reads, which is CPython, the standard (not free threaded) build, a final release of the runtime's minor, whose magic number is the runtime's. The runtime's minor comes from `PYTHON_VERSION` in its config, which must be a final release (the magic number of a release does not change, and may until it is final); when a layer of the runtime holds a standard library `.pyc` its magic number is read and the interpreter's must equal it. It is this interpreter when it is one, else `python3.<minor>` or `python3` on the path, else the build is refused, before anything is installed or written, saying what it needs and where it looked. Bytecode of another interpreter is never shipped.

**f.3 The shim and the stand-in.** The shim (`shim.py <module>:<name>`) is SDD 19 f.6 and nothing more. It moves descriptors 0 and 1 aside (duplicated for the stream; 0 then reads `/dev/null` and 1 is standard error), so a `print`, or a library, cannot touch the stream. If standard input holds no byte it writes nothing and exits 0. Otherwise it reads the stream one batch at a time, calls the function, and writes the answer as one record batch, flushed (the engine writes the next batch only after it has read this answer); at the end of input it writes the end-of-stream marker and exits 0. An input that has a schema and no batch writes nothing: the output schema is not known before the function has answered. The function's return is a record batch, or a table, which is one batch. Any exception, a bad return, a module that does not import or a name it lacks is a message on standard error and status 1, with no marker. The function is `wrapped` if the object has a `wrapped` (a `KernelSpec`, or the stand-in's), called as `f(batch)`; a stateful kernel has `setup(ctx)` called once (`ctx.instance` 0, `ctx.device` `None`) and is called as `f(state, batch)`. The stand-in `moruna` holds `kernel` and `polars`, which decorate as the engine's do (a function annotated Polars frame to frame is wrapped through `_declare`, which is copied beside it); declarations are accepted and not read, because the engine's decorator checks them when `moruna build` imports the module, and any other name of `moruna` is an `AttributeError` naming it. The script's own directory comes first on `sys.path`, so `import moruna` in an image finds the stand-in.

**f.4 Reproducible tars.** A layer's entries are every path, parents included, in sorted path order; each has time 0, user and group 0 and no names, a directory the mode 0755, a symbolic link 0777, and a file 0644 or, if its source had any execute bit, 0755; the format is PAX with no extended headers beyond what a path needs; file contents are read from the source, and a path made twice, or a source that is a device or a fifo, is refused. The tar is streamed to a file under the build's temporary directory while it is hashed, and moved into place by its digest.

**f.5 The runtime.** `RUNTIME_INDEX` is a digest of the official image's index. The registry client is anonymous: a token is asked for when the registry challenges, redirects are followed with the credential dropped when the host changes, and every body is hashed as it is read and refused when it is not the digest asked for. The index is verified against the pinned digest, the platform's manifest is chosen from it by operating system and architecture (exactly one), then its config and layers, each into the cache (`blobs/sha256/<hex>`, default `~/.cache/moruna/build`); anything already in the cache that hashes right is not fetched. `--runtime-layout` and `--runtime-reference` take the same image from an OCI layout on this machine instead (an air-gapped build, or a test), with the same checks, and the config's operating system and architecture must be the image's platform.

## g. Concurrency within the component

None. A build is sequential, and two builds into one layout are not safe against one another.

## h. Behaviour

Each of these is a refusal, a sentence naming the cause, exit status 2: no lock (and that a lock is needed); both locks; a lock line that is not `name==version` or has no hash or a hash that is not 64 hex; a `pyarrow` pin older than 17; a platform other than the two; no standard CPython 3.14 to read the wheels' tags from; `uv` absent; a wheel the platform has none of, a hash that does not match and anything else `uv` refuses (its last line is quoted); a runtime whose architecture is not the image's, or whose blobs do not hash right; a registry that answers anything but success; a kernel the module does not bind to a name; a named file that does not exist or is not under the directory the module is imported from; an `--out` that holds files and is not a layout.

## i. Configuration

The options of d, and the constants `RUNTIME_INDEX`, `SHIM_PYARROW`, `PYARROW_FLOOR` and the paths of e.1. No environment variable is read except `XDG_CACHE_HOME`, for the default cache.

## j. Observability

The command prints what it built (d). Refusals go to standard error. `uv`'s own output is not shown unless it fails.

## k. Tests

Run with no engine built and no network (`python -m pytest --noconftest python/tests/test_shim.py python/tests/test_build.py`): the shim is run as a child process with the stream on its descriptors, from a tree laid out as layer 3 is; `uv` installs hand-made wheels from a directory for a platform other than the machine's; a tiny OCI layout stands in for the runtime and a fake registry serves the client.

**BD-T1 a_batch_in_is_a_batch_out.** A schema, four batches (one empty), the marker: four answers, in order, each the function's output for its own input, and the marker. BD-I7, f.3.
**BD-T2 an_input_with_no_byte_writes_no_byte.** No byte, and a schema with no batch: no output, status 0.
**BD-T3 an_error_is_a_status_and_a_message.** A function that raises on its second batch: the first answer, no marker, status 1, the traceback and the cause on standard error; an entry that is not `module:name`, a module that does not import, a missing name, a return that is no batch, an input that is not Arrow. BD-I8.
**BD-T4 the_function_cannot_corrupt_the_stream.** A function that prints and reads standard input. BD-I7.
**BD-T5 the_shapes_a_function_takes.** A table is one batch (an empty one is an empty batch); a stateful kernel is set up once and called with its state; an undecorated function; the stand-in refuses the rest of `moruna`; the shim's usage.
**BD-T6 the_shim_answers_before_the_input_ends.** Over pipes: each answer arrives before the next batch is written. f.3.
**BD-T7 no_lock_no_build.** No lock: refused, a lock is needed, nothing fetched or written; two locks; the nearest lock above the module; `--lock`. BD-I4.
**BD-T8 a_lock_pins_every_requirement_with_a_hash.** Loose pins, no hash, a short hash, an option line and an editable line, each refused naming the line; markers, continuations and comments read; the requirements text is in name order. BD-I4, e.2.
**BD-T9 the_shim_has_pyarrow.** Added when the lock lacks it; the lock's own kept; one older than 17 refused; it is in layer 2. f.2.
**BD-T10 the_layout_is_an_image.** The layout's files, every blob named by its bytes, the manifest, the media types, the config (command, environment, working directory, labels, diff ids), and the layers unpacked: layer 1, the shim, the stand-in, `_declare.py`, the function, the site-packages with no `uv_cache.json` or `.lock`.
**BD-T11 the_same_inputs_are_the_same_image.** One function built twice into two directories from two working directories, with the source's time and mode changed between: every digest, `index.json` and every blob equal. BD-I2.
**BD-T12 shared_layers.** Two functions of one lock: layers 1 to 3 equal, layer 4 different, nine blobs in a layout of both, a name rebuilt is the same image and writes nothing. BD-I3.
**BD-T13 a_dependency_change_rebuilds_two.** A changed lock: layers 1, 3 and 4 equal. BD-I3.
**BD-T14 the_target_is_the_images_platform.** Each platform gets its wheel, a `win32` marker is false, the architecture of the config follows; a runtime of the other architecture, a wheel the platform lacks and a hash that does not match are refused. BD-I5.
**BD-T15 a_uv_lock_is_read.** The runtime dependencies uv pins, with hashes and markers, without the project or its development group; a lock uv cannot read is refused with uv's words. e.2.
**BD-T16 the_tar_is_reproducible.** Sorted entries with parents (bytecode is a file like another), time 0, owner 0:0, fixed modes, the same digest for files of other times, modes and order; a path made twice and a fifo refused. BD-I1, BD-I2, f.4.
**BD-T17 layer_four.** The module at its import path, the `__init__.py` of its packages, the files it names; a file outside the import root or missing refused; `/app` always. e.1.
**BD-T18 a_layout_is_written_whole_or_not_at_all.** A directory that is not a layout is not written into; a corrupt blob already in the layout is replaced.
**BD-T19 the_kernel_and_its_runtime.** A kernel is found in its module by the name it is bound to, with its fingerprint and statefulness in the labels; a function in a package is imported as one; the runtime layout's blobs, marker and reference are checked. BD-I6.
**BD-T20 the_command_prints_the_image.** The command builds, and prints the image's digest and every layer's digest and size, as text and as JSON, with the building Moruna's version in the config; a kernel named without a function, a module that does not load, a function it lacks, no lock and a platform of neither kind are each a sentence and status 2.
**BD-T21 the_interpreter.** The toolchain is a standard final CPython of the runtime's minor with its magic number: this one, or one on the path when this is the free threaded build or another; a different minor, a different magic number, a pre-release runtime and a runtime with no `PYTHON_VERSION` are each refused, naming what was needed and where it looked; the runtime's magic number is read from a `.pyc` in its layers when it has one. f.2.
**BD-T22 the_runtime_is_fetched_by_digest.** A fake registry: the token, the pinned index, the platform's manifest only, a redirect that drops the credential, every blob verified and kept by digest, a second build asking the registry for nothing; a tampered blob refused and not kept; a platform the index lacks and a registry error refused. BD-I6, f.5.
**BD-T30 bytecode_is_shipped.** Every importable source of layers 2 to 4 has its `.pyc` beside it for the runtime's tag, with the runtime's magic number, hash based and unchecked, the hash of the source beside it, `co_filename` the path in the image; the shim script has none; bytecode a wheel shipped is not in the layer. BD-I2, f.2.
**BD-T31 bytecode_is_reproducible.** pyarrow's own sources compiled in two directories by two processes under different hash seeds and working directories: every `.pyc` the same bytes (and BD-T11 holds with bytecode in the layers). BD-I2.
**BD-T32 wheels_only.** A pin with only a source distribution is refused, and its `setup.py`, which writes a marker if it is ever run, never is. BD-I4, f.2.
**BD-T23 the_image_converts_to_layer_files.** (needs `MORUNA_LAYER_BIN`) KC2's converter writes a layer file for each layer of an image built over the stand-in runtime.

Linux, the network and a sandbox host (`python/tests/test_build_image.py`; the variables it names; BD-T29 needs root and `chroot`, not the sandbox):
**BD-T24 a_built_image_runs_as_a_kernel.** `moruna build` of `examples/image_kernel/kernel.py:add_doubled`, then the version 2 job `examples/image_job.rs` writes, run by `moruna run`: Parquet in, the function adds a column, Parquet out, every row and every value checked.
**BD-T25 a_failing_function_is_named.** A function that raises: the run ends naming status 1 with the traceback quoted (SDD 19 f.7).
**BD-T26 reproducible_and_shared_for_real.** The real runtime and wheels: the same function built by the command and by `moruna.build` is one image, file for file; two functions of one lock share every layer but the last.
**BD-T29 the_shipped_bytecode_is_the_bytecode_used.** The built image unpacked into a writable root and run by its own CPython with `-v`, without `PYTHONDONTWRITEBYTECODE`: pyarrow, the stand-in `moruna` and the kernel are each imported from the `.pyc` beside the source, and nothing under `/opt` or `/app` is written, which a `.pyc` the interpreter refused would cause. f.2.
**BD-T27 the_real_image_converts_to_layer_files.** The converter over the built layout writes a layer file for each layer.

**BD-T28 the_engines_own_objects.** (needs the built wheel, no network) A real `KernelSpec` is read as a function, with its fingerprint and statefulness; `moruna.build` and the command refuse a project with no lock the same way, before anything is fetched or written.

`examples/image_job.rs` carries its own tests (`cargo test -p moruna-job --example image_job`): the document is a DAG of three tasks, passes `check`, is version 2 and reads back.

## l. Implementation notes for the agent

`_build.py`, `_shim.py` and `_standin.py` import nothing of `moruna._core`; only the command (`main`) and `moruna.build` do, to load a module and to name the version. A change to `_shim.py`, `_standin.py` or `_declare.py` changes layer 3 for every image, which is the point. Layer 1's digest changes only when `RUNTIME_INDEX` does.

## m. Open items

None.

## n. Traceability

| Parent | Invariant | Test |
|---|---|---|
| KA3: build the four layers from a kernel | BD-I1, BD-I3 | BD-T10, BD-T12, BD-T13, BD-T17 |
| KA3: resolve from the lock, refuse without | BD-I4, BD-I5 | BD-T7 to BD-T9, BD-T14, BD-T15 |
| KA3: reproducible layers | BD-I2 | BD-T11, BD-T16, BD-T26 |
| KA3: the shim against SDD 19 f.6 | BD-I7, BD-I8 | BD-T1 to BD-T6, BD-T25 |
| KA3: the `image` kind runs it; the converter reads it | | BD-T24, BD-T27, BD-T23 |
| optimized-runtime 7: layer 1 by digest | BD-I6 | BD-T19, BD-T22 |

## o. Deferred (post-v1)

**BD-O1. Other languages:** a JVM runtime, a static binary; each needs its own layer 1, lock reader and shim. **BD-O2. The builder in its own VM** with one port open, the package resolver (optimized-runtime section 7): today it runs on the machine where the user runs it, as `moruna package` does, and the in-VM builder needs the kernel to run jobs (wave D). What keeps that safe meanwhile is that the dependency step runs no package code: wheels only (`--no-build`), `--require-hashes`, `--no-deps`, no build backend and no sdist (BD-T32); the one program that runs is this builder's own interpreter compiling sources to bytecode, which executes none of them. **BD-O3. Pushing to a registry.** **BD-O4. Imports a function makes of its siblings found by reading its source:** a function names the files it needs with `--file`. **BD-O5. The standard library's bytecode:** layer 1 is the official image, never rebuilt, and whether it holds the `.pyc` of the standard library is for BD-T29's run to show; if it does not, a layer of them is a later item.
