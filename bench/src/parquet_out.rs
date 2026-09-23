//! The Parquet writer: row count, column mix, null ratio and row group size, the
//! four knobs preamble 6.5 names, plus the mean and the variance of the long
//! text column.
//!
//! Values are drawn one column at a time from that column's own stream, so the
//! contents of a column do not move when another column is added or when the row
//! group size changes: only the physical layout of the file changes with the row
//! group size.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Builder, Int64Builder, RecordBatch, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use crate::error::{BenchError, Result};
use crate::rng::Rng;

/// The compressions the generator offers. Both are deterministic; the default is
/// no compression, so that a file's bytes depend on the generator and not on a
/// compression library's internal tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// No compression.
    Uncompressed,
    /// Snappy, the common default in the wild.
    Snappy,
}

impl Codec {
    /// The command line name.
    pub fn name(self) -> &'static str {
        match self {
            Codec::Uncompressed => "uncompressed",
            Codec::Snappy => "snappy",
        }
    }

    /// Parse a command line codec name.
    pub fn parse(text: &str) -> Result<Codec> {
        match text.to_ascii_lowercase().as_str() {
            "uncompressed" | "none" => Ok(Codec::Uncompressed),
            "snappy" => Ok(Codec::Snappy),
            other => Err(BenchError::Shape(format!(
                "compression {other}, expected uncompressed or snappy"
            ))),
        }
    }

    fn parquet(self) -> Compression {
        match self {
            Codec::Uncompressed => Compression::UNCOMPRESSED,
            Codec::Snappy => Compression::SNAPPY,
        }
    }
}

/// The shape of a generated Parquet file.
#[derive(Debug, Clone, PartialEq)]
pub struct ParquetSpec {
    /// Number of rows.
    pub rows: usize,
    /// Number of `i64` columns, named `i64_0` onwards.
    pub int_cols: usize,
    /// Number of `f64` columns, named `f64_0` onwards.
    pub float_cols: usize,
    /// Number of short string columns, named `str_0` onwards.
    pub short_string_cols: usize,
    /// Number of long text columns, named `text_0` onwards.
    pub text_cols: usize,
    /// The mean length in bytes of a long text value.
    pub text_mean_len: f64,
    /// The standard deviation of that length. The variance is its square.
    pub text_len_stddev: f64,
    /// The probability that any one value is null. Zero makes every column non
    /// nullable in the schema as well as in the data.
    pub null_ratio: f64,
    /// Rows per row group. The writer is given this as its maximum and is fed
    /// batches of exactly this many rows, so every row group but the last holds
    /// exactly this many.
    pub row_group_rows: usize,
    /// The page compression.
    pub compression: Codec,
}

impl Default for ParquetSpec {
    fn default() -> Self {
        ParquetSpec {
            rows: 16_384,
            int_cols: 2,
            float_cols: 2,
            short_string_cols: 1,
            text_cols: 1,
            text_mean_len: 256.0,
            text_len_stddev: 96.0,
            null_ratio: 0.0,
            row_group_rows: 8_192,
            compression: Codec::Uncompressed,
        }
    }
}

/// The short string vocabulary. Fixed, so a value depends on the seed alone.
const SHORT_WORDS: [&str; 16] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliett",
    "kilo", "lima", "mike", "november", "oscar", "papa",
];

/// The long text vocabulary, mixed case and punctuated, because the `normalise`
/// kernel of preamble 6.5 runs a regex over this column and a corpus of one
/// repeated lowercase word would not exercise it.
const TEXT_WORDS: [&str; 24] = [
    "Morsel",
    "arena;",
    "budget",
    "SPILL",
    "staging,",
    "reactor",
    "placement",
    "Scheduler",
    "amplification",
    "throughput!",
    "backpressure",
    "checkpoint.",
    "Tier",
    "device",
    "pinned",
    "host,",
    "queue",
    "MANIFEST",
    "segment",
    "resume?",
    "kernel",
    "batch",
    "Column",
    "row-group",
];

