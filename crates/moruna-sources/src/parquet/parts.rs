//! What the parts of one row group share as they decode (system design 3A, item 6.3l).
//!
//! The drive cuts a row group into parts so that more than one worker can decode it (6.3a), and a
//! smaller arena cuts it into more of them, since the morsel a read is sized to follows the
//! sink's share of the arena. Each part is decoded by a reader of its own, and a column reader
//! begins at its column chunk's first page, which in a dictionary-encoded column is the
//! dictionary page: every part decompressed and decoded every column's dictionary again. On the
//! NYC year at 384 MiB (1,480 reads of 46 row groups) that was 2.9 of 6.2 s of decode CPU, and
//! about 1.6 ms of the 2 ms each part added (6.3l).
//!
//! Here each column's dictionary page is decompressed once a group, by the first of its parts to
//! decode, into arena buffers of that part's read (`Kept`): admission sees them as bytes that
//! read has taken (6.3c), and they are freed when the last part holding them has decoded, since
//! every part's fetch holds the group's `GroupDecode` and the source lets go of it with the
//! group's fetch (`ParquetSource::claim_fetch`). Every part's column readers take the kept
//! page in place of the file's (`WithKeptDictionary`), so the parts stay independent: any worker
//! decodes any part, in any order, several at once. What a part still does for itself is decode
//! the kept page into its column reader's dictionary (`set_dict`, a copy of the dictionary's
//! values), and decompress the page its first row falls in when a cut is inside it.
//!
//! The reader is built through parquet's public lower-level API: the group's column chunks as
//! `RowGroups`, and `ParquetRecordBatchReader::try_new_with_row_groups` over the projection's
//! field levels, which are worked out once a group as well.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use moruna_kernel::{Allocator, Result, SplitId, Tier};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, RowGroups, RowSelection};
use parquet::arrow::{
    FieldLevels, ProjectionMask, parquet_to_arrow_field_levels, parquet_to_arrow_schema,
};
use parquet::column::page::{Page, PageIterator, PageMetadata, PageReader};
use parquet::errors::Result as ParquetResult;
use parquet::file::metadata::{ParquetMetaData, RowGroupMetaData};
use parquet::file::serialized_reader::SerializedPageReader;

use super::plan::FileMeta;
use super::reader::ArenaChunks;
use crate::util::source_err;

/// A row group's fetched column chunks, and the decode state its parts share. A clone shares
/// both.
#[derive(Clone)]
pub(crate) struct GroupFetch {
    pub(crate) chunks: ArenaChunks,
    pub(crate) decode: Arc<GroupDecode>,
}

impl GroupFetch {
    /// A fresh fetch of a group: nothing decoded yet.
    pub(crate) fn new(chunks: ArenaChunks) -> GroupFetch {
        GroupFetch {
            chunks,
            decode: Arc::new(GroupDecode::default()),
        }
    }
}

/// The decode state the parts of one row group share, made by the first part to decode.
#[derive(Default)]
pub(crate) struct GroupDecode {
    kept: Mutex<Option<Arc<Kept>>>,
}

/// What a group's parts take instead of working it out again.
pub(crate) struct Kept {
    levels: FieldLevels,
    /// By leaf column of the group: its dictionary page, decompressed, in arena buffers; `None`
    /// for a column outside the projection or one with no dictionary page.
    dictionaries: Vec<Option<Page>>,
}

impl GroupDecode {
    /// The group's kept state, made now by the first part to ask, from `chunks` (whose pages
    /// it takes are counted as that part's) and into arena buffers of `alloc`, the asking part's
    /// read. A part that asks while it is being made waits for it: making it is the work the
    /// part would otherwise do itself. A failure keeps nothing, so a later part tries again.
    pub(crate) fn kept(
        &self,
        file: &FileMeta,
        indexed: &ParquetMetaData,
        chunks: &Arc<ArenaChunks>,
        alloc: &dyn Allocator,
        tier: Tier,
        split: SplitId,
    ) -> Result<Arc<Kept>> {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(kept) = kept.as_ref() {
            return Ok(Arc::clone(kept));
        }
        let made = Arc::new(make(file, indexed, chunks, alloc, tier, split)?);
        *kept = Some(Arc::clone(&made));
        Ok(made)
    }
}

