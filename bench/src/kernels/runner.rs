//! Running a kernel over a generated dataset and reporting its amplification.
//!
//! This is what `amoru-bench kernel <name>` does and what
//! `bench/tests/kernels.rs` measures with: read a Parquet file the generator
//! wrote, feed it to a kernel one morsel at a time, and report bytes in, bytes
//! out and the ratio, beside the band the kernel declares. Preamble 6.7 asks a
//! benchmark to record the host, and the caller prints `host::banner()` above
//! this, so a figure is never reported without the machine that produced it.

use std::fs::File;
use std::path::{Path, PathBuf};

use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use crate::error::{BenchError, Result};
use crate::kernels::adversarial::Adversarial;
use crate::kernels::embed_score::EmbedScore;
use crate::kernels::identity::Identity;
use crate::kernels::normalise::Normalise;
use crate::kernels::tokenise::TokeniseExplode;
use crate::kernels::wide_intermediate::WideIntermediate;
use crate::kernels::{BenchKernel, BenchPayload, KERNEL_NAMES};

/// The dataset of `bench/README.md` each kernel is meant for.
pub fn default_dataset(kernel: &str) -> Result<&'static str> {
    match kernel {
        "identity" => Ok("identity-mixed"),
        "normalise" => Ok("text-normalise"),
        "tokenise-explode" => Ok("text-explode"),
        "adversarial" => Ok("small-row-groups"),
        "wide-intermediate" => Ok("wide-mixed"),
        "embed-score" => Ok("numeric-embed"),
        other => Err(BenchError::Usage(format!(
            "no kernel named {other}; the suite holds {}",
            KERNEL_NAMES.join(", ")
        ))),
    }
}

/// The weights dataset `embed-score` is built from by default: the one whose
/// first dimension matches the numeric column count of `numeric-embed`.
pub const DEFAULT_WEIGHTS: &str = "embed-weights-numeric";

/// Build a kernel by the name preamble 6.5 gives it.
///
/// `dataset_rows` is the row count of the dataset the kernel will see, which
/// only `adversarial` reads: it is where its midpoint comes from, and it must be
/// the dataset's own row count rather than a morsel's. `weights` is the
/// safetensors file `embed-score` loads, and is required for it alone.
pub fn kernel_by_name(
    name: &str,
    dataset_rows: u64,
    weights: Option<&Path>,
) -> Result<Box<dyn BenchKernel>> {
    match name {
        "identity" => Ok(Box::new(Identity::new())),
        "normalise" => Ok(Box::new(Normalise::default())),
        "tokenise-explode" => Ok(Box::new(TokeniseExplode::default())),
        "adversarial" => Ok(Box::new(Adversarial::new(dataset_rows))),
        "wide-intermediate" => Ok(Box::new(WideIntermediate::new())),
        "embed-score" => match weights {
            Some(path) => Ok(Box::new(EmbedScore::from_safetensors(path)?)),
            None => Err(BenchError::Usage(format!(
                "embed-score needs --weights <file>, for example the generated {DEFAULT_WEIGHTS}.safetensors"
            ))),
        },
        other => Err(BenchError::Usage(format!(
            "no kernel named {other}; the suite holds {}",
            KERNEL_NAMES.join(", ")
        ))),
    }
}

/// The `'static` spelling of a kernel name, which is what `BenchError::Kernel`
/// carries. An unknown name becomes `kernel`, because a report that names the
/// wrong thing is worse than one that names nothing.
pub fn band_name(name: &str) -> &'static str {
    KERNEL_NAMES
        .iter()
        .copied()
        .find(|known| *known == name)
        .unwrap_or("kernel")
}

/// What one run of a kernel over a dataset measured.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// The kernel's name.
    pub kernel: String,
    /// The file it read.
    pub file: PathBuf,
    /// Rows in.
    pub rows_in: u64,
    /// Rows out.
    pub rows_out: u64,
    /// Bytes in, as `BenchPayload::bytes` counts them.
    pub bytes_in: u64,
    /// Bytes out.
    pub bytes_out: u64,
    /// Morsels the file was read in.
    pub morsels: u64,
    /// The band the kernel declares, if it declares one.
    pub band: Option<(f64, f64)>,
}

