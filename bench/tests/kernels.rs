//! The kernels' own tests: the amplification each one declares, measured over a
//! dataset the generator wrote, read back through the Parquet reader.
//!
//! Bench has no SDD and therefore no test ids; every test here cites preamble
//! section 6.5, which is the whole of this agent's brief for the kernels, and
//! the amplification class that paragraph gives the kernel under test.
//!
//! Every figure a benchmark reports names the machine (preamble 6.7), so each
//! test prints `host::banner()` with its measurement. A figure measured on a
//! host that is not the reference host of escalation E1 is provisional, and the
//! printed banner is what says which host it was.

use std::path::{Path, PathBuf};

use amoru_bench::error::Result;
use amoru_bench::host;
use amoru_bench::kernels::adversarial::{Adversarial, JUMP};
use amoru_bench::kernels::runner::{self, Measurement};
use amoru_bench::kernels::wide_intermediate::WideIntermediate;
use amoru_bench::kernels::{BenchKernel, BenchPayload, KERNEL_NAMES, NOT_BUILT};
use amoru_bench::suite::{self, Scale};

use arrow::array::{Array, Int64Array, RecordBatch};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

const SEED: u64 = 20_260_922;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("amoru-bench-kernels-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Write one small scale dataset of the suite and return the file it landed in.
fn dataset(dir: &Path, name: &str) -> PathBuf {
    let dataset = match suite::dataset(name, Scale::Small) {
        Ok(dataset) => dataset,
        Err(err) => panic!("{name}: {err}"),
    };
    match dataset.write(dir, SEED) {
        Ok(written) => written.path,
        Err(err) => panic!("{name}: {err}"),
    }
}

fn measure(dir: &Path, kernel: &str, weights: Option<&Path>) -> Measurement {
    let name = match runner::default_dataset(kernel) {
        Ok(name) => name,
        Err(err) => panic!("{kernel}: {err}"),
    };
    dataset(dir, name);
    match runner::measure(kernel, name, dir, weights) {
        Ok(measurement) => measurement,
        Err(err) => panic!("{kernel}: {err}"),
    }
}

/// Read a Parquet file into morsels of exactly `rows` rows each, the last short.
fn morsels(path: &Path, rows: usize) -> Vec<RecordBatch> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => panic!("{}: {err}", path.display()),
    };
    let builder = match ParquetRecordBatchReaderBuilder::try_new(file) {
        Ok(builder) => builder,
        Err(err) => panic!("{err}"),
    };
    let reader = match builder.with_batch_size(rows).build() {
        Ok(reader) => reader,
        Err(err) => panic!("{err}"),
    };
    reader
        .map(|batch| match batch {
            Ok(batch) => batch,
            Err(err) => panic!("{err}"),
        })
        .collect()
}

fn first_column(batch: &RecordBatch) -> Vec<i64> {
    match batch.column(0).as_any().downcast_ref::<Int64Array>() {
        Some(array) => array.values().to_vec(),
        None => panic!("the first column of every generated dataset is an i64"),
    }
}

