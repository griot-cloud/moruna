//! A batch read in place from its record, and a batch written as records into windows.
//!
//! A data-plane record is contracts e.7's page-aligned record: a schema message, a record batch
//! message, then the body, each non-empty buffer on a page boundary from the record's start
//! (`moruna_ipc_check::record`). Reading one runs the engine's own record check before arrow-ipc
//! sees a byte, since arrow-ipc panics on some messages it cannot read. Writing one encodes the
//! batch with arrow-ipc's stream encoder, lays it out as a record (`reframe`, the layout the
//! engine's check expects), checks the framing as the engine will, and copies each buffer once,
//! from the batch's own memory into its place in the window.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_buffer::Buffer;
use arrow_ipc::convert::fb_to_schema;
use arrow_ipc::reader::read_record_batch;
use arrow_ipc::writer::StreamEncoder;
use arrow_schema::{DataType, Schema};
use moruna_ipc_check::fb::Table;
use moruna_ipc_check::format;
use moruna_ipc_check::frame::{self, CONTINUATION};
use moruna_ipc_check::record::{Need, check_record, framing_need, reframe, stream_parts};
use moruna_ipc_check::schema::check_schema;
use moruna_ipc_check::walk::{Body, Plan, Rule, Span, walk};

use crate::error::FunctionError;
use crate::lane::{Lane, Mapped, Posted};

/// What every buffer of a batch read in place holds on to. [`crate::serve`] counts its holders
/// when the function returns: any but its own is a part of the batch the function kept.
pub(crate) struct Lease {
    _keep: Option<Arc<dyn Mapped>>,
}

impl Lease {
    pub(crate) fn new(keep: Option<Arc<dyn Mapped>>) -> Arc<Lease> {
        Arc::new(Lease { _keep: keep })
    }
}

fn input(what: impl std::fmt::Display) -> FunctionError {
    FunctionError::new(format!("the input record: {what}"))
}

/// The whole message `bytes` (its marker and length, the flatbuffer and its padding), read.
fn message(bytes: &[u8]) -> Result<arrow_ipc::Message<'_>, FunctionError> {
    let fb = bytes
        .get(8..)
        .ok_or_else(|| input("a message shorter than its header"))?;
    arrow_ipc::root_as_message(fb).map_err(input)
}

/// The batch `posted` holds, read in place: every buffer a slice of the record (one that is not
/// aligned for its type is copied, as arrow-ipc does), each holding `lease`.
pub(crate) fn read(posted: &Posted, lease: &Arc<Lease>) -> Result<RecordBatch, FunctionError> {
    // SAFETY: `Posted`'s contract: `len` bytes at `addr` are readable and no one writes them until
    // the batch is released, which `serve` does only once nothing holds `lease`.
    let bytes = unsafe { std::slice::from_raw_parts(posted.addr.as_ptr(), posted.len) };
    let end = match framing_need(bytes).map_err(input)? {
        Need::Framing(n) if n <= bytes.len() => n,
        _ => return Err(input("the record is shorter than its framing")),
    };
    let framing = &bytes[..end];
    let checked = check_record(framing, bytes.len() as u64).map_err(input)?;
    let (schema_at, batch_at) = stream_parts(framing).map_err(input)?;
    let schema = message(&framing[schema_at])?
        .header_as_schema()
        .ok_or_else(|| input("the first message is not a schema"))?;
    let schema = Arc::new(fb_to_schema(schema));
    let batch_message = message(&framing[batch_at])?;
    let batch = batch_message
        .header_as_record_batch()
        .ok_or_else(|| input("the second message is not a record batch"))?;
    let owner: Arc<dyn arrow_buffer::alloc::Allocation> = lease.clone();
    // SAFETY: as above, and the record's buffers hold `lease` for as long as they live, which
    // keeps the batch unreleased (and on a host its memory alive) until the last is dropped.
    let whole = unsafe { Buffer::from_custom_allocation(posted.addr, posted.len, owner) };
    // The body is the record after its framing: the check bounded every buffer by that, not by
    // the length the message declares (which a damaged message can make shorter than its
    // buffers), so arrow-ipc never slices past it.
    let body = whole.slice(checked.body_start as usize);
    read_record_batch(
        &body,
        batch,
        schema,
        &HashMap::new(),
        None,
        &batch_message.version(),
    )
    .map_err(input)
}

