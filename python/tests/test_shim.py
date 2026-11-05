"""BD-T1 to BD-T6 (SDD 30 k): the shim against the contract of SDD 19 f.6.

The shim is run as an image runs it, as a child process with the Arrow stream on its standard
input and output, from a tree laid out as layer 3 is (``/opt/moruna/shim.py`` and the stand-in
``moruna`` package beside it). No engine is built or imported: the kernels' modules do
``import moruna`` and find the stand-in.
"""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import textwrap
import threading
from pathlib import Path

import builder_support as support
import pyarrow as pa
import pytest

build = support.load("_build.py", "moruna_build")
shim = support.load("_shim.py", "moruna_shim")

SCHEMA = pa.schema([("id", pa.int64())])

# The end-of-stream marker of the Arrow IPC stream format: a continuation and a zero length.
END = b"\xff\xff\xff\xff\x00\x00\x00\x00"


def lay_out_layer_three(root: Path) -> Path:
    """Layer 3's files written out as the image has them; the directory holding ``shim.py``."""
    build.stage_shim(root / "opt" / "moruna")
    return root / "opt" / "moruna"


def stream(*batches: pa.RecordBatch, schema: pa.Schema = SCHEMA) -> bytes:
    sink = io.BytesIO()
    with pa.ipc.new_stream(sink, schema) as writer:
        for batch in batches:
            writer.write_batch(batch)
    return sink.getvalue()


def batch(*values: int) -> pa.RecordBatch:
    return pa.RecordBatch.from_arrays([pa.array(values, pa.int64())], schema=SCHEMA)


@pytest.fixture
def image(tmp_path: Path) -> Path:
    """The tree of layers 3 and 4: the shim, the stand-in and an ``/app``."""
    lay_out_layer_three(tmp_path)
    (tmp_path / "app").mkdir()
    return tmp_path


def run_shim(
    image: Path, entry: str, stdin: bytes, **extra: str
) -> subprocess.CompletedProcess[bytes]:
    """The shim as the image's command runs it: its environment is the image's, so the stand-in
    ``moruna`` (beside the script) is found and the machine's own is not; the entry, the role,
    the params and the arguments in the environment, as the runner gives them."""
    env = {
        "PATH": os.environ.get("PATH", ""),
        "PYTHONPATH": f"{image / 'app'}",
        "PYTHONDONTWRITEBYTECODE": "1",
        "MORUNA_ENTRY": entry,
        **extra,
    }
    return subprocess.run(
        [sys.executable, str(image / "opt" / "moruna" / "shim.py")],
        input=stdin,
        capture_output=True,
        env=env,
        cwd=image / "app",
        check=False,
        timeout=60,
    )


def kernel_module(image: Path, name: str, source: str) -> str:
    (image / "app" / f"{name}.py").write_text(textwrap.dedent(source))
    return name


def read_all(data: bytes) -> list[pa.RecordBatch]:
    return list(pa.ipc.open_stream(io.BytesIO(data)))


ADD_COLUMN = """
    import moruna
    import pyarrow as pa
    import pyarrow.compute as pc

    @moruna.function(input_schema={"id": "int64"}, output_schema={"adds": {"doubled": "int64"}})
    def add_doubled(batch):
        return batch.append_column("doubled", pc.multiply(batch.column("id"), 2))
"""


def test_bd_t1_one_batch_out_for_each_batch_in_in_order(image: Path) -> None:
    """BD-T1 a_batch_in_is_a_batch_out: a schema, N batches, the marker; N answers, in order,
    each the function's output for its own input; exit 0."""
    entry = kernel_module(image, "kernel_a", ADD_COLUMN) + ":add_doubled"
    inputs = [batch(1, 2, 3), batch(), batch(10), batch(4, 5)]
    done = run_shim(image, entry, stream(*inputs))
    assert done.returncode == 0, done.stderr.decode()
    out = read_all(done.stdout)
    assert [b.num_rows for b in out] == [3, 0, 1, 2]
    assert [b.column("id").to_pylist() for b in out] == [[1, 2, 3], [], [10], [4, 5]]
    assert [b.column("doubled").to_pylist() for b in out] == [[2, 4, 6], [], [20], [8, 10]]
    assert out[0].schema.names == ["id", "doubled"]
    assert done.stdout.endswith(END), "the stream ends with its marker"
    assert pa.ipc.open_stream(io.BytesIO(done.stdout)).read_all().num_rows == 6


