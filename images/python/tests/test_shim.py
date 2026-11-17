"""The shim's tests on a host: records, the ring protocol against a model of the engine's half,
and `serve` end to end over fake native calls. Run from images/python:

    python -m unittest discover -s tests
"""

import ctypes
import json
import os
import struct
import unittest

import pyarrow as pa

from moruna_shim import record, ring, serve
from moruna_shim.plane import Plane

FIXTURES = os.path.join(os.path.dirname(__file__), "fixtures")


def batches():
    yield pa.record_batch({"x": pa.array([0, 1, 2, 3], pa.int64())})
    yield pa.record_batch(
        {
            "n": pa.array([1, None, 3], pa.int32()),
            "s": pa.array(["a", None, "ccc"]),
            "l": pa.array([[1, 2], [], None], pa.list_(pa.int64())),
        }
    )
    yield pa.record_batch({"x": pa.array([], pa.float64())})
    yield pa.record_batch({"big": pa.array(range(100_000), pa.int64())})


def entries(rec):
    """The absolute offsets and lengths of a record's body buffers."""
    messages = record._messages(rec)
    (_, _, _), (b0, b1, _) = messages[0], messages[1]
    fb = memoryview(rec)[b0 + 8 : b1]
    start, count = record._buffers(fb)
    out = []
    for i in range(count):
        off, n = struct.unpack_from("<qq", fb, start + i * record.BUFFER_ENTRY)
        out.append((b1 + off, n))
    return out


class Records(unittest.TestCase):
    def test_a_record_reads_back_and_every_buffer_is_on_a_page(self):
        for batch in batches():
            window = bytearray(16 << 20)
            n = record.write(batch, window)
            rec = bytes(window[:n])
            self.assertEqual(record.read(pa.py_buffer(rec)), batch)
            for at, length in entries(rec):
                if length:
                    self.assertEqual(at % record.PAGE, 0, (at, length))
                    self.assertLessEqual(at + length, n)

    def test_a_record_too_big_for_its_window_is_refused(self):
        batch = pa.record_batch({"x": pa.array(range(10_000), pa.int64())})
        with self.assertRaises(record.RecordError):
            record.write(batch, bytearray(4096))

    def test_the_fixture_the_engine_checks_is_what_the_shim_writes(self):
        batch = pa.record_batch(
            {"n": pa.array([1, None, 3], pa.int32()), "s": pa.array(["a", None, "ccc"])}
        )
        window = bytearray(1 << 20)
        n = record.write(batch, window)
        with open(os.path.join(FIXTURES, "record.bin"), "rb") as f:
            self.assertEqual(f.read(), bytes(window[:n]))


