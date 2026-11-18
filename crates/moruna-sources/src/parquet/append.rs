//! A range decoded a batch at a time into the arena (system design 3A, item 6.3a; G-I2).
//!
//! The decoder builds its batches in the heap, outside the arena. Built whole, a range of a row
//! group held its whole decoded size there, about one and a half to two times over while its
//! buffers grew, until the decode copy put it in the arena; two workers decoding at once held
//! twice that, past the headroom the arena leaves (at a budget of 672 MiB, 339 MB against
//! 280 MB). Here the decoder gives the range in batches, and each batch is copied once into
//! buffers in the arena that hold the whole range, so a worker holds one batch outside the
//! arena whatever its range.
//!
//! The arena buffers are reserved with the first batch, from the read's charge: a column of
//! fixed width at exactly its rows, a validity bitmap at a bit a row, and a string or binary
//! column's offsets at one a row. Its bytes are known exactly only for a whole row group whose
//! file states them (Parquet's size statistics): there they are reserved at that figure rounded
//! up to the power of two the arena's size class charges anyway (02 e.2), and a group that
//! decodes to more than its file states is refused, naming the column and both figures, since a
//! second copy is what G-I2 forbids. For a part of a group, or a file that does not state them,
//! a share by rows is no bound (one long row in a part is enough, SO-I9), so that column's bytes
//! stay where the decoder put them until the range ends, and are then copied once into an arena
//! buffer of exactly their size.
//!
//! **The rule this keeps:** what a read holds outside the arena never exceeds its own decoded
//! bytes (1.0x), which is what the arena sizing assumes of every byte in flight (the runtime's
//! `arena_host_bytes`). A batch of its fixed-width columns is a part of them, and so are the
//! bytes of a string or binary column it holds whole; so however many workers decode at once,
//! the reads in flight hold no more outside the arena than the headroom the sizing left for them,
//! string-heavy parts included.
//!
//! Only flat columns are written this way: fixed width, boolean, string and binary (dictionaries
//! unpacked, as `decode_copy` unpacks them). A range with any other column is decoded whole and
//! copied by `decode_copy`, as before.

use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayData, ArrayRef, RecordBatch, RecordBatchOptions, make_array,
};
use moruna_kernel::arrow::buffer::{BooleanBuffer, Buffer as ArrowBuffer, NullBuffer};
use moruna_kernel::arrow::datatypes::{DataType, Field, Fields, Schema, SchemaRef};
use moruna_kernel::arrow::util::bit_mask::set_bits;
use moruna_kernel::{Allocator, Buffer, Result, SplitId, Tier};

use crate::util::source_err;

/// How a column's values are laid out.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Layout {
    /// `width` bytes a value.
    Fixed(usize),
    /// A bit a value.
    Bits,
    /// Offsets of `width` bytes, and the bytes they point into.
    Bytes(usize),
    /// No buffer at all.
    Nothing,
}

/// The layout of a column of `data_type` once its dictionary is unpacked; `None` for a type
/// this module does not write.
fn layout(data_type: &DataType) -> Option<Layout> {
    let plain = match data_type {
        DataType::Dictionary(_, values) => values.as_ref(),
        other => other,
    };
    match plain {
        DataType::Null => Some(Layout::Nothing),
        DataType::Boolean => Some(Layout::Bits),
        DataType::Utf8 | DataType::Binary => Some(Layout::Bytes(4)),
        DataType::LargeUtf8 | DataType::LargeBinary => Some(Layout::Bytes(8)),
        DataType::FixedSizeBinary(width) => usize::try_from(*width).ok().map(Layout::Fixed),
        other => other.primitive_width().map(Layout::Fixed),
    }
}

/// Whether every column of `schema` can be written a batch at a time.
pub(crate) fn writable(schema: &Schema) -> bool {
    schema
        .fields()
        .iter()
        .all(|field| layout(field.data_type()).is_some())
}