impl Measurement {
    /// Output bytes over input bytes. Zero bytes in gives zero, not a division
    /// by zero, because an empty file is a report and not a panic.
    pub fn amplification(&self) -> f64 {
        if self.bytes_in == 0 {
            0.0
        } else {
            self.bytes_out as f64 / self.bytes_in as f64
        }
    }

    /// Whether the measured amplification falls inside the declared band.
    pub fn in_band(&self) -> bool {
        match self.band {
            None => true,
            Some((low, high)) => {
                let measured = self.amplification();
                measured >= low && measured <= high
            }
        }
    }

    /// The report line, one per run.
    pub fn line(&self) -> String {
        let band = match self.band {
            Some((low, high)) => format!("band {low:.2} to {high:.2}, {}", {
                if self.in_band() { "inside" } else { "OUTSIDE" }
            }),
            None => "no declared band".to_string(),
        };
        format!(
            "{} over {}: {} rows in, {} rows out, {} bytes in, {} bytes out, \
             amplification {:.3} ({band}), {} morsels",
            self.kernel,
            self.file.display(),
            self.rows_in,
            self.rows_out,
            self.bytes_in,
            self.bytes_out,
            self.amplification(),
            self.morsels,
        )
    }
}

/// Run `kernel` over every morsel of a Parquet file and measure it.
///
/// The morsel size is the kernel's own `preferred_rows` hint, so a kernel that
/// explodes its input is not handed a morsel it would explode into gigabytes.
pub fn measure_parquet(kernel: &dyn BenchKernel, path: &Path) -> Result<Measurement> {
    let file = File::open(path).map_err(|e| BenchError::io("open", path, e))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let mut measurement = Measurement {
        kernel: kernel.name().to_string(),
        file: path.to_path_buf(),
        rows_in: 0,
        rows_out: 0,
        bytes_in: 0,
        bytes_out: 0,
        morsels: 0,
        band: kernel.hints().amplification_band,
    };
    let batch_size = kernel.hints().preferred_rows.unwrap_or(8_192) as usize;
    let reader = builder.with_batch_size(batch_size).build()?;
    let mut state = kernel.init()?;
    for batch in reader {
        let batch = batch?;
        let input = BenchPayload::Table(batch);
        measurement.rows_in += input.rows();
        measurement.bytes_in += input.bytes();
        measurement.morsels += 1;
        let output = kernel.apply(state.as_mut(), input)?;
        measurement.rows_out += output.rows();
        measurement.bytes_out += output.bytes();
    }
    Ok(measurement)
}

/// The row count a Parquet file's footer declares, which is what `adversarial`
/// needs before it sees a single morsel.
pub fn parquet_rows(path: &Path) -> Result<u64> {
    let file = File::open(path).map_err(|e| BenchError::io("open", path, e))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let rows: i64 = builder
        .metadata()
        .row_groups()
        .iter()
        .map(|group| group.num_rows())
        .sum();
    Ok(rows.max(0) as u64)
}

