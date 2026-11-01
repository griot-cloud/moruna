//! A data-plane task's standard streams (SDD 29 f.7 and f.8): descriptor 0 an Arrow IPC
//! stream read from its input slot, descriptor 1 one written to its output slot, and
//! what it writes as text, to 2 (and to 1 when it has no output slot), records in the
//! engine's log ring.
//!
//! An unmodified program that speaks Arrow on its standard streams needs no Moruna
//! library: this server makes the slots' records into a stream and the stream back into
//! records. Reading 0 gives the first record whole (its schema message, its record batch
//! message and its body, which is a valid stream message as it lies) and every later one
//! from its record batch message on, then the end-of-stream marker once the engine has
//! closed the slot. What is written to 1 is parsed a message at a time; each record batch
//! message and its body are laid out as a record (`moruna_ipc_check::record::reframe`) in
//! the window and put. These are copies, as any `read` and `write` are; a program that
//! makes the three calls itself moves no byte.
//!
//! The server never waits inside a request for the engine (SDD 30 f.10): a read that needs
//! the next batch, and a write that needs a window, look first at the slot's `SIGNALED`
//! (ABI 2.2: the task's `batch_get` would not wait), and when it is not raised say so
//! ([`Io::Waits`]); the call is answered later, when the server's watch on the slot sees it
//! raised. The engine is itself a program of this server, so a wait here would stop it.

use alloc::vec::Vec;

use linux_protocol::log;
use moruna_ipc_check::record::{self, Need};
use moruna_linux::types::errno;
use moruna_record::ring::Ring;
use moruna_svc::{Process, Signals};
use moruna_sys::{
    Mapping, RawHandle, WaitItem,
    plane::{Region, plane_get, plane_put},
};

/// The end-of-stream marker: the continuation and a length of zero.
const END: [u8; 8] = [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0];

/// The longest line a log record carries; a longer one is cut there.
const LINE_MAX: usize = 4096;

/// The most bytes written to 1 that wait for the rest of their message: a record batch
/// and its body, which the window must hold anyway.
const PENDING_MAX: usize = 256 << 20;

/// What a read or a write of a plane descriptor did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Io {
    /// It is done: the bytes read, or the write's bytes all put or held for a message.
    Done(u64),
    /// It needs its slot ready first: the call is answered later.
    Waits,
}

/// Whether the slot whose handle value is `slot` would give a batch or a window, or its
/// end, now: its `SIGNALED` (ABI 2.2), looked at without waiting.
fn ready(slot: RawHandle) -> bool {
    let mut items = [WaitItem::raw(slot, Signals::SIGNALED)];
    moruna_sys::wait(&mut items, moruna_sys::now()).is_ok()
        && items[0].observed().contains(Signals::SIGNALED)
}

/// What a task's descriptor 0 is reading.
struct Reading {
    region: Region,
    /// The stream's bytes are the region's `from .. end`; `at` is the next to give.
    at: u64,
    end: u64,
}

pub struct PlaneIo {
    /// The input slot's number in the task's plane, and the value of the handle to it the
    /// server's watch holds.
    input: Option<(u32, RawHandle)>,
    output: Option<(u32, RawHandle)>,
    reading: Option<Reading>,
    /// No record has been given yet: the first is given with its schema message.
    first: bool,
    /// The input has ended; how much of the end marker has been given.
    ended: Option<usize>,
    /// Bytes written to 1 that do not yet make a whole message.
    written: Vec<u8>,
    /// The schema message the stream written to 1 began with.
    schema: Option<Vec<u8>>,
    log: Mapping<'static>,
    lines: [Vec<u8>; 3],
    seq: u64,
    /// Log records the ring had no room for.
    pub dropped: u64,
}

fn io(_: moruna_svc::Error) -> u64 {
    errno::EIO
}

impl PlaneIo {
    pub fn new(
        input: Option<(u32, RawHandle)>,
        output: Option<(u32, RawHandle)>,
        log: Mapping<'static>,
    ) -> PlaneIo {
        PlaneIo {
            input,
            output,
            reading: None,
            first: true,
            ended: None,
            written: Vec::new(),
            schema: None,
            log,
            lines: [Vec::new(), Vec::new(), Vec::new()],
            seq: 0,
            dropped: 0,
        }
    }