/// One column's arena buffers, filled a batch at a time.
struct Column {
    field: Arc<Field>,
    layout: Layout,
    values: Buffer,
    /// Bytes of the column's values written; bits for `Layout::Bits`.
    filled: usize,
    /// A bytes column whose bytes have no exact figure: each batch's, where the decoder put
    /// them, until `finish` copies them once into an arena buffer of exactly their size.
    held: Option<Vec<(ArrowBuffer, usize, usize)>>,
    offsets: Option<Buffer>,
    validity: Option<Buffer>,
    nulls: usize,
}

/// A range's columns in the arena, filled a batch at a time (see the module).
pub(crate) struct Appender<'a> {
    alloc: &'a dyn Allocator,
    tier: Tier,
    split: SplitId,
    schema: Arc<Schema>,
    columns: Vec<Column>,
    rows: usize,
    /// The rows written so far.
    at: usize,
}

/// An arena buffer of `bytes`, zeroed when `zeroed` (a bitmap is written by setting bits).
fn reserve(alloc: &dyn Allocator, tier: Tier, bytes: usize, zeroed: bool) -> Result<Buffer> {
    let mut buffer = alloc.alloc(bytes, tier)?;
    if zeroed {
        buffer[..bytes].fill(0);
    }
    Ok(buffer)
}

impl<'a> Appender<'a> {
    /// The arena buffers for `rows` rows of `schema`. `exact`, when given, is each column's
    /// decoded bytes as the file states them for exactly these rows (a whole row group with size
    /// statistics); without it a bytes column's bytes are held until `finish`. `schema` must be
    /// `writable`.
    pub(crate) fn reserve(
        split: SplitId,
        schema: &SchemaRef,
        rows: usize,
        exact: Option<&[u64]>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> Result<Appender<'a>> {
        let mut columns = Vec::with_capacity(schema.fields().len());
        let mut fields = Vec::with_capacity(schema.fields().len());
        for (index, field) in schema.fields().iter().enumerate() {
            let Some(layout) = layout(field.data_type()) else {
                return Err(source_err(
                    split,
                    format!("column {} cannot be decoded in batches", field.name()),
                ));
            };
            let plain = match field.data_type() {
                DataType::Dictionary(_, values) => values.as_ref().clone(),
                other => other.clone(),
            };
            let field = Arc::new(
                Field::new(field.name(), plain, field.is_nullable())
                    .with_metadata(field.metadata().clone()),
            );
            let bitmap = rows.div_ceil(8);
            let validity = field
                .is_nullable()
                .then(|| reserve(alloc, tier, bitmap, true))
                .transpose()?;
            let (values, offsets, held) = match layout {
                Layout::Fixed(width) => (reserve(alloc, tier, rows * width, false)?, None, None),
                Layout::Bits => (reserve(alloc, tier, bitmap, true)?, None, None),
                Layout::Nothing => (reserve(alloc, tier, 0, false)?, None, None),
                Layout::Bytes(width) => {
                    let mut offsets = reserve(alloc, tier, (rows + 1) * width, false)?;
                    offsets[..width].fill(0);
                    match exact.and_then(|bytes| bytes.get(index)) {
                        // The file's figure counts the offsets beside the bytes; what is left
                        // over a count of them is the bytes, and its bitmap if it has nulls.
                        Some(stated) => {
                            let bytes = stated.saturating_sub(((rows + 1) * width) as u64);
                            let room = usize::try_from(bytes.max(1))
                                .unwrap_or(usize::MAX)
                                .checked_next_power_of_two()
                                .unwrap_or(usize::MAX);
                            (reserve(alloc, tier, room, false)?, Some(offsets), None)
                        }
                        None => (
                            reserve(alloc, tier, 0, false)?,
                            Some(offsets),
                            Some(Vec::new()),
                        ),
                    }
                }
            };
            fields.push(Arc::clone(&field));
            columns.push(Column {
                field,
                layout,
                values,
                filled: 0,
                held,
                offsets,
                validity,
                nulls: 0,
            });
        }
        let schema =
            Arc::new(Schema::new(Fields::from(fields)).with_metadata(schema.metadata().clone()));
        Ok(Appender {
            alloc,
            tier,
            split,
            schema,
            columns,
            rows,
            at: 0,
        })
    }

