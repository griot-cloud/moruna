//! stdout and stderr (std's `sys::stdio`): lines of the task's log, [`crate::plane::Log`], the
//! ring the engine gives a native task as its start slot `log`. Both streams write into the one
//! log, whose records name the stream `stderr`, as a Linux task's descriptor 2 does; each newline
//! ends a record. A task with no log slot has nowhere to write: its output is dropped, as std's
//! own unsupported platforms drop it. stdin is empty, in std's file.

use super::lock::Lock;
use crate::plane::Log;

#[expect(
    clippy::large_enum_variant,
    reason = "the one sink is a static: boxing its log would only add an allocation"
)]
enum Sink {
    /// Not looked for yet: the slot is taken at the first write.
    Unopened,
    Open(Log),
    /// The task has no log.
    None,
}

static SINK: Lock<Sink> = Lock::new(Sink::Unopened);

impl Sink {
    fn log(&mut self) -> Option<&mut Log> {
        if let Sink::Unopened = self {
            *self = Log::take().map_or(Sink::None, Sink::Open);
        }
        match self {
            Sink::Open(log) => Some(log),
            _ => None,
        }
    }
}

/// Writes `bytes` to the log; every byte is taken, whether or not the log keeps the line.
pub fn write(bytes: &[u8]) -> usize {
    if let Some(log) = SINK.lock().log() {
        log.write_bytes(bytes);
    }
    bytes.len()
}

/// Writes the line the streams left without a newline, and how many lines the log dropped.
pub fn finish() {
    if let Sink::Open(log) = &mut *SINK.lock() {
        log.finish();
    }
}

/// [`finish`], unless a thread holds the log now: an abort does not wait on one.
pub fn finish_unless_busy() {
    if let Some(mut sink) = SINK.try_lock()
        && let Sink::Open(log) = &mut *sink
    {
        log.finish();
    }
}