def test_bd_t2_no_input_writes_nothing(image: Path) -> None:
    """BD-T2 an_input_with_no_byte_writes_no_byte: a run that applied no batch ends the input
    without a byte; the shim writes nothing and exits 0. A schema and no batch writes nothing
    either: the output schema is not known until the function has answered."""
    entry = kernel_module(image, "kernel_a", ADD_COLUMN) + ":add_doubled"
    done = run_shim(image, entry, b"")
    assert (done.returncode, done.stdout) == (0, b"")
    done = run_shim(image, entry, stream())
    assert (done.returncode, done.stdout) == (0, b"")
    assert done.stderr == b""


def test_bd_t3_an_error_ends_with_a_status_and_a_message(image: Path) -> None:
    """BD-T3 an_error_is_a_nonzero_exit_and_stderr: the function raises on its second batch; the
    first answer is out, the marker is not, the status is 1 and standard error names the
    batch's cause with the traceback."""
    entry = (
        kernel_module(
            image,
            "kernel_bad",
            """
            def explode(batch):
                if batch.column("id")[0].as_py() == 2:
                    raise ValueError("two is not allowed")
                return batch
            """,
        )
        + ":explode"
    )
    done = run_shim(image, entry, stream(batch(1), batch(2), batch(3)))
    assert done.returncode == 1
    err = done.stderr.decode()
    assert "the function failed" in err and "ValueError: two is not allowed" in err
    assert "Traceback" in err and "kernel_bad.py" in err
    assert not done.stdout.endswith(END), "an error leaves the stream without its marker"
    reader = pa.ipc.open_stream(io.BytesIO(done.stdout))
    assert reader.read_next_batch().column("id").to_pylist() == [1]
    with pytest.raises(StopIteration):
        reader.read_next_batch()  # the second batch was never answered


@pytest.mark.parametrize(
    ("entry", "said"),
    [
        ("nope", "is not <module>:<name>"),
        ("no_such_module_anywhere:f", "the module does not import"),
        ("kernel_a:missing", "kernel_a has no missing"),
    ],
)
def test_bd_t3_an_entry_that_is_not_a_function_is_named(image: Path, entry: str, said: str) -> None:
    kernel_module(image, "kernel_a", ADD_COLUMN)
    done = run_shim(image, entry, stream(batch(1)))
    assert done.returncode == 1 and said in done.stderr.decode()
    assert done.stdout == b""


def test_bd_t3_a_return_that_is_no_batch_is_named(image: Path) -> None:
    entry = kernel_module(image, "kernel_ret", "def f(batch):\n    return [1, 2]\n") + ":f"
    done = run_shim(image, entry, stream(batch(1)))
    assert done.returncode == 1
    assert "returned list for batch 0" in done.stderr.decode()


def test_bd_t3_input_that_is_not_arrow_is_a_failure(image: Path) -> None:
    entry = kernel_module(image, "kernel_a", ADD_COLUMN) + ":add_doubled"
    done = run_shim(image, entry, b"this is not an arrow stream at all, just text\n" * 4)
    assert done.returncode == 1
    assert done.stderr, "the reason is on standard error"


def test_bd_t4_the_function_cannot_corrupt_the_stream(image: Path) -> None:
    """BD-T4 print_goes_to_standard_error: a function that prints, and one that reads standard
    input, can neither add to the output stream nor take the input's bytes."""
    entry = (
        kernel_module(
            image,
            "kernel_noisy",
            """
            import sys

            def noisy(batch):
                print("a line on stdout")
                sys.stdout.write("more")
                leftover = sys.stdin.read()
                assert leftover == "", leftover
                return batch
            """,
        )
        + ":noisy"
    )
    done = run_shim(image, entry, stream(batch(1, 2), batch(3)))
    assert done.returncode == 0, done.stderr.decode()
    assert [b.column("id").to_pylist() for b in read_all(done.stdout)] == [[1, 2], [3]]
    assert b"a line on stdout" in done.stderr