    pub fn has_input(&self) -> bool {
        self.input.is_some()
    }

    pub fn has_output(&self) -> bool {
        self.output.is_some()
    }

    /// `read(0)`: up to `count` bytes of the input stream into `buf` of the task, or
    /// [`Io::Waits`] when the next batch has not been pushed.
    pub fn read(&mut self, process: &Process, buf: u64, count: u64) -> Result<Io, u64> {
        let Some((slot, handle)) = self.input else {
            return Ok(Io::Done(0));
        };
        if let Some(given) = self.ended {
            let rest = &END[given..];
            let n = rest.len().min(count as usize);
            crate::memory::write(process, buf, &rest[..n])?;
            self.ended = Some(given + n);
            return Ok(Io::Done(n as u64));
        }
        if self.reading.as_ref().is_none_or(|r| r.at == r.end) {
            if !ready(handle) {
                return Ok(Io::Waits);
            }
            match plane_get(process, slot).map_err(io)? {
                None => {
                    self.reading = None;
                    self.ended = Some(0);
                    return self.read(process, buf, count);
                }
                Some(region) => {
                    let (from, end) = self.stream_span(&region)?;
                    self.reading = Some(Reading {
                        region,
                        at: from,
                        end,
                    });
                }
            }
        }
        let Some(r) = &mut self.reading else {
            return Ok(Io::Done(0));
        };
        let n = (r.end - r.at).min(count);
        let mut bytes = alloc::vec![0u8; n as usize];
        r.region.read(r.at, &mut bytes).map_err(io)?;
        crate::memory::write(process, buf, &bytes)?;
        r.at += n;
        Ok(Io::Done(n))
    }

    /// The part of a record that continues the stream: from its start for the first,
    /// from its record batch message for the others, to the end of its body.
    fn stream_span(&mut self, region: &Region) -> Result<(u64, u64), u64> {
        let mut framing = alloc::vec![0u8; (region.len() as usize).min(4096)];
        region.read(0, &mut framing).map_err(io)?;
        let end = loop {
            match record::framing_need(&framing).map_err(|_| errno::EIO)? {
                Need::Framing(n) if n <= framing.len() => break n,
                Need::Framing(n) | Need::More(n) => {
                    if n as u64 > region.len() || n <= framing.len() {
                        return Err(errno::EIO);
                    }
                    let from = framing.len();
                    framing.resize(n, 0);
                    region.read(from as u64, &mut framing[from..]).map_err(io)?;
                }
            }
        };
        framing.truncate(end);
        let checked = record::check_record(&framing, region.len()).map_err(|_| errno::EIO)?;
        let (_, batch) = record::stream_parts(&framing).map_err(|_| errno::EIO)?;
        let from = if self.first { 0 } else { batch.start as u64 };
        self.first = false;
        Ok((from, checked.body_start + checked.body_length))
    }

    /// `write(1)`: `data` continues the output stream; each whole record batch message
    /// and its body is put on the output slot as a record. The bytes are taken whole;
    /// [`Io::Waits`] when a record waits for its window, and the write is answered once it
    /// has been put ([`PlaneIo::flush`]).
    pub fn write(&mut self, process: &Process, data: &[u8]) -> Result<Io, u64> {
        if self.output.is_none() {
            return Err(errno::EBADF);
        }
        if self.written.len() + data.len() > PENDING_MAX {
            return Err(errno::EFBIG);
        }
        self.written.extend_from_slice(data);
        Ok(if self.flush(process)? {
            Io::Done(data.len() as u64)
        } else {
            Io::Waits
        })
    }

    /// Puts every whole record batch message written; `false` when one waits for its
    /// window.
    pub fn flush(&mut self, process: &Process) -> Result<bool, u64> {
        let Some((slot, handle)) = self.output else {
            return Err(errno::EBADF);
        };
        loop {
            let Some((message, body)) = whole_message(&self.written)? else {
                return Ok(true);
            };
            if message == 0 {
                // The end of the stream: nothing follows on this descriptor.
                self.written.clear();
                return Ok(true);
            }
            let bytes = &self.written[..message];
            match kind(bytes)? {
                Kind::Schema => {
                    if self.schema.is_some() {
                        return Err(errno::EINVAL);
                    }
                    self.schema = Some(bytes.to_vec());
                }
                Kind::Batch => {
                    let schema = self.schema.as_ref().ok_or(errno::EINVAL)?;
                    if !ready(handle) {
                        return Ok(false);
                    }
                    put(
                        process,
                        slot,
                        schema,
                        bytes,
                        &self.written[message..message + body],
                    )?;
                }
                // A data-plane record carries no dictionaries.
                Kind::Other => return Err(errno::EINVAL),
            }
            self.written.drain(..message + body);
        }
    }