    /// Copy `batch`, the next rows of the range, into the arena buffers.
    pub(crate) fn append(&mut self, batch: &RecordBatch) -> Result<()> {
        let len = batch.num_rows();
        if self.at + len > self.rows {
            return Err(source_err(
                self.split,
                format!(
                    "the decoder gave {} rows for a range of {} (SO-I4)",
                    self.at + len,
                    self.rows
                ),
            ));
        }
        for (column, array) in self.columns.iter_mut().zip(batch.columns()) {
            let array = plain(array, self.split)?;
            column.append(&array.to_data(), self.at, self.split)?;
        }
        self.at += len;
        Ok(())
    }

    /// The range as one batch over the arena buffers, once every row has been appended.
    pub(crate) fn finish(self) -> Result<RecordBatch> {
        if self.at != self.rows {
            return Err(source_err(
                self.split,
                format!(
                    "the decoder returned {} rows for a range of {} (SO-I4)",
                    self.at, self.rows
                ),
            ));
        }
        let (rows, split, alloc, tier) = (self.rows, self.split, self.alloc, self.tier);
        let columns = self
            .columns
            .into_iter()
            .map(|column| column.finish(rows, split, alloc, tier))
            .collect::<Result<Vec<ArrayRef>>>()?;
        RecordBatch::try_new_with_options(
            self.schema,
            columns,
            &RecordBatchOptions::new().with_row_count(Some(rows)),
        )
        .map_err(|e| source_err(split, format!("building the decoded range: {e}")))
    }
}

/// A dictionary column as a plain array of its value type; anything else as it is.
fn plain(array: &ArrayRef, split: SplitId) -> Result<ArrayRef> {
    match array.data_type() {
        DataType::Dictionary(_, values) => {
            moruna_kernel::arrow::compute::cast(array, values.as_ref())
                .map_err(|e| source_err(split, format!("unpacking a dictionary column: {e}")))
        }
        _ => Ok(Arc::clone(array)),
    }
}

impl Column {
    /// Copy `data`'s rows to rows `at..` of this column.
    fn append(&mut self, data: &ArrayData, at: usize, split: SplitId) -> Result<()> {
        let (len, offset) = (data.len(), data.offset());
        if let Some(validity) = self.validity.as_mut() {
            match data.nulls() {
                Some(nulls) => {
                    let bits = nulls.inner();
                    self.nulls +=
                        set_bits(&mut validity[..], bits.values(), at, bits.offset(), len);
                }
                None => set_ones(&mut validity[..], at, len),
            }
        } else if data.null_count() > 0 {
            return Err(source_err(
                split,
                format!(
                    "column {} is not nullable and holds nulls",
                    self.field.name()
                ),
            ));
        }
        match self.layout {
            Layout::Nothing => {}
            Layout::Fixed(width) => {
                let from = &data.buffers()[0].as_slice()[offset * width..(offset + len) * width];
                self.values[at * width..(at + len) * width].copy_from_slice(from);
                self.filled = (at + len) * width;
            }
            Layout::Bits => {
                set_bits(
                    &mut self.values[..],
                    data.buffers()[0].as_slice(),
                    at,
                    offset,
                    len,
                );
                self.filled = at + len;
            }
            Layout::Bytes(width) => {
                let starts = offsets(&data.buffers()[0], width, offset, len);
                let (first, last) = (starts[0], starts[len]);
                let bytes = &data.buffers()[1].as_slice()[first..last];
                let base = self.filled;
                if self.held.is_none() && base + bytes.len() > self.values.len() {
                    return Err(source_err(
                        split,
                        format!(
                            "column {} of the row group decodes to more than the {} bytes its \
                             file states for it; copying it again to grow its buffer is what the \
                             one decode copy forbids (G-I2), so the read is refused",
                            self.field.name(),
                            self.values.len()
                        ),
                    ));
                }
                if width == 4 && base + bytes.len() > i32::MAX as usize {
                    return Err(source_err(
                        split,
                        format!(
                            "column {} holds more than 2 GiB of bytes in one range",
                            self.field.name()
                        ),
                    ));
                }
                match self.held.as_mut() {
                    Some(held) => {
                        // The batches before this one are dropped now, so their buffers are
                        // ours alone: what the decoder grew them to beyond their bytes goes back,
                        // and a held column holds its own bytes and no more (the rule above).
                        for (buffer, _, _) in held.iter_mut() {
                            buffer.shrink_to_fit();
                        }
                        held.push((data.buffers()[1].clone(), first, last));
                    }
                    None => self.values[base..base + bytes.len()].copy_from_slice(bytes),
                }
                self.filled += bytes.len();
                let out = self.offsets.as_mut().expect("a bytes column has offsets");
                for (row, start) in starts.iter().enumerate().skip(1) {
                    let value = (base + start - first) as u64;
                    let at_byte = (at + row) * width;
                    let slot = &mut out[at_byte..at_byte + width];
                    if width == 4 {
                        slot.copy_from_slice(&(value as u32).to_le_bytes());
                    } else {
                        slot.copy_from_slice(&value.to_le_bytes());
                    }
                }
            }
        }
        Ok(())
    }