class Engine:
    """The engine's half of the protocol, as kernel/plane/core/src/ring.rs writes it."""

    def __init__(self, words, geometry, mode, generation=0):
        self.w, self.g = words, geometry
        g = geometry
        for at in (ring.IN_HEAD, ring.IN_TAIL, ring.OUT_HEAD, ring.OUT_TAIL, ring.FREE_HEAD,
                   ring.FREE_TAIL, ring.ENGINE_WAITS, ring.TASK_WAITS, ring.CLOSED,
                   ring.ATTACHED, ring.TASK_BELL, ring.ENGINE_BELL, ring.TASK_NOTE):
            words.store(at, 0)
        for at, v in ((ring.VERSION, ring.VERSION_VALUE), (ring.REGION_BYTES, g.region_bytes),
                      (ring.IN_CAP, g.in_cap), (ring.OUT_CAP, g.out_cap),
                      (ring.FREE_CAP, g.free_cap), (ring.WINDOWS, g.windows),
                      (ring.WINDOW_BYTES, g.window_bytes), (ring.WINDOWS_AT, g.windows_at()),
                      (ring.MODE, mode), (ring.GENERATION, generation),
                      (ring.MAGIC, ring.MAGIC_VALUE)):
            words.store(at, v)
        self.in_head = self.out_tail = self.free_head = 0
        self.bell = 0

    def ring(self):
        """Raises the task's bell; whether the task said it waits (the engine wakes it)."""
        self.bell = (self.bell + 1) & 0xFFFFFFFF
        self.w.store(ring.TASK_BELL, self.bell)
        if self.w.load(ring.TASK_WAITS) == 0:
            return False
        self.w.store(ring.TASK_WAITS, 0)
        return True

    def post(self, offset, length, cookie, rows=0):
        at = ring.ENTRIES_AT + (self.in_head % self.g.in_cap) * ring.IN_ENTRY
        struct.pack_into("<QQQQ", self.w.view, at, offset, length, cookie, rows)
        self.in_head += 1
        self.w.store(ring.IN_HEAD, self.in_head)
        self.ring()

    def give(self, n):
        at = self.g.free_entries() + (self.free_head % self.g.free_cap) * ring.FREE_ENTRY
        self.w.store(at, n)
        self.free_head += 1
        self.w.store(ring.FREE_HEAD, self.free_head)
        self.ring()

    def take(self):
        if self.w.load(ring.OUT_HEAD) == self.out_tail:
            return None
        at = self.g.out_entries() + (self.out_tail % self.g.out_cap) * ring.OUT_ENTRY
        self.out_tail += 1
        self.w.store(ring.OUT_TAIL, self.out_tail)
        return struct.unpack_from("<QQQQ", self.w.view, at)

    def close(self):
        self.w.store(ring.CLOSED, 1)
        self.ring()


def geometry(windows=2, window=1 << 20, region=4 << 20):
    return ring.Geometry(region, 64, 64, max(windows, 1), windows, window)


