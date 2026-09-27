#!/usr/bin/env python3
"""Make compiled bytecode byte-for-byte reproducible by dropping marshal's unused references.

    pyc.py <dir>...

marshal marks an object FLAG_REF, so that a later occurrence can point back at it, whenever
the object's reference count is above one when it is written, whether or not anything in the
file ever points back. The free-threaded build shares constants between every code object in
the process and frees some of them late, so which constants have a second reference while
compileall writes a file varies from run to run, and two builds of one tree write a few .pyc
files that differ only in those flags (and so in the numbering of the references that are
used). This rewrites every .pyc under the given directories to the one canonical form: the
flag only on objects a TYPE_REF actually names, references renumbered to match. The file
keeps its size, and the rewrite is checked: the rewritten file must load to an object equal to
the original's, or this fails.

Run it with the interpreter the .pyc files are for (marshal format 5, CPython 3.14).
"""

import marshal
import os
import struct
import sys

FLAG_REF = 0x80
HEADER = 16  # magic, flags, and the source hash or mtime and size
# Types that carry nothing after the type byte and are never flagged.
BARE = set(b"0NFTS.")
LONG_STRINGS = set(b"stuaA")
SEQUENCES = set(b"([<>")


class Parser:
    def __init__(self, data: bytes):
        self.data = data
        self.pos = 0
        self.flagged: list[int] = []  # offset of each flagged type byte, in reference order
        self.refs: list[tuple[int, int]] = []  # (offset of the index, index) of each TYPE_REF

    def u8(self) -> int:
        v = self.data[self.pos]
        self.pos += 1
        return v

    def i32(self) -> int:
        (v,) = struct.unpack_from("<i", self.data, self.pos)
        self.pos += 4
        return v

    def skip(self, n: int) -> None:
        if n < 0 or self.pos + n > len(self.data):
            raise ValueError(f"truncated at {self.pos}")
        self.pos += n

    def obj(self) -> None:
        at = self.pos
        byte = self.u8()
        code = byte & ~FLAG_REF
        if byte & FLAG_REF:
            # References are numbered in the order their objects start, as marshal.c reads them.
            self.flagged.append(at)
        if code in BARE:
            return
        if code == ord("r"):
            self.refs.append((self.pos, self.i32()))
        elif code == ord("i"):
            self.skip(4)
        elif code == ord("I") or code == ord("g"):
            self.skip(8)
        elif code == ord("y"):
            self.skip(16)
        elif code == ord("f"):
            self.skip(self.u8())
        elif code == ord("x"):
            self.skip(self.u8())
            self.skip(self.u8())
        elif code == ord("l"):
            self.skip(abs(self.i32()) * 2)
        elif code in LONG_STRINGS:
            self.skip(self.i32())
        elif code in b"zZ":
            self.skip(self.u8())
        elif code in SEQUENCES:
            for _ in range(self.i32()):
                self.obj()
        elif code == ord(")"):
            for _ in range(self.u8()):
                self.obj()
        elif code == ord("{"):
            while self.data[self.pos] != ord("0"):
                self.obj()
                self.obj()
            self.pos += 1
        elif code == ord(":"):  # slice: start, stop, step
            for _ in range(3):
                self.obj()
        elif code == ord("c"):
            # argcount, posonlyargcount, kwonlyargcount, stacksize, flags; code, consts,
            # names, localsplusnames, localspluskinds, filename, name, qualname; firstlineno;
            # linetable, exceptiontable.
            self.skip(20)
            for _ in range(8):
                self.obj()
            self.skip(4)
            for _ in range(2):
                self.obj()
        else:
            raise ValueError(f"unknown marshal type {code:#x} at {at}")


def canonical(data: bytes) -> bytes:
    p = Parser(data)
    p.pos = HEADER
    p.obj()
    if p.pos != len(data):
        raise ValueError(f"{len(data) - p.pos} trailing bytes")
    used = sorted({index for _, index in p.refs})
    renumber = {old: new for new, old in enumerate(used)}
    out = bytearray(data)
    for index, at in enumerate(p.flagged):
        if index not in renumber:
            out[at] &= ~FLAG_REF
    for at, index in p.refs:
        struct.pack_into("<i", out, at, renumber[index])
    return bytes(out)


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    paths = []
    for top in argv[1:]:
        for root, dirs, files in os.walk(top):
            dirs.sort()
            paths += [os.path.join(root, f) for f in sorted(files) if f.endswith(".pyc")]
    changed = 0
    for path in paths:
        with open(path, "rb") as f:
            data = f.read()
        new = canonical(data)
        if new == data:
            continue
        if marshal.loads(new[HEADER:]) != marshal.loads(data[HEADER:]):
            raise SystemExit(f"pyc.py: {path}: the canonical form loads to something else")
        st = os.stat(path)
        with open(path, "wb") as f:
            f.write(new)
        os.utime(path, ns=(st.st_atime_ns, st.st_mtime_ns))
        changed += 1
    print(f"pyc.py: {len(paths)} files, {changed} rewritten", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
