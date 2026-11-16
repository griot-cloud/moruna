"""The rings between the engine and a task: the task's half (architecture/data-os.md section 5).

The layout and the protocol are the engine's (kernel/plane/core/src/ring.rs); this is the same
task half, in Python. Words are little-endian u64s at fixed byte offsets of the rings object.
Each side sleeps on its bell, a 32-bit futex word the other side raises after each post and
wakes only when the sleeper said it waits.
"""

import struct

MAGIC_VALUE = int.from_bytes(b"MORUNARG", "little")
VERSION_VALUE = 3

MAGIC = 0
VERSION = 8
REGION_BYTES = 16
IN_CAP = 24
OUT_CAP = 32
FREE_CAP = 40
WINDOWS = 48
WINDOW_BYTES = 56
WINDOWS_AT = 64
MODE = 72
GENERATION = 80
IN_HEAD = 128
IN_TAIL = 192
OUT_HEAD = 256
OUT_TAIL = 320
FREE_HEAD = 384
FREE_TAIL = 448
ENGINE_WAITS = 512
TASK_WAITS = 576
CLOSED = 640
ATTACHED = 704
TASK_BELL = 768
ENGINE_BELL = 832
TASK_NOTE = 896

ENTRIES_AT = 4096
IN_ENTRY = 32
OUT_ENTRY = 32
FREE_ENTRY = 8
PAGE = 4096
CAP_MAX = 1 << 16
WINDOWS_MAX = 1 << 12


PROTECT = 1
UNMAP = 2


class Bad(Exception):
    """A word the engine wrote that could not be true."""


class Words:
    """The u64 words of a shared object, over any writable buffer."""

    def __init__(self, buffer):
        self.view = memoryview(buffer).cast("B")

    def load(self, at):
        return struct.unpack_from("<Q", self.view, at)[0]

    def store(self, at, value):
        struct.pack_into("<Q", self.view, at, value)


def pages(n):
    return (n + PAGE - 1) & ~(PAGE - 1)


class Geometry:
    def __init__(self, region_bytes, in_cap, out_cap, free_cap, windows, window_bytes):
        self.region_bytes = region_bytes
        self.in_cap = in_cap
        self.out_cap = out_cap
        self.free_cap = free_cap
        self.windows = windows
        self.window_bytes = window_bytes

    def check(self):
        def cap(c):
            return c > 0 and c & (c - 1) == 0 and c <= CAP_MAX

        if not (cap(self.in_cap) and cap(self.out_cap) and cap(self.free_cap)):
            raise Bad("a ring's capacity is not a power of two within bounds")
        if not (0 < self.windows <= WINDOWS_MAX and self.windows <= self.free_cap):
            raise Bad("the windows do not fit the free ring")
        if self.window_bytes == 0 or self.window_bytes % PAGE:
            raise Bad("a window is not whole pages")
        if self.region_bytes == 0 or self.region_bytes % PAGE:
            raise Bad("the region is not whole pages")

    def out_entries(self):
        return ENTRIES_AT + self.in_cap * IN_ENTRY

    def free_entries(self):
        return self.out_entries() + self.out_cap * OUT_ENTRY

    def windows_at(self):
        return pages(self.free_entries() + self.free_cap * FREE_ENTRY)

    def window_at(self, w):
        return self.windows_at() + w * self.window_bytes

    def object_bytes(self):
        return self.windows_at() + self.windows * self.window_bytes