class Protocol(unittest.TestCase):
    def test_the_task_reads_what_the_engine_posts_and_posts_back(self):
        g = geometry()
        w = ring.Words(bytearray(g.object_bytes()))
        e = Engine(w, g, ring.PROTECT)
        e.give(0)
        e.give(1)
        t = ring.Task(w)
        self.assertEqual(w.load(ring.ATTACHED), ring.VERSION_VALUE)
        self.assertIsNone(t.next())
        e.post(4096, 100, 7, 4)
        self.assertEqual(t.next(), (4096, 100, 7, 4))
        self.assertEqual(t.window(), 0)
        self.assertTrue(t.post(0, 0, 100, 7))
        self.assertTrue(t.release())
        self.assertFalse(t.release())
        self.assertEqual(w.load(ring.IN_TAIL), 1)
        self.assertEqual(e.take(), (0, 0, 100, 7))
        self.assertFalse(t.ended())
        e.close()
        self.assertTrue(t.ended())

    def test_several_records_for_one_batch_then_the_release_and_the_exit_code(self):
        g = geometry()
        w = ring.Words(bytearray(g.object_bytes()))
        e = Engine(w, g, ring.PROTECT)
        e.give(0)
        e.give(1)
        t = ring.Task(w)
        e.post(4096, 100, 7, 4)
        t.next()
        for n in (0, 1):
            self.assertEqual(t.window(), n)
            self.assertTrue(t.post(n, 0, 50, 7))
        self.assertEqual(w.load(ring.IN_TAIL), 0)
        self.assertTrue(t.release())
        self.assertEqual((e.take(), e.take()), ((0, 0, 50, 7), (1, 0, 50, 7)))
        woke = []
        w.store(ring.ENGINE_WAITS, 1)
        self.assertTrue(t.exit(-3, woke.append))
        self.assertEqual(woke, [ring.ENGINE_BELL])
        self.assertEqual(w.load(ring.TASK_EXIT), 1)
        self.assertEqual(w.load(ring.TASK_EXIT_CODE), (1 << 64) - 3)

    def test_words_that_could_not_be_true_are_refused(self):
        g = geometry()
        w = ring.Words(bytearray(g.object_bytes()))
        with self.assertRaises(ring.Bad):
            ring.Task(w)
        e = Engine(w, g, ring.UNMAP)
        t = ring.Task(w)
        w.store(ring.IN_HEAD, 1000)
        with self.assertRaises(ring.Bad):
            t.next()
        w.store(ring.IN_HEAD, 1)
        struct.pack_into("<QQ", w.view, ring.ENTRIES_AT, g.region_bytes, 8)
        with self.assertRaises(ring.Bad):
            t.next()
        e.give(0)
        w.store(g.free_entries(), 99)
        with self.assertRaises(ring.Bad):
            t.window()
        w.store(ring.MODE, 9)
        with self.assertRaises(ring.Bad):
            ring.Task(w)

    def test_a_reattach_keeps_the_inputs_taken(self):
        g = geometry()
        w = ring.Words(bytearray(g.object_bytes()))
        e = Engine(w, g, ring.UNMAP)
        e.post(0, 10, 1)
        t = ring.Task(w)
        t.next()
        w2 = ring.Words(bytearray(g.object_bytes()))
        e2 = Engine(w2, g, ring.UNMAP, 1)
        e2.post(0, 10, 1)
        again = ring.Task.reattach(w2, t)
        self.assertIsNone(again.next())
        self.assertTrue(again.release())
        w2.store(ring.IN_TAIL, 5)
        with self.assertRaises(ring.Bad):
            ring.Task.reattach(w2, t)

    def test_the_layout_is_the_engines(self):
        with open(os.path.join(FIXTURES, "ring-golden.json")) as f:
            golden = json.load(f)
        g = ring.Geometry(1 << 20, 64, 64, 2, 2, 64 * 1024)
        self.assertEqual(g.windows_at(), golden["windows_at"])
        words = {int(k): v for k, v in golden["words"].items()}
        self.assertEqual(words[ring.MAGIC], ring.MAGIC_VALUE)
        self.assertEqual(words[ring.WINDOWS_AT], g.windows_at())
        self.assertEqual(words[ring.IN_CAP], g.in_cap)
        self.assertEqual(words[ring.OUT_CAP], g.out_cap)
        self.assertEqual(words[ring.FREE_CAP], g.free_cap)


class FakeCalls:
    """The native calls over ctypes buffers, the engine's half answering each wait."""

    def __init__(self, batches_in, mode, windows=2):
        self.g = geometry(windows)
        self.region = ctypes.create_string_buffer(self.g.region_bytes)
        self.mode = mode
        self.generations = []
        self.outputs = []
        self.made = 0
        self.wakes = 0
        self.sleeps = 0
        self.pending = list(batches_in)
        self.cookie = 0
        self.at = 0
        self._new_rings()

    def _new_rings(self):
        rings = ctypes.create_string_buffer(self.g.object_bytes())
        words = ring.Words(rings)
        engine = Engine(words, self.g, self.mode, len(self.generations))
        if self.generations:
            old = self.generations[-1][2]
            engine.in_head = old.in_head
            words.store(ring.IN_TAIL, old.w.load(ring.IN_TAIL))
            words.store(ring.IN_HEAD, old.in_head)
            words.store(ring.CLOSED, old.w.load(ring.CLOSED))
            for i in range(old.in_head):
                at = ring.ENTRIES_AT + (i % self.g.in_cap) * ring.IN_ENTRY
                words.view[at:at + ring.IN_ENTRY] = old.w.view[at:at + ring.IN_ENTRY]
        for n in range(self.g.windows):
            engine.give(n)
        self.generations.append((rings, words, engine))
        self.current = None

    def engine(self):
        return self.generations[-1][2]

    def _collect(self, rings, engine):
        while True:
            out = engine.take()
            if out is None:
                return
            n, off, length, cookie = out
            at = self.g.window_at(n) + off
            data = bytes(rings[at:at + length])
            self.outputs.append((cookie, record.read(pa.py_buffer(data))))
            if self.mode == ring.PROTECT:
                engine.give(n)

    def _feed(self):
        e = self.engine()
        if self.pending:
            batch = self.pending.pop(0)
            window = (ctypes.c_char * (self.g.region_bytes - self.at)).from_buffer(
                self.region, self.at
            )
            n = record.write(batch, window)
            self.cookie += 1
            e.post(self.at, n, self.cookie)
            self.at += record.round_up(n, ring.PAGE)
        else:
            e.close()

    def copied(self):
        return 0

    def batch_get(self, slot):
        self.made += 1
        if slot == 0:
            return ctypes.addressof(self.region)
        return ctypes.addressof(self.generations[-1][0])

    def batch_put(self, slot, addr, length):
        self.made += 1
        rings, _, engine = self.generations[-1]
        self._collect(rings, engine)
        self._new_rings()

    def futex_wait(self, addr, expected, deadline):
        """The task sleeps on its bell: the engine runs meanwhile, taking its records and
        posting the next batch (or the end) when the task has read every one."""
        self.made += 1
        self.sleeps += 1
        rings, words, engine = self.generations[-1]
        assert addr == ctypes.addressof(rings) + ring.TASK_BELL
        assert deadline == (1 << 64) - 1
        self._collect(rings, engine)
        if words.load(ring.IN_HEAD) == words.load(ring.IN_TAIL) and not words.load(ring.CLOSED):
            self._feed()

    def futex_wake(self, addr):
        self.made += 1
        self.wakes += 1
        rings, _, _ = self.generations[-1]
        assert addr == ctypes.addressof(rings) + ring.ENGINE_BELL