    /// The column as an array over its arena buffers; a held column's bytes copied into one
    /// of exactly their size, from `alloc` in `tier`.
    fn finish(
        self,
        rows: usize,
        split: SplitId,
        alloc: &dyn Allocator,
        tier: Tier,
    ) -> Result<ArrayRef> {
        let arrow = |buffer: Buffer| -> Result<ArrowBuffer> {
            let len = buffer.len();
            Ok(buffer.into_arrow_buffer()?.slice_with_length(0, len))
        };
        let mut builder = ArrayData::builder(self.field.data_type().clone()).len(rows);
        match self.layout {
            Layout::Nothing => {}
            Layout::Fixed(_) | Layout::Bits => builder = builder.add_buffer(arrow(self.values)?),
            Layout::Bytes(_) => {
                let offsets = self.offsets.expect("a bytes column has offsets");
                let values = match self.held {
                    Some(held) => {
                        let mut values = alloc.alloc(self.filled, tier)?;
                        let mut at = 0;
                        for (buffer, first, last) in held {
                            values[at..at + last - first]
                                .copy_from_slice(&buffer.as_slice()[first..last]);
                            at += last - first;
                        }
                        values
                    }
                    // What the stated figure's rounding left unused goes back with the tail.
                    None => self.values.split_at(self.filled).0,
                };
                builder = builder
                    .add_buffer(arrow(offsets)?)
                    .add_buffer(arrow(values)?);
            }
        }
        if let Some(validity) = self.validity
            && self.nulls > 0
        {
            let bits = BooleanBuffer::new(arrow(validity)?, 0, rows);
            builder = builder.nulls(Some(NullBuffer::new(bits)));
        }
        let data = builder.build().map_err(|e| {
            source_err(
                split,
                format!("building column {} of the range: {e}", self.field.name()),
            )
        })?;
        Ok(make_array(data))
    }
}

/// Set bits `from..from + len` of `bitmap`: rows with no null.
fn set_ones(bitmap: &mut [u8], from: usize, len: usize) {
    let end = from + len;
    let mut bit = from;
    while bit < end && !bit.is_multiple_of(8) {
        moruna_kernel::arrow::util::bit_util::set_bit(bitmap, bit);
        bit += 1;
    }
    let whole = (end - bit) / 8;
    bitmap[bit / 8..bit / 8 + whole].fill(0xFF);
    bit += whole * 8;
    while bit < end {
        moruna_kernel::arrow::util::bit_util::set_bit(bitmap, bit);
        bit += 1;
    }
}

