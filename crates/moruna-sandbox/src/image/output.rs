//! An image's output read as an Arrow IPC stream (SDD 19 f.6): message by message from the
//! pipe, each checked by the hardened decoder before arrow-ipc sees it (contracts e.8). The
//! process is not trusted, so no length it writes is allocated before it is checked against
//! what one message may be.

use std::io::{BufReader, ErrorKind, Read};

use moruna_kernel::arrow::buffer::Buffer;
use moruna_kernel::arrow::ipc;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::ipc::Decoder;

/// The continuation marker that may precede a message's length.
const CONTINUATION: [u8; 4] = [0xff; 4];
/// How much of the pipe is read at once.
const READ_BUFFER: usize = 1 << 20;

/// What the next thing on the pipe is.
#[derive(Debug)]
pub enum Next {
    /// A record batch.
    Batch(RecordBatch),
    /// The end of the stream: the marker, or the pipe closed at a message boundary.
    End,
}

/// The stream read from a pipe.
pub struct Output<R> {
    reader: BufReader<R>,
    decoder: Decoder,
}

impl<R: Read> Output<R> {
    /// A stream that has read nothing.
    pub fn new(reader: R) -> Output<R> {
        Output {
            reader: BufReader::with_capacity(READ_BUFFER, reader),
            decoder: Decoder::new(),
        }
    }

    /// Exactly `n` bytes; fewer only at the end of the stream, which is `Ok(None)` when no byte
    /// was read and an error when some were.
    fn take(&mut self, n: usize, what: &str) -> Result<Option<Vec<u8>>, String> {
        let mut out = vec![0u8; n];
        let mut got = 0;
        while got < n {
            match self.reader.read(&mut out[got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("reading the output: {e}")),
            }
        }
        match got {
            0 if n > 0 => Ok(None),
            g if g < n => Err(format!("the output ends inside {what}")),
            _ => Ok(Some(out)),
        }
    }

    /// The next batch, or the end. `limit` is the most one message or body may be: a stream that
    /// declares more is refused before a byte of it is held. Every refusal is the text of why the
    /// bytes are not an Arrow IPC stream.
    pub fn next(&mut self, limit: u64) -> Result<Next, String> {
        loop {
            let Some(first) = self.take(4, "a message's length")? else {
                return Ok(Next::End);
            };
            let word = if first == CONTINUATION {
                self.take(4, "a message's length")?
                    .ok_or("the output ends after a continuation marker")?
            } else {
                first
            };
            let len = u32::from_le_bytes([word[0], word[1], word[2], word[3]]) as u64;
            if len == 0 {
                return Ok(Next::End);
            }
            if len > limit {
                return Err(format!(
                    "a message declares {len} bytes, more than the {limit} one may hold"
                ));
            }
            let message = self
                .take(len as usize, "a message")?
                .ok_or_else(|| format!("the output ends inside a message of {len} bytes"))?;
            let parsed = ipc::root_as_message(&message)
                .map_err(|e| format!("a message is not an Arrow IPC message: {e}"))?;
            let body_len = u64::try_from(parsed.bodyLength())
                .ok()
                .filter(|b| *b <= limit)
                .ok_or_else(|| {
                    format!(
                        "a message declares a body of {} bytes, more than the {limit} one may \
                         hold",
                        parsed.bodyLength()
                    )
                })?;
            let body = if body_len == 0 {
                Vec::new()
            } else {
                self.take(body_len as usize, "a message body")?
                    .ok_or_else(|| format!("the output ends inside a body of {body_len} bytes"))?
            };
            if let Some(batch) = self.decoder.message(&parsed, &Buffer::from_vec(body))? {
                return Ok(Next::Batch(batch));
            }
        }
    }
}