class Serve(unittest.TestCase):
    def run_mode(self, mode, windows=2):
        sent = [pa.record_batch({"x": pa.array([i, i + 1], pa.int64())}) for i in range(5)]
        calls = FakeCalls(sent, mode, windows)
        plane = Plane(calls)

        def double(batch):
            return pa.record_batch({"x": pa.compute.multiply(batch["x"], 2)})

        n = serve(double, plane)
        rings, _, engine = calls.generations[-1]
        calls._collect(rings, engine)
        self.assertEqual(n, 5)
        got = [b["x"].to_pylist() for _, b in sorted(calls.outputs, key=lambda c: c[0])]
        self.assertEqual(got, [[2 * i, 2 * i + 2] for i in range(5)])
        self.assertEqual([c for c, _ in calls.outputs], [1, 2, 3, 4, 5])
        return calls, plane

    def test_protect_mode_serves_every_batch_in_place(self):
        calls, plane = self.run_mode(ring.PROTECT)
        self.assertGreater(calls.sleeps, 0)
        self.assertEqual(plane.wakes, calls.wakes)
        self.assertEqual(plane.handed, 0)

    def test_unmap_mode_hands_the_rings_back_a_group_at_a_time(self):
        calls, plane = self.run_mode(ring.UNMAP)
        self.assertGreaterEqual(plane.handed, 1)

    def test_unmap_mode_with_one_window_answers_each_batch_once(self):
        self.run_mode(ring.UNMAP, windows=1)


if __name__ == "__main__":
    unittest.main()


class Main(unittest.TestCase):
    def test_the_command_names_a_module_and_a_function(self):
        import moruna_shim.__main__ as entry

        self.assertEqual(entry.main(["moruna_shim"]), 2)
        self.assertEqual(entry.main(["moruna_shim", "no-colon"]), 2)
        served = []
        real = entry.serve
        entry.serve = served.append
        try:
            self.assertEqual(entry.main(["moruna_shim", "json:dumps"]), 0)
        finally:
            entry.serve = real
        self.assertEqual(served, [json.dumps])