/// Build the named kernel for the named dataset in `data_dir` and measure it.
pub fn measure(
    kernel_name: &str,
    dataset: &str,
    data_dir: &Path,
    weights: Option<&Path>,
) -> Result<Measurement> {
    let path = data_dir.join(format!("{dataset}.parquet"));
    if !path.exists() {
        return Err(BenchError::Usage(format!(
            "{} does not exist; write it first with `amoru-bench dataset {dataset} --out {}`",
            path.display(),
            data_dir.display()
        )));
    }
    let rows = parquet_rows(&path)?;
    let kernel = kernel_by_name(kernel_name, rows, weights)?;
    measure_parquet(kernel.as_ref(), &path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{Dataset, DatasetKind};
    use crate::parquet_out::ParquetSpec;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("amoru-bench-runner-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn write(dir: &Path, name: &str, spec: ParquetSpec) {
        let dataset = Dataset {
            name: name.to_string(),
            kind: DatasetKind::Parquet(spec),
        };
        dataset.write(dir, 11).expect("write");
    }

    #[test]
    fn every_kernel_name_has_a_default_dataset_and_an_unknown_one_is_a_usage_error() {
        for name in KERNEL_NAMES {
            assert!(!default_dataset(name).expect(name).is_empty());
        }
        let err = default_dataset("torch-score").expect_err("not built");
        assert!(err.to_string().contains("no kernel named"), "{err}");
    }

    #[test]
    fn kernels_build_by_name_and_embed_score_asks_for_its_weights() {
        for name in [
            "identity",
            "normalise",
            "tokenise-explode",
            "adversarial",
            "wide-intermediate",
        ] {
            let built = kernel_by_name(name, 100, None).map(|kernel| kernel.name());
            assert_eq!(built.ok(), Some(name));
        }
        let refused = kernel_by_name("embed-score", 10, None)
            .err()
            .map(|e| e.to_string());
        assert!(
            refused.as_deref().unwrap_or_default().contains("--weights"),
            "{refused:?}"
        );
        let refused = kernel_by_name("nope", 10, None)
            .err()
            .map(|e| e.to_string());
        assert!(
            refused
                .as_deref()
                .unwrap_or_default()
                .contains("no kernel named nope"),
            "{refused:?}"
        );
    }

    #[test]
    fn the_adversarial_kernel_is_told_the_file_s_row_count() {
        let dir = scratch("rows");
        write(
            &dir,
            "small",
            ParquetSpec {
                rows: 512,
                row_group_rows: 64,
                ..ParquetSpec::default()
            },
        );
        let path = dir.join("small.parquet");
        assert_eq!(parquet_rows(&path).expect("rows"), 512);
        let measurement = measure("adversarial", "small", &dir, None).expect("measure");
        // Half the rows once, half four times.
        assert_eq!(measurement.rows_in, 512);
        assert_eq!(measurement.rows_out, 256 + 256 * 4);
        assert!(measurement.in_band(), "{}", measurement.line());
        assert!(measurement.line().contains("amplification"));
        assert!(measurement.line().contains("inside"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_kernel_name_becomes_its_static_spelling() {
        assert_eq!(band_name("identity"), "identity");
        assert_eq!(band_name("embed-score"), "embed-score");
        assert_eq!(band_name("torch-score"), "kernel");
    }

    #[test]
    fn a_measurement_with_no_band_and_one_with_no_bytes_report_honestly() {
        let mut measurement = Measurement {
            kernel: "k".to_string(),
            file: PathBuf::from("none"),
            rows_in: 0,
            rows_out: 0,
            bytes_in: 0,
            bytes_out: 0,
            morsels: 0,
            band: None,
        };
        assert_eq!(measurement.amplification(), 0.0);
        assert!(measurement.in_band());
        assert!(measurement.line().contains("no declared band"));
        measurement.band = Some((1.0, 2.0));
        assert!(!measurement.in_band());
        assert!(measurement.line().contains("OUTSIDE"));
    }

    #[test]
    fn a_missing_file_and_an_unrunnable_kernel_are_errors_not_panics() {
        let dir = scratch("missing");
        let err = measure("identity", "nope", &dir, None).expect_err("no such file");
        assert!(err.to_string().contains("does not exist"), "{err}");

        write(
            &dir,
            "wide",
            ParquetSpec {
                rows: 64,
                row_group_rows: 32,
                ..ParquetSpec::default()
            },
        );
        let err = measure("wide-intermediate", "wide", &dir, None).expect_err("not wired");
        assert!(err.to_string().contains("is not wired"), "{err}");

        let err = measure_parquet(&Identity::new(), &dir.join("absent.parquet"))
            .expect_err("no such file");
        assert!(err.to_string().starts_with("io: open"), "{err}");
        let err = parquet_rows(&dir.join("absent.parquet")).expect_err("no such file");
        assert!(err.to_string().starts_with("io: open"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