/// The group's field levels for the projection, and each projected column's dictionary page
/// decompressed into an arena buffer of `alloc`.
fn make(
    file: &FileMeta,
    indexed: &ParquetMetaData,
    chunks: &Arc<ArenaChunks>,
    alloc: &dyn Allocator,
    tier: Tier,
    split: SplitId,
) -> Result<Kept> {
    let parquet_err = |e: parquet::errors::ParquetError| source_err(split, e.to_string());
    let schema = file.metadata.file_metadata().schema_descr();
    // The embedded Arrow schema, when the writer left one, is the hint the whole-group reader
    // takes too (`ArrowReaderMetadata`), so both decode to the same types.
    let hint = parquet_to_arrow_schema(schema, file.metadata.file_metadata().key_value_metadata())
        .map_err(parquet_err)?;
    let mask = ProjectionMask::leaves(schema, file.projected.iter().copied());
    let levels =
        parquet_to_arrow_field_levels(schema, mask, Some(hint.fields())).map_err(parquet_err)?;
    let group = indexed.row_group(0);
    let mut dictionaries = vec![None; group.num_columns()];
    for &leaf in &file.projected {
        let mut pages = page_reader(indexed, chunks, leaf).map_err(parquet_err)?;
        if !pages
            .peek_next_page()
            .map_err(parquet_err)?
            .is_some_and(|page| page.is_dict)
        {
            continue;
        }
        let Some(Page::DictionaryPage {
            buf,
            num_values,
            encoding,
            is_sorted,
        }) = pages.get_next_page().map_err(parquet_err)?
        else {
            continue;
        };
        let mut buffer = alloc.alloc(buf.len(), tier)?;
        buffer[..buf.len()].copy_from_slice(&buf);
        let len = buf.len();
        if let Some(kept) = dictionaries.get_mut(leaf) {
            *kept = Some(Page::DictionaryPage {
                buf: Bytes::from_owner(buffer).slice(..len),
                num_values,
                encoding,
                is_sorted,
            });
        }
    }
    Ok(Kept {
        levels,
        dictionaries,
    })
}

/// The pages of column `leaf` of `indexed`'s one row group, at their located offsets.
fn page_reader(
    indexed: &ParquetMetaData,
    chunks: &Arc<ArenaChunks>,
    leaf: usize,
) -> ParquetResult<SerializedPageReader<ArenaChunks>> {
    let group = indexed.row_group(0);
    let locations = indexed
        .offset_index()
        .and_then(|index| index.first())
        .and_then(|columns| columns.get(leaf))
        .map(|column| column.page_locations().clone());
    SerializedPageReader::new(
        Arc::clone(chunks),
        group.column(leaf),
        usize::try_from(group.num_rows()).unwrap_or(0),
        locations,
    )
}

/// A reader of `selection`'s rows of `indexed`'s one row group, in batches of `batch_rows`, its
/// column readers taking `kept`'s dictionary pages instead of decompressing their own.
pub(crate) fn reader(
    indexed: &ParquetMetaData,
    chunks: &Arc<ArenaChunks>,
    kept: &Kept,
    batch_rows: usize,
    selection: RowSelection,
) -> ParquetResult<ParquetRecordBatchReader> {
    let part = Part {
        indexed,
        chunks,
        kept,
    };
    ParquetRecordBatchReader::try_new_with_row_groups(
        &kept.levels,
        &part,
        batch_rows,
        Some(selection),
    )
}

/// One part's view of its group's column chunks.
struct Part<'a> {
    indexed: &'a ParquetMetaData,
    chunks: &'a Arc<ArenaChunks>,
    kept: &'a Kept,
}

impl RowGroups for Part<'_> {
    fn num_rows(&self) -> usize {
        usize::try_from(self.indexed.row_group(0).num_rows()).unwrap_or(0)
    }

    fn column_chunks(&self, i: usize) -> ParquetResult<Box<dyn PageIterator>> {
        let pages = page_reader(self.indexed, self.chunks, i)?;
        let dictionary = self.kept.dictionaries.get(i).cloned().flatten();
        let reader: Box<dyn PageReader> = Box::new(WithKeptDictionary { pages, dictionary });
        Ok(Box::new(OneReader(Some(reader))))
    }

    fn row_groups(&self) -> Box<dyn Iterator<Item = &RowGroupMetaData> + '_> {
        Box::new(self.indexed.row_groups().iter())
    }

    fn metadata(&self) -> &ParquetMetaData {
        self.indexed
    }
}

/// The page reader of one row group's column: the one a part reads.
struct OneReader(Option<Box<dyn PageReader>>);

impl Iterator for OneReader {
    type Item = ParquetResult<Box<dyn PageReader>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.take().map(Ok)
    }
}

impl PageIterator for OneReader {}

/// A column's pages with its dictionary page served from what the group kept: the file's
/// dictionary page is stepped over, never read, and every other page is the file's.
struct WithKeptDictionary<P> {
    pages: P,
    /// The kept dictionary page, until it is served or skipped.
    dictionary: Option<Page>,
}

