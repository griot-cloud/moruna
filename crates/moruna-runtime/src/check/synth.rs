//! Synthetic batches from a declared input schema, byte-exact from a seed (MH 4.9).
//!
//! Every value below is a function of the seed, the batch index and the column index alone, so
//! two runs of `moruna check` with one seed hand a kernel the same bytes, on any host.

use std::sync::Arc;

use moruna_kernel::arrow::array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, Date64Array, Decimal128Array, Float32Array,
    Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, LargeBinaryArray,
    LargeStringArray, ListArray, StringArray, StringViewArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use moruna_kernel::arrow::buffer::{NullBuffer, OffsetBuffer};
use moruna_kernel::arrow::datatypes::{DataType, Field, SchemaRef, TimeUnit};
use moruna_kernel::arrow::record_batch::{RecordBatch, RecordBatchOptions};
use moruna_kernel::{MorunaError, Result};

/// The five batches of MH 4.9, in the order they are run.
pub const BATCHES: [&str; 5] = ["empty", "one_row", "preferred", "all_null", "edges"];
/// Rows of the `preferred` batch when the kernel names no `preferred_rows`.
pub const DEFAULT_PREFERRED_ROWS: u64 = 1024;
/// The cap on the `preferred` batch, so a kernel that asks for a million rows is still checked
/// in a second.
pub const MAX_PREFERRED_ROWS: u64 = 65_536;
/// Rows of the `all_null` batch.
pub const ALL_NULL_ROWS: usize = 16;

const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const SECONDS_1970_TO_2100: u64 = 4_102_444_800;
const DAYS_1970_TO_2100: u64 = 47_482;
/// 0001-01-01 and 9999-12-31, in days and in seconds since the epoch.
const DAY_MIN: i32 = -719_162;
const DAY_MAX: i32 = 2_932_896;
const SECOND_MIN: i64 = -62_135_596_800;
const SECOND_MAX: i64 = 253_402_300_799;

/// SplitMix64 (Steele, Lea and Flood 2014), the generator of MH 4.9.
pub struct Rng(u64);