class Example(unittest.TestCase):
    def test_kd2s_transform_gives_kd2s_rows(self):
        import sys

        sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "examples"))
        from kd2_task import transform

        x = pa.array([0, 1, 2], pa.int64())
        y = pa.array([(v * 7919) % 10007 for v in range(3)], pa.int64())
        out = transform(pa.record_batch({"x": x, "y": y}))
        self.assertEqual(out["x"].to_pylist(), [0, 1, 2])
        self.assertEqual(out["y"].to_pylist(), [3 * v + (v * 7919) % 10007 for v in range(3)])


class Bells(unittest.TestCase):
    def test_a_post_wakes_a_sleeping_engine_once(self):
        g = geometry()
        w = ring.Words(bytearray(g.object_bytes()))
        e = Engine(w, g, ring.PROTECT)
        e.give(0)
        t = ring.Task(w)
        woken = []
        self.assertTrue(t.post(0, 0, 8, 1))
        self.assertFalse(t.ring(woken.append), "the engine is awake")
        w.store(ring.ENGINE_WAITS, 1)
        self.assertTrue(t.post(0, 0, 8, 2))
        self.assertTrue(t.ring(woken.append))
        self.assertEqual(woken, [ring.ENGINE_BELL])
        self.assertEqual(w.load(ring.ENGINE_WAITS), 0, "one wake a sleep")
        self.assertEqual(w.load(ring.ENGINE_BELL), 2)

    def test_the_task_sleeps_only_when_nothing_came(self):
        g = geometry()
        w = ring.Words(bytearray(g.object_bytes()))
        e = Engine(w, g, ring.PROTECT)
        t = ring.Task(w)
        slept = []
        t.sleep(t.has_input, lambda at, v: slept.append((at, v)))
        self.assertEqual(slept, [(ring.TASK_BELL, 0)])
        self.assertEqual(w.load(ring.TASK_WAITS), 0)
        e.post(0, 8, 1)
        t.sleep(t.has_input, lambda at, v: slept.append((at, v)))
        self.assertEqual(len(slept), 1, "an input was there: no sleep")
        self.assertFalse(t.has_window())
        e.give(0)
        self.assertTrue(t.has_window())


class NoShell(unittest.TestCase):
    """The data OS never runs a shell, and a task starts by exec (data-os.md section 3a): the
    shim starts no process at all."""

    IMPORTS = ("subprocess", "pty", "multiprocessing", "shlex", "pexpect")
    OS_CALLS = ("system", "popen", "fork", "forkpty", "posix_spawn", "posix_spawnp")

    def test_the_shim_starts_no_process_and_no_shell(self):
        import ast

        here = os.path.join(os.path.dirname(__file__), "..", "moruna_shim")
        for name in sorted(os.listdir(here)):
            if not name.endswith(".py"):
                continue
            with open(os.path.join(here, name)) as f:
                tree = ast.parse(f.read())
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    for a in node.names:
                        self.assertNotIn(a.name.split(".")[0], self.IMPORTS, name)
                elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
                    self.assertNotIn(node.module.split(".")[0], self.IMPORTS, name)
                elif (
                    isinstance(node, ast.Attribute)
                    and isinstance(node.value, ast.Name)
                    and node.value.id == "os"
                ):
                    attr = node.attr
                    spawns = attr in self.OS_CALLS or attr.startswith(("exec", "spawn"))
                    self.assertFalse(spawns, f"{name} calls os.{attr}")
                elif isinstance(node, ast.keyword) and node.arg == "shell":
                    self.fail(f"{name} passes shell=")
                elif isinstance(node, ast.Constant) and isinstance(node.value, str):
                    for word in ("/bin/sh", "sh -c", "/bin/bash"):
                        self.assertNotIn(word, node.value, name)

    def test_the_readme_s_command_is_exec_form(self):
        with open(os.path.join(os.path.dirname(__file__), "..", "README.md")) as f:
            for line in f:
                if line.startswith(("CMD", "ENTRYPOINT")):
                    self.assertTrue(line.split(None, 1)[1].startswith("["), line)
