//! Page locations for a row group whose file carries no page index (f.3, RB3).
//!
//! A ranged read of a row group selects its rows with a `RowSelection`. Without an offset index
//! the decoder cannot tell which rows a page holds until it has decompressed and decoded it, so
//! it decodes every page before the range just to skip it: the k-th range of a group pays for the
//! k ranges before it, and a group read in n ranges costs about n²/2 of them. Files written
//! without a page index are common (it is not pyarrow's default), so the source locates the
//! pages itself, from their headers alone: a header gives the page's kind and size and, for a
//! column with no repetition, its rows. With those locations the decoder skips whole pages by
//! offset and decompresses only the pages a range covers, so the ranges of one group together
//! cost what the group costs, and can decode on different workers at once.
//!
//! The headers are Thrift compact structs (`PageHeader` in parquet.thrift); the walker reads the
//! four fields it needs and skips every other field by its wire type. Anything it cannot read
//! (a repeated column, an unknown type, an index page, rows that do not add up to the group's)
//! yields no locations, and the read selects rows as it did before.

use std::sync::Arc;

use parquet::file::metadata::{OffsetIndexBuilder, ParquetMetaData, ParquetMetaDataBuilder};
use parquet::file::page_index::offset_index::OffsetIndexMetaData;
use parquet::file::reader::ChunkReader;

const DATA_PAGE: i64 = 0;
const DICTIONARY_PAGE: i64 = 2;
const DATA_PAGE_V2: i64 = 3;

/// How deep a skipped value may nest (a page header's statistics are two levels down).
const DEPTH: u8 = 16;

/// Whether the pages of `metadata`'s `projected` leaf columns can be located from their
/// headers: a header gives a page's rows only in a column with no repetition, so every
/// projected column must have none. A row group whose pages are found to be unlocatable later
/// (`one_group_indexed`) is told apart by the source.
pub(crate) fn locatable(metadata: &ParquetMetaData, projected: &[usize]) -> bool {
    let schema = metadata.file_metadata().schema_descr();
    projected
        .iter()
        .all(|&leaf| leaf < schema.num_columns() && schema.column(leaf).max_rep_level() == 0)
}

/// Row group `row_group` of `metadata` on its own, with page locations for its `projected` leaf
/// columns, read from the column chunks in `chunks`; `None` when its pages cannot be located
/// from their headers. The returned metadata holds one row group, so a reader built on it
/// selects row group 0.
pub(crate) fn one_group_indexed(
    metadata: &ParquetMetaData,
    row_group: usize,
    projected: &[usize],
    chunks: &impl ChunkReader,
) -> Option<Arc<ParquetMetaData>> {
    let group = metadata.row_groups().get(row_group)?;
    if !locatable(metadata, projected) {
        return None;
    }
    let mut index = Vec::with_capacity(group.num_columns());
    for leaf in 0..group.num_columns() {
        if !projected.contains(&leaf) {
            // Never read: a reader only opens the columns of its projection.
            index.push(OffsetIndexBuilder::new().build());
            continue;
        }
        let chunk = group.column(leaf);
        let start = u64::try_from(
            chunk
                .dictionary_page_offset()
                .unwrap_or_else(|| chunk.data_page_offset()),
        )
        .ok()?;
        let length = usize::try_from(chunk.compressed_size()).ok()?;
        let bytes = chunks.get_bytes(start, length).ok()?;
        index.push(locate(&bytes, start, group.num_rows())?);
    }
    let one = ParquetMetaDataBuilder::new(metadata.file_metadata().clone())
        .add_row_group(group.clone())
        .set_offset_index(Some(vec![index]))
        .build();
    Some(Arc::new(one))
}

/// The data pages of one column chunk, `bytes` starting at file offset `start`, as an offset
/// index; `None` unless they hold exactly `rows` rows.
fn locate(bytes: &[u8], start: u64, rows: i64) -> Option<OffsetIndexMetaData> {
    let mut builder = OffsetIndexBuilder::new();
    let (mut at, mut counted, mut data_pages) = (0usize, 0i64, 0usize);
    while at < bytes.len() {
        let head = header(&bytes[at..])?;
        let total = head
            .header_len
            .checked_add(usize::try_from(head.compressed).ok()?)?;
        match head.kind {
            DATA_PAGE | DATA_PAGE_V2 => {
                let page_rows = head.rows?;
                builder.append_offset_and_size(
                    i64::try_from(start.checked_add(at as u64)?).ok()?,
                    i32::try_from(total).ok()?,
                );
                builder.append_row_count(page_rows);
                counted = counted.checked_add(page_rows)?;
                data_pages += 1;
            }
            // The decoder finds the dictionary as whatever precedes the first data page.
            DICTIONARY_PAGE if data_pages == 0 => {}
            _ => return None,
        }
        at = at.checked_add(total)?;
    }
    (at == bytes.len() && counted == rows).then(|| builder.build())
}

