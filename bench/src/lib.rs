//! The Moruna benchmark data generator.
//!
//! Preamble section 6.5: in wave 1 the bench agent delivers, under `bench/`, a
//! generator that "writes Parquet with controllable row count, column mix (ints,
//! floats, short strings, long text with configurable mean length and variance),
//! null ratio and row-group size, to local disk and to an S3-compatible store
//! (MinIO in a container), and writes safetensors and aligned binary tensors of
//! controllable shape". The aligned binary format is `MRB1`, defined in
//! `architecture/sdd/01-contracts.md` section e.4, the one section of that
//! document this crate reads.
//!
//! Three properties hold across the whole crate:
//!
//! 1. Every dataset is a pure function of the seed and the shape arguments. Two
//!    runs of the same command produce byte identical files, and no wall clock,
//!    host name, iteration order or address ever reaches a data byte.
//! 2. Nothing is written outside the output directory, and the S3 half is
//!    skipped with a printed note, never an error, when the store is not
//!    configured.
//! 3. Every run names the machine and the generator version in its output, as
//!    preamble 6.7 asks of a benchmark.
//!
//! The second wave 1 deliverable, the benchmark kernels, is `kernels`: the six
//! preamble 6.5 names, each a struct whose `apply` has the shape contracts d.7
//! gives `Kernel::apply`, with the amplification each declares proved against a
//! generated dataset in `bench/tests/kernels.rs`.

#![deny(missing_docs)]

pub mod cli;
pub mod dataset;
pub mod dtype;
pub mod error;
pub mod host;
pub mod kernels;
pub mod manifest;
pub mod mrb1;
pub mod parquet_out;
pub mod rng;
pub mod s3;
pub mod safetensors_out;
pub mod suite;

use std::io::Write;

use crate::cli::{Action, Plan};
use crate::error::Result;

/// The generator's version, which is the workspace version.
pub const GENERATOR_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The format revision. It changes whenever the bytes a given seed and shape
/// produce change, so a dataset regenerated after a bump is expected to differ
/// and a dataset regenerated without one is not.
pub const GENERATOR_FORMAT: u32 = 1;

/// Run a parsed plan, writing the report to `out`.
pub fn execute(plan: &Plan, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "{}", host::banner()).map_err(stdout_error)?;
    match &plan.action {
        Action::Help => {
            writeln!(out, "{}", cli::usage()).map_err(stdout_error)?;
        }
        Action::Version => {}
        Action::List => {
            writeln!(out, "suite at scale {}:", plan.scale.name()).map_err(stdout_error)?;
            for dataset in suite::datasets(plan.scale)? {
                writeln!(
                    out,
                    "  {:<20} {:<24} {}",
                    dataset.name,
                    dataset.file_name(),
                    dataset.detail()
                )
                .map_err(stdout_error)?;
            }
        }
        Action::Measure(run) => {
            let measurement = kernels::runner::measure(
                &run.kernel,
                &run.dataset,
                &plan.out,
                run.weights.as_deref(),
            )?;
            writeln!(out, "  {}", measurement.line()).map_err(stdout_error)?;
            if !measurement.in_band() {
                return Err(error::BenchError::Kernel {
                    kernel: kernels::runner::band_name(&run.kernel),
                    detail: format!(
                        "measured amplification {:.3} is outside the declared band",
                        measurement.amplification()
                    ),
                });
            }
        }
        Action::Write(datasets) => {
            writeln!(
                out,
                "writing {} dataset(s) with seed {} into {}",
                datasets.len(),
                plan.seed,
                plan.out.display()
            )
            .map_err(stdout_error)?;
            let mut written = Vec::with_capacity(datasets.len());
            for dataset in datasets {
                let file = dataset.write(&plan.out, plan.seed)?;
                tracing::info!(
                    dataset = %file.dataset,
                    bytes = file.bytes,
                    "dataset written"
                );
                writeln!(
                    out,
                    "  {:<24} {:>12} bytes  blake3 {}  {}",
                    file.relative,
                    file.bytes,
                    &file.hash[..16],
                    file.detail
                )
                .map_err(stdout_error)?;
                written.push(file);
            }
            let path = manifest::write(&plan.out, plan.seed, plan.scale.name(), &written)?;
            writeln!(out, "  manifest {}", path.display()).map_err(stdout_error)?;
            upload(plan, &written, out)?;
        }
    }
    Ok(())
}

fn upload(plan: &Plan, written: &[dataset::Written], out: &mut dyn Write) -> Result<()> {
    if !plan.upload {
        writeln!(out, "S3 upload skipped: --local-only").map_err(stdout_error)?;
        return Ok(());
    }
    match s3::from_env() {
        s3::Discovery::Missing(missing) => {
            writeln!(out, "{}", s3::skip_note(&missing)).map_err(stdout_error)?;
            Ok(())
        }
        s3::Discovery::Configured(target) => {
            writeln!(
                out,
                "uploading to {} bucket {} prefix {}",
                target.endpoint, target.bucket, target.prefix
            )
            .map_err(stdout_error)?;
            let files: Vec<(String, std::path::PathBuf)> = written
                .iter()
                .map(|file| (file.relative.clone(), file.path.clone()))
                .collect();
            target.upload(&files, out)
        }
    }
}

