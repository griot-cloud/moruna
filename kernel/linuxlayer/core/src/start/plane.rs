// SPDX-License-Identifier: MPL-2.0

//! A task's input and output slots as the ports its descriptors 0 and 1 are built on
//! (`fs::task_streams`), ported from `kernel/linux/server/src/plane.rs` (SDD 29 f.7, f.8).
//!
//! Reading 0 gives the first record whole (its schema message, its record batch message
//! and its body, a valid stream message as it lies) and every later one from its record
//! batch message on. Each record batch written to 1 is laid out as a record in the
//! output window and put. The slot's calls are a [`Slot`], so the framing runs in host
//! tests against records in memory.

use moruna_ipc_check::record::{self, Need};

use crate::{
    fs::task_streams::{MessageKind, PlaneInput, PlaneOutput},
    prelude::*,
};

/// A batch or a window the core mapped for the personality.
pub(super) trait Region: Send {
    /// Its length in bytes.
    fn len(&self) -> u64;
    /// Copies its bytes `at..` into `out`.
    fn read(&self, at: u64, out: &mut [u8]) -> Result<()>;
    /// Writes `data` at `at`.
    fn write(&self, at: u64, data: &[u8]) -> Result<()>;
}

/// One of the task's slots, on the core (`moruna_sys::plane`).
pub(super) trait Slot: Send + Sync {
    type Region: Region;
    /// Waits until the slot would give a batch, a window or its end; `EINTR` when the
    /// thread is interrupted for a signal.
    fn wait(&self) -> Result<()>;
    /// Whether it would give one now.
    fn ready(&self) -> bool;
    /// The next batch or window; `None` once the engine has closed the slot.
    fn get(&self) -> Result<Option<Self::Region>>;
    /// Puts the record of `len` bytes at the start of `window`.
    fn put(&self, window: Self::Region, len: u64) -> Result<()>;
}

fn invalid(_: String) -> Error {
    Error::with_message(Errno::EIO, "a record the engine gave is not one")
}

/// The input slot: descriptor 0.
pub(super) struct Input<S: Slot> {
    slot: S,
    /// The record being read, and where the stream's part of it starts.
    current: Mutex<Option<(S::Region, u64)>>,
}

impl<S: Slot> Input<S> {
    pub(super) fn new(slot: S) -> Self {
        Self {
            slot,
            current: Mutex::new(None),
        }
    }
}

/// The part of `region`'s record that continues the stream: from its start when `first`,
/// from its record batch message otherwise, to the end of its body.
fn stream_span(region: &impl Region, first: bool) -> Result<(u64, u64)> {
    let mut framing = vec![0u8; (region.len() as usize).min(4096)];
    region.read(0, &mut framing)?;
    let end = loop {
        match record::framing_need(&framing).map_err(invalid)? {
            Need::Framing(n) if n <= framing.len() => break n,
            Need::Framing(n) | Need::More(n) => {
                if n as u64 > region.len() || n <= framing.len() {
                    return_errno_with_message!(Errno::EIO, "a record's framing runs past it");
                }
                let from = framing.len();
                framing.resize(n, 0);
                region.read(from as u64, &mut framing[from..])?;
            }
        }
    };
    framing.truncate(end);
    let checked = record::check_record(&framing, region.len()).map_err(invalid)?;
    let (_, batch) = record::stream_parts(&framing).map_err(invalid)?;
    let from = if first { 0 } else { batch.start as u64 };
    Ok((from, checked.body_start + checked.body_length))
}

impl<S: Slot> PlaneInput for Input<S> {
    fn next(&self, first: bool) -> Result<Option<u64>> {
        self.slot.wait()?;
        let mut current = self.current.lock();
        *current = None;
        let Some(region) = self.slot.get()? else {
            return Ok(None);
        };
        let (from, end) = stream_span(&region, first)?;
        *current = Some((region, from));
        Ok(Some(end - from))
    }

    fn read(&self, at: u64, out: &mut [u8]) -> Result<()> {
        let current = self.current.lock();
        let Some((region, from)) = current.as_ref() else {
            return_errno_with_message!(Errno::EINVAL, "no record is being read");
        };
        region.read(from + at, out)
    }

    fn ready(&self) -> bool {
        self.slot.ready()
    }
}

/// The output slot: descriptor 1.
pub(super) struct Output<S: Slot> {
    slot: S,
}

impl<S: Slot> Output<S> {
    pub(super) fn new(slot: S) -> Self {
        Self { slot }
    }
}

/// The message's header type, from its flatbuffer after the 8-byte prefix.
fn kind(message: &[u8]) -> Result<MessageKind> {
    use moruna_ipc_check::{fb::Table, format};
    let bad = |_| Error::with_message(Errno::EINVAL, "not an Arrow IPC message");
    let table = Table::root(message.get(8..).ok_or_else(|| bad(()))?).map_err(|_| bad(()))?;
    Ok(
        match table
            .u8(format::message::HEADER_TYPE, 0)
            .map_err(|_| bad(()))?
        {
            format::header::SCHEMA => MessageKind::Schema,
            format::header::RECORD_BATCH => MessageKind::RecordBatch,
            _ => MessageKind::Other,
        },
    )
}

impl<S: Slot> PlaneOutput for Output<S> {
    fn frame(&self, message: &[u8]) -> Result<(MessageKind, usize)> {
        let kind = kind(message)?;
        let body = moruna_ipc_check::frame::body_length(message)
            .ok()
            .and_then(|b| usize::try_from(b).ok())
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "a message's body length"))?;
        Ok((kind, body))
    }

    fn put(&self, schema: &[u8], batch: &[u8], body: &[u8]) -> Result<()> {
        let reframed = record::reframe(schema, batch)
            .map_err(|_| Error::with_message(Errno::EINVAL, "a batch its schema does not frame"))?;
        self.slot.wait()?;
        let Some(window) = self.slot.get()? else {
            return_errno_with_message!(Errno::ENOSPC, "the engine closed the output slot");
        };
        if reframed.record_len > window.len() {
            return_errno_with_message!(Errno::EFBIG, "a record larger than its window");
        }
        window.write(0, &reframed.framing)?;
        for &(from, to, len) in &reframed.moves {
            let bytes = body
                .get(from as usize..(from + len) as usize)
                .ok_or_else(|| {
                    Error::with_message(Errno::EINVAL, "a body shorter than its batch")
                })?;
            window.write(to, bytes)?;
        }
        self.slot.put(window, reframed.record_len)
    }
}