impl<P: PageReader> PageReader for WithKeptDictionary<P> {
    fn get_next_page(&mut self) -> ParquetResult<Option<Page>> {
        match self.dictionary.take() {
            Some(kept) if self.file_page_is_dictionary()? => {
                self.pages.skip_next_page()?;
                Ok(Some(kept))
            }
            // The file's first page is not the dictionary this group kept: read the file's.
            _ => self.pages.get_next_page(),
        }
    }

    fn peek_next_page(&mut self) -> ParquetResult<Option<PageMetadata>> {
        self.pages.peek_next_page()
    }

    fn skip_next_page(&mut self) -> ParquetResult<()> {
        self.dictionary = None;
        self.pages.skip_next_page()
    }

    fn at_record_boundary(&mut self) -> ParquetResult<bool> {
        self.pages.at_record_boundary()
    }
}

impl<P: PageReader> WithKeptDictionary<P> {
    fn file_page_is_dictionary(&mut self) -> ParquetResult<bool> {
        Ok(self
            .pages
            .peek_next_page()?
            .is_some_and(|page| page.is_dict))
    }
}

impl<P: PageReader> Iterator for WithKeptDictionary<P> {
    type Item = ParquetResult<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        self.get_next_page().transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::basic::Encoding;
    use std::collections::VecDeque;

    /// Pages served from a list: what a column chunk's reader gives, and what it was asked.
    struct Listed {
        pages: VecDeque<Page>,
        read: usize,
        skipped: usize,
    }

    impl PageReader for Listed {
        fn get_next_page(&mut self) -> ParquetResult<Option<Page>> {
            self.read += 1;
            Ok(self.pages.pop_front())
        }

        fn peek_next_page(&mut self) -> ParquetResult<Option<PageMetadata>> {
            Ok(self.pages.front().map(|page| PageMetadata {
                num_rows: None,
                num_levels: None,
                is_dict: matches!(page, Page::DictionaryPage { .. }),
            }))
        }

        fn skip_next_page(&mut self) -> ParquetResult<()> {
            self.skipped += 1;
            self.pages.pop_front();
            Ok(())
        }
    }

    impl Iterator for Listed {
        type Item = ParquetResult<Page>;

        fn next(&mut self) -> Option<Self::Item> {
            self.get_next_page().transpose()
        }
    }

    fn dictionary(bytes: &'static [u8]) -> Page {
        Page::DictionaryPage {
            buf: Bytes::from_static(bytes),
            num_values: 1,
            encoding: Encoding::PLAIN,
            is_sorted: false,
        }
    }

    fn data(bytes: &'static [u8]) -> Page {
        Page::DataPage {
            buf: Bytes::from_static(bytes),
            num_values: 1,
            encoding: Encoding::RLE_DICTIONARY,
            def_level_encoding: Encoding::RLE,
            rep_level_encoding: Encoding::RLE,
            statistics: None,
        }
    }

    fn listed(pages: Vec<Page>) -> Listed {
        Listed {
            pages: pages.into(),
            read: 0,
            skipped: 0,
        }
    }

    #[test]
    fn the_kept_dictionary_is_served_and_the_files_is_stepped_over() {
        let mut pages = WithKeptDictionary {
            pages: listed(vec![dictionary(b"file"), data(b"rows")]),
            dictionary: Some(dictionary(b"kept")),
        };
        assert!(pages.peek_next_page().unwrap().unwrap().is_dict);
        assert!(pages.at_record_boundary().is_ok());
        let first: Vec<Page> = pages.by_ref().map(|page| page.expect("a page")).collect();
        assert_eq!(first.len(), 2);
        assert_eq!(&first[0].buffer()[..], b"kept");
        assert_eq!(&first[1].buffer()[..], b"rows");
        assert_eq!(
            (pages.pages.read, pages.pages.skipped),
            (2, 1),
            "the file's never read"
        );
    }

    #[test]
    fn a_file_whose_first_page_is_no_dictionary_is_read_as_it_is() {
        let mut pages = WithKeptDictionary {
            pages: listed(vec![data(b"rows")]),
            dictionary: Some(dictionary(b"kept")),
        };
        assert_eq!(
            &pages.get_next_page().unwrap().unwrap().buffer()[..],
            b"rows"
        );
        assert!(pages.get_next_page().unwrap().is_none());
    }

    #[test]
    fn skipping_the_dictionary_skips_both() {
        let mut pages = WithKeptDictionary {
            pages: listed(vec![dictionary(b"file"), data(b"rows")]),
            dictionary: Some(dictionary(b"kept")),
        };
        pages.skip_next_page().unwrap();
        assert_eq!(
            &pages.get_next_page().unwrap().unwrap().buffer()[..],
            b"rows"
        );
        assert_eq!(pages.pages.skipped, 1);
    }
}