class Task:
    """The task's half: it takes inputs and windows, and posts outputs."""

    def __init__(self, words):
        w = words
        if w.load(MAGIC) != MAGIC_VALUE:
            raise Bad("no rings object")
        if w.load(VERSION) != VERSION_VALUE:
            raise Bad("another version of the rings")
        self.geometry = Geometry(
            w.load(REGION_BYTES),
            w.load(IN_CAP),
            w.load(OUT_CAP),
            w.load(FREE_CAP),
            w.load(WINDOWS),
            w.load(WINDOW_BYTES),
        )
        self.geometry.check()
        if w.load(WINDOWS_AT) != self.geometry.windows_at():
            raise Bad("the windows are not where the sizes put them")
        self.mode = w.load(MODE)
        if self.mode not in (PROTECT, UNMAP):
            raise Bad("an unknown mode")
        self.generation = w.load(GENERATION)
        self.in_tail = w.load(IN_TAIL)
        self.in_next = self.in_tail
        self.out_head = w.load(OUT_HEAD)
        self.free_tail = w.load(FREE_TAIL)
        self.engine_bell = w.load(ENGINE_BELL) & 0xFFFFFFFF
        self.note_value = w.load(TASK_NOTE)
        self.words = w
        w.store(ATTACHED, VERSION_VALUE)

    @classmethod
    def reattach(cls, words, previous):
        """The half of the next rings object (unmap mode), the inputs taken and not released
        kept: the engine carried the input ring over."""
        t = cls(words)
        if t.in_tail != previous.in_tail or vars(t.geometry) != vars(previous.geometry):
            raise Bad("the next rings object does not carry the input ring")
        t.in_next = previous.in_next
        return t

    def next(self):
        """The next input as (offset, length, cookie, rows), or None while there is none."""
        w, g = self.words, self.geometry
        head = w.load(IN_HEAD)
        if head < self.in_next or head - self.in_tail > g.in_cap:
            raise Bad("the engine's input ring is not one")
        if head == self.in_next:
            return None
        at = ENTRIES_AT + (self.in_next % g.in_cap) * IN_ENTRY
        entry = struct.unpack_from("<QQQQ", w.view, at)
        if entry[0] + entry[1] > g.region_bytes:
            raise Bad("an input past the region")
        self.in_next += 1
        return entry

    def release(self):
        if self.in_tail == self.in_next:
            return False
        self.in_tail += 1
        self.words.store(IN_TAIL, self.in_tail)
        return True

    def ended(self):
        w = self.words
        return w.load(CLOSED) != 0 and w.load(IN_HEAD) == self.in_next

    def window(self):
        """A window's number to write the next record in, or None while none is given."""
        w, g = self.words, self.geometry
        head = w.load(FREE_HEAD)
        if head < self.free_tail or head - self.free_tail > g.free_cap:
            raise Bad("the engine's free ring is not one")
        if head == self.free_tail:
            return None
        n = w.load(g.free_entries() + (self.free_tail % g.free_cap) * FREE_ENTRY)
        if n >= g.windows:
            raise Bad("the engine gave a window that is not one")
        self.free_tail += 1
        w.store(FREE_TAIL, self.free_tail)
        return n

    def post(self, window, offset, length, cookie):
        """Posts a record; False when the output ring is full."""
        w, g = self.words, self.geometry
        if self.out_head - w.load(OUT_TAIL) >= g.out_cap:
            return False
        at = g.out_entries() + (self.out_head % g.out_cap) * OUT_ENTRY
        struct.pack_into("<QQQQ", w.view, at, window, offset, length, cookie)
        self.out_head += 1
        w.store(OUT_HEAD, self.out_head)
        return True

    def ring(self, wake):
        """Raises the engine's bell after a post, and calls `wake` on it (clearing the word:
        one wake a sleep) if the engine said it sleeps. Whether it woke the engine."""
        w = self.words
        self.engine_bell = (self.engine_bell + 1) & 0xFFFFFFFF
        w.store(ENGINE_BELL, self.engine_bell)
        if w.load(ENGINE_WAITS) == 0:
            return False
        w.store(ENGINE_WAITS, 0)
        wake(ENGINE_BELL)
        return True

    def note(self, wake):
        """Raises the task's note and rings the engine's bell: before the task ends, so an
        engine asleep on its bell reads the task's slots."""
        self.note_value += 1
        self.words.store(TASK_NOTE, self.note_value)
        return self.ring(wake)

    def has_input(self):
        w = self.words
        return w.load(IN_HEAD) != self.in_next or w.load(CLOSED) != 0

    def has_window(self):
        return self.words.load(FREE_HEAD) != self.free_tail

    def sleep(self, ready, wait):
        """Says it waits, reads its bell, and calls `wait(at, value)` on it only if `ready()`
        still says no; the engine raises the bell and wakes it when it posts."""
        w = self.words
        w.store(TASK_WAITS, 1)
        seq = w.load(TASK_BELL) & 0xFFFFFFFF
        try:
            if not ready():
                wait(TASK_BELL, seq)
        finally:
            w.store(TASK_WAITS, 0)