impl ParquetSpec {
    /// Validate the spec against what the writer and the format can express.
    pub fn validate(&self) -> Result<()> {
        if self.columns() == 0 {
            return Err(BenchError::Shape(
                "a Parquet file needs at least one column".to_string(),
            ));
        }
        if self.row_group_rows == 0 {
            return Err(BenchError::Shape(
                "row-group-rows must be at least 1".to_string(),
            ));
        }
        if !(0.0..=1.0).contains(&self.null_ratio) {
            return Err(BenchError::Shape(format!(
                "null-ratio {} is outside 0.0 to 1.0",
                self.null_ratio
            )));
        }
        if self.text_cols > 0 && self.text_mean_len < 1.0 {
            return Err(BenchError::Shape(format!(
                "text-mean-len {} must be at least 1",
                self.text_mean_len
            )));
        }
        if self.text_len_stddev < 0.0 {
            return Err(BenchError::Shape(format!(
                "text-len-stddev {} is negative",
                self.text_len_stddev
            )));
        }
        Ok(())
    }

    /// The total number of columns.
    pub fn columns(&self) -> usize {
        self.int_cols + self.float_cols + self.short_string_cols + self.text_cols
    }

    /// Whether a value may be null.
    fn nullable(&self) -> bool {
        self.null_ratio > 0.0
    }

    /// The column names, in schema order.
    pub fn column_names(&self) -> Vec<String> {
        let mut names = Vec::with_capacity(self.columns());
        for i in 0..self.int_cols {
            names.push(format!("i64_{i}"));
        }
        for i in 0..self.float_cols {
            names.push(format!("f64_{i}"));
        }
        for i in 0..self.short_string_cols {
            names.push(format!("str_{i}"));
        }
        for i in 0..self.text_cols {
            names.push(format!("text_{i}"));
        }
        names
    }

    /// The Arrow schema.
    pub fn schema(&self) -> SchemaRef {
        let nullable = self.nullable();
        let mut fields = Vec::with_capacity(self.columns());
        for name in self.column_names() {
            let data_type = if name.starts_with("i64_") {
                DataType::Int64
            } else if name.starts_with("f64_") {
                DataType::Float64
            } else {
                DataType::Utf8
            };
            fields.push(Field::new(name, data_type, nullable));
        }
        Arc::new(Schema::new(fields))
    }

    /// The number of row groups the file will hold.
    pub fn row_groups(&self) -> usize {
        self.rows.div_ceil(self.row_group_rows.max(1))
    }
}

/// One column's generator state: its name and its stream.
struct Column {
    name: String,
    rng: Rng,
}

impl Column {
    fn new(dataset: &str, name: &str, seed: u64) -> Column {
        Column {
            name: name.to_string(),
            rng: Rng::substream(seed, &format!("parquet:{dataset}:{name}")),
        }
    }
}

/// Build one short string value.
fn short_string(rng: &mut Rng, out: &mut String) {
    out.clear();
    out.push_str(SHORT_WORDS[rng.below(SHORT_WORDS.len())]);
    out.push('-');
    // A two digit suffix keeps the values short and gives the column a
    // cardinality worth dictionary encoding.
    let suffix = rng.below(100);
    if suffix < 10 {
        out.push('0');
    }
    out.push_str(&suffix.to_string());
}

/// Build one long text value of about `mean` bytes with the given deviation.
fn long_text(rng: &mut Rng, mean: f64, stddev: f64, out: &mut String) {
    out.clear();
    let target = rng.normal_len(mean, stddev, 1);
    while out.len() < target {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(TEXT_WORDS[rng.below(TEXT_WORDS.len())]);
    }
    // Trim on a character boundary: every word above is ASCII, so a byte index
    // is a character boundary and the value lands on exactly `target` bytes.
    out.truncate(target);
}