fn stdout_error(source: std::io::Error) -> error::BenchError {
    error::BenchError::io("write", "<report>", source)
}

/// Parse and run one command line, the arguments after the program name.
pub fn run(args: &[String], out: &mut dyn Write) -> Result<()> {
    execute(&cli::parse(args)?, out)
}

/// Install the log subscriber the binary uses. Log level comes from `RUST_LOG`,
/// warnings and above by default, and the events go to standard error so that
/// the report on standard output stays machine readable. Returns whether it was
/// installed: a second call in one process is a no operation, not a panic.
pub fn install_tracing() -> bool {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .is_ok()
}

/// The whole process, minus the argument and stream plumbing: the exit code the
/// binary returns, with any error printed to `err`.
pub fn main_with(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    match run(args, out) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(err, "moruna-bench: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        const PREFIX: &str = "moruna-bench-lib";
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

    fn run_line(line: &str) -> (String, Result<()>) {
        let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        let mut out: Vec<u8> = Vec::new();
        let result = run(&args, &mut out);
        (String::from_utf8_lossy(&out).into_owned(), result)
    }

    #[test]
    fn every_run_opens_with_the_machine_and_the_generator_version() {
        let (text, result) = run_line("version");
        assert!(result.is_ok());
        assert!(text.starts_with("moruna-bench "), "{text}");
        assert!(text.contains(GENERATOR_VERSION), "{text}");
        assert!(text.contains(&host::machine()), "{text}");
    }

    #[test]
    fn help_prints_the_usage() {
        let (text, result) = run_line("help");
        assert!(result.is_ok());
        assert!(text.contains("usage: moruna-bench"), "{text}");
    }

    #[test]
    fn list_prints_every_dataset_without_writing_anything() {
        let (text, result) = run_line("list --scale small");
        assert!(result.is_ok());
        for name in [
            "identity-mixed",
            "text-normalise",
            "text-explode",
            "numeric-embed",
            "nulls-heavy",
            "wide-mixed",
            "small-row-groups",
            "embed-weights",
            "embed-weights-numeric",
            "embed-weights-half",
            "embed-weights-mrb1",
            "score-bias-mrb1",
            "token-blocks-mrb1",
        ] {
            assert!(text.contains(name), "{name} missing from {text}");
        }
        assert!(!PathBuf::from(cli::DEFAULT_OUT).exists());
    }

    #[test]
    fn writing_one_dataset_reports_it_and_leaves_a_manifest() {
        let dir = temp("one");
        let (text, result) = run_line(&format!(
            "dataset score-bias-mrb1 --scale small --local-only --out {}",
            dir.display()
        ));
        assert!(result.is_ok(), "{text}");
        assert!(text.contains("score-bias-mrb1.mrb1"), "{text}");
        assert!(text.contains("blake3"), "{text}");
        assert!(text.contains("S3 upload skipped: --local-only"), "{text}");
        assert!(dir.join("score-bias-mrb1.mrb1").exists());
        assert!(dir.join(manifest::FILE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_process_entry_point_maps_an_error_to_a_non_zero_exit_code() {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        assert_eq!(main_with(&["version".to_string()], &mut out, &mut err), 0);
        assert!(err.is_empty());
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        assert_eq!(main_with(&["nonsense".to_string()], &mut out, &mut err), 1);
        let text = String::from_utf8_lossy(&err).into_owned();
        assert!(text.starts_with("moruna-bench: usage:"), "{text}");
    }

    #[test]
    fn the_log_subscriber_installs_once_and_never_panics() {
        let first = install_tracing();
        let second = install_tracing();
        assert!(first || !second);
        assert!(!second, "a second install must be a no operation");
    }

    #[test]
    fn a_usage_error_is_returned_and_nothing_is_written() {
        let dir = temp("bad");
        let (_, result) = run_line(&format!("dataset nope --out {}", dir.display()));
        assert!(result.is_err());
        assert!(!dir.exists());
    }

    #[test]
    fn the_upload_half_prints_a_note_when_the_store_is_not_configured() {
        // The generator must never fail because there is no object store: the
        // quality gate runs on hosts that have none.
        let dir = temp("note");
        let mut out: Vec<u8> = Vec::new();
        let plan = cli::parse(&[
            "dataset".to_string(),
            "score-bias-mrb1".to_string(),
            "--out".to_string(),
            dir.display().to_string(),
        ])
        .expect("parse");
        let result = execute(&plan, &mut out);
        assert!(result.is_ok(), "{result:?}");
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(
            text.contains("S3 upload skipped") || text.contains("uploading to"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
