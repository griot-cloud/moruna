//! The named datasets the benchmark suite works over.
//!
//! Preamble 6.5 names the kernels the wave 1 half of the suite feeds: identity,
//! normalise (a regex over text), tokenise-explode, adversarial, wide
//! intermediate and embed-score. Each dataset below exists because one of those
//! needs that shape; the comment on each says which. The suite is written at one
//! of two scales: `full`, the sizes a benchmark run uses, and `small`, the same
//! shapes at a few thousand rows, which is what a test or a smoke run writes.

use crate::amb1::TensorSpec;
use crate::dataset::{Dataset, DatasetKind};
use crate::dtype::DType;
use crate::error::{BenchError, Result};
use crate::parquet_out::{Codec, ParquetSpec};

/// How large the suite is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    /// Benchmark sizes.
    Full,
    /// A few thousand rows per dataset: a smoke run, and what the tests use.
    Small,
}

impl Scale {
    /// The command line name.
    pub fn name(self) -> &'static str {
        match self {
            Scale::Full => "full",
            Scale::Small => "small",
        }
    }

    /// Parse a command line scale name.
    pub fn parse(text: &str) -> Result<Scale> {
        match text.to_ascii_lowercase().as_str() {
            "full" => Ok(Scale::Full),
            "small" => Ok(Scale::Small),
            other => Err(BenchError::Shape(format!(
                "scale {other}, expected full or small"
            ))),
        }
    }

    fn rows(self, full: usize, small: usize) -> usize {
        match self {
            Scale::Full => full,
            Scale::Small => small,
        }
    }
}

fn parquet(name: &str, spec: ParquetSpec) -> Dataset {
    Dataset {
        name: name.to_string(),
        kind: DatasetKind::Parquet(spec),
    }
}

fn tensor(name: &str, dtype: DType, shape: Vec<i64>) -> Result<TensorSpec> {
    TensorSpec::new(name, dtype, shape)
}

/// Every dataset of the suite, in a fixed order.
pub fn datasets(scale: Scale) -> Result<Vec<Dataset>> {
    Ok(vec![
        // identity (amplification about 1): a plain mixed table, no nulls, large
        // row groups, so the measurement is the runtime and not the decoding.
        parquet(
            "identity-mixed",
            ParquetSpec {
                rows: scale.rows(2_000_000, 4_096),
                int_cols: 4,
                float_cols: 4,
                short_string_cols: 2,
                text_cols: 0,
                null_ratio: 0.0,
                row_group_rows: 65_536,
                ..ParquetSpec::default()
            },
        ),
        // normalise (a regex over text, amplification about 1.5): mixed case and
        // punctuated text of moderate length, with a few nulls to exercise the
        // null path of the kernel.
        parquet(
            "text-normalise",
            ParquetSpec {
                rows: scale.rows(400_000, 2_048),
                int_cols: 1,
                float_cols: 0,
                short_string_cols: 1,
                text_cols: 2,
                text_mean_len: 512.0,
                text_len_stddev: 192.0,
                null_ratio: 0.05,
                row_group_rows: 32_768,
                ..ParquetSpec::default()
            },
        ),
        // tokenise-explode (amplification 5 to 10): long text, high variance, so
        // the row count out of the kernel varies sharply per morsel.
        parquet(
            "text-explode",
            ParquetSpec {
                rows: scale.rows(200_000, 1_024),
                int_cols: 1,
                float_cols: 0,
                short_string_cols: 0,
                text_cols: 1,
                text_mean_len: 2_048.0,
                text_len_stddev: 1_024.0,
                null_ratio: 0.0,
                row_group_rows: 16_384,
                ..ParquetSpec::default()
            },
        ),
        // embed-score (numeric columns to a tensor and back): float columns only,
        // no nulls, so the kernel's matmul is the whole cost.
        parquet(
            "numeric-embed",
            ParquetSpec {
                rows: scale.rows(1_000_000, 4_096),
                int_cols: 2,
                float_cols: 16,
                short_string_cols: 0,
                text_cols: 0,
                null_ratio: 0.0,
                row_group_rows: 65_536,
                ..ParquetSpec::default()
            },
        ),
        // the null path at a ratio no reader can shortcut, across every column
        // type the generator writes.
        parquet(
            "nulls-heavy",
            ParquetSpec {
                rows: scale.rows(500_000, 2_048),
                int_cols: 4,
                float_cols: 4,
                short_string_cols: 2,
                text_cols: 1,
                text_mean_len: 256.0,
                text_len_stddev: 128.0,
                null_ratio: 0.35,
                row_group_rows: 65_536,
                ..ParquetSpec::default()
            },
        ),
        // wide-intermediate (Python NumPy, amplification about 20): many columns,
        // so the intermediate the kernel builds is wide as well as tall.
        parquet(
            "wide-mixed",
            ParquetSpec {
                rows: scale.rows(200_000, 1_024),
                int_cols: 32,
                float_cols: 32,
                short_string_cols: 8,
                text_cols: 2,
                text_mean_len: 128.0,
                text_len_stddev: 64.0,
                null_ratio: 0.02,
                row_group_rows: 8_192,
                compression: Codec::Snappy,
            },
        ),
        // adversarial (amplification jumps at the midpoint): many small row
        // groups, so a reader that assumes a row group is a morsel is caught.
        parquet(
            "small-row-groups",
            ParquetSpec {
                rows: scale.rows(100_000, 4_096),
                int_cols: 2,
                float_cols: 2,
                short_string_cols: 0,
                text_cols: 1,
                text_mean_len: 64.0,
                text_len_stddev: 48.0,
                null_ratio: 0.0,
                row_group_rows: 1_024,
                ..ParquetSpec::default()
            },
        ),
        // embed-score's weights, the shape a TensorSource loads.
        Dataset {
            name: "embed-weights".to_string(),
            kind: DatasetKind::SafeTensors(vec![
                tensor("weight", DType::F32, vec![128, 64])?,
                tensor("bias", DType::F32, vec![64])?,
            ]),
        },
        // the same weights at half precision, which is what a model file holds.
        Dataset {
            name: "embed-weights-half".to_string(),
            kind: DatasetKind::SafeTensors(vec![
                tensor("weight", DType::F16, vec![256, 128])?,
                tensor("bias", DType::BF16, vec![128])?,
            ]),
        },
        // the same weights again in the aligned binary format of contracts e.4.
        Dataset {
            name: "embed-weights-amb1".to_string(),
            kind: DatasetKind::Amb1(tensor("weight", DType::F32, vec![128, 64])?),
        },
        Dataset {
            name: "score-bias-amb1".to_string(),
            kind: DatasetKind::Amb1(tensor("bias", DType::F64, vec![64])?),
        },
        // a rank 3 integer tensor: the tokenise-explode kernel's identifier
        // blocks, and the generator's proof that ndim above 2 writes.
        Dataset {
            name: "token-blocks-amb1".to_string(),
            kind: DatasetKind::Amb1(tensor("tokens", DType::I64, vec![32, 16, 8])?),
        },
    ])
}