/// Build one record batch of `rows` rows.
fn batch(
    spec: &ParquetSpec,
    schema: &SchemaRef,
    columns: &mut [Column],
    rows: usize,
) -> Result<RecordBatch> {
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());
    let mut scratch = String::new();
    for column in columns.iter_mut() {
        if column.name.starts_with("i64_") {
            let mut builder = Int64Builder::with_capacity(rows);
            for _ in 0..rows {
                if column.rng.bernoulli(spec.null_ratio) {
                    builder.append_null();
                } else {
                    builder.append_value(column.rng.next_u64() as i64);
                }
            }
            arrays.push(Arc::new(builder.finish()));
        } else if column.name.starts_with("f64_") {
            let mut builder = Float64Builder::with_capacity(rows);
            for _ in 0..rows {
                if column.rng.bernoulli(spec.null_ratio) {
                    builder.append_null();
                } else {
                    builder.append_value(column.rng.next_f64() * 2.0 - 1.0);
                }
            }
            arrays.push(Arc::new(builder.finish()));
        } else {
            let long = column.name.starts_with("text_");
            let per_value = if long {
                spec.text_mean_len as usize + 1
            } else {
                12
            };
            let mut builder = StringBuilder::with_capacity(rows, rows * per_value);
            for _ in 0..rows {
                if column.rng.bernoulli(spec.null_ratio) {
                    builder.append_null();
                } else {
                    if long {
                        long_text(
                            &mut column.rng,
                            spec.text_mean_len,
                            spec.text_len_stddev,
                            &mut scratch,
                        );
                    } else {
                        short_string(&mut column.rng, &mut scratch);
                    }
                    builder.append_value(&scratch);
                }
            }
            arrays.push(Arc::new(builder.finish()));
        }
    }
    Ok(RecordBatch::try_new(Arc::clone(schema), arrays)?)
}

/// The `created_by` string embedded in every file: the generator and its format
/// revision rather than the Parquet library's version, so that the bytes of a
/// dataset are a function of the generator alone.
pub fn created_by() -> String {
    format!(
        "moruna-bench {} (generator format {})",
        crate::GENERATOR_VERSION,
        crate::GENERATOR_FORMAT
    )
}

fn key_value_metadata(dataset: &str, seed: u64, spec: &ParquetSpec) -> Vec<KeyValue> {
    // A fixed order, so the footer bytes do not move between runs.
    vec![
        KeyValue::new("moruna_bench.dataset".to_string(), dataset.to_string()),
        KeyValue::new("moruna_bench.seed".to_string(), seed.to_string()),
        KeyValue::new(
            "moruna_bench.version".to_string(),
            crate::GENERATOR_VERSION.to_string(),
        ),
        KeyValue::new(
            "moruna_bench.format".to_string(),
            crate::GENERATOR_FORMAT.to_string(),
        ),
        KeyValue::new(
            "moruna_bench.null_ratio".to_string(),
            format!("{:.6}", spec.null_ratio),
        ),
        KeyValue::new(
            "moruna_bench.row_group_rows".to_string(),
            spec.row_group_rows.to_string(),
        ),
    ]
}

