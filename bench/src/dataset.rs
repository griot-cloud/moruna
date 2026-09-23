//! A named dataset: what it is, where it lands and what writing it produced.

use std::path::{Path, PathBuf};

use crate::error::{BenchError, Result};
use crate::mrb1::{self, TensorSpec};
use crate::parquet_out::{self, ParquetSpec};
use crate::safetensors_out;

/// The three shapes preamble 6.5 names.
#[derive(Debug, Clone, PartialEq)]
pub enum DatasetKind {
    /// A Parquet file.
    Parquet(ParquetSpec),
    /// A safetensors file holding one or more tensors.
    SafeTensors(Vec<TensorSpec>),
    /// A single `MRB1` tensor file (contracts e.4).
    Amb1(TensorSpec),
}

/// A dataset the generator can write.
#[derive(Debug, Clone, PartialEq)]
pub struct Dataset {
    /// The name on the command line and in the manifest.
    pub name: String,
    /// What is written.
    pub kind: DatasetKind,
}

/// One file on disk, after writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The dataset that produced it.
    pub dataset: String,
    /// The path relative to the output directory, which is also the object key
    /// suffix in the S3 compatible store.
    pub relative: String,
    /// The absolute or relative path on disk.
    pub path: PathBuf,
    /// The file's length in bytes.
    pub bytes: u64,
    /// BLAKE3 of the file, so two runs can be compared without keeping both.
    pub hash: String,
    /// A one line description for the printed report and the manifest.
    pub detail: String,
}

impl Dataset {
    /// The file name a dataset is written to.
    pub fn file_name(&self) -> String {
        match self.kind {
            DatasetKind::Parquet(_) => format!("{}.parquet", self.name),
            DatasetKind::SafeTensors(_) => format!("{}.safetensors", self.name),
            DatasetKind::Amb1(_) => format!("{}.mrb1", self.name),
        }
    }

    /// A one line description, used by `list` and by the manifest.
    pub fn detail(&self) -> String {
        match &self.kind {
            DatasetKind::Parquet(spec) => format!(
                "parquet: {} rows, {} i64, {} f64, {} short string, {} text (mean {:.0}, stddev {:.0}), null ratio {:.3}, {} rows per row group, {}",
                spec.rows,
                spec.int_cols,
                spec.float_cols,
                spec.short_string_cols,
                spec.text_cols,
                spec.text_mean_len,
                spec.text_len_stddev,
                spec.null_ratio,
                spec.row_group_rows,
                spec.compression.name(),
            ),
            DatasetKind::SafeTensors(specs) => {
                let parts: Vec<String> = specs
                    .iter()
                    .map(|s| format!("{} {} {}", s.name, s.dtype.name(), s.shape_text()))
                    .collect();
                format!("safetensors: {}", parts.join(", "))
            }
            DatasetKind::Amb1(spec) => {
                format!("mrb1: {} {}", spec.dtype.name(), spec.shape_text())
            }
        }
    }