/// One dataset of the suite by name.
pub fn dataset(name: &str, scale: Scale) -> Result<Dataset> {
    let all = datasets(scale)?;
    all.iter()
        .find(|dataset| dataset.name == name)
        .cloned()
        .ok_or_else(|| {
            let names: Vec<&str> = all.iter().map(|d| d.name.as_str()).collect();
            BenchError::Usage(format!(
                "no dataset named {name}; the suite holds {}",
                names.join(", ")
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_suite_names_are_unique_and_stable() {
        let all = datasets(Scale::Full).expect("suite");
        let mut names: Vec<&str> = all.iter().map(|d| d.name.as_str()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count);
        assert_eq!(count, 12);
    }

    #[test]
    fn the_suite_covers_every_shape_preamble_6_5_names() {
        let all = datasets(Scale::Small).expect("suite");
        let mut parquet = 0;
        let mut safetensors = 0;
        let mut amb1 = 0;
        let mut with_text = 0;
        let mut with_nulls = 0;
        let mut compressed = 0;
        for dataset in &all {
            match &dataset.kind {
                DatasetKind::Parquet(spec) => {
                    parquet += 1;
                    if spec.text_cols > 0 {
                        with_text += 1;
                    }
                    if spec.null_ratio > 0.0 {
                        with_nulls += 1;
                    }
                    if spec.compression == Codec::Snappy {
                        compressed += 1;
                    }
                    assert!(spec.validate().is_ok(), "{}", dataset.name);
                }
                DatasetKind::SafeTensors(specs) => {
                    safetensors += 1;
                    assert!(!specs.is_empty());
                }
                DatasetKind::Amb1(_) => amb1 += 1,
            }
        }
        assert_eq!(parquet, 7);
        assert_eq!(safetensors, 2);
        assert_eq!(amb1, 3);
        assert!(with_text >= 4);
        assert!(with_nulls >= 3);
        assert_eq!(compressed, 1);
    }

    #[test]
    fn the_small_scale_is_small_and_keeps_every_other_knob() {
        let full = datasets(Scale::Full).expect("suite");
        let small = datasets(Scale::Small).expect("suite");
        assert_eq!(full.len(), small.len());
        for (a, b) in full.iter().zip(small.iter()) {
            assert_eq!(a.name, b.name);
            if let (DatasetKind::Parquet(x), DatasetKind::Parquet(y)) = (&a.kind, &b.kind) {
                assert!(y.rows <= 4_096, "{}", b.name);
                assert!(x.rows > y.rows, "{}", b.name);
                assert_eq!(x.null_ratio, y.null_ratio);
                assert_eq!(x.row_group_rows, y.row_group_rows);
                assert_eq!(x.columns(), y.columns());
            }
        }
    }

    #[test]
    fn a_dataset_is_found_by_name_and_an_unknown_one_lists_the_suite() {
        let found = dataset("text-explode", Scale::Small).expect("dataset");
        assert_eq!(found.name, "text-explode");
        let err = dataset("nope", Scale::Small).expect_err("unknown");
        let text = err.to_string();
        assert!(text.contains("nope"), "{text}");
        assert!(text.contains("identity-mixed"), "{text}");
    }

    #[test]
    fn scale_names_round_trip() {
        assert_eq!(Scale::parse("full").ok(), Some(Scale::Full));
        assert_eq!(Scale::parse("SMALL").ok(), Some(Scale::Small));
        assert_eq!(Scale::Full.name(), "full");
        assert_eq!(Scale::Small.name(), "small");
        assert!(Scale::parse("huge").is_err());
    }
}
