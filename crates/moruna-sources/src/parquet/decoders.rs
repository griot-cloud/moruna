//! What a Parquet source's decoders hold outside the arena whatever their batch (system design
//! 3A, item 6.3d): the run claims it beside the arena's least, before the sinks' writers and
//! before the rest is divided.
//!
//! A read decodes its range with one column reader per projected leaf column, outside the
//! arena. Each holds, whatever the batch:
//!
//! - its codec's decompression context: for Zstandard one per column reader, in the C library's
//!   memory (`ZSTD_sizeof_DCtx` 95,968 bytes, 6.3d's M1), made again with every reader;
//! - the column reader itself: its current page decompressed, its dictionary page decoded, and
//!   its level and value decoders' buffers.
//!
//! What it holds beside them grows with the batch, which `BuildCtx::decode_batch_bytes` counts
//! (four times the batch), and with the input's page and row group: at 5,000-row groups an
//! Int64 column held 380 KB, at 1,000-row groups 131 KB.
//!
//! Measured (6.3d decoder measurement, 2026-11-18, the base build 2d070f64 on the Mac): tables
//! of 64 and 384 columns of one kind, 8 row groups of 1,000 rows written by pyarrow with its
//! defaults (a dictionary, then plain), read into a count sink at 2 GiB on 1 and 4 workers; the
//! process's peak, per column added between 64 and 384:
//!
//! | Column | Zstandard | Uncompressed |
//! |---|---|---|
//! | Int8, four values | 140 KB | 50 KB |
//! | Int64, distinct | 131 KB | 72 KB |
//! | Float64, distinct | 158 KB | 78 KB |
//! | Timestamp (us), distinct | 128 KB | 84 KB |
//! | String, sixteen values of 8 to 12 bytes | 136 KB | 75 KB |
//! | String, distinct, 13 bytes | 217 KB | 158 KB |
//!
//! The difference, 44 to 90 KB a column, is the decompression context's pages the process
//! touched. The peak did not grow with the workers (1 against 4) in these runs, where the count
//! sink kept no more than one decode in flight; the run claims one decode per worker.

use moruna_kernel::arrow::datatypes::SchemaRef;
use moruna_kernel::{MorunaError, Result};
use parquet::basic::Type;

/// What one fixed-width column's decoder holds whatever its batch, compressed or not: the most
/// measured of Int8, Int64, Float64 and timestamp columns (Float64 under Zstandard, 158 KB; see
/// the module's table).
pub const FIXED_COLUMN_DECODE_BYTES: u64 = 158_000;

/// What one byte array column's decoder holds whatever its batch, compressed or not: the most
/// measured of strings with a dictionary and distinct strings (distinct, under Zstandard,
/// 217 KB; see the module's table).
pub const BYTE_ARRAY_COLUMN_DECODE_BYTES: u64 = 217_000;

/// The arena's smallest buffer: its first size class (system design 02 e.2; moruna-arena's
/// `GRANULE`, 64 KiB). Every buffer a read lands in costs at least this much of the arena.
pub const ARENA_LEAST_BUFFER_BYTES: u64 = 64 << 10;

/// The least one read of `schema`'s columns takes of the arena, whatever its rows (system
/// design 3A, 6.3d): a read lands each column in arena buffers of its own (`append`: values and
/// a validity bitmap, offsets besides for a byte array), each at least the arena's smallest
/// buffer. A read of 768 one-byte columns takes 96 MiB of the arena however few rows it holds.
pub fn read_least_arena_bytes(schema: &SchemaRef) -> Result<u64> {
    let descr = parquet::arrow::ArrowSchemaConverter::new()
        .convert(schema)
        .map_err(|e| MorunaError::Plan(format!("parquet schema: {e}")))?;
    Ok(descr
        .columns()
        .iter()
        .map(|column| match column.physical_type() {
            Type::BYTE_ARRAY => 3 * ARENA_LEAST_BUFFER_BYTES,
            _ => 2 * ARENA_LEAST_BUFFER_BYTES,
        })
        .sum())
}

/// What one decode of `schema`'s leaf columns holds outside the arena whatever its batch: each
/// leaf column's figure, a byte array's or a fixed width's (system design 3A, 6.3d).
pub fn decode_fixed_bytes(schema: &SchemaRef) -> Result<u64> {
    let descr = parquet::arrow::ArrowSchemaConverter::new()
        .convert(schema)
        .map_err(|e| MorunaError::Plan(format!("parquet schema: {e}")))?;
    Ok(descr
        .columns()
        .iter()
        .map(|column| match column.physical_type() {
            Type::BYTE_ARRAY => BYTE_ARRAY_COLUMN_DECODE_BYTES,
            _ => FIXED_COLUMN_DECODE_BYTES,
        })
        .sum())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};

    use super::*;

    /// Each leaf column counts its kind's figure: a string a byte array's, the rest a fixed
    /// width's; a struct counts its leaves.
    #[test]
    fn each_leaf_column_counts_its_kinds_figure() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int8, true),
            Field::new("b", DataType::Utf8, true),
            Field::new("c", DataType::Float64, false),
            Field::new(
                "d",
                DataType::Struct(
                    vec![
                        Field::new("x", DataType::Int64, true),
                        Field::new("y", DataType::Binary, true),
                    ]
                    .into(),
                ),
                true,
            ),
        ]));
        assert_eq!(
            decode_fixed_bytes(&schema).expect("a schema"),
            3 * FIXED_COLUMN_DECODE_BYTES + 2 * BYTE_ARRAY_COLUMN_DECODE_BYTES
        );
        assert_eq!(
            read_least_arena_bytes(&schema).expect("a schema"),
            (3 * 2 + 2 * 3) * ARENA_LEAST_BUFFER_BYTES,
            "two buffers a fixed-width column, three a byte array"
        );
        let empty = Arc::new(Schema::empty());
        assert_eq!(decode_fixed_bytes(&empty).expect("a schema"), 0);
        assert_eq!(read_least_arena_bytes(&empty).expect("a schema"), 0);
    }
}
