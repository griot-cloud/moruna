//! A `ChunkReader` over arena buffers (section l).
//!
//! The byte ranges a row group needs are fetched by the reactor into buffers from the `alloc`
//! the read received, and handed to the Parquet decoder as `bytes::Bytes` over those buffers
//! through `Bytes::from_owner`, so nothing is copied before decoding. The decoder's own output
//! is what the decode copy (f.4) then moves into the arena.
//!
//! The document names `ParquetRecordBatchStreamBuilder` over a custom `AsyncFileReader`.
//! That trait's methods return `futures::future::BoxFuture`, and the `futures` crate is not in
//! the preamble's dependency table or in this crate's d.2, so the reads are awaited first and
//! the synchronous builder decodes from the bytes already in the arena. The property the
//! document asks for is unchanged: page-aligned ranges, `Bytes` over an arena buffer with no
//! copy, and the decode copy after decoding. Reported as a documentation item.

use bytes::{Buf, Bytes};
use parquet::errors::{ParquetError, Result as ParquetResult};
use parquet::file::reader::{ChunkReader, Length};

/// The byte ranges of one file that are resident in the arena, by absolute offset.
pub(crate) struct ArenaChunks {
    file_len: u64,
    ranges: Vec<(u64, Bytes)>,
}

impl ArenaChunks {
    /// `ranges` are `(absolute offset, bytes)`, in ascending offset order and non-overlapping.
    pub(crate) fn new(file_len: u64, ranges: Vec<(u64, Bytes)>) -> ArenaChunks {
        ArenaChunks { file_len, ranges }
    }

    fn slice(&self, start: u64, length: usize) -> ParquetResult<Bytes> {
        let end = start + length as u64;
        for (offset, bytes) in &self.ranges {
            let range_end = offset + bytes.len() as u64;
            if start >= *offset && end <= range_end {
                let from = (start - offset) as usize;
                return Ok(bytes.slice(from..from + length));
            }
        }
        Err(ParquetError::General(format!(
            "bytes {start}..{end} are not among the ranges this read fetched"
        )))
    }
}

impl Length for ArenaChunks {
    fn len(&self) -> u64 {
        self.file_len
    }
}

impl ChunkReader for ArenaChunks {
    type T = bytes::buf::Reader<Bytes>;

    fn get_read(&self, start: u64) -> ParquetResult<Self::T> {
        for (offset, bytes) in &self.ranges {
            let range_end = offset + bytes.len() as u64;
            if start >= *offset && start < range_end {
                let from = (start - offset) as usize;
                return Ok(bytes.slice(from..).reader());
            }
        }
        Err(ParquetError::General(format!(
            "byte {start} is not among the ranges this read fetched"
        )))
    }

    fn get_bytes(&self, start: u64, length: usize) -> ParquetResult<Bytes> {
        self.slice(start, length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn ranges_are_served_and_gaps_are_named() {
        let chunks = ArenaChunks::new(100, vec![(10, Bytes::from_static(b"0123456789"))]);
        assert_eq!(chunks.len(), 100);
        assert_eq!(chunks.get_bytes(12, 3).unwrap(), Bytes::from_static(b"234"));
        assert!(chunks.get_bytes(5, 3).is_err());
        assert!(chunks.get_bytes(18, 5).is_err());
        let mut out = Vec::new();
        chunks.get_read(15).unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, b"56789");
        assert!(chunks.get_read(90).is_err());
    }
}