/// The `len + 1` offsets of a bytes array's rows, from `offset`, as byte positions.
fn offsets(buffer: &ArrowBuffer, width: usize, offset: usize, len: usize) -> Vec<usize> {
    let raw = &buffer.as_slice()[offset * width..(offset + len + 1) * width];
    raw.chunks_exact(width)
        .map(|chunk| {
            if width == 4 {
                i32::from_le_bytes(chunk.try_into().unwrap_or_default()) as usize
            } else {
                i64::from_le_bytes(chunk.try_into().unwrap_or_default()) as usize
            }
        })
        .collect()
}

/// The bytes a range's arena buffers hold once filled: what the decode copied.
pub(crate) fn payload_bytes(batch: &RecordBatch) -> u64 {
    fn of(data: &ArrayData) -> u64 {
        let own: usize = data.buffers().iter().map(|b| b.len()).sum::<usize>()
            + data.nulls().map_or(0, |n| n.inner().inner().len());
        own as u64 + data.child_data().iter().map(of).sum::<u64>()
    }
    batch.columns().iter().map(|c| of(&c.to_data())).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::arrow::array::{
        BooleanArray, DictionaryArray, FixedSizeBinaryArray, Float64Array, Int64Array,
        LargeBinaryArray, NullArray, StringArray,
    };
    use moruna_kernel::arrow::datatypes::Int32Type;
    use moruna_testkit::FakeAllocator;

    /// Rows of every flat kind, nulls in the nullable ones, a dictionary and one long string.
    fn batch(rows: usize) -> RecordBatch {
        let text = |r: usize| {
            if r == rows / 2 {
                "x".repeat(10_000)
            } else {
                format!("t{r}")
            }
        };
        RecordBatch::try_from_iter([
            (
                "id",
                Arc::new(Int64Array::from_iter(
                    (0..rows).map(|r| (r % 5 != 0).then_some(r as i64)),
                )) as ArrayRef,
            ),
            (
                "fare",
                Arc::new(Float64Array::from_iter_values((0..rows).map(|r| r as f64))) as ArrayRef,
            ),
            (
                "flag",
                Arc::new(BooleanArray::from_iter(
                    (0..rows).map(|r| (r % 7 != 0).then_some(r % 3 == 0)),
                )) as ArrayRef,
            ),
            (
                "text",
                Arc::new(StringArray::from_iter(
                    (0..rows).map(|r| (r % 11 != 0).then(|| text(r))),
                )) as ArrayRef,
            ),
            (
                "blob",
                Arc::new(LargeBinaryArray::from_iter_values(
                    (0..rows).map(|r| vec![r as u8; r % 4]),
                )) as ArrayRef,
            ),
            (
                "code",
                Arc::new(
                    FixedSizeBinaryArray::try_from_iter((0..rows).map(|r| [r as u8, 1, 2]))
                        .expect("fixed"),
                ) as ArrayRef,
            ),
            (
                "zone",
                Arc::new(
                    (0..rows)
                        .map(|r| ["a", "bb", "ccc"][r % 3])
                        .collect::<DictionaryArray<Int32Type>>(),
                ) as ArrayRef,
            ),
            ("nothing", Arc::new(NullArray::new(rows)) as ArrayRef),
        ])
        .expect("the batch")
    }

    /// `whole` appended in slices of `each` rows, reserved with `exact`; the finished batch.
    fn appended(
        whole: &RecordBatch,
        each: usize,
        exact: Option<&[u64]>,
        alloc: &FakeAllocator,
    ) -> Result<RecordBatch> {
        let mut into = Appender::reserve(
            7,
            &whole.schema(),
            whole.num_rows(),
            exact,
            alloc,
            Tier::Host,
        )?;
        let mut at = 0;
        while at < whole.num_rows() {
            let len = each.min(whole.num_rows() - at);
            into.append(&whole.slice(at, len))?;
            at += len;
        }
        into.finish()
    }

    /// What every column decodes to, as a file that states its sizes would say.
    fn stated(whole: &RecordBatch) -> Vec<u64> {
        whole
            .columns()
            .iter()
            .map(|column| plain(column, 7).expect("plain").get_array_memory_size() as u64)
            .collect()
    }

    fn assert_same(got: &RecordBatch, whole: &RecordBatch) {
        assert_eq!(got.num_rows(), whole.num_rows());
        for (index, column) in whole.columns().iter().enumerate() {
            let want = plain(column, 7).expect("plain");
            assert_eq!(got.column(index).as_ref(), want.as_ref(), "column {index}");
        }
        assert!(
            !matches!(got.schema().field(6).data_type(), DataType::Dictionary(..)),
            "the dictionary is unpacked, as decode_copy unpacks it"
        );
    }

    #[test]
    fn a_range_appended_in_batches_is_the_range_copied_once() {
        let whole = batch(1000);
        let figures = stated(&whole);
        for exact in [Some(figures.as_slice()), None] {
            let alloc = FakeAllocator::new();
            let got = appended(&whole, 64, exact, &alloc).expect("appended");
            assert_same(&got, &whole);
            // One copy: the arena holds the payload's bytes and nothing else, the stated
            // figure's unused tail given back.
            assert_eq!(alloc.in_use(Tier::Host), payload_bytes(&got));
            drop(got);
            assert_eq!(alloc.in_use(Tier::Host), 0);
        }
    }

    #[test]
    fn a_held_column_takes_any_row_however_long() {
        // A share by rows would be a few hundred bytes; the long row is ten thousand.
        let whole = batch(40);
        let alloc = FakeAllocator::new();
        let got = appended(&whole, 3, None, &alloc).expect("appended");
        assert_same(&got, &whole);
    }

    #[test]
    fn a_group_that_decodes_to_more_than_its_file_states_is_refused() {
        let whole = batch(1000);
        let mut figures = stated(&whole);
        figures[3] = 8;
        let error = appended(&whole, 64, Some(&figures), &FakeAllocator::new()).unwrap_err();
        assert!(error.to_string().contains("more than the"), "{error}");
        assert!(error.to_string().contains("text"), "{error}");
    }

    #[test]
    fn rows_that_do_not_add_up_are_refused() {
        let whole = batch(10);
        let alloc = FakeAllocator::new();
        let mut short =
            Appender::reserve(7, &whole.schema(), 20, None, &alloc, Tier::Host).expect("reserved");
        short.append(&whole).expect("ten rows");
        assert!(short.finish().is_err(), "ten rows of twenty");
        let mut long =
            Appender::reserve(7, &whole.schema(), 5, None, &alloc, Tier::Host).expect("reserved");
        assert!(long.append(&whole).is_err(), "ten rows into five");
    }

    #[test]
    fn nulls_in_a_column_that_holds_none_are_refused() {
        let declared = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let alloc = FakeAllocator::new();
        let mut into =
            Appender::reserve(7, &declared, 2, None, &alloc, Tier::Host).expect("reserved");
        let nulls = RecordBatch::try_from_iter([(
            "id",
            Arc::new(Int64Array::from(vec![Some(1), None])) as ArrayRef,
        )])
        .expect("the batch");
        let error = into.append(&nulls).unwrap_err();
        assert!(error.to_string().contains("holds nulls"), "{error}");
    }

    #[test]
    fn only_flat_columns_are_written_in_batches() {
        let flat = batch(4).schema();
        assert!(writable(&flat));
        let nested = Schema::new(vec![Field::new(
            "tags",
            DataType::List(Arc::new(Field::new("item", DataType::Int32, true))),
            true,
        )]);
        assert!(!writable(&nested));
        let error = Appender::reserve(
            7,
            &Arc::new(nested),
            1,
            None,
            &FakeAllocator::new(),
            Tier::Host,
        )
        .err()
        .expect("refused");
        assert!(
            error.to_string().contains("cannot be decoded in batches"),
            "{error}"
        );
    }
}
