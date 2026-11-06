//! A plain Arrow IPC stream and the plane's records, for a program that reads a stream on its
//! standard input and writes one on its standard output (SDD 29 f.8, the descriptors of a
//! program with no Moruna library, as the Linux personality serves them on the kernel).
//!
//! A record is a stream's schema message, one record batch message and its body, laid out with
//! every buffer on a page. Its bytes are a valid stream as they lie, so a reader is given the
//! first record whole and every later one from its record batch message on ([`stream_bytes`]),
//! then the end-of-stream marker ([`END`]). A stream a program writes is read a message at a
//! time ([`Reader`]): the schema message kept, each record batch message re-laid as a record
//! ([`moruna_ipc_check::record::reframe`], exactly `encode_framing`'s layout) and its body read
//! from the stream straight into its place in a window. A dictionary message, a compressed
//! batch or a message past [`MESSAGE_MAX`] is refused with the reason, as the personality
//! refuses them: a record on the plane carries neither, and the check would refuse it.

use std::io::{ErrorKind, Read};

use moruna_ipc_check::fb::Table;
use moruna_ipc_check::format::{header, message};
use moruna_ipc_check::frame::{self, CONTINUATION};
use moruna_ipc_check::record::{self, FRAMING_MAX, Reframed};

/// The end-of-stream marker.
pub const END: [u8; 8] = frame::END_OF_STREAM;

/// The most bytes one message of a stream may declare: a record's framing is at most this.
pub const MESSAGE_MAX: usize = FRAMING_MAX;

/// The bytes of `record` a stream reader is given: the whole record when it is the stream's
/// first, its record batch message and body after; either way only to the end of the body the
/// message declares. A record may run past it (a buffer of no bytes is placed at the next page,
/// so the record's length is that page's start), and a reader takes the zeros there for the
/// end of the stream: a program given them stopped after its first batch (0.6.3).
pub fn stream_bytes(record: &[u8], first: bool) -> Result<&[u8], String> {
    let framing = match record::framing_need(record)? {
        record::Need::Framing(n) => &record[..n],
        record::Need::More(n) => {
            return Err(format!(
                "a record of {} bytes, inside its framing (which needs {n})",
                record.len()
            ));
        }
    };
    let (_, batch) = record::stream_parts(framing)?;
    let body = frame::body_length(&framing[batch.clone()])?;
    let end = u64::try_from(body)
        .ok()
        .and_then(|body| usize::try_from(body).ok())
        .and_then(|body| framing.len().checked_add(body))
        .filter(|end| *end <= record.len())
        .ok_or_else(|| {
            format!(
                "a record of {} bytes whose batch message declares a body of {body} bytes after \
                 its {}-byte framing",
                record.len(),
                framing.len()
            )
        })?;
    Ok(if first {
        &record[..end]
    } else {
        &record[batch.start..end]
    })
}

/// The schema message of `record`, as a stream carries it.
pub fn schema_of(record: &[u8]) -> Result<&[u8], String> {
    let framing = match record::framing_need(record)? {
        record::Need::Framing(n) => &record[..n],
        record::Need::More(n) => {
            return Err(format!(
                "a record of {} bytes, inside its framing (which needs {n})",
                record.len()
            ));
        }
    };
    let (schema, _) = record::stream_parts(framing)?;
    Ok(&record[schema])
}

/// A record batch read from a stream and not yet laid: its record's framing and where its
/// body's buffers go.
#[derive(Debug)]
pub struct Pending {
    laid: Reframed,
    body: u64,
}

impl Pending {
    /// The record's length: the bytes of window it needs.
    pub fn len(&self) -> u64 {
        self.laid.record_len
    }

    /// Whether the record has no bytes (never: it has its framing).
    pub fn is_empty(&self) -> bool {
        self.laid.record_len == 0
    }
}

/// A stream read a message at a time.
pub struct Reader<R> {
    input: R,
    schema: Option<Vec<u8>>,
}

impl<R: Read> Reader<R> {
    /// A stream that has read nothing.
    pub fn new(input: R) -> Reader<R> {
        Reader {
            input,
            schema: None,
        }
    }