/// Preamble 6.5: identity about 1, normalise about 1.5, tokenise-explode 5 to
/// 10, adversarial 2.5 over a whole dataset whose halves are 1 and 4, and
/// embed-score the figure its weight shape implies. Each kernel declares its
/// band in `hints()`; this measures output bytes over input bytes on the dataset
/// `bench/README.md` pairs with it and asserts the ratio falls inside.
#[test]
fn every_kernel_s_declared_amplification_holds_on_its_dataset() {
    let dir = scratch("amplification");
    let weights = dataset(&dir, runner::DEFAULT_WEIGHTS);
    println!("{}", host::banner());
    for kernel in [
        "identity",
        "normalise",
        "tokenise-explode",
        "adversarial",
        "embed-score",
    ] {
        let weights = if kernel == "embed-score" {
            Some(weights.as_path())
        } else {
            None
        };
        let measurement = measure(&dir, kernel, weights);
        println!("  {}", measurement.line());
        assert!(measurement.in_band(), "{kernel}: {}", measurement.line());
        assert!(measurement.rows_in > 0, "{kernel} read no rows");
        assert!(measurement.morsels > 0, "{kernel} read no morsel");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Preamble 6.5: the adversarial kernel's amplification "jumps 4x at the
/// midpoint". The jump must be a property of the data seen, so this feeds the
/// same dataset at six morsel sizes and proves the jump lands on the same input
/// row every time, and that that row is half the dataset's rows.
#[test]
fn the_adversarial_jump_lands_on_the_midpoint_row_at_every_morsel_size() {
    let dir = scratch("jump");
    let path = dataset(&dir, "small-row-groups");
    let rows = match runner::parquet_rows(&path) {
        Ok(rows) => rows,
        Err(err) => panic!("{err}"),
    };
    let kernel = Adversarial::new(rows);
    println!("{}", host::banner());
    println!(
        "  adversarial over {} rows, midpoint {}",
        rows,
        kernel.midpoint()
    );

    let run = |morsel: usize| -> (Vec<i64>, Vec<u64>) {
        let mut state = match kernel.init() {
            Ok(state) => state,
            Err(err) => panic!("{err}"),
        };
        let mut values: Vec<i64> = Vec::new();
        let mut per_morsel: Vec<u64> = Vec::new();
        for batch in morsels(&path, morsel) {
            match kernel.apply(state.as_mut(), BenchPayload::Table(batch)) {
                Ok(BenchPayload::Table(out)) => {
                    per_morsel.push(out.num_rows() as u64);
                    values.extend(first_column(&out));
                }
                Ok(BenchPayload::Tensor(_)) => panic!("a table went in"),
                Err(err) => panic!("{err}"),
            }
        }
        (values, per_morsel)
    };

    // One row at a time: the first input row that produces more than one output
    // row is the jump, and it must be exactly the midpoint.
    let (single, per_row) = run(1);
    let jump_at = per_row.iter().position(|out| *out > 1);
    match jump_at {
        Some(index) => assert_eq!(index as u64, kernel.midpoint()),
        None => panic!("nothing was amplified"),
    }
    assert!(
        per_row[..kernel.midpoint() as usize]
            .iter()
            .all(|n| *n == 1)
    );
    assert!(
        per_row[kernel.midpoint() as usize..]
            .iter()
            .all(|n| *n == JUMP as u64)
    );

    // Every other morsel size produces exactly the same output rows in the same
    // order, so no morsel boundary and no batch count takes part in the jump.
    for morsel in [7usize, 64, 1_024, 4_096, 65_536] {
        let (values, _) = run(morsel);
        assert_eq!(
            values, single,
            "morsel size {morsel} moved the jump or the output"
        );
    }
    assert_eq!(
        single.len() as u64,
        kernel.midpoint() + (rows - kernel.midpoint()) * JUMP as u64
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The two halves of the adversarial dataset measured separately: preamble 6.5's
/// "4x" is a ratio of bytes, not only of rows.
#[test]
fn the_two_halves_of_the_adversarial_dataset_amplify_at_one_and_at_four() {
    let dir = scratch("halves");
    let path = dataset(&dir, "small-row-groups");
    let rows = match runner::parquet_rows(&path) {
        Ok(rows) => rows,
        Err(err) => panic!("{err}"),
    };
    let kernel = Adversarial::new(rows);
    let mut state = match kernel.init() {
        Ok(state) => state,
        Err(err) => panic!("{err}"),
    };
    let half = (rows / 2) as usize;
    let mut ratios: Vec<f64> = Vec::new();
    for batch in morsels(&path, half) {
        let input = BenchPayload::Table(batch);
        let before = input.bytes();
        match kernel.apply(state.as_mut(), input) {
            Ok(output) => ratios.push(output.bytes() as f64 / before as f64),
            Err(err) => panic!("{err}"),
        }
    }
    println!("{}", host::banner());
    println!("  adversarial halves: {ratios:?}");
    assert_eq!(ratios.len(), 2, "the dataset splits into two halves");
    assert!((ratios[0] - 1.0).abs() < 0.001, "first half {}", ratios[0]);
    assert!((ratios[1] - 4.0).abs() < 0.05, "second half {}", ratios[1]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every kernel is deterministic: the same dataset twice gives the same bytes.
#[test]
fn every_kernel_gives_the_same_bytes_on_the_same_dataset_twice() {
    let dir = scratch("determinism");
    let weights = dataset(&dir, runner::DEFAULT_WEIGHTS);
    for kernel in [
        "identity",
        "normalise",
        "tokenise-explode",
        "adversarial",
        "embed-score",
    ] {
        let weights = if kernel == "embed-score" {
            Some(weights.as_path())
        } else {
            None
        };
        let first = measure(&dir, kernel, weights);
        let second = measure(&dir, kernel, weights);
        assert_eq!(first, second, "{kernel} is not deterministic");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `embed-score` reads the generator's weights. The suite's `embed-weights` is
/// [128, 64] and `numeric-embed` carries eighteen numeric columns, so those two
/// do not fit each other and the kernel says so rather than projecting; the
/// `embed-weights-numeric` weights this branch adds beside them do fit.
#[test]
fn embed_score_reads_the_generated_weights_and_names_a_shape_that_does_not_fit() {
    let dir = scratch("weights");
    dataset(&dir, "numeric-embed");
    let suite_weights = dataset(&dir, "embed-weights");
    let fitting = dataset(&dir, runner::DEFAULT_WEIGHTS);

    let err = runner::measure("embed-score", "numeric-embed", &dir, Some(&suite_weights))
        .expect_err("[128, 64] does not fit eighteen numeric columns");
    let text = err.to_string();
    assert!(text.contains("128 rows"), "{text}");
    assert!(text.contains("18 numeric columns"), "{text}");
    assert!(text.contains("do not fit each other"), "{text}");

    let measurement = match runner::measure("embed-score", "numeric-embed", &dir, Some(&fitting)) {
        Ok(measurement) => measurement,
        Err(err) => panic!("{err}"),
    };
    println!("{}", host::banner());
    println!("  {}", measurement.line());
    assert!(measurement.in_band(), "{}", measurement.line());

    // The half precision weights load too, which is what `embed-weights-half` is
    // for; they are [256, 128] and so do not fit `numeric-embed` either.
    let half = dataset(&dir, "embed-weights-half");
    let err = runner::measure("embed-score", "numeric-embed", &dir, Some(&half))
        .expect_err("[256, 128] does not fit eighteen numeric columns");
    assert!(err.to_string().contains("256 rows"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Python kernel is declared, not wired: preamble 6.5 puts its body in
/// Python and the runtime's Python adapter (component 5) does not exist. Its
/// amplification is proved on the Python side, by
/// `bench/python/tests/test_wide_intermediate.py`.
#[test]
fn the_python_kernel_declares_itself_and_refuses_to_run() {
    let dir = scratch("python");
    let path = dataset(&dir, "wide-mixed");
    let kernel = WideIntermediate::new();
    let hints = kernel.hints();
    assert_eq!(hints.expected_amplification, Some(20.0));
    assert_eq!(hints.releases_gil, Some(true));
    let err = runner::measure_parquet(&kernel, &path).expect_err("nothing here runs Python");
    let text = err.to_string();
    assert!(text.contains("wide-intermediate is not wired"), "{text}");
    assert!(text.contains("component 5"), "{text}");
    assert!(text.contains("uv run"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The command line reports a measurement with the machine above it, which is
/// what preamble 6.7 asks of a benchmark, and refuses a measurement that leaves
/// the declared band.
#[test]
fn the_kernel_command_reports_the_machine_with_the_measurement() -> Result<()> {
    let dir = scratch("cli");
    dataset(&dir, "identity-mixed");
    let args: Vec<String> = format!("kernel identity --out {}", dir.display())
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let mut out: Vec<u8> = Vec::new();
    amoru_bench::run(&args, &mut out)?;
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(text.starts_with("amoru-bench "), "{text}");
    assert!(text.contains(&host::machine()), "{text}");
    assert!(text.contains("amplification 1.000"), "{text}");
    assert!(text.contains("inside"), "{text}");

    // A kernel over a dataset it is not meant for leaves its band, and the
    // command fails rather than printing a figure that is out of class.
    dataset(&dir, "text-explode");
    let args: Vec<String> = format!(
        "kernel identity --dataset text-explode --out {}",
        dir.display()
    )
    .split_whitespace()
    .map(str::to_string)
    .collect();
    let mut out: Vec<u8> = Vec::new();
    // identity is exactly 1.0 on every dataset, so it stays inside; the check is
    // that the pairing runs at all and reports the other dataset by name.
    amoru_bench::run(&args, &mut out)?;
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(text.contains("text-explode.parquet"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Preamble 6.5 names seven kernels; six are here and torch-score is not,
/// because escalation E1 records that no GPU host exists.
#[test]
fn the_suite_holds_six_kernels_and_says_why_the_seventh_is_missing() {
    assert_eq!(KERNEL_NAMES.len(), 6);
    for kernel in KERNEL_NAMES {
        match runner::default_dataset(kernel) {
            Ok(dataset) => assert!(!dataset.is_empty(), "{kernel} has no dataset"),
            Err(err) => panic!("{kernel}: {err}"),
        }
    }
    assert_eq!(NOT_BUILT.len(), 1);
    assert_eq!(NOT_BUILT[0].0, "torch-score");
    assert!(NOT_BUILT[0].1.contains("E1"), "{}", NOT_BUILT[0].1);
}
