# The Python shim for ring mode

`moruna_shim` runs a Python batch function as an image stage's task on the Moruna data OS,
with no copy and no call per batch between the engine and the task
(`architecture/data-os.md` section 5). It is image-side code, carried in the image's layer;
it is not part of Moruna's runtime.

## How a task runs

- The engine pushes the task's input region on slot 0 and its rings object on slot 1, once.
  The shim maps each once (`batch_get`).
- Each batch the engine posts on the input ring is read in place as a `pyarrow.RecordBatch`
  (`pyarrow.foreign_buffer` over the region).
- The batch the function returns is laid out as a data-plane record (contracts e.7) in a
  window of the rings object and posted on the output ring.
- The shim sleeps only when its input ring is empty (after reading it a while) or no window
  is free, on its bell: a futex word of the rings object the engine raises after each post
  and wakes only when the shim said it waits. After each record it raises the engine's bell,
  and wakes the engine only when the engine sleeps on it. The kernel keys a futex on a shared
  memory object by the object and the offset, so each wake reaches the other process.
- While the kernel cannot take write access back from a window itself, the shim hands the
  rings object back with `batch_put` once it has posted a group, and maps the next.

One interpreter per process; a stage runs as many processes as it asks for.

## Using it in an image

The image's layer carries `moruna_shim` on the interpreter's path and declares the protocol
in its environment, which is how the engine chooses ring mode:

```dockerfile
COPY images/python/moruna_shim /usr/lib/moruna/moruna_shim
ENV PYTHONPATH=/usr/lib/moruna MORUNA_PLANE=rings
CMD ["python3", "-m", "moruna_shim", "mypackage.kernels:score"]
```

The command is exec form, a JSON array: the engine resolves its first word against the
image's `PATH` and the personality execs it. Never the shell form (`CMD python3 -m ...`),
which is `sh -c`: the data OS never runs a shell (`architecture/data-os.md` §3a). The shim
itself starts no process and no shell; a test checks its sources for it.

`MODULE:FUNCTION` names a function that takes a `pyarrow.RecordBatch` and returns one. An
image that does not declare `MORUNA_PLANE=rings` is served on its descriptors 0 and 1 as an
Arrow IPC stream, which copies, as before.

`MORUNA_SHIM_SPINS` sets how many times the shim reads the input ring before it waits
(20,000 by default).

## Tests

On a workstation:

```bash
cd images/python && python3 -m unittest discover -s tests
```

`tests/fixtures/record.bin` is a record the shim writes, which the engine's check passes
(`moruna-plane`'s `p_the_python_shim_speaks_the_engine_s_records_and_rings`), and
`tests/fixtures/ring-golden.json` is the rings layout the engine writes, which the shim's tests
read.