def test_bd_t5_a_table_is_one_batch_and_an_empty_one_is_an_empty_batch(image: Path) -> None:
    entry = (
        kernel_module(
            image,
            "kernel_table",
            """
            import pyarrow as pa

            def as_table(batch):
                table = pa.Table.from_batches([batch, batch])
                return table.slice(0, 0) if batch.num_rows == 0 else table
            """,
        )
        + ":as_table"
    )
    done = run_shim(image, entry, stream(batch(1, 2), batch()))
    assert done.returncode == 0, done.stderr.decode()
    out = read_all(done.stdout)
    assert [b.num_rows for b in out] == [4, 0], "one batch for each input, never two"


def test_bd_t5_a_stateful_kernel_is_set_up_once_and_called_with_its_state(image: Path) -> None:
    entry = (
        kernel_module(
            image,
            "kernel_state",
            """
            import moruna
            import pyarrow as pa

            class Running:
                def setup(self, ctx):
                    assert (ctx.instance, ctx.device) == (0, None)
                    return {"seen": 0}

                def __call__(self, state, batch):
                    state["seen"] += batch.num_rows
                    return batch.append_column("seen", pa.array([state["seen"]] * batch.num_rows))

            running = moruna.function(Running(), stateful=True)
            """,
        )
        + ":running"
    )
    done = run_shim(image, entry, stream(batch(1, 2), batch(3)))
    assert done.returncode == 0, done.stderr.decode()
    assert [b.column("seen").to_pylist() for b in read_all(done.stdout)] == [[2, 2], [3]]


def test_bd_t5_an_undecorated_function_runs_too(image: Path) -> None:
    entry = kernel_module(image, "kernel_plain", "def f(batch):\n    return batch\n") + ":f"
    done = run_shim(image, entry, stream(batch(7)))
    assert done.returncode == 0 and read_all(done.stdout)[0].column("id").to_pylist() == [7]


def test_bd_t5_the_stand_in_refuses_the_rest_of_moruna(image: Path) -> None:
    entry = (
        kernel_module(image, "kernel_run", "import moruna\nmoruna.run\n\ndef f(b):\n    return b\n")
        + ":f"
    )
    done = run_shim(image, entry, stream(batch(1)))
    assert done.returncode == 1
    assert (
        "moruna.run" in done.stderr.decode()
        and "holds only Task, Source, Function, Sink, Split, function and polars"
        in done.stderr.decode()
    )


def test_bd_t5_usage(image: Path) -> None:
    done = subprocess.run(
        [sys.executable, str(image / "opt" / "moruna" / "shim.py"), "an:entry"],
        capture_output=True,
        input=b"",
        check=False,
        timeout=60,
    )
    assert done.returncode == shim.EXIT_USAGE and b"usage" in done.stderr
    done = subprocess.run(
        [sys.executable, str(image / "opt" / "moruna" / "shim.py")],
        capture_output=True, input=b"", check=False, timeout=60,
        env={"PATH": os.environ.get("PATH", "")},
    )  # fmt: skip
    assert done.returncode == shim.EXIT_FAILED and b"MORUNA_ENTRY names no entry" in done.stderr


def test_bd_t6_serve_reads_what_a_pipe_gives_one_message_at_a_time() -> None:
    """BD-T6 the_shim_answers_before_the_input_ends: ``serve`` flushes each answer, so an engine
    that writes the next batch only after reading this answer does not wait for ever."""
    read_end, write_end = os.pipe()
    source = os.fdopen(read_end, "rb")
    sink_read, sink_write = os.pipe()
    sink = os.fdopen(sink_write, "wb")
    out_reader = os.fdopen(sink_read, "rb")
    writer = pa.ipc.new_stream(os.fdopen(write_end, "wb", buffering=0), SCHEMA)

    done: list[int] = []
    thread = threading.Thread(target=lambda: done.append(shim.serve(lambda b: b, source, sink)))
    thread.start()
    writer.write_batch(batch(1, 2))
    reply = pa.ipc.open_stream(out_reader)
    assert reply.read_next_batch().column("id").to_pylist() == [1, 2], "answered before the end"
    writer.write_batch(batch(3))
    assert reply.read_next_batch().column("id").to_pylist() == [3]
    writer.close()
    thread.join(timeout=30)
    assert done == [2] and not thread.is_alive()
    with pytest.raises(StopIteration):
        reply.read_next_batch()
    for handle in (source, sink, out_reader):
        handle.close()