/// Arrow IPC bytes held in several buffers, read as one.
struct Chunks {
    buffers: Vec<Buffer>,
    /// Where each buffer starts.
    starts: Vec<usize>,
    len: usize,
}

impl Chunks {
    fn new(buffers: Vec<Buffer>) -> Chunks {
        let mut starts = Vec::with_capacity(buffers.len());
        let mut len = 0;
        for b in &buffers {
            starts.push(len);
            len += b.len();
        }
        Chunks {
            buffers,
            starts,
            len,
        }
    }

    /// Copies `out.len()` bytes from `at`; `false` when they are not all there.
    fn copy(&self, at: usize, out: &mut [u8]) -> bool {
        if at.checked_add(out.len()).is_none_or(|end| end > self.len) {
            return false;
        }
        let mut i = self.starts.partition_point(|&s| s <= at).saturating_sub(1);
        let mut done = 0;
        while done < out.len() {
            let Some(b) = self.buffers.get(i) else {
                return false;
            };
            let from = at + done - self.starts[i];
            let n = (b.len() - from).min(out.len() - done);
            out[done..done + n].copy_from_slice(&b.as_slice()[from..from + n]);
            done += n;
            i += 1;
        }
        true
    }

    fn bytes(&self, at: usize, n: usize) -> Option<Vec<u8>> {
        let mut out = vec![0u8; n];
        self.copy(at, &mut out).then_some(out)
    }

    /// The whole message at `at`.
    fn message(&self, at: usize) -> Result<Vec<u8>, FunctionError> {
        let head = self
            .bytes(at, 8)
            .ok_or_else(|| output("the encoder wrote no message"))?;
        if head[..4] != CONTINUATION {
            return Err(output("the encoder wrote a message without its marker"));
        }
        let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        self.bytes(at, 8 + len)
            .ok_or_else(|| output("the encoder wrote a message past its bytes"))
    }
}

fn output(what: impl std::fmt::Display) -> FunctionError {
    FunctionError::new(format!("the output batch: {what}"))
}

/// A batch encoded, and laid out as a record.
pub(crate) struct Encoded {
    chunks: Chunks,
    /// Where the body starts in the encoder's bytes.
    body_at: usize,
    framing: Vec<u8>,
    /// The record's length: its framing and its body.
    pub(crate) record_len: u64,
    /// Each non-empty buffer: its offset in the encoder's body, its offset in the record, its
    /// length.
    moves: Vec<(u64, u64, u64)>,
}

fn has_dictionary(t: &DataType) -> bool {
    match t {
        DataType::Dictionary(..) => true,
        DataType::List(f)
        | DataType::LargeList(f)
        | DataType::ListView(f)
        | DataType::LargeListView(f)
        | DataType::FixedSizeList(f, _)
        | DataType::Map(f, _) => has_dictionary(f.data_type()),
        DataType::Struct(fields) => fields.iter().any(|f| has_dictionary(f.data_type())),
        DataType::Union(fields, _) => fields.iter().any(|(_, f)| has_dictionary(f.data_type())),
        DataType::RunEndEncoded(ends, values) => {
            has_dictionary(ends.data_type()) || has_dictionary(values.data_type())
        }
        _ => false,
    }
}

/// Refuses a schema with a dictionary anywhere in it: a data-plane record carries none.
fn no_dictionaries(schema: &Schema) -> Result<(), FunctionError> {
    match schema
        .fields()
        .iter()
        .find(|f| has_dictionary(f.data_type()))
    {
        Some(f) => Err(output(format!(
            "column {:?} is dictionary encoded, and a data-plane record carries no \
             dictionaries: cast it to its values",
            f.name()
        ))),
        None => Ok(()),
    }
}