/// What locating a page needs from its header.
struct Head {
    /// The header's own encoded length; the page body follows it.
    header_len: usize,
    kind: i64,
    compressed: i64,
    /// A data page's rows: `num_rows` of a v2 header, `num_values` of a v1 header (the same
    /// thing in a column with no repetition, the only kind located).
    rows: Option<i64>,
}

fn header(bytes: &[u8]) -> Option<Head> {
    let mut wire = Wire { bytes, at: 0 };
    let (mut kind, mut compressed, mut rows) = (None, None, None);
    let mut last = 0i16;
    loop {
        let (id, ty) = wire.field(&mut last)?;
        match (id, ty) {
            (_, STOP) => break,
            (1, I32) => kind = Some(wire.zigzag()?),
            (3, I32) => compressed = Some(wire.zigzag()?),
            // DataPageHeader.num_values
            (5, STRUCT) => rows = wire.struct_i32(1)?,
            // DataPageHeaderV2.num_rows
            (8, STRUCT) => rows = wire.struct_i32(3)?,
            _ => wire.skip(ty, DEPTH)?,
        }
    }
    Some(Head {
        header_len: wire.at,
        kind: kind?,
        compressed: compressed?,
        rows,
    })
}

const STOP: u8 = 0;
const BOOL_TRUE: u8 = 1;
const BOOL_FALSE: u8 = 2;
const BYTE: u8 = 3;
const I16: u8 = 4;
const I32: u8 = 5;
const I64: u8 = 6;
const DOUBLE: u8 = 7;
const BINARY: u8 = 8;
const LIST: u8 = 9;
const SET: u8 = 10;
const MAP: u8 = 11;
const STRUCT: u8 = 12;

