"""A record batch as a data-plane record (contracts e.7), written into a window.

A record is a schema message, a record batch message, then the body, every non-empty body
buffer starting on a page from the record's start. pyarrow writes a stream with buffers
packed at 8 bytes; this lays the same two messages out as a record, rewriting the buffer
entries and the body length of the record batch message in place, as the engine's
`moruna_ipc_check::record::reframe` does for descriptor 1.

Reading a record needs nothing of this: its first two messages and its body are a valid
stream message as they lie, which `pyarrow.ipc.open_stream` reads in place.
"""

import struct

import pyarrow as pa

PAGE = 4096
CONTINUATION = 0xFFFFFFFF

# Message: version, header_type, header, bodyLength.
MESSAGE_HEADER = 2
MESSAGE_BODY_LENGTH = 3
# RecordBatch: length, nodes, buffers, compression.
BATCH_BUFFERS = 2
BATCH_COMPRESSION = 3
BUFFER_ENTRY = 16


class RecordError(Exception):
    """A batch that cannot be laid out as a record."""


def round_up(n, to):
    return (n + to - 1) // to * to


def _messages(stream):
    """The whole messages of a stream: each its 8-byte prefix, metadata and body."""
    out = []
    at = 0
    view = memoryview(stream)
    while at + 8 <= len(view):
        marker, length = struct.unpack_from("<Ii", view, at)
        if marker != CONTINUATION:
            raise RecordError("a stream message without its continuation marker")
        if length == 0:
            break
        meta_end = at + 8 + length
        body = _field_i64(view[at + 8 : meta_end], MESSAGE_BODY_LENGTH)
        out.append((at, meta_end, body))
        at = meta_end + body
    return out


def _table(fb, at):
    """A table's position and its vtable's."""
    vt = at - struct.unpack_from("<i", fb, at)[0]
    return at, vt


def _field_pos(fb, table, field):
    at, vt = table
    vt_len = struct.unpack_from("<H", fb, vt)[0]
    slot = 4 + 2 * field
    if slot + 2 > vt_len:
        return None
    off = struct.unpack_from("<H", fb, vt + slot)[0]
    return at + off if off else None


def _root(fb):
    return _table(fb, struct.unpack_from("<I", fb, 0)[0])


def _field_i64(fb, field):
    pos = _field_pos(fb, _root(fb), field)
    return struct.unpack_from("<q", fb, pos)[0] if pos is not None else 0


def _buffers(fb):
    """Where the record batch's buffer vector starts in the flatbuffer, and its length."""
    message = _root(fb)
    header = _field_pos(fb, message, MESSAGE_HEADER)
    if header is None:
        raise RecordError("the record batch message has no header")
    batch = _table(fb, header + struct.unpack_from("<I", fb, header)[0])
    if _field_pos(fb, batch, BATCH_COMPRESSION) is not None:
        raise RecordError("a data-plane record is never compressed")
    vec = _field_pos(fb, batch, BATCH_BUFFERS)
    if vec is None:
        return 0, 0
    start = vec + struct.unpack_from("<I", fb, vec)[0]
    return start + 4, struct.unpack_from("<I", fb, start)[0]


def stream_of(batch):
    """pyarrow's stream of `batch`: its schema message and one record batch message."""
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue()


def layout(stream):
    """The record of a one-batch stream: its framing (the two messages, the entries and the
    body length rewritten), its length, and each non-empty buffer's (offset in the stream,
    offset in the record, length)."""
    messages = _messages(stream)
    if len(messages) < 2:
        raise RecordError("a stream of a schema and one record batch")
    (s0, s1, _), (b0, b1, body) = messages[0], messages[1]
    framing = bytearray(stream[s0:s1]) + bytearray(stream[b0:b1])
    rb = len(framing) - (b1 - b0)
    fb = memoryview(framing)[rb + 8 :]
    start, count = _buffers(fb)
    entries = [struct.unpack_from("<qq", fb, start + i * BUFFER_ENTRY) for i in range(count)]
    for offset, length in entries:
        if offset < 0 or length < 0 or offset + length > body:
            raise RecordError("a buffer outside the body")
    body_base = len(framing)
    cursor = round_up(body_base, PAGE)
    placed = []
    end = 0
    for offset, length in entries:
        relative = cursor - body_base
        if length:
            end = max(end, relative + length)
        placed.append((offset, cursor, length, relative))
        cursor = round_up(cursor + length, PAGE)
    body_len = round_up(end, 8)
    for i, (_, _, length, relative) in enumerate(placed):
        at = relative if length else min(relative, body_len)
        struct.pack_into("<qq", fb, start + i * BUFFER_ENTRY, at, length)
    pos = _field_pos(fb, _root(fb), MESSAGE_BODY_LENGTH)
    if pos is None:
        if body_len:
            raise RecordError("the record batch message has no body length to set")
    else:
        struct.pack_into("<q", fb, pos, body_len)
    body_start = b1
    moves = [(body_start + o, to, n) for (o, to, n, _) in placed if n]
    return bytes(framing), len(framing) + body_len, moves


def write(batch, window):
    """Writes `batch` as a record at the start of `window` (a writable buffer); its length."""
    stream = stream_of(batch)
    framing, length, moves = layout(stream)
    view = memoryview(window).cast("B")
    if length > len(view):
        raise RecordError(
            f"a record of {length} bytes does not fit a window of {len(view)} bytes"
        )
    source = memoryview(stream).cast("B")
    view[: len(framing)] = framing
    end = len(framing)
    for src, to, n in moves:
        view[end:to] = bytes(to - end)
        view[to : to + n] = source[src : src + n]
        end = to + n
    view[end:length] = bytes(length - end)
    return length


def read(buffer):
    """The record batch of a record lying in `buffer` (a pyarrow Buffer), read in place."""
    return pa.ipc.open_stream(buffer).read_next_batch()