/// `batch` encoded and laid out as a record, its framing passed by the engine's own check.
pub(crate) fn encode(batch: &RecordBatch) -> Result<Encoded, FunctionError> {
    no_dictionaries(batch.schema_ref())?;
    let mut encoder = StreamEncoder::try_new(batch.schema_ref())?;
    let chunks = Chunks::new(encoder.encode(batch)?);
    let schema = chunks.message(0)?;
    let at = schema.len();
    let mut batch_message = chunks.message(at)?;
    let body_at = at + batch_message.len();
    leave_out_all_valid(&schema, &mut batch_message, &chunks, body_at)?;
    let laid = reframe(&schema, &batch_message).map_err(output)?;
    check_record(&laid.framing, laid.record_len).map_err(output)?;
    Ok(Encoded {
        body_at,
        chunks,
        framing: laid.framing,
        record_len: laid.record_len,
        moves: laid.moves,
    })
}

/// The root table of the whole message `bytes` (its marker and length, then the flatbuffer).
fn message_table(bytes: &[u8]) -> Result<Table<'_>, String> {
    let len = frame::message_len(bytes)?;
    Table::root(&bytes[8..8 + len])
}

/// The header table of the whole message `bytes`, which must be of header type `want`.
fn header_table(bytes: &[u8], want: u8) -> Result<Table<'_>, String> {
    let message = message_table(bytes)?;
    if message.u8(format::message::HEADER_TYPE, format::header::NONE)? != want {
        return Err(String::from("a message of another type"));
    }
    message
        .table(format::message::HEADER)?
        .ok_or_else(|| String::from("a message without its header"))
}

/// Every buffer the check's walk reaches, numbered in entry order, and of the validity bitmaps
/// among them, the entry and the bits it covers.
#[derive(Default)]
struct Bitmaps {
    next: usize,
    found: Vec<(usize, Span, usize)>,
}

impl Plan for Bitmaps {
    fn take(&mut self, _: &str, _: &'static str, span: Span, rule: Rule) -> Result<Span, String> {
        if let Rule::Validity(bits) = rule {
            self.found.push((self.next, span, bits));
        }
        self.next += 1;
        Ok(span)
    }
}

/// Whether the first `bits` bits of the bitmap at `at` are all set (least significant first).
fn all_set(chunks: &Chunks, at: usize, bits: usize) -> bool {
    let Some(bytes) = chunks.bytes(at, bits.div_ceil(8)) else {
        return false;
    };
    let (whole, rest) = (bits / 8, bits % 8);
    let mask = (1u8 << rest).wrapping_sub(1);
    bytes[..whole].iter().all(|b| *b == 0xff) && (rest == 0 || bytes[whole] & mask == mask)
}

/// Leaves out every validity bitmap with no bit clear: arrow-ipc writes one for each column and
/// list child, all ones when nothing is null, and in a record each takes a page of its own. A
/// zero-length bitmap is how the format says "no nulls", and its node already counts none, so
/// the batch read back is the same; this is what the gate's own encoder wrote. The bitmaps are
/// found by the engine's own walk (`moruna_ipc_check::walk`), the order arrow-ipc reads them in.
fn leave_out_all_valid(
    schema: &[u8],
    batch_message: &mut [u8],
    chunks: &Chunks,
    body_at: usize,
) -> Result<(), FunctionError> {
    let fields = header_table(schema, format::header::SCHEMA)
        .and_then(check_schema)
        .map_err(output)?;
    let body = Body {
        len: usize::try_from(frame::body_length(batch_message).map_err(output)?)
            .map_err(|_| output("a negative body length"))?,
        addr: 0,
    };
    let version = message_table(batch_message)
        .and_then(|m| m.i16(format::message::VERSION, format::version::V1))
        .map_err(output)?;
    let mut bitmaps = Bitmaps::default();
    let record_batch = header_table(batch_message, format::header::RECORD_BATCH).map_err(output)?;
    walk(record_batch, body, version, &fields, Some(&mut bitmaps)).map_err(output)?;
    for (entry, span, bits) in bitmaps.found {
        if span.len > 0 && all_set(chunks, body_at + span.offset, bits) {
            frame::write_entry(batch_message, entry, span.offset as i64, 0).map_err(output)?;
        }
    }
    Ok(())
}

