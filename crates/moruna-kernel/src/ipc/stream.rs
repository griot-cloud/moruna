//! An Arrow IPC stream read message by message, each checked (CT-I13) before arrow-ipc decodes it
//! (contracts e.8).
//!
//! The framing is the stream format's, as arrow-ipc's own `StreamDecoder` reads it: an optional
//! continuation marker, a little-endian `u32` length, the message flatbuffer, then the body the
//! message declares; a length of zero is the end-of-stream marker, and a stream may also end at a
//! message boundary without one. Each message goes through `check` and then to arrow-ipc's own
//! decoders (`fb_to_schema`, `RecordBatchDecoder`, `read_dictionary`), so no byte reaches
//! arrow-ipc that the check has not seen.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use arrow::array::ArrayRef;
use arrow::buffer::Buffer as ArrowBuffer;
use arrow::datatypes::SchemaRef;
use arrow::ipc;
use arrow::ipc::reader::{RecordBatchDecoder, read_dictionary};
use arrow::record_batch::RecordBatch;

use super::check::{Checked, check};

/// The continuation marker that may precede a message's length.
const CONTINUATION: [u8; 4] = [0xff; 4];

/// The state of one stream: its schema and its dictionaries, as messages arrive.
#[derive(Default)]
pub struct Decoder {
    schema: Option<SchemaRef>,
    dictionaries: HashMap<i64, ArrayRef>,
}

impl Decoder {
    /// A decoder that has read nothing.
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// The stream's schema, once its schema message has been read.
    pub fn schema(&self) -> Option<&SchemaRef> {
        self.schema.as_ref()
    }

    /// One message and its body: checked (CT-I13), then decoded; `Some` for a record batch,
    /// whose arrays point into `body`.
    pub fn message(
        &mut self,
        message: &ipc::Message<'_>,
        body: &ArrowBuffer,
    ) -> Result<Option<RecordBatch>, String> {
        let version = message.version();
        match check(message, body.as_slice(), self.schema.as_deref())? {
            Checked::Schema(fb) => {
                if self.schema.is_some() {
                    return Err("the stream has a second schema".into());
                }
                self.schema = Some(Arc::new(ipc::convert::fb_to_schema(fb)));
                Ok(None)
            }
            Checked::Batch(batch) => {
                let schema = self
                    .schema
                    .clone()
                    .ok_or("a record batch comes before the schema")?;
                RecordBatchDecoder::try_new(body, batch, schema, &self.dictionaries, &version)
                    .and_then(|decoder| decoder.read_record_batch())
                    .map(Some)
                    .map_err(|e| format!("a record batch: {e}"))
            }
            Checked::Dictionary(dictionary) => {
                let schema = self
                    .schema
                    .clone()
                    .ok_or("a dictionary comes before the schema")?;
                read_dictionary(body, dictionary, &schema, &mut self.dictionaries, &version)
                    .map(|()| None)
                    .map_err(|e| format!("dictionary {}: {e}", dictionary.id()))
            }
            Checked::Empty => Ok(None),
        }
    }
}

/// Where a message's flatbuffer starts after its framing, and how long it is; `None` for the
/// end-of-stream marker.
struct Header {
    skip: usize,
    len: Option<usize>,
}

/// The framing at the start of `bytes`, which holds at least four bytes and, after a
/// continuation marker, eight.
fn header(bytes: &[u8]) -> Result<Header, String> {
    let word = |at: usize| -> Result<u32, String> {
        bytes
            .get(at..at + 4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .ok_or_else(|| "the stream ends inside a message's length".to_string())
    };
    let (skip, len) = if bytes.get(..4) == Some(&CONTINUATION[..]) {
        (8, word(4)?)
    } else {
        (4, word(0)?)
    };
    Ok(Header {
        skip,
        len: (len != 0).then_some(len as usize),
    })
}

/// The body length a message declares, which must fit in what is left of the stream.
fn body_len(message: &ipc::Message<'_>, left: u64) -> Result<usize, String> {
    let declared = message.bodyLength();
    match u64::try_from(declared) {
        Ok(len) if len <= left => usize::try_from(len)
            .map_err(|_| format!("a message body of {len} bytes does not fit this platform")),
        _ => Err(format!(
            "a message declares a body of {declared} bytes and {left} are left in the stream"
        )),
    }
}

fn parse(bytes: &[u8]) -> Result<ipc::Message<'_>, String> {
    ipc::root_as_message(bytes).map_err(|e| format!("a message is not an Arrow IPC message: {e}"))
}