/// A Thrift compact-protocol reader over one header's bytes.
struct Wire<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Wire<'_> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(b)
    }

    fn advance(&mut self, n: usize) -> Option<()> {
        let end = self.at.checked_add(n)?;
        (end <= self.bytes.len()).then(|| self.at = end)
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn zigzag(&mut self) -> Option<i64> {
        let v = self.varint()?;
        Some(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    /// The next field's id and wire type; the type is `STOP` at the end of a struct.
    fn field(&mut self, last: &mut i16) -> Option<(i16, u8)> {
        let head = self.byte()?;
        let ty = head & 0x0f;
        if ty == STOP {
            return Some((0, STOP));
        }
        let delta = head >> 4;
        let id = if delta == 0 {
            i16::try_from(self.zigzag()?).ok()?
        } else {
            last.checked_add(i16::from(delta))?
        };
        *last = id;
        Some((id, ty))
    }

    /// A struct's i32 field `want`, every other field skipped.
    fn struct_i32(&mut self, want: i16) -> Option<Option<i64>> {
        let mut found = None;
        let mut last = 0i16;
        loop {
            let (id, ty) = self.field(&mut last)?;
            match (id, ty) {
                (_, STOP) => return Some(found),
                (id, I32) if id == want => found = Some(self.zigzag()?),
                _ => self.skip(ty, DEPTH)?,
            }
        }
    }

    /// Skip a field's value of wire type `ty` (a boolean field carries its value in its type).
    fn skip(&mut self, ty: u8, depth: u8) -> Option<()> {
        match ty {
            BOOL_TRUE | BOOL_FALSE => Some(()),
            _ => self.skip_value(ty, depth),
        }
    }

    /// Skip a value of wire type `ty`; inside a collection a boolean is a byte of its own.
    fn skip_value(&mut self, ty: u8, depth: u8) -> Option<()> {
        let depth = depth.checked_sub(1)?;
        match ty {
            BOOL_TRUE | BOOL_FALSE | BYTE => self.advance(1),
            I16 | I32 | I64 => self.varint().map(|_| ()),
            DOUBLE => self.advance(8),
            BINARY => {
                let n = usize::try_from(self.varint()?).ok()?;
                self.advance(n)
            }
            LIST | SET => {
                let head = self.byte()?;
                let mut n = u64::from(head >> 4);
                if n == 15 {
                    n = self.varint()?;
                }
                for _ in 0..n {
                    self.skip_value(head & 0x0f, depth)?;
                }
                Some(())
            }
            MAP => {
                let n = self.varint()?;
                if n == 0 {
                    return Some(());
                }
                let kinds = self.byte()?;
                for _ in 0..n {
                    self.skip_value(kinds >> 4, depth)?;
                    self.skip_value(kinds & 0x0f, depth)?;
                }
                Some(())
            }
            STRUCT => {
                let mut last = 0i16;
                loop {
                    let (_, field_ty) = self.field(&mut last)?;
                    if field_ty == STOP {
                        return Some(());
                    }
                    self.skip(field_ty, depth)?;
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Float64Array, Int64Array, StringArray};
    use arrow::record_batch::RecordBatch;
    use bytes::Bytes;
    use parquet::arrow::ArrowWriter;
    use parquet::arrow::arrow_reader::{
        ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection,
    };
    use parquet::basic::Compression;
    use parquet::file::properties::{EnabledStatistics, WriterProperties};
    use parquet::file::reader::{FileReader, SerializedFileReader};

    /// A file of one 20 000-row group with small pages, no page index, statistics on, a
    /// dictionary column and a nullable one: every kind of header a taxi file has.
    fn file(v2: bool) -> Bytes {
        let rows = 20_000i64;
        let batch = RecordBatch::try_from_iter([
            (
                "id",
                Arc::new(Int64Array::from_iter_values(0..rows)) as ArrayRef,
            ),
            (
                "fare",
                Arc::new(Float64Array::from_iter(
                    (0..rows).map(|i| (i % 7 != 0).then_some(i as f64 * 0.5)),
                )) as ArrayRef,
            ),
            (
                "zone",
                Arc::new(StringArray::from_iter_values(
                    (0..rows).map(|i| format!("zone-{}", i % 13)),
                )) as ArrayRef,
            ),
        ])
        .unwrap();
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_data_page_row_count_limit(1_000)
            .set_write_batch_size(500)
            .set_statistics_enabled(EnabledStatistics::Page)
            .set_offset_index_disabled(true)
            .set_writer_version(if v2 {
                parquet::file::properties::WriterVersion::PARQUET_2_0
            } else {
                parquet::file::properties::WriterVersion::PARQUET_1_0
            })
            .build();
        let mut out = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut out, batch.schema(), Some(props)).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        Bytes::from(out)
    }

    fn read_range(
        bytes: &Bytes,
        meta: Arc<ParquetMetaData>,
        start: usize,
        end: usize,
    ) -> RecordBatch {
        let rows = meta.row_group(0).num_rows() as usize;
        let arrow = ArrowReaderMetadata::try_new(meta, ArrowReaderOptions::new()).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(bytes.clone(), arrow)
            .with_row_groups(vec![0])
            .with_batch_size(end - start)
            .with_row_selection(RowSelection::from_consecutive_ranges(
                std::iter::once(start..end),
                rows,
            ))
            .build()
            .unwrap();
        let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
        arrow::compute::concat_batches(&batches[0].schema(), &batches).unwrap()
    }

    #[test]
    fn pages_are_located_from_their_headers() {
        for v2 in [false, true] {
            let bytes = file(v2);
            let meta = SerializedFileReader::new(bytes.clone())
                .unwrap()
                .metadata()
                .clone();
            assert!(meta.offset_index().is_none(), "the file has no page index");
            let meta = Arc::new(meta);
            assert!(locatable(&meta, &[0, 1, 2]));
            assert!(!locatable(&meta, &[0, 9]), "no such column");
            assert!(one_group_indexed(&meta, 0, &[0, 9], &bytes).is_none());
            let indexed = one_group_indexed(&meta, 0, &[0, 1, 2], &bytes).expect("located");
            let index = indexed.offset_index().expect("an offset index");
            for column in &index[0] {
                let pages = column.page_locations();
                assert!(pages.len() >= 10, "small pages: {}", pages.len());
                assert_eq!(pages[0].first_row_index, 0);
            }
            // Every range reads the same rows with the index as without it.
            for (start, end) in [
                (0, 20_000),
                (0, 1),
                (3_333, 9_999),
                (19_999, 20_000),
                (7_000, 8_000),
            ] {
                assert_eq!(
                    read_range(&bytes, Arc::clone(&indexed), start, end),
                    read_range(&bytes, Arc::clone(&meta), start, end),
                    "v2 {v2}: rows {start}..{end}"
                );
            }
        }
    }

    #[test]
    fn a_projection_locates_only_its_columns() {
        let bytes = file(false);
        let meta = SerializedFileReader::new(bytes.clone())
            .unwrap()
            .metadata()
            .clone();
        let indexed = one_group_indexed(&meta, 0, &[1], &bytes).expect("located");
        let index = indexed.offset_index().unwrap();
        assert!(index[0][0].page_locations().is_empty());
        assert!(!index[0][1].page_locations().is_empty());
        assert!(index[0][2].page_locations().is_empty());
    }

    #[test]
    fn a_header_it_cannot_read_yields_no_locations() {
        assert!(header(&[0x15]).is_none(), "cut short");
        assert!(locate(&[0x15, 0x00, 0x00], 0, 1).is_none());
        assert!(header(&[0x1d]).is_none(), "an unknown wire type");
    }
}