impl Encoded {
    /// Writes the record at the start of `window`, every byte of it: the framing, each buffer in
    /// its place, and zeros between.
    pub(crate) fn write_into(&self, window: &mut [u8]) -> Result<(), FunctionError> {
        let have = window.len() as u64;
        if self.record_len > have {
            return Err(FunctionError::window_too_small(self.record_len, have));
        }
        window[..self.framing.len()].copy_from_slice(&self.framing);
        let mut at = self.framing.len();
        for &(from, to, len) in &self.moves {
            let (to, len) = (to as usize, len as usize);
            if to < at {
                return Err(output("the record's buffers overlap"));
            }
            window[at..to].fill(0);
            if !self
                .chunks
                .copy(self.body_at + from as usize, &mut window[to..to + len])
            {
                return Err(output("a buffer past the encoder's body"));
            }
            at = to + len;
        }
        window[at..self.record_len as usize].fill(0);
        Ok(())
    }

    /// The record, in a buffer of its own: what the host's engine posts.
    #[cfg(any(test, not(any(target_os = "moruna", moruna_function_sys))))]
    pub(crate) fn to_vec(&self) -> Result<Vec<u8>, FunctionError> {
        let mut out = vec![0u8; self.record_len as usize];
        self.write_into(&mut out)?;
        Ok(out)
    }
}

/// Takes a free window, waiting for one, writes `record` into it and posts it as an answer to
/// `cookie`.
fn put(lane: &mut dyn Lane, record: &Encoded, cookie: u64) -> Result<(), FunctionError> {
    let window = loop {
        match lane.window().map_err(FunctionError::rings)? {
            Some(w) => break w,
            None if lane.wait_window() => {}
            None => return Err(FunctionError::rings("the engine gives no more windows")),
        }
    };
    // SAFETY: `Window`'s contract: the window's `len` bytes are this task's to write until the
    // record in it is posted, below, after this borrow ends.
    let bytes = unsafe { std::slice::from_raw_parts_mut(window.addr.as_ptr(), window.len) };
    record.write_into(bytes)?;
    if !lane.post(window.number, record.record_len, cookie) {
        return Err(FunctionError::rings("the output ring is full"));
    }
    Ok(())
}

