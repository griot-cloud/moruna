//! The 4G.3 measurement's Rust side (work item 4.2): one fused `expr` against Arrow compute
//! called once per node, on a month of trips already decoded to an Arrow IPC file, so only the
//! computing is timed. `bench/kernels/expr_4g3.py` drives it, interleaved with the Python
//! baselines, and checks the outputs against 0.6.6's.
//!
//! ```text
//! expr_4g3 expr TREE.json IN.arrow [--reps N] [--out OUT.arrow]
//! expr_4g3 arrow-enrich IN.arrow [--reps N]
//! ```
//!
//! `expr` evaluates the tree's outputs over every batch; with `--out` it then writes them, a
//! `keep` output applied as the filter and dropped (Enrich's rows), else as they are (Audit's).
//! `arrow-enrich` computes Enrich's columns and mask with one arrow-rs call per node of
//! `bench/vm/workloads.py`'s, plus the cast of the duration to a double that pyarrow's `divide`
//! makes implicitly. Each prints one JSON line: the rows, the batches, the seconds to check and
//! compile the tree, and the seconds of each repetition after one untimed warm-up.

#![allow(clippy::result_large_err)]

use std::fs::File;
use std::sync::Arc;
use std::time::Instant;

use moruna_kernel::arrow::array::{
    ArrayRef, AsArray, BooleanArray, Float64Array, RecordBatch, Scalar,
};
use moruna_kernel::arrow::compute::filter_record_batch;
use moruna_kernel::arrow::compute::kernels::{boolean, cast, cmp, numeric};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::ipc::reader::FileReader;
use moruna_kernel::arrow::ipc::writer::FileWriter;
use moruna_kernels::tree::Tree;

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

fn read(path: &str) -> Fallible<Vec<RecordBatch>> {
    let reader = FileReader::try_new(File::open(path)?, None)?;
    Ok(reader.collect::<Result<Vec<_>, _>>()?)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn f64_scalar(v: f64) -> Scalar<ArrayRef> {
    Scalar::new(Arc::new(Float64Array::from(vec![v])) as ArrayRef)
}

/// Enrich's columns and mask, one arrow-rs call per pyarrow call of workloads.enrich.
fn arrow_enrich(batch: &RecordBatch) -> Fallible<RecordBatch> {
    let col = |n: &str| batch.column_by_name(n).cloned().ok_or("a column");
    let (pu, dropoff) = (col("pickup_at")?, col("dropoff_at")?);
    let (dist, fare, tip) = (
        col("trip_distance")?,
        col("fare_amount")?,
        col("tip_amount")?,
    );
    let elapsed = numeric::sub_wrapping(&dropoff, &pu)?; // subtract
    let micros = cast::cast(&elapsed, &DataType::Int64)?; // cast to int64
    let micros = cast::cast(&micros, &DataType::Float64)?; // divide's implicit cast
    let dur = numeric::div(&micros, &f64_scalar(1_000_000.0))?; // divide
    let ok = boolean::and(
        &boolean::and(
            &cmp::gt_eq(&dur, &f64_scalar(60.0))?,
            &cmp::lt_eq(&dur, &f64_scalar(4.0 * 3600.0))?,
        )?,
        &boolean::and(
            &boolean::and(
                &cmp::gt(&dist, &f64_scalar(0.0))?,
                &cmp::lt(&dist, &f64_scalar(100.0))?,
            )?,
            &cmp::gt(&fare, &f64_scalar(0.0))?,
        )?,
    )?;
    let hours = numeric::div(&dur, &f64_scalar(3600.0))?;
    let mph = numeric::div(&dist, &hours)?;
    let fare_per_mile = numeric::div(&fare, &dist)?;
    let tip_share = numeric::div(&tip, &fare)?;
    let mut fields: Vec<Field> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    let mut columns = batch.columns().to_vec();
    for (name, array) in [
        ("duration_s", dur),
        ("mph", mph),
        ("fare_per_mile", fare_per_mile),
        ("tip_share", tip_share),
        ("keep", Arc::new(ok) as ArrayRef),
    ] {
        fields.push(Field::new(name, array.data_type().clone(), true));
        columns.push(array);
    }
    Ok(RecordBatch::try_new(
        Arc::new(Schema::new(fields)),
        columns,
    )?)
}

/// `out`'s `keep` applied as the filter and dropped; `out` itself when it has none.
fn rows(out: &RecordBatch) -> Fallible<RecordBatch> {
    let Some(at) = out.schema().column_with_name("keep").map(|(i, _)| i) else {
        return Ok(out.clone());
    };
    let keep: BooleanArray = out.column(at).as_boolean().clone();
    let mut kept = filter_record_batch(out, &keep)?;
    kept.remove_column(at);
    Ok(kept)
}

fn main() -> Fallible<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().ok_or("a mode: expr or arrow-enrich")?.as_str();
    let reps: usize = flag(&args, "--reps")
        .map(|r| r.parse())
        .transpose()?
        .unwrap_or(3);
    let (batches, tree) = match mode {
        "expr" => {
            let text = std::fs::read_to_string(args.get(1).ok_or("a tree")?)?;
            let tree = Tree::from_json(&serde_json::from_str(&text)?)?;
            (read(args.get(2).ok_or("an input")?)?, Some(tree))
        }
        "arrow-enrich" => (read(args.get(1).ok_or("an input")?)?, None),
        other => return Err(format!("unknown mode {other}").into()),
    };
    let schema = batches.first().ok_or("no batches")?.schema();
    let started = Instant::now();
    let program = tree.as_ref().map(|t| t.bind(&schema)).transpose()?;
    let bind_s = started.elapsed().as_secs_f64();
    let run = |keep: bool| -> Fallible<Vec<RecordBatch>> {
        let mut out = Vec::with_capacity(if keep { batches.len() } else { 0 });
        for b in &batches {
            let o = match &program {
                Some(p) => p.eval(b)?,
                None => arrow_enrich(b)?,
            };
            if keep {
                out.push(o);
            }
        }
        Ok(out)
    };
    run(false)?;
    let mut times = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        run(false)?;
        times.push(t.elapsed().as_secs_f64());
    }
    if let Some(path) = flag(&args, "--out") {
        let out: Vec<RecordBatch> = run(true)?.iter().map(rows).collect::<Fallible<_>>()?;
        let mut writer = FileWriter::try_new(
            File::create(path)?,
            &out.first().ok_or("no output")?.schema(),
        )?;
        for b in &out {
            writer.write(b)?;
        }
        writer.finish()?;
    }
    let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
    let report = serde_json::json!({
        "mode": mode,
        "rows": total,
        "batches": batches.len(),
        "bind_s": bind_s,
        "instructions": program.as_ref().map(|p| p.instructions()),
        "registers": program.as_ref().map(|p| p.registers()),
        "times_s": times,
    });
    println!("{report}");
    Ok(())
}