/// Every record batch of a stream held whole in `bytes`, decoded in place: each batch's arrays
/// point into `bytes`.
pub fn read_stream(bytes: &ArrowBuffer) -> Result<Vec<RecordBatch>, String> {
    let all = bytes.as_slice();
    let mut decoder = Decoder::new();
    let mut batches = Vec::new();
    let mut at = 0usize;
    while at < all.len() {
        let rest = &all[at..];
        let framing = header(rest)?;
        let Some(len) = framing.len else {
            if rest.len() > framing.skip {
                return Err(format!(
                    "{} bytes follow the end-of-stream marker",
                    rest.len() - framing.skip
                ));
            }
            break;
        };
        let message = rest
            .get(framing.skip..)
            .and_then(|r| r.get(..len))
            .ok_or_else(|| format!("the stream ends inside a message of {len} bytes"))?;
        let parsed = parse(message)?;
        let start = at + framing.skip + len;
        let body_len = body_len(&parsed, (all.len() - start) as u64)?;
        let body = bytes.slice_with_length(start, body_len);
        if let Some(batch) = decoder.message(&parsed, &body)? {
            batches.push(batch);
        }
        at = start + body_len;
    }
    Ok(batches)
}

/// A stream read from `reader` one message at a time, for a stream too large to hold whole.
/// Each message and its body are read into memory of their declared sizes, which must fit in
/// the `limit` bytes the stream was said to hold, so no length read from the stream is
/// allocated before it is known to be there.
pub struct ReadStream<R> {
    reader: R,
    left: u64,
    decoder: Decoder,
    ended: bool,
}

impl<R: Read> ReadStream<R> {
    /// A stream of at most `limit` bytes from `reader`.
    pub fn new(reader: R, limit: u64) -> ReadStream<R> {
        ReadStream {
            reader,
            left: limit,
            decoder: Decoder::new(),
            ended: false,
        }
    }

    /// Up to `n` bytes: fewer when the stream or its limit ends first, which the caller judges.
    fn take(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let n = n.min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let mut out = vec![0u8; n];
        let mut got = 0;
        while got < n {
            match self.reader.read(&mut out[got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("reading the stream: {e}")),
            }
        }
        out.truncate(got);
        self.left -= got as u64;
        Ok(out)
    }

    /// The next record batch; `Ok(None)` at the end of the stream.
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, String> {
        while !self.ended {
            let mut framing = self.take(4)?;
            if framing.is_empty() {
                self.ended = true;
                break;
            }
            if framing == CONTINUATION {
                framing.extend(self.take(4)?);
            }
            let Some(len) = header(&framing)?.len else {
                self.ended = true;
                break;
            };
            let message = self.take(len)?;
            if message.len() < len {
                return Err(format!("the stream ends inside a message of {len} bytes"));
            }
            let parsed = parse(&message)?;
            let body_len = body_len(&parsed, self.left)?;
            let body = self.take(body_len)?;
            if body.len() < body_len {
                return Err(format!("the stream ends inside a body of {body_len} bytes"));
            }
            if let Some(batch) = self
                .decoder
                .message(&parsed, &ArrowBuffer::from_vec(body))?
            {
                return Ok(Some(batch));
            }
        }
        Ok(None)
    }
}

impl<R: Read> Iterator for ReadStream<R> {
    type Item = Result<RecordBatch, String>;

    /// The next record batch, or the error that ends the stream.
    fn next(&mut self) -> Option<Self::Item> {
        let next = self.next_batch();
        if next.is_err() {
            self.ended = true;
        }
        next.transpose()
    }
}