/// Writes `batch` into windows as records answering `cookie`: one record when it fits a window,
/// and otherwise its rows cut into records that each fit one, in order, as the gate's tasks
/// always cut an answer (`images/gate/record/src/parts.rs`). The records it posted.
pub(crate) fn write(
    lane: &mut dyn Lane,
    batch: &RecordBatch,
    cookie: u64,
) -> Result<u64, FunctionError> {
    let have = lane.window_bytes();
    let mut rest = batch.clone();
    let mut records = 0;
    loop {
        let all = encode(&rest)?;
        if all.record_len <= have {
            put(lane, &all, cookie)?;
            return Ok(records + 1);
        }
        let rows = rest.num_rows();
        if rows <= 1 {
            return Err(FunctionError::window_too_small(all.record_len, have));
        }
        // Rows in proportion to the room past one row's record, then halved until they fit.
        let one = encode(&rest.slice(0, 1))?;
        if one.record_len > have {
            return Err(FunctionError::window_too_small(one.record_len, have));
        }
        let per_row = (all.record_len.saturating_sub(one.record_len) / (rows as u64 - 1)).max(1);
        let mut n = (((have - one.record_len) / per_row) as usize)
            .saturating_add(1)
            .clamp(1, rows - 1);
        let part = loop {
            if n == 1 {
                break one;
            }
            let part = encode(&rest.slice(0, n))?;
            if part.record_len <= have {
                break part;
            }
            n /= 2;
        };
        put(lane, &part, cookie)?;
        records += 1;
        rest = rest.slice(n, rows - n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::types::Int32Type;
    use arrow_array::{ArrayRef, DictionaryArray, Int64Array, StringArray, StructArray};
    use arrow_schema::Field;

    fn batch(n: i64) -> RecordBatch {
        let a: ArrayRef = Arc::new(Int64Array::from_iter_values(0..n));
        let s: ArrayRef = Arc::new(StringArray::from_iter_values(
            (0..n).map(|i| format!("v{i}")),
        ));
        RecordBatch::try_from_iter([("a", a), ("s", s)]).unwrap()
    }

    #[test]
    fn chunks_copy_across_their_buffers() {
        let c = Chunks::new(vec![
            Buffer::from_vec(vec![1u8, 2, 3]),
            Buffer::from_vec(Vec::<u8>::new()),
            Buffer::from_vec(vec![4u8, 5]),
            Buffer::from_vec(vec![6u8]),
        ]);
        assert_eq!(c.bytes(0, 6), Some(vec![1, 2, 3, 4, 5, 6]));
        assert_eq!(c.bytes(2, 3), Some(vec![3, 4, 5]));
        assert_eq!(c.bytes(5, 1), Some(vec![6]));
        assert_eq!(c.bytes(6, 0), Some(vec![]));
        assert_eq!(c.bytes(4, 3), None);
        assert_eq!(c.bytes(usize::MAX, 2), None);
        let bad = Chunks::new(vec![Buffer::from_vec(vec![0u8; 8])]);
        assert!(bad.message(0).is_err(), "no marker");
        let short = Chunks::new(vec![Buffer::from_vec(vec![
            0xffu8, 0xff, 0xff, 0xff, 9, 0, 0, 0,
        ])]);
        assert!(short.message(0).is_err(), "a length past the bytes");
        assert!(Chunks::new(vec![]).message(0).is_err(), "nothing");
    }

    #[test]
    fn a_record_is_page_aligned_and_checks() {
        let e = encode(&batch(100)).unwrap();
        let r = e.to_vec().unwrap();
        assert_eq!(r.len() as u64, e.record_len);
        let Ok(Need::Framing(end)) = framing_need(&r) else {
            panic!("no framing")
        };
        let checked = check_record(&r[..end], r.len() as u64).unwrap();
        assert_eq!(checked.rows, 100);
        for (at, len) in checked.buffers {
            assert!(len == 0 || at % 4096 == 0, "a buffer at {at}");
        }
        // A window shorter than the record is refused, and nothing is written past it.
        let mut small = vec![7u8; 4096];
        let err = e.write_into(&mut small).unwrap_err();
        assert_eq!(err, FunctionError::window_too_small(e.record_len, 4096));
    }

    #[test]
    fn dictionaries_anywhere_are_refused_by_name() {
        let d: ArrayRef = Arc::new(
            vec!["a", "b", "a"]
                .into_iter()
                .collect::<DictionaryArray<Int32Type>>(),
        );
        let top = RecordBatch::try_from_iter([("d", d.clone())]).unwrap();
        let e = encode(&top).err().unwrap();
        assert!(
            e.message().contains("column \"d\" is dictionary encoded"),
            "{e}"
        );
        let inner = StructArray::from(vec![(
            Arc::new(Field::new("x", d.data_type().clone(), false)),
            d,
        )]);
        let nested = RecordBatch::try_from_iter([("st", Arc::new(inner) as ArrayRef)]).unwrap();
        assert!(encode(&nested).is_err());
        for t in [
            DataType::List(Arc::new(Field::new("i", DataType::Int8, true))),
            DataType::RunEndEncoded(
                Arc::new(Field::new("e", DataType::Int32, false)),
                Arc::new(Field::new("v", DataType::Utf8, true)),
            ),
            DataType::Utf8,
        ] {
            assert!(!has_dictionary(&t), "{t}");
        }
        let union = DataType::Union(
            arrow_schema::UnionFields::try_new(
                [0],
                [Field::new(
                    "u",
                    DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Utf8)),
                    true,
                )],
            )
            .unwrap(),
            arrow_schema::UnionMode::Dense,
        );
        assert!(has_dictionary(&union));
    }

    #[test]
    fn all_set_reads_only_the_bits_it_covers() {
        let c = Chunks::new(vec![Buffer::from_vec(vec![0xffu8, 0x07, 0x00])]);
        assert!(all_set(&c, 0, 0));
        assert!(all_set(&c, 0, 8));
        assert!(
            all_set(&c, 0, 11),
            "the eleventh bit is the third of the second byte"
        );
        assert!(!all_set(&c, 0, 12));
        assert!(all_set(&c, 1, 3));
        assert!(!all_set(&c, 2, 1));
        assert!(!all_set(&c, 3, 1), "past the bytes");
    }

    /// The non-empty buffers of a record, and the record read back.
    fn laid(b: &RecordBatch) -> (usize, u64, RecordBatch) {
        let e = encode(b).unwrap();
        let r = e.to_vec().unwrap();
        let Ok(Need::Framing(end)) = framing_need(&r) else {
            panic!("no framing")
        };
        let checked = check_record(&r[..end], r.len() as u64).unwrap();
        let buffers = checked.buffers.iter().filter(|(_, len)| *len > 0).count();
        let mut stream = r.clone();
        stream.extend_from_slice(&moruna_ipc_check::frame::END_OF_STREAM);
        let mut reader =
            arrow_ipc::reader::StreamReader::try_new(std::io::Cursor::new(stream), None).unwrap();
        (buffers, e.record_len, reader.next().unwrap().unwrap())
    }

    #[test]
    fn a_bitmap_with_nothing_null_is_left_out_and_the_rest_kept() {
        use arrow_array::ListArray;
        // No nulls: the values, the offsets and the data, no bitmap; four pages in all.
        let none = batch(1);
        let (buffers, len, back) = laid(&none);
        assert_eq!((buffers, back), (3, none.clone()));
        assert!(len <= 4 * 4096, "{len}");
        // A null keeps its column's bitmap, and only that one.
        let a: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), None, Some(3)]));
        let s: ArrayRef = Arc::new(StringArray::from(vec!["x", "y", "z"]));
        let some = RecordBatch::try_from_iter([("a", a), ("s", s)]).unwrap();
        let (buffers, _, back) = laid(&some);
        assert_eq!((buffers, back), (4, some.clone()));
        // A slice whose nulls are all outside it has none left.
        let (buffers, _, back) = laid(&some.slice(2, 1));
        assert_eq!((buffers, back), (3, some.slice(2, 1)));
        // A list and its child, nothing null: the offsets and the values.
        let l: ArrayRef = Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(vec![
            Some(vec![Some(1), Some(2)]),
            Some(vec![]),
        ]));
        let lists = RecordBatch::try_from_iter([("l", l)]).unwrap();
        let (buffers, _, back) = laid(&lists);
        assert_eq!((buffers, back), (2, lists.clone()));
        // A null in the child only: the child's bitmap stays, the list's goes.
        let l: ArrayRef = Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(vec![
            Some(vec![Some(1), None]),
        ]));
        let child_null = RecordBatch::try_from_iter([("l", l)]).unwrap();
        let (buffers, _, back) = laid(&child_null);
        assert_eq!((buffers, back), (3, child_null));
        // A struct whose child has nulls: the struct's own bitmap goes.
        let i: ArrayRef = Arc::new(Int64Array::from(vec![None, Some(2)]));
        let st: ArrayRef = Arc::new(StructArray::from(vec![(
            Arc::new(Field::new("x", DataType::Int64, true)),
            i,
        )]));
        let nested = RecordBatch::try_from_iter([("st", st)]).unwrap();
        let (buffers, _, back) = laid(&nested);
        assert_eq!((buffers, back), (2, nested));
    }

    #[test]
    fn a_record_reads_back_in_place() {
        let b = batch(1000);
        let r = encode(&b).unwrap().to_vec().unwrap();
        // Page-aligned memory, as a region is.
        let mut words = vec![0u64; r.len().div_ceil(8) + 512];
        let base = words.as_mut_ptr() as usize;
        let start = (base.div_ceil(4096) * 4096 - base) / 8;
        let bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(words.as_mut_ptr().add(start) as *mut u8, r.len())
        };
        bytes.copy_from_slice(&r);
        let posted = Posted {
            cookie: 1,
            addr: std::ptr::NonNull::new(bytes.as_mut_ptr()).unwrap(),
            len: r.len(),
            keep: None,
        };
        let lease = Lease::new(None);
        let got = read(&posted, &lease).unwrap();
        assert_eq!(got, b);
        let values = got.column(0).to_data().buffers()[0].as_ptr() as usize;
        let at = bytes.as_ptr() as usize;
        assert!(
            values >= at && values < at + r.len(),
            "the values are read in place"
        );
        assert_eq!(Arc::strong_count(&lease), 2, "the batch holds the lease");
        drop(got);
        assert_eq!(Arc::strong_count(&lease), 1);
    }
}