/// Write the file and return the number of row groups it holds.
pub fn write(dataset: &str, spec: &ParquetSpec, seed: u64, path: &Path) -> Result<usize> {
    spec.validate()?;
    let schema = spec.schema();
    let mut columns: Vec<Column> = spec
        .column_names()
        .iter()
        .map(|name| Column::new(dataset, name, seed))
        .collect();

    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(spec.row_group_rows))
        .set_compression(spec.compression.parquet())
        .set_created_by(created_by())
        .set_key_value_metadata(Some(key_value_metadata(dataset, seed, spec)))
        .build();

    let file = File::create(path).map_err(|e| BenchError::io("create", path, e))?;
    let mut writer = ArrowWriter::try_new(file, Arc::clone(&schema), Some(props))?;
    let mut written = 0usize;
    while written < spec.rows {
        let rows = spec.row_group_rows.min(spec.rows - written);
        writer.write(&batch(spec, &schema, &mut columns, rows)?)?;
        written += rows;
    }
    let metadata = writer.close()?;
    Ok(metadata.num_row_groups())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_follows_the_column_mix() {
        let spec = ParquetSpec {
            int_cols: 2,
            float_cols: 1,
            short_string_cols: 1,
            text_cols: 2,
            null_ratio: 0.1,
            ..ParquetSpec::default()
        };
        assert_eq!(spec.columns(), 6);
        assert_eq!(
            spec.column_names(),
            vec!["i64_0", "i64_1", "f64_0", "str_0", "text_0", "text_1"]
        );
        let schema = spec.schema();
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
        assert_eq!(schema.field(2).data_type(), &DataType::Float64);
        assert_eq!(schema.field(3).data_type(), &DataType::Utf8);
        assert!(schema.field(0).is_nullable());
    }

    #[test]
    fn a_zero_null_ratio_makes_every_column_non_nullable() {
        let spec = ParquetSpec {
            null_ratio: 0.0,
            ..ParquetSpec::default()
        };
        assert!(spec.schema().fields().iter().all(|f| !f.is_nullable()));
    }

    #[test]
    fn a_spec_the_format_cannot_express_is_refused() {
        let empty = ParquetSpec {
            int_cols: 0,
            float_cols: 0,
            short_string_cols: 0,
            text_cols: 0,
            ..ParquetSpec::default()
        };
        assert!(matches!(empty.validate(), Err(BenchError::Shape(_))));
        let zero_group = ParquetSpec {
            row_group_rows: 0,
            ..ParquetSpec::default()
        };
        assert!(matches!(zero_group.validate(), Err(BenchError::Shape(_))));
        let bad_ratio = ParquetSpec {
            null_ratio: 1.5,
            ..ParquetSpec::default()
        };
        assert!(matches!(bad_ratio.validate(), Err(BenchError::Shape(_))));
        let no_text = ParquetSpec {
            text_mean_len: 0.0,
            ..ParquetSpec::default()
        };
        assert!(matches!(no_text.validate(), Err(BenchError::Shape(_))));
        let bad_dev = ParquetSpec {
            text_len_stddev: -1.0,
            ..ParquetSpec::default()
        };
        assert!(matches!(bad_dev.validate(), Err(BenchError::Shape(_))));
        assert!(ParquetSpec::default().validate().is_ok());
    }

    #[test]
    fn codec_names_round_trip() {
        assert_eq!(Codec::parse("uncompressed").ok(), Some(Codec::Uncompressed));
        assert_eq!(Codec::parse("NONE").ok(), Some(Codec::Uncompressed));
        assert_eq!(Codec::parse("Snappy").ok(), Some(Codec::Snappy));
        assert_eq!(Codec::Snappy.name(), "snappy");
        assert_eq!(Codec::Uncompressed.name(), "uncompressed");
        assert_eq!(Codec::Snappy.parquet(), Compression::SNAPPY);
        assert_eq!(Codec::Uncompressed.parquet(), Compression::UNCOMPRESSED);
        assert!(Codec::parse("brotli").is_err());
    }

    #[test]
    fn short_strings_and_long_text_are_shaped_as_described() {
        let mut rng = Rng::new(1);
        let mut out = String::new();
        for _ in 0..200 {
            short_string(&mut rng, &mut out);
            assert!(out.len() <= 12, "{out}");
            assert!(out.contains('-'), "{out}");
        }
        for _ in 0..200 {
            long_text(&mut rng, 128.0, 32.0, &mut out);
            assert!(!out.is_empty());
            assert!(out.is_char_boundary(out.len()));
        }
    }

    #[test]
    fn row_groups_are_counted_from_the_row_group_size() {
        let spec = ParquetSpec {
            rows: 10,
            row_group_rows: 4,
            ..ParquetSpec::default()
        };
        assert_eq!(spec.row_groups(), 3);
    }

    #[test]
    fn created_by_names_the_generator_and_not_the_parquet_library() {
        let text = created_by();
        assert!(text.starts_with("moruna-bench "), "{text}");
        assert!(text.contains(crate::GENERATOR_VERSION), "{text}");
    }

    #[test]
    fn the_key_value_metadata_order_is_fixed() {
        let keys: Vec<String> = key_value_metadata("d", 1, &ParquetSpec::default())
            .into_iter()
            .map(|kv| kv.key)
            .collect();
        assert_eq!(
            keys,
            vec![
                "moruna_bench.dataset",
                "moruna_bench.seed",
                "moruna_bench.version",
                "moruna_bench.format",
                "moruna_bench.null_ratio",
                "moruna_bench.row_group_rows",
            ]
        );
    }
}
