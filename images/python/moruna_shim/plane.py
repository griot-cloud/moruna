"""The task's plane in ring mode, through the Moruna kernel's own calls.

A task holds no handle: its slots are numbers. Slot 0 maps its input region read only and
slot 1 its rings object writable, once each (`batch_get`). It sleeps on its bell only when it
has nothing to do (`futex_wait`, which the kernel keys by the shared object, so the engine's
wake reaches it), wakes the engine the same way when the engine sleeps, and in unmap mode
hands the rings object back once a group is posted (`batch_put`). The native calls go through
libc's `syscall`, which the personality never sees: the kernel answers a native number itself.
"""

import ctypes
import os

import pyarrow as pa

from . import record, ring

BASE = 0x1_0000
FUTEX_WAIT = BASE + 38
FUTEX_WAKE = BASE + 39
BATCH_GET = BASE + 0x100
BATCH_PUT = BASE + 0x101
PLANE_PROBE = BASE + 0x10F
PROBE_COPIED = 1
SHOULD_WAIT = 6
TIMED_OUT = 8

# A sleep on the bell has no deadline: the kernel keys the bell by the shared object, so the
# engine's wake reaches it, and takes the futex queue's lock before it reads the bell, which
# orders this task's "I wait" before the read.
FOREVER = (1 << 64) - 1

# Reads of the input ring before a wait: a batch the engine posts within them costs no call.
SPINS = int(os.environ.get("MORUNA_SHIM_SPINS", "20000"))


class KernelError(OSError):
    """A native call the kernel refused, with its error code."""


class Calls:
    """The four native calls the shim makes."""

    def __init__(self):
        libc = ctypes.CDLL(None, use_errno=True)
        self._syscall = libc.syscall
        self._syscall.restype = ctypes.c_long
        self.made = 0

    def _call(self, *args):
        self.made += 1
        r = self._syscall(*[ctypes.c_ulong(a) for a in args])
        if r == -1:
            code = ctypes.get_errno()
            raise KernelError(code, f"native call {args[0]:#x} refused with code {code}")
        return r

    def batch_get(self, slot):
        return self._call(BATCH_GET, slot)

    def batch_put(self, slot, addr, length):
        return self._call(BATCH_PUT, slot, addr, length)

    def futex_wait(self, addr, expected, deadline):
        """Sleeps while the u32 at `addr` holds `expected`, until a wake or the deadline."""
        try:
            self._call(FUTEX_WAIT, addr, expected, deadline)
        except KernelError as e:
            if e.errno not in (SHOULD_WAIT, TIMED_OUT):
                raise

    def futex_wake(self, addr):
        return self._call(FUTEX_WAKE, addr, 1)

    def copied(self):
        """The bytes the kernel copied into or out of this process (a kernel built with
        `probe`), or None."""
        try:
            return self._call(PLANE_PROBE, PROBE_COPIED, 0)
        except KernelError:
            return None


def _mapped(addr, length):
    return (ctypes.c_char * length).from_address(addr)


class Plane:
    """The region and the rings object, each mapped once."""

    def __init__(self, calls=None):
        self.calls = calls or Calls()
        self.waits = 0
        self.wakes = 0
        self.handed = 0
        region_addr = self.calls.batch_get(0)
        self._attach()
        self.region_addr = region_addr
        self.region_len = self.half.geometry.region_bytes
        self.region = pa.foreign_buffer(region_addr, self.region_len, base=self)

    def _attach(self, previous=None):
        self.rings_addr = self.calls.batch_get(1)
        # The header's sizes are on the first page, which the slot mapped whatever they say.
        head = ring.Words(_mapped(self.rings_addr, ring.PAGE))
        geometry = ring.Task(head).geometry
        self.rings_len = geometry.object_bytes()
        self.rings = _mapped(self.rings_addr, self.rings_len)
        self.words = ring.Words(self.rings)
        if previous is None:
            self.half = ring.Task(self.words)
        else:
            self.half = ring.Task.reattach(self.words, previous)

    def batch(self):
        """The next batch in place, its entry, or None while there is none."""
        entry = self.half.next()
        if entry is None:
            return None
        offset, length, _, _ = entry
        return entry, record.read(self.region.slice(offset, length))

    def next(self):
        """The next batch, waiting when the ring is empty; None at the end."""
        while True:
            got = self.batch()
            if got is not None:
                return got
            if self.half.ended():
                return None
            for _ in range(SPINS):
                if self.words.load(ring.IN_HEAD) != self.half.in_next or self.half.ended():
                    break
            else:
                if self.half.mode == ring.UNMAP and self._posted_since_hand_back():
                    self.hand_back()
                    continue
                self._sleep(self.half.has_input)

    def _posted_since_hand_back(self):
        return self.half.out_head > 0

    def _sleep(self, ready):
        self.waits += 1

        def wait(at, value):
            self.calls.futex_wait(self.rings_addr + at, value, FOREVER)

        self.half.sleep(ready, wait)

    def window(self):
        """A window's number and its memory, waiting for one when none is free."""
        while True:
            w = self.half.window()
            if w is not None:
                g = self.half.geometry
                return w, _mapped(self.rings_addr + g.window_at(w), g.window_bytes)
            if self.half.mode == ring.UNMAP and self._posted_since_hand_back():
                self.hand_back()
                continue
            self._sleep(self.half.has_window)

    def post(self, w, length, cookie):
        # A window is posted once, and the output ring holds every window, so it is never
        # full for a task that keeps the protocol.
        if not self.half.post(w, 0, length, cookie):
            raise ring.Bad("the output ring is full")
        if self.half.ring(lambda at: self.calls.futex_wake(self.rings_addr + at)):
            self.wakes += 1

    def release(self):
        self.half.release()

    def hand_back(self):
        """Unmap mode: the rings object goes back with the records on it, and the next is
        mapped (waiting for it)."""
        self.handed += 1
        self.calls.batch_put(1, self.rings_addr, self.rings_len)
        self._attach(self.half)

    def finish(self):
        if self.half.mode == ring.UNMAP and self._posted_since_hand_back():
            self.calls.batch_put(1, self.rings_addr, self.rings_len)
