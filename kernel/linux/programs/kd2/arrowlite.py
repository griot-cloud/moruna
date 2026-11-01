"""Arrow IPC streams of flat, non-null Int64 columns, read with the standard library only.

KD2's boot (SDD 30 k) runs CPython 3.14 from the python:3.14-slim image, which has no pyarrow,
and judges the rows on a host that may have none either; this is the little of the format both
need. It reads a stream message by message (the continuation marker, the metadata length, the
flatbuffer `Message`, the body), names a schema's fields and a record batch's buffers, and says
why it refuses anything else: a type that is not a signed 64-bit integer, a column with nulls,
compression, a dictionary.

The flatbuffer reading follows the format's rules exactly: a table's vtable is at the table's
offset minus the signed offset stored there, its first two 16-bit words are the vtable's and the
table's sizes, and field `i`'s offset is the 16-bit word at `4 + 2 * i`; offsets to tables,
vectors and strings are unsigned 32-bit and relative to where they are stored.
"""

import struct

CONTINUATION = 0xFFFFFFFF
# Message.header_type (Schema.fbs, Message.fbs).
SCHEMA = 1
RECORD_BATCH = 3
# Field.type_type: Int.
TYPE_INT = 2


class Refused(Exception):
    """A stream this reader does not take, and why."""


class Table:
    def __init__(self, buf, pos):
        self.buf = buf
        self.pos = pos
        vtable = pos - struct.unpack_from("<i", buf, pos)[0]
        self.vtable = vtable
        self.vsize = struct.unpack_from("<H", buf, vtable)[0]

    def _field(self, i):
        at = 4 + 2 * i
        if at >= self.vsize:
            return 0
        return struct.unpack_from("<H", self.buf, self.vtable + at)[0]

    def scalar(self, i, fmt, default=0):
        off = self._field(i)
        if off == 0:
            return default
        return struct.unpack_from("<" + fmt, self.buf, self.pos + off)[0]

    def _indirect(self, i):
        off = self._field(i)
        if off == 0:
            return None
        at = self.pos + off
        return at + struct.unpack_from("<I", self.buf, at)[0]

    def table(self, i):
        at = self._indirect(i)
        return None if at is None else Table(self.buf, at)

    def string(self, i):
        at = self._indirect(i)
        if at is None:
            return None
        n = struct.unpack_from("<I", self.buf, at)[0]
        return bytes(self.buf[at + 4 : at + 4 + n]).decode("utf-8")

    def vector(self, i):
        """(start of the elements, count), or (None, 0)."""
        at = self._indirect(i)
        if at is None:
            return None, 0
        return at + 4, struct.unpack_from("<I", self.buf, at)[0]

    def tables(self, i):
        start, n = self.vector(i)
        out = []
        for k in range(n):
            at = start + 4 * k
            out.append(Table(self.buf, at + struct.unpack_from("<I", self.buf, at)[0]))
        return out

    def structs(self, i, fmt):
        start, n = self.vector(i)
        size = struct.calcsize("<" + fmt)
        return [struct.unpack_from("<" + fmt, self.buf, start + size * k) for k in range(n)]


def message(meta):
    """The `Message` table of one message's metadata."""
    root = struct.unpack_from("<I", meta, 0)[0]
    return Table(meta, root)


def schema_fields(msg):
    """The field names of a schema message, every one a non-null-safe Int64 or refused."""
    schema = msg.table(2)
    names = []
    for field in schema.tables(1):
        name = field.string(0)
        if field.scalar(2, "B") != TYPE_INT:
            raise Refused(f"column {name!r} is not an integer")
        int_type = field.table(3)
        if int_type.scalar(0, "i") != 64 or not int_type.scalar(1, "?"):
            raise Refused(f"column {name!r} is not a signed 64-bit integer")
        if field.table(4) is not None:
            raise Refused(f"column {name!r} is dictionary encoded")
        names.append(name)
    return names


def batch_layout(msg, fields):
    """A record batch message's rows and, for each field, its value buffer (offset, length)."""
    batch = msg.table(2)
    if batch.table(3) is not None:
        raise Refused("the record batch is compressed")
    rows = batch.scalar(0, "q")
    nodes = batch.structs(1, "qq")
    buffers = batch.structs(2, "qq")
    if len(nodes) != len(fields) or len(buffers) != 2 * len(fields):
        raise Refused(f"{len(nodes)} nodes and {len(buffers)} buffers for {len(fields)} columns")
    layout = []
    for k, name in enumerate(fields):
        length, nulls = nodes[k]
        if length != rows or nulls != 0:
            raise Refused(f"column {name!r} has {nulls} nulls in {length} of {rows} rows")
        offset, size = buffers[2 * k + 1]
        if size < 8 * rows:
            raise Refused(f"column {name!r}'s values are {size} bytes for {rows} rows")
        layout.append((offset, size))
    return rows, layout


class Reader:
    """Messages from a stream given as a function that reads exactly n bytes, or fewer at the end."""

    def __init__(self, read_exact):
        self.read_exact = read_exact

    def next(self):
        """(metadata bytes, Message, body bytes), or None at the end-of-stream marker or EOF."""
        head = self.read_exact(4)
        if len(head) < 4:
            return None
        (word,) = struct.unpack("<I", head)
        if word == CONTINUATION:
            head = self.read_exact(4)
            if len(head) < 4:
                raise Refused("the stream ends inside a message's length")
            (word,) = struct.unpack("<I", head)
        if word == 0:
            return None
        meta = self.read_exact(word)
        if len(meta) < word:
            raise Refused("the stream ends inside a message's metadata")
        msg = message(meta)
        body_len = msg.scalar(3, "q")
        body = self.read_exact(body_len) if body_len else b""
        if len(body) < body_len:
            raise Refused("the stream ends inside a message's body")
        return meta, msg, body


def frame(meta, body):
    """One message as a stream carries it: the marker, the padded metadata, the body."""
    pad = (-len(meta)) % 8
    return struct.pack("<II", CONTINUATION, len(meta) + pad) + meta + b"\0" * pad + body


END = struct.pack("<II", CONTINUATION, 0)


def columns(data):
    """Every column of a whole stream held in `data`, as lists of ints: (names, {name: values})."""
    at = 0

    def read_exact(n):
        nonlocal at
        piece = data[at : at + n]
        at += len(piece)
        return piece

    reader = Reader(read_exact)
    first = reader.next()
    if first is None or first[1].scalar(1, "B") != SCHEMA:
        raise Refused("the stream does not start with a schema")
    names = schema_fields(first[1])
    values = {n: [] for n in names}
    while True:
        item = reader.next()
        if item is None:
            return names, values
        _, msg, body = item
        if msg.scalar(1, "B") != RECORD_BATCH:
            raise Refused(f"a message of type {msg.scalar(1, 'B')} after the schema")
        rows, layout = batch_layout(msg, names)
        for name, (offset, _) in zip(names, layout):
            values[name].extend(struct.unpack_from(f"<{rows}q", body, offset))
