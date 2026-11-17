// SPDX-License-Identifier: MPL-2.0

//! A task's text as records in the engine's log ring (`moruna-record`), one a line, as
//! `kernel/linux/server/src/plane.rs` writes them: the task's name, the descriptor, and a
//! `stream` header of `stdout` or `stderr` (`linux_protocol::log`).

use core::sync::atomic::{AtomicU64, Ordering};

use moruna_record::ring::Ring;

use crate::{fs::task_streams::TextSink, prelude::*};

/// The header naming the descriptor a line was written to (`linux_protocol::log`).
const STREAM: &str = "stream";
const STDOUT: &str = "stdout";
const STDERR: &str = "stderr";

/// The ring's bytes, shared with the engine: lent one borrow at a time.
pub(super) trait RingBytes: Send + Sync {
    /// Runs `f` on the bytes; `None` when they cannot be reached.
    fn with<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> Option<R>;
}

/// The log ring.
pub(super) struct RingText<B: RingBytes> {
    bytes: B,
    name: String,
    seq: AtomicU64,
}

impl<B: RingBytes> RingText<B> {
    /// The ring in `bytes`, whose records carry `name`; `EINVAL` when they are not a ring.
    pub(super) fn new(bytes: B, name: String) -> Result<Self> {
        if bytes.with(|b| Ring::attach(b).is_ok()) != Some(true) {
            return_errno_with_message!(Errno::EINVAL, "the log is not a ring");
        }
        Ok(Self {
            bytes,
            name,
            seq: AtomicU64::new(0),
        })
    }
}

impl<B: RingBytes> TextSink for RingText<B> {
    fn line(&self, fd: u8, line: &[u8]) -> bool {
        let stream = if fd == 1 { STDOUT } else { STDERR };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.bytes
            .with(|bytes| {
                let Ok(ring) = Ring::attach(bytes) else {
                    return false;
                };
                ring.producer()
                    .push_record(
                        seq,
                        ostd::timer::monotonic_nanos() as i64,
                        u32::from(fd),
                        &self.name,
                        &[(STREAM, stream)],
                        line,
                    )
                    .is_ok()
            })
            .unwrap_or(false)
    }
}