    /// Text written to descriptor `fd` (1 or 2): each line becomes a log record.
    pub fn log(&mut self, name: &str, fd: usize, data: &[u8]) {
        for &b in data {
            if b == b'\n' {
                self.flush_line(name, fd);
            } else if self.lines[fd].len() < LINE_MAX {
                self.lines[fd].push(b);
            }
        }
    }

    /// The lines left without their newline, at the task's end.
    pub fn finish(&mut self, name: &str) {
        for fd in 1..3 {
            if !self.lines[fd].is_empty() {
                self.flush_line(name, fd);
            }
        }
    }

    fn flush_line(&mut self, name: &str, fd: usize) {
        let line = core::mem::take(&mut self.lines[fd]);
        let stream = if fd == 1 { log::STDOUT } else { log::STDERR };
        self.seq += 1;
        let pushed = match self.log.shared_region() {
            Ok(region) => match Ring::attach(region) {
                Ok(ring) => ring
                    .producer()
                    .push_record(
                        self.seq,
                        moruna_sys::now() as i64,
                        fd as u32,
                        name,
                        &[(log::STREAM, stream)],
                        &line,
                    )
                    .is_ok(),
                Err(_) => false,
            },
            Err(_) => false,
        };
        if !pushed {
            self.dropped += 1;
        }
    }
}

enum Kind {
    Schema,
    Batch,
    Other,
}

/// The message's header type, from its flatbuffer.
fn kind(message: &[u8]) -> Result<Kind, u64> {
    use moruna_ipc_check::{fb::Table, format};
    let table = Table::root(message.get(8..).ok_or(errno::EINVAL)?).map_err(|_| errno::EINVAL)?;
    Ok(
        match table
            .u8(format::message::HEADER_TYPE, 0)
            .map_err(|_| errno::EINVAL)?
        {
            format::header::SCHEMA => Kind::Schema,
            format::header::RECORD_BATCH => Kind::Batch,
            _ => Kind::Other,
        },
    )
}

/// The length of the whole message at the start of `stream` (its prefix included) and of
/// its body, once both are there; `(0, 0)` for the end marker.
fn whole_message(stream: &[u8]) -> Result<Option<(usize, usize)>, u64> {
    if stream.len() < 8 {
        return Ok(None);
    }
    if stream[..4] != [0xff; 4] {
        return Err(errno::EINVAL);
    }
    let len = u32::from_le_bytes([stream[4], stream[5], stream[6], stream[7]]) as usize;
    if len == 0 {
        return Ok(Some((0, 0)));
    }
    let message = 8 + len;
    if stream.len() < message {
        return Ok(None);
    }
    let body =
        moruna_ipc_check::frame::body_length(&stream[..message]).map_err(|_| errno::EINVAL)?;
    let body = usize::try_from(body).map_err(|_| errno::EINVAL)?;
    if stream.len() < message + body {
        return Ok(None);
    }
    Ok(Some((message, body)))
}

/// Lays the stream's `batch` message and its `body` out as a record in the output slot's
/// window and puts it.
fn put(process: &Process, slot: u32, schema: &[u8], batch: &[u8], body: &[u8]) -> Result<(), u64> {
    let reframed = record::reframe(schema, batch).map_err(|_| errno::EINVAL)?;
    let window = plane_get(process, slot).map_err(io)?.ok_or(errno::ENOSPC)?;
    if reframed.record_len > window.len() {
        return Err(errno::EFBIG);
    }
    window.write(0, &reframed.framing).map_err(io)?;
    for &(from, to, len) in &reframed.moves {
        let bytes = body
            .get(from as usize..(from + len) as usize)
            .ok_or(errno::EINVAL)?;
        window.write(to, bytes).map_err(io)?;
    }
    plane_put(process, slot, window, 0, reframed.record_len).map_err(io)
}