    /// Exactly `buf.len()` bytes; `Ok(false)` when the stream ended before the first of them,
    /// an error when it ended inside them.
    fn fill(&mut self, buf: &mut [u8], what: &str) -> Result<bool, String> {
        let mut got = 0;
        while got < buf.len() {
            match self.input.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("reading the stream: {e}")),
            }
        }
        match got {
            0 if !buf.is_empty() => Ok(false),
            g if g < buf.len() => Err(format!("the stream ends inside {what}")),
            _ => Ok(true),
        }
    }

    /// `n` bytes read and dropped.
    fn skip(&mut self, mut n: u64) -> Result<(), String> {
        let mut scratch = [0u8; 4096];
        while n > 0 {
            let k = n.min(scratch.len() as u64) as usize;
            if !self.fill(&mut scratch[..k], "a message body")? {
                return Err("the stream ends inside a message body".into());
            }
            n -= k as u64;
        }
        Ok(())
    }

    /// The next message, whole as a stream carries it (the continuation marker, its length,
    /// the flatbuffer); `None` at the end of the stream: the marker, or the stream closed at a
    /// message's boundary.
    fn message(&mut self) -> Result<Option<Vec<u8>>, String> {
        let mut word = [0u8; 4];
        if !self.fill(&mut word, "a message's length")? {
            return Ok(None);
        }
        if word == CONTINUATION && !self.fill(&mut word, "a message's length")? {
            return Err("the stream ends after a continuation marker".into());
        }
        let len = u32::from_le_bytes(word) as usize;
        if len == 0 {
            return Ok(None);
        }
        if len > MESSAGE_MAX {
            return Err(format!(
                "a message declares {len} bytes, more than the {MESSAGE_MAX} one may hold"
            ));
        }
        let mut whole = vec![0u8; 8 + len];
        whole[..4].copy_from_slice(&CONTINUATION);
        whole[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        if !self.fill(&mut whole[8..], "a message")? {
            return Err(format!("the stream ends inside a message of {len} bytes"));
        }
        Ok(Some(whole))
    }

    /// The next record batch, its schema read first when it is the stream's first; `None` at
    /// the end of the stream.
    pub fn batch(&mut self) -> Result<Option<Pending>, String> {
        loop {
            let Some(whole) = self.message()? else {
                return Ok(None);
            };
            let table = Table::root(&whole[8..])
                .map_err(|e| format!("a message is not an Arrow IPC message: {e}"))?;
            let kind = table.u8(message::HEADER_TYPE, header::NONE)?;
            let body = table.i64(message::BODY_LENGTH, 0)?;
            let body = u64::try_from(body)
                .map_err(|_| format!("a message declares a body of {body} bytes"))?;
            match kind {
                header::SCHEMA if self.schema.is_none() => {
                    self.skip(body)?;
                    self.schema = Some(whole);
                }
                header::SCHEMA => return Err("the stream holds a second schema message".into()),
                header::DICTIONARY_BATCH => {
                    return Err(
                        "the stream holds a dictionary message, and a batch on the data plane \
                         carries no dictionaries"
                            .into(),
                    );
                }
                header::RECORD_BATCH => {
                    let schema = self
                        .schema
                        .as_deref()
                        .ok_or("a record batch message comes before the schema message")?;
                    let laid = record::reframe(schema, &whole)?;
                    return Ok(Some(Pending { laid, body }));
                }
                other => {
                    return Err(format!(
                        "the stream holds a {} message, which is no part of a stream of batches",
                        header::name(other)
                    ));
                }
            }
        }
    }

    /// Lays `pending` into `window` as a record: its framing at the start, its body read from
    /// the stream straight into its buffers' places. The record's length.
    pub fn lay(&mut self, pending: Pending, window: &mut [u8]) -> Result<u64, String> {
        let Pending { laid, body } = pending;
        if laid.record_len > window.len() as u64 {
            return Err(format!(
                "a record batch of {} bytes as a record, more than the {} bytes of the window",
                laid.record_len,
                window.len()
            ));
        }
        // A window is a fresh memfd, zero, so the padding between the buffers is.
        window[..laid.framing.len()].copy_from_slice(&laid.framing);
        let mut moves = laid.moves.clone();
        moves.sort_by_key(|m| m.0);
        let mut at = 0u64;
        for (from, to, len) in moves {
            if from < at {
                return Err(format!(
                    "two buffers of the record batch overlap at byte {from} of its body"
                ));
            }
            self.skip(from - at)?;
            let to = to as usize;
            let end = to + len as usize;
            if !self.fill(&mut window[to..end], "a message body")? {
                return Err("the stream ends inside a message body".into());
            }
            at = from + len;
        }
        self.skip(body - at)?;
        Ok(laid.record_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_its_own_stream() {
        let r = moruna_testkit::plane::record(&moruna_testkit::plane::mixed(5).unwrap()).unwrap();
        let first = stream_bytes(&r, true).unwrap();
        assert_eq!(first.len(), r.len());
        let later = stream_bytes(&r, false).unwrap();
        assert!(later.len() < r.len());
        assert_eq!(&r[r.len() - later.len()..], later);
        assert_eq!(schema_of(&r).unwrap(), &r[..r.len() - later.len()]);
        assert!(stream_bytes(&r[..4], true).is_err());
        assert!(schema_of(&[0xff; 4]).is_err());
    }

    /// A batch whose last buffer has no bytes (a string column of nulls): its record runs a
    /// page past its body, and a reader given those zeros ended the stream after one batch.
    #[test]
    fn a_stream_of_records_holds_every_batch_when_a_last_buffer_is_empty() {
        use std::sync::Arc;

        use arrow::array::{ArrayRef, Int64Array, RecordBatch, StringArray};
        use arrow::datatypes::{DataType, Field, Schema};

        let schema = Arc::new(Schema::new(vec![
            Field::new("x", DataType::Int64, false),
            Field::new("gap", DataType::Utf8, true),
        ]));
        let batch = |rows: i64| {
            let x: ArrayRef = Arc::new(Int64Array::from_iter_values(0..rows));
            let gap: ArrayRef = Arc::new(StringArray::from(vec![None::<&str>; rows as usize]));
            RecordBatch::try_new(Arc::clone(&schema), vec![x, gap]).unwrap()
        };
        let (one, two) = (
            moruna_testkit::plane::record(&batch(3)).unwrap(),
            moruna_testkit::plane::record(&batch(5)).unwrap(),
        );
        let first = stream_bytes(&one, true).unwrap();
        assert!(first.len() < one.len(), "the record runs past its body");
        let mut stream = first.to_vec();
        stream.extend_from_slice(stream_bytes(&two, false).unwrap());
        stream.extend_from_slice(&END);
        let read = arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(stream), None)
            .unwrap()
            .map(|b| b.unwrap().num_rows())
            .collect::<Vec<_>>();
        assert_eq!(read, vec![3, 5]);
    }
}