# --- in process: the same functions, where a coverage tool can see them -------------------------


def buffered(data: bytes) -> io.BufferedReader:
    return io.BufferedReader(io.BytesIO(data))  # type: ignore[arg-type]


def module_on_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, name: str, source: str) -> None:
    (tmp_path / f"{name}.py").write_text(textwrap.dedent(source))
    monkeypatch.syspath_prepend(str(tmp_path))
    monkeypatch.delitem(sys.modules, name, raising=False)


def test_bd_t6_main_in_process(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    module_on_path(tmp_path, monkeypatch, "inproc_ok", "def f(batch):\n    return batch\n")
    sink = io.BytesIO()
    monkeypatch.setattr(
        shim, "take_streams", lambda: (buffered(stream(batch(1), batch(2, 3))), sink)
    )
    monkeypatch.setenv("MORUNA_ENTRY", "inproc_ok:f")
    assert shim.main([]) == shim.EXIT_DONE
    assert [b.num_rows for b in read_all(sink.getvalue())] == [1, 2]


def test_bd_t6_main_names_what_went_wrong(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    module_on_path(tmp_path, monkeypatch, "inproc_bad", "def f(batch):\n    raise KeyError('k')\n")
    monkeypatch.setattr(shim, "take_streams", lambda: (buffered(stream(batch(1))), io.BytesIO()))
    monkeypatch.setenv("MORUNA_ENTRY", "inproc_bad:f")
    assert shim.main([]) == shim.EXIT_FAILED
    assert "the function failed" in capsys.readouterr().err
    monkeypatch.setenv("MORUNA_ENTRY", "inproc_bad")
    assert shim.main([]) == shim.EXIT_FAILED
    assert "is not <module>:<name>" in capsys.readouterr().err
    assert shim.main(["x:y"]) == shim.EXIT_USAGE and "usage" in capsys.readouterr().err


def test_bd_t6_resolve_and_as_batch(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    module_on_path(
        tmp_path,
        monkeypatch,
        "inproc_kinds",
        """
        import types

        not_callable = 5
        spec = types.SimpleNamespace(wrapped=5, stateful=False)
        stateful_without_setup = types.SimpleNamespace(wrapped=lambda s, b: b, stateful=True)
        """,
    )
    for entry, said in [
        ("inproc_kinds:not_callable", "int, which is not callable"),
        ("inproc_kinds:spec", "SimpleNamespace, which is not callable"),
        ("inproc_kinds:stateful_without_setup", "stateful and has no setup"),
    ]:
        with pytest.raises(shim.ShimError, match=said):
            shim.resolve(entry)
    for entry, said in [
        ("no_such_inproc_module:f", "the module does not import"),
        ("inproc_kinds:absent", "inproc_kinds has no absent"),
        ("nocolon", "is not <module>:<name>"),
    ]:
        with pytest.raises(shim.ShimError, match=said):
            shim.resolve(entry)
    assert shim.as_batch(batch(1), 0).num_rows == 1
    table = pa.Table.from_batches([batch(1), batch(2)])
    assert shim.as_batch(table, 0).num_rows == 2
    assert shim.as_batch(table.slice(0, 0), 0).num_rows == 0
    with pytest.raises(shim.ShimError, match="returned NoneType for batch 4"):
        shim.as_batch(None, 4)


def test_bd_t6_serve_with_nothing_to_answer() -> None:
    sink = io.BytesIO()
    assert shim.serve(lambda b: b, buffered(b""), sink) == 0
    assert shim.serve(lambda b: b, buffered(stream()), sink) == 0
    assert sink.getvalue() == b""


def test_bd_t6_a_stateful_object_resolves_to_a_call_with_its_state(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    module_on_path(
        tmp_path,
        monkeypatch,
        "inproc_state",
        """
        import types

        class Doubler:
            def setup(self, ctx):
                return 2

            def __call__(self, state, batch):
                return state

        kernel = types.SimpleNamespace(wrapped=Doubler(), stateful=True)
        """,
    )
    assert shim.resolve("inproc_state:kernel")(batch(1)) == 2


@pytest.mark.parametrize("explicit_workdir", [False, True])
def test_image_workdir_precedes_user_import_and_preserves_relative_assets(
    image: Path, explicit_workdir: bool
) -> None:
    """The generated command starts in /, then genuinely enters /app before user code."""
    (image / "app" / "bias.txt").write_text("7")
    entry = kernel_module(
        image,
        "relative_assets",
        """
        from pathlib import Path
        import moruna
        import pyarrow as pa
        import pyarrow.compute as pc

        IMPORT_BIAS = int(Path("bias.txt").read_text())

        @moruna.function
        def apply(batch):
            bias = IMPORT_BIAS + int(Path("bias.txt").read_text())
            return pa.RecordBatch.from_arrays([pc.add(batch.column("id"), bias)], schema=batch.schema)
        """,
    ) + ":apply"
    args = [sys.executable, str(image / "opt" / "moruna" / "shim.py")]
    if explicit_workdir:
        args += ["--workdir", str(image / "app")]
    done = subprocess.run(
        args,
        input=stream(batch(1, 2), batch(3)),
        capture_output=True,
        env={"PATH": os.environ.get("PATH", ""), "PYTHONPATH": str(image / "app"),
             "PYTHONDONTWRITEBYTECODE": "1", "MORUNA_ENTRY": entry},
        cwd="/" if explicit_workdir else image / "app",
        check=False,
        timeout=60,
    )
    assert done.returncode == shim.EXIT_DONE, done.stderr.decode()
    assert [b.column("id").to_pylist() for b in read_all(done.stdout)] == [[15, 16], [17]]
    assert done.stdout.endswith(END)


def test_explicit_workdir_failure_is_not_a_completed_stream(image: Path) -> None:
    done = subprocess.run(
        [sys.executable, str(image / "opt" / "moruna" / "shim.py"), "--workdir",
         str(image / "absent")],
        input=stream(batch(1)), capture_output=True, check=False, timeout=60,
        env={"PATH": os.environ.get("PATH", ""), "MORUNA_ENTRY": "relative_assets:apply"},
    )
    assert done.returncode == shim.EXIT_FAILED
    assert b"FileNotFoundError" in done.stderr
    assert done.stdout == b""


TASKS = """
    import json
    import moruna
    import pyarrow as pa

    ROWS = pa.schema([("id", pa.int64())])


    class Numbers(moruna.Source):
        \"\"\"`rows` rows of `id`, in splits of `split`.\"\"\"

        gives = ROWS

        def __init__(self, rows: int, split: int = 4) -> None:
            self.rows, self.split = rows, split

        def plan(self):
            count = -(-self.rows // self.split)
            return [moruna.Split(i, min(self.split, self.rows - i * self.split)) for i in range(count)]

        def read(self, split_id, start, end):
            first = split_id * self.split + start
            return pa.record_batch({"id": pa.array(range(first, first + end - start), pa.int64())})


    class Offset(moruna.Function):
        needs = {"warehouse.read": "the warehouse, read only"}

        def __init__(self, by: int) -> None:
            self.by = by
            self.seen = 0

        def __call__(self, batch):
            self.seen += batch.num_rows
            extra = int(self.ports["warehouse.read"].call(b"orders"))
            window = self.args["window"]
            ids = [i + self.by + extra + window for i in batch.column("id").to_pylist()]
            return pa.record_batch({"id": pa.array(ids, pa.int64())})

        def finish(self):
            return pa.record_batch({"id": pa.array([-self.seen], pa.int64())})


    class Keep(moruna.Sink):
        takes = ROWS

        def __init__(self) -> None:
            self.rows = 0

        def write(self, batch):
            self.rows += batch.num_rows

        def finish(self):
            return {"snapshot_id": self.rows, "version": "v1"}
"""


def test_a_source_writes_every_row_of_its_plan(image: Path) -> None:
    """MORUNA_ROLE=source: no input, every split read by position, one stream out."""
    entry = kernel_module(image, "roles", TASKS) + ":Numbers"
    done = run_shim(image, entry, b"", MORUNA_ROLE="source", MORUNA_PARAMS='{"rows": 10}')
    assert done.returncode == 0, done.stderr.decode()
    assert sum(b.num_rows for b in read_all(done.stdout)) == 10
    assert [b.num_rows for b in read_all(done.stdout)] == [4, 4, 2]
    refused = run_shim(image, entry, b"", MORUNA_ROLE="source", MORUNA_PARAMS='{"rowz": 1}')
    assert refused.returncode == 1 and b"refused its params ['rowz']" in refused.stderr
    wrong = run_shim(image, entry, b"", MORUNA_ROLE="actor")
    assert wrong.returncode == 1 and b"MORUNA_ROLE is 'actor'" in wrong.stderr


def test_a_sink_writes_back_its_commit(image: Path) -> None:
    entry = kernel_module(image, "roles", TASKS) + ":Keep"
    done = run_shim(image, entry, stream(batch(1, 2), batch(3)), MORUNA_ROLE="sink")
    assert done.returncode == 0, done.stderr.decode()
    assert json.loads(done.stdout) == {"snapshot_id": 3, "version": "v1"}
    other = pa.RecordBatch.from_arrays([pa.array(["x"])], names=["id"])
    bad = run_shim(image, entry, stream(other, schema=other.schema), MORUNA_ROLE="sink")
    assert bad.returncode == 1 and b"Keep takes" in bad.stderr


def test_a_function_has_its_params_arguments_ports_and_finish(image: Path) -> None:
    """The params construct it, the arguments are self.args, each need is a port called over
    the port protocol on the socket the runner gives, and finish's batch comes last."""
    import socket as socket_module  # noqa: PLC0415

    entry = kernel_module(image, "roles", TASKS) + ":Offset"
    task_end, runner_end = socket_module.socketpair()
    calls: list[tuple[str, bytes]] = []

    def serve() -> None:
        with runner_end:
            while True:
                head = runner_end.recv(6, socket_module.MSG_WAITALL)
                if not head:
                    return
                assert head[:4] == b"MRP1"
                name = runner_end.recv(int.from_bytes(head[4:6], "little"), socket_module.MSG_WAITALL)
                size = int.from_bytes(runner_end.recv(8, socket_module.MSG_WAITALL), "little")
                body = runner_end.recv(size, socket_module.MSG_WAITALL) if size else b""
                calls.append((name.decode(), body))
                answer = b"100"
                runner_end.sendall(b"\x00" + len(answer).to_bytes(8, "little") + answer)

    server = threading.Thread(target=serve)
    server.start()
    env = {
        "PATH": os.environ.get("PATH", ""), "PYTHONPATH": str(image / "app"),
        "MORUNA_ENTRY": entry, "MORUNA_ROLE": "function", "MORUNA_PARAMS": '{"by": 1}',
        "MORUNA_ARGS": '{"window": 1000}', "MORUNA_PORTS": '["warehouse.read"]',
        "MORUNA_PORT_SOCKET": f"fd:{task_end.fileno()}",
    }  # fmt: skip
    done = subprocess.run(
        [sys.executable, str(image / "opt" / "moruna" / "shim.py")],
        input=stream(batch(1, 2), batch(3)), capture_output=True, env=env, check=False,
        timeout=60, pass_fds=(task_end.fileno(),),
    )  # fmt: skip
    task_end.close()
    server.join(timeout=30)
    assert done.returncode == 0, done.stderr.decode()
    out = [b.column("id").to_pylist() for b in read_all(done.stdout)]
    assert out == [[1102, 1103], [1104], [-3]], "one batch per batch in, then finish's"
    assert calls == [("warehouse.read", b"orders")] * 2


def test_a_port_that_refuses_and_a_task_without_a_socket(image: Path) -> None:
    entry = kernel_module(image, "roles", TASKS) + ":Offset"
    done = run_shim(
        image, entry, stream(batch(1)), MORUNA_PARAMS='{"by": 1}', MORUNA_PORTS='["warehouse.read"]'
    )
    assert done.returncode == 1 and b"MORUNA_PORT_SOCKET names no socket" in done.stderr
    connection = shim._Socket("tcp:1")
    with pytest.raises(shim.ShimError, match="fd:<descriptor> or unix:<path>"):
        connection.call("x", b"")