    /// Write the dataset under `out_dir` and describe what landed.
    pub fn write(&self, out_dir: &Path, seed: u64) -> Result<Written> {
        std::fs::create_dir_all(out_dir)
            .map_err(|e| BenchError::io("create_dir_all", out_dir, e))?;
        let relative = self.file_name();
        let path = out_dir.join(&relative);
        let detail = match &self.kind {
            DatasetKind::Parquet(spec) => {
                let groups = parquet_out::write(&self.name, spec, seed, &path)?;
                format!("{} ({groups} row groups)", self.detail())
            }
            DatasetKind::SafeTensors(specs) => {
                let bytes = safetensors_out::encode(&self.name, specs, seed)?;
                write_bytes(&path, &bytes)?;
                self.detail()
            }
            DatasetKind::Amb1(spec) => {
                let bytes = mrb1::encode(spec, seed);
                write_bytes(&path, &bytes)?;
                self.detail()
            }
        };
        let bytes = std::fs::read(&path).map_err(|e| BenchError::io("read", &path, e))?;
        Ok(Written {
            dataset: self.name.clone(),
            relative,
            path,
            bytes: bytes.len() as u64,
            hash: blake3::hash(&bytes).to_hex().to_string(),
            detail,
        })
    }
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|e| BenchError::io("write", path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtype::DType;

    fn tmp(name: &str) -> PathBuf {
        const PREFIX: &str = "moruna-bench-dataset";
        // Unique per process and per call: several executors run the gate at the same
        // time on one machine, and a fixed name under the system temp directory made two
        // runs delete each other's files (three generator tests failed that way on
        // 2026-09-22, green in isolation and red under a concurrent gate).
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("{}-{}-{}-{}", PREFIX, std::process::id(), n, name));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn tensor(name: &str, dtype: DType, shape: Vec<i64>) -> TensorSpec {
        match TensorSpec::new(name, dtype, shape) {
            Ok(spec) => spec,
            Err(err) => panic!("{err}"),
        }
    }

    #[test]
    fn file_names_follow_the_kind() {
        let parquet = Dataset {
            name: "mixed".into(),
            kind: DatasetKind::Parquet(ParquetSpec::default()),
        };
        assert_eq!(parquet.file_name(), "mixed.parquet");
        let st = Dataset {
            name: "w".into(),
            kind: DatasetKind::SafeTensors(vec![tensor("a", DType::F32, vec![2])]),
        };
        assert_eq!(st.file_name(), "w.safetensors");
        let amb = Dataset {
            name: "t".into(),
            kind: DatasetKind::Amb1(tensor("t", DType::F32, vec![2])),
        };
        assert_eq!(amb.file_name(), "t.mrb1");
    }

    #[test]
    fn the_detail_line_names_every_knob_of_preamble_6_5() {
        let detail = Dataset {
            name: "mixed".into(),
            kind: DatasetKind::Parquet(ParquetSpec {
                rows: 10,
                null_ratio: 0.25,
                row_group_rows: 5,
                ..ParquetSpec::default()
            }),
        }
        .detail();
        assert!(detail.contains("10 rows"), "{detail}");
        assert!(detail.contains("null ratio 0.250"), "{detail}");
        assert!(detail.contains("5 rows per row group"), "{detail}");
        assert!(detail.contains("uncompressed"), "{detail}");

        let st = Dataset {
            name: "w".into(),
            kind: DatasetKind::SafeTensors(vec![tensor("weight", DType::F32, vec![4, 2])]),
        }
        .detail();
        assert_eq!(st, "safetensors: weight f32 [4,2]");

        let amb = Dataset {
            name: "t".into(),
            kind: DatasetKind::Amb1(tensor("t", DType::I64, vec![8])),
        }
        .detail();
        assert_eq!(amb, "mrb1: i64 [8]");
    }

    #[test]
    fn writing_creates_the_directory_and_reports_the_file() {
        let dir = tmp("write");
        let dataset = Dataset {
            name: "tiny".into(),
            kind: DatasetKind::Parquet(ParquetSpec {
                rows: 64,
                row_group_rows: 16,
                ..ParquetSpec::default()
            }),
        };
        let written = dataset.write(&dir, 1).expect("write");
        assert_eq!(written.relative, "tiny.parquet");
        assert!(written.path.exists());
        assert!(written.bytes > 0);
        assert_eq!(written.hash.len(), 64);
        assert!(
            written.detail.contains("4 row groups"),
            "{}",
            written.detail
        );

        let amb = Dataset {
            name: "t".into(),
            kind: DatasetKind::Amb1(tensor("t", DType::F32, vec![16])),
        }
        .write(&dir, 1)
        .expect("write");
        assert_eq!(amb.bytes, 4096 + 64);

        let st = Dataset {
            name: "w".into(),
            kind: DatasetKind::SafeTensors(vec![tensor("weight", DType::F32, vec![4, 2])]),
        }
        .write(&dir, 1)
        .expect("write");
        assert!(st.bytes > 32);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_into_an_impossible_directory_is_an_error_not_a_panic() {
        let base = tmp("file");
        let dir = base.join("out");
        let file = base;
        let _ = std::fs::remove_dir_all(&file);
        std::fs::write(&file, b"x").expect("fixture");
        let err = Dataset {
            name: "t".into(),
            kind: DatasetKind::Amb1(tensor("t", DType::F32, vec![1])),
        }
        .write(&dir, 1)
        .expect_err("should fail");
        assert!(matches!(err, BenchError::Io { .. }));
        let _ = std::fs::remove_file(&file);
    }
}