impl Rng {
    /// The stream for one column of one batch: `seed XOR ((batch << 32 | column) * GOLDEN)`.
    pub fn for_column(seed: u64, batch: usize, column: usize) -> Rng {
        let key = ((batch as u64) << 32) | column as u64;
        Rng(seed ^ key.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// How one column of one batch is filled.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Fill {
    /// Random values; a nullable column is null where a draw is a multiple of `null_every`.
    Random { null_every: Option<u64> },
    /// Every row null.
    Null,
    /// The type's edge values, cycled.
    Edges,
}

/// How a batch chooses the fill of each column.
type FillFor = Box<dyn Fn(&Field) -> Fill>;

/// The rows of the `preferred` batch (MH 4.9).
pub fn preferred_rows(hint: Option<u64>) -> u64 {
    hint.unwrap_or(DEFAULT_PREFERRED_ROWS)
        .clamp(1, MAX_PREFERRED_ROWS)
}

/// Build batch `index` of [`BATCHES`] for `schema`.
///
/// A column whose type the generator does not know is a `Plan` error naming the column and the
/// type, which `moruna check` reports as "not checkable".
pub fn batch(schema: &SchemaRef, index: usize, seed: u64, preferred: u64) -> Result<RecordBatch> {
    let (rows, fill_for): (usize, FillFor) = match index {
        0 => (0, Box::new(|_| Fill::Random { null_every: None })),
        1 => (1, Box::new(|_| Fill::Random { null_every: None })),
        2 => (
            preferred as usize,
            Box::new(|f: &Field| Fill::Random {
                null_every: f.is_nullable().then_some(8),
            }),
        ),
        3 => (
            ALL_NULL_ROWS,
            Box::new(|f: &Field| {
                if f.is_nullable() {
                    Fill::Null
                } else {
                    Fill::Random { null_every: None }
                }
            }),
        ),
        _ => (
            schema
                .fields()
                .iter()
                .map(|f| edges_len(f.data_type()))
                .max()
                .unwrap_or(1)
                .max(1),
            Box::new(|_| Fill::Edges),
        ),
    };
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
    for (at, field) in schema.fields().iter().enumerate() {
        let mut rng = Rng::for_column(seed, index, at);
        let array = column(field.data_type(), rows, fill_for(field), &mut rng)
            .map_err(|e| MorunaError::Plan(format!("column `{}`: {e}", field.name())))?;
        columns.push(array);
    }
    RecordBatch::try_new_with_options(
        schema.clone(),
        columns,
        &RecordBatchOptions::new().with_row_count(Some(rows)),
    )
    .map_err(|e| MorunaError::Plan(format!("building a synthetic batch: {e}")))
}

/// How many edge values a type has (MH 4.9); the `edges` batch has as many rows as the longest.
fn edges_len(dt: &DataType) -> usize {
    match dt {
        DataType::Boolean => 2,
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => 5,
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => 3,
        DataType::Float32 | DataType::Float64 => 8,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => 5,
        DataType::Binary | DataType::LargeBinary => 3,
        DataType::Date32 | DataType::Date64 | DataType::Timestamp(_, _) => 5,
        DataType::Decimal128(_, _) => 3,
        DataType::List(_) => 3,
        _ => 1,
    }
}

/// Whether row `row` is null under `fill`, drawing from `rng` exactly once per row when the
/// column can be null at random.
fn is_null(fill: Fill, rng: &mut Rng) -> bool {
    match fill {
        Fill::Null => true,
        Fill::Edges => false,
        Fill::Random { null_every: None } => false,
        Fill::Random {
            null_every: Some(n),
        } => rng.below(n) == 0,
    }
}

/// `values[i]` for row `i` of a column: a random draw, an edge value, or null.
fn values<T: Copy>(
    rows: usize,
    fill: Fill,
    rng: &mut Rng,
    edges: &[T],
    mut draw: impl FnMut(&mut Rng) -> T,
) -> Vec<Option<T>> {
    (0..rows)
        .map(|row| {
            if is_null(fill, rng) {
                None
            } else if fill == Fill::Edges {
                Some(edges[row % edges.len()])
            } else {
                Some(draw(rng))
            }
        })
        .collect()
}

fn strings(rows: usize, fill: Fill, rng: &mut Rng) -> Vec<Option<String>> {
    let edges = ["", "a", &"x".repeat(1024), "\u{e9}\u{4e2d}\u{1f600}", " "];
    (0..rows)
        .map(|row| {
            if is_null(fill, rng) {
                None
            } else if fill == Fill::Edges {
                Some(edges[row % edges.len()].to_string())
            } else {
                let len = rng.below(17) as usize;
                Some(
                    (0..len)
                        .map(|_| ALNUM[rng.below(ALNUM.len() as u64) as usize] as char)
                        .collect(),
                )
            }
        })
        .collect()
}

fn bytes(rows: usize, fill: Fill, rng: &mut Rng) -> Vec<Option<Vec<u8>>> {
    let edges: [Vec<u8>; 3] = [Vec::new(), vec![0u8], vec![0xffu8; 1024]];
    (0..rows)
        .map(|row| {
            if is_null(fill, rng) {
                None
            } else if fill == Fill::Edges {
                Some(edges[row % edges.len()].clone())
            } else {
                let len = rng.below(17) as usize;
                Some((0..len).map(|_| rng.next_u64() as u8).collect())
            }
        })
        .collect()
}

fn unit_scale(unit: &TimeUnit) -> i64 {
    match unit {
        TimeUnit::Second => 1,
        TimeUnit::Millisecond => 1_000,
        TimeUnit::Microsecond => 1_000_000,
        TimeUnit::Nanosecond => 1_000_000_000,
    }
}

fn float_draw(rng: &mut Rng) -> f64 {
    ((rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64) * 2.0e6 - 1.0e6
}

/// One column of `rows` rows of type `dt`.
fn column(dt: &DataType, rows: usize, fill: Fill, rng: &mut Rng) -> Result<ArrayRef> {
    macro_rules! signed {
        ($array:ty, $t:ty) => {
            Arc::new(<$array>::from(values(
                rows,
                fill,
                rng,
                &[<$t>::MIN, <$t>::MAX, 0, -1, 1],
                |r| r.next_u64() as $t,
            ))) as ArrayRef
        };
    }
    macro_rules! unsigned {
        ($array:ty, $t:ty) => {
            Arc::new(<$array>::from(values(
                rows,
                fill,
                rng,
                &[0, <$t>::MAX, 1],
                |r| r.next_u64() as $t,
            ))) as ArrayRef
        };
    }
    Ok(match dt {
        DataType::Boolean => Arc::new(BooleanArray::from(values(
            rows,
            fill,
            rng,
            &[false, true],
            |r| r.next_u64() & 1 == 1,
        ))),
        DataType::Int8 => signed!(Int8Array, i8),
        DataType::Int16 => signed!(Int16Array, i16),
        DataType::Int32 => signed!(Int32Array, i32),
        DataType::Int64 => signed!(Int64Array, i64),
        DataType::UInt8 => unsigned!(UInt8Array, u8),
        DataType::UInt16 => unsigned!(UInt16Array, u16),
        DataType::UInt32 => unsigned!(UInt32Array, u32),
        DataType::UInt64 => unsigned!(UInt64Array, u64),
        DataType::Float64 => Arc::new(Float64Array::from(values(
            rows,
            fill,
            rng,
            &[
                0.0,
                -0.0,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::MIN_POSITIVE,
                f64::MAX,
                f64::MIN,
            ],
            float_draw,
        ))),
        DataType::Float32 => Arc::new(Float32Array::from(values(
            rows,
            fill,
            rng,
            &[
                0.0,
                -0.0,
                f32::NAN,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::MIN_POSITIVE,
                f32::MAX,
                f32::MIN,
            ],
            |r| float_draw(r) as f32,
        ))),
        DataType::Utf8 => Arc::new(StringArray::from(strings(rows, fill, rng))),
        DataType::LargeUtf8 => Arc::new(LargeStringArray::from(strings(rows, fill, rng))),
        DataType::Utf8View => Arc::new(StringViewArray::from(strings(rows, fill, rng))),
        DataType::Binary => {
            let v = bytes(rows, fill, rng);
            Arc::new(BinaryArray::from_iter(v.iter().map(|b| b.as_deref())))
        }
        DataType::LargeBinary => {
            let v = bytes(rows, fill, rng);
            Arc::new(LargeBinaryArray::from_iter(v.iter().map(|b| b.as_deref())))
        }
        DataType::Date32 => Arc::new(Date32Array::from(values(
            rows,
            fill,
            rng,
            &[0, -1, 1, DAY_MIN, DAY_MAX],
            |r| r.below(DAYS_1970_TO_2100) as i32,
        ))),
        DataType::Date64 => Arc::new(Date64Array::from(values(
            rows,
            fill,
            rng,
            &[
                0,
                -86_400_000,
                86_400_000,
                i64::from(DAY_MIN) * 86_400_000,
                i64::from(DAY_MAX) * 86_400_000,
            ],
            |r| r.below(DAYS_1970_TO_2100) as i64 * 86_400_000,
        ))),
        DataType::Timestamp(unit, tz) => {
            let scale = unit_scale(unit);
            let edges: [i64; 5] = if *unit == TimeUnit::Nanosecond {
                [0, -1, 1, i64::MIN + 1, i64::MAX]
            } else {
                [0, -1, 1, SECOND_MIN * scale, SECOND_MAX * scale]
            };
            let v = values(rows, fill, rng, &edges, |r| {
                r.below(SECONDS_1970_TO_2100) as i64 * scale
            });
            let array: ArrayRef = match unit {
                TimeUnit::Second => {
                    Arc::new(TimestampSecondArray::from(v).with_timezone_opt(tz.clone()))
                }
                TimeUnit::Millisecond => {
                    Arc::new(TimestampMillisecondArray::from(v).with_timezone_opt(tz.clone()))
                }
                TimeUnit::Microsecond => {
                    Arc::new(TimestampMicrosecondArray::from(v).with_timezone_opt(tz.clone()))
                }
                TimeUnit::Nanosecond => {
                    Arc::new(TimestampNanosecondArray::from(v).with_timezone_opt(tz.clone()))
                }
            };
            array
        }
        DataType::Decimal128(p, s) => {
            let digits = u32::from((*p).min(38));
            let top = 10i128.pow(digits) - 1;
            let modulus = 10u64.pow(digits.min(18));
            let v = values(rows, fill, rng, &[0, top, -top], |r| {
                i128::from(r.below(modulus))
            });
            Arc::new(
                Decimal128Array::from(v)
                    .with_precision_and_scale(*p, *s)
                    .map_err(|e| MorunaError::Plan(e.to_string()))?,
            )
        }
        DataType::List(item) => list(item, rows, fill, rng)?,
        other => {
            return Err(MorunaError::Plan(format!(
                "moruna check cannot generate values of type {}",
                moruna_kernel::declare::type_name(other)
            )));
        }
    })
}

/// A list column: lengths 0 to 3 at random, or the edge lists `[]`, `[e0]`, `[e0, e1]`, whose
/// items come from the item type's own generator.
fn list(item: &Arc<Field>, rows: usize, fill: Fill, rng: &mut Rng) -> Result<ArrayRef> {
    let mut lengths = Vec::with_capacity(rows);
    let mut valid = Vec::with_capacity(rows);
    for row in 0..rows {
        let null = is_null(fill, rng);
        valid.push(!null);
        let len = if null {
            0
        } else if fill == Fill::Edges {
            row % 3
        } else {
            rng.below(4) as usize
        };
        lengths.push(len);
    }
    let total: usize = lengths.iter().sum();
    let item_fill = if fill == Fill::Edges {
        Fill::Edges
    } else {
        Fill::Random {
            null_every: item.is_nullable().then_some(8),
        }
    };
    let values = column(item.data_type(), total, item_fill, rng)?;
    let nulls = if valid.iter().all(|v| *v) {
        None
    } else {
        Some(NullBuffer::from(valid))
    };
    ListArray::try_new(
        item.clone(),
        OffsetBuffer::from_lengths(lengths),
        values,
        nulls,
    )
    .map(|a| Arc::new(a) as ArrayRef)
    .map_err(|e| MorunaError::Plan(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::arrow::datatypes::Schema;

    fn every_type() -> SchemaRef {
        let tz: Arc<str> = Arc::from("UTC");
        Arc::new(Schema::new(vec![
            Field::new("b", DataType::Boolean, true),
            Field::new("i8", DataType::Int8, true),
            Field::new("i16", DataType::Int16, true),
            Field::new("i32", DataType::Int32, false),
            Field::new("i64", DataType::Int64, true),
            Field::new("u8", DataType::UInt8, true),
            Field::new("u16", DataType::UInt16, true),
            Field::new("u32", DataType::UInt32, true),
            Field::new("u64", DataType::UInt64, true),
            Field::new("f32", DataType::Float32, true),
            Field::new("f64", DataType::Float64, true),
            Field::new("s", DataType::Utf8, true),
            Field::new("ls", DataType::LargeUtf8, true),
            Field::new("sv", DataType::Utf8View, true),
            Field::new("bin", DataType::Binary, true),
            Field::new("lbin", DataType::LargeBinary, true),
            Field::new("d32", DataType::Date32, true),
            Field::new("d64", DataType::Date64, true),
            Field::new("ts_s", DataType::Timestamp(TimeUnit::Second, None), true),
            Field::new(
                "ts_ms",
                DataType::Timestamp(TimeUnit::Millisecond, None),
                true,
            ),
            Field::new(
                "ts_us",
                DataType::Timestamp(TimeUnit::Microsecond, Some(tz)),
                true,
            ),
            Field::new(
                "ts_ns",
                DataType::Timestamp(TimeUnit::Nanosecond, None),
                true,
            ),
            Field::new("dec", DataType::Decimal128(10, 2), true),
            Field::new(
                "list",
                DataType::List(Arc::new(Field::new("item", DataType::Int64, true))),
                true,
            ),
        ]))
    }

    #[test]
    fn every_batch_of_every_type_builds_with_the_documented_row_counts() {
        let schema = every_type();
        let rows: Vec<usize> = (0..BATCHES.len())
            .map(|i| batch(&schema, i, 7, 100).expect("batch").num_rows())
            .collect();
        assert_eq!(rows, vec![0, 1, 100, ALL_NULL_ROWS, 8]);
        let all_null = batch(&schema, 3, 7, 100).expect("all null");
        for (field, column) in schema.fields().iter().zip(all_null.columns()) {
            if field.is_nullable() {
                assert_eq!(column.null_count(), ALL_NULL_ROWS, "{}", field.name());
            } else {
                assert_eq!(column.null_count(), 0, "{}", field.name());
            }
        }
    }

    #[test]
    fn a_seed_fixes_every_byte_and_a_different_seed_changes_them() {
        let schema = every_type();
        for index in 0..BATCHES.len() {
            let a = batch(&schema, index, 42, 64).expect("a");
            let b = batch(&schema, index, 42, 64).expect("b");
            assert_eq!(a, b, "batch {index}");
        }
        let a = batch(&schema, 2, 42, 64).expect("a");
        let c = batch(&schema, 2, 43, 64).expect("c");
        assert_ne!(a, c);
    }

    #[test]
    fn splitmix_matches_the_reference_sequence() {
        // The first outputs of SplitMix64 from state 0, as published with the algorithm.
        let mut rng = Rng(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
    }

    #[test]
    fn preferred_rows_default_and_clamp() {
        assert_eq!(preferred_rows(None), DEFAULT_PREFERRED_ROWS);
        assert_eq!(preferred_rows(Some(0)), 1);
        assert_eq!(preferred_rows(Some(10_000_000)), MAX_PREFERRED_ROWS);
    }

    #[test]
    fn an_unknown_type_names_the_column() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "when",
            DataType::Time32(TimeUnit::Second),
            true,
        )]));
        let err = batch(&schema, 1, 0, 1).expect_err("unknown type");
        assert!(err.to_string().contains("column `when`"), "{err}");
    }
}
