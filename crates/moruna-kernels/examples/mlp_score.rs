//! Score (`bench/vm/workloads.py`, W4) through `mlp_forward`, and 4G.1's timing of its network
//! (system design 4G.1, item 4.6).
//!
//! Score's features are computed here with plain Arrow arrays and Rust's `f64` arithmetic, the
//! operations `workloads.score` applies with NumPy, in its order, until `expr` (item 4.2) computes
//! them; the network is `mlp_forward`'s CPU reference; the prediction is rounded to six decimals
//! as NumPy's `np.round(y, 6)` rounds (`rint(y * 1e6) / 1e6`).
//!
//! ```text
//! mlp_score score MODEL.json EMBEDDING.json PRECISION TRIPS.arrow OUT.arrow
//! mlp_score time  MODEL.json PRECISION FEATURES.arrow ROWS REPS OUT.arrow
//! ```
//!
//! `score` reads an Arrow IPC file of trips (the normalised NYC columns), batch by batch, and
//! writes Score's four output columns. `time` configures the kernel once, runs the forward pass
//! over the first ROWS rows of an IPC file of 23 Float64 feature columns REPS times on this
//! thread, prints one JSON line of the times, and writes the output once.

use std::fs::File;
use std::sync::Arc;
use std::time::Instant;

use moruna_kernel::arrow::array::{Array, ArrayRef, AsArray, Float32Array, Float64Array};
use moruna_kernel::arrow::datatypes::{
    DataType, Field, Float64Type, Int64Type, Schema, TimeUnit, TimestampMicrosecondType,
};
use moruna_kernel::arrow::ipc::reader::FileReader;
use moruna_kernel::arrow::ipc::writer::FileWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernels::mlp::{MlpForward, Model, Precision, Scratch};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("score") if args.len() == 6 => score(&args[1], &args[2], &args[3], &args[4], &args[5]),
        Some("time") if args.len() == 7 => time(
            &args[1],
            &args[2],
            &args[3],
            args[4].parse()?,
            args[5].parse()?,
            &args[6],
        ),
        _ => Err(
            "usage: mlp_score score MODEL EMBEDDING PRECISION TRIPS OUT | \
                  time MODEL PRECISION FEATURES ROWS REPS OUT"
                .into(),
        ),
    }
}

fn kernel(model: &str, precision: &str) -> Res<MlpForward> {
    let model = Model::from_json(&serde_json::from_reader(File::open(model)?)?)?;
    Ok(MlpForward::configure(model, Precision::parse(precision)?))
}

fn column<'a>(batch: &'a RecordBatch, name: &str) -> Res<&'a ArrayRef> {
    batch
        .column_by_name(name)
        .ok_or_else(|| format!("the trips have no column `{name}`").into())
}

/// A timestamp column's microseconds, as `.cast(pa.int64())` gives them; Score's timestamps have
/// no nulls.
fn micros(batch: &RecordBatch, name: &str) -> Res<Vec<i64>> {
    let col = column(batch, name)?;
    if col.null_count() > 0 {
        return Err(format!("`{name}` has nulls, which Score does not define").into());
    }
    match col.data_type() {
        DataType::Timestamp(TimeUnit::Microsecond, _) => Ok(col
            .as_primitive::<TimestampMicrosecondType>()
            .values()
            .to_vec()),
        other => Err(format!("`{name}` is {other}, not a timestamp in microseconds").into()),
    }
}

/// `pc.fill_null(batch[c], d).to_numpy().astype(np.float64)`.
fn num(batch: &RecordBatch, name: &str, fill: f64) -> Res<Vec<f64>> {
    let col = column(batch, name)?;
    Ok(match col.data_type() {
        DataType::Float64 => col
            .as_primitive::<Float64Type>()
            .iter()
            .map(|v| v.unwrap_or(fill))
            .collect(),
        DataType::Int64 => col
            .as_primitive::<Int64Type>()
            .iter()
            .map(|v| v.map_or(fill, |v| v as f64))
            .collect(),
        other => return Err(format!("`{name}` is {other}, not Float64 or Int64").into()),
    })
}

/// `np.clip(x, lo, hi)` on floats: NaN stays NaN, and a tie keeps the bound (`clip(-0.0, 0.0,
/// 1.0)` is `+0.0`), as `images/gate/functions/src/pyfloat.rs`'s `np_clip`.
fn clip(x: f64, lo: f64, hi: f64) -> f64 {
    let m = if x.is_nan() || x > lo { x } else { lo };
    if m.is_nan() || m < hi { m } else { hi }
}

/// Score's 23 features, one Float64 column each, as `workloads.score` builds `X`.
fn features(batch: &RecordBatch, emb: &[Vec<f64>]) -> Res<Vec<ArrayRef>> {
    let n = batch.num_rows();
    let a = micros(batch, "pickup_at")?;
    let b = micros(batch, "dropoff_at")?;
    let dist: Vec<f64> = num(batch, "trip_distance", 0.0)?
        .into_iter()
        .map(|d| clip(d, 0.0, 200.0))
        .collect();
    let dur: Vec<f64> = a
        .iter()
        .zip(&b)
        .map(|(a, b)| clip(b.wrapping_sub(*a) as f64 / 60e6, 0.0, 240.0))
        .collect();
    let hour: Vec<f64> = a
        .iter()
        .map(|a| a.div_euclid(3_600_000_000).rem_euclid(24) as f64)
        .collect();
    let step = std::f64::consts::PI / 12.0;
    let zone = |name: &str| -> Res<Vec<usize>> {
        Ok(num(batch, name, 0.0)?
            .into_iter()
            .map(|v| (v as i64).clamp(0, 299) as usize)
            .collect())
    };
    let (pu, du) = (zone("pu_location_id")?, zone("do_location_id")?);
    let mut cols: Vec<Vec<f64>> = vec![
        dist.iter().map(|d| d.ln_1p()).collect(),
        dur.iter().map(|d| d.ln_1p()).collect(),
        hour.iter().map(|h| (h * step).sin()).collect(),
        hour.iter().map(|h| (h * step).cos()).collect(),
        a.iter()
            .map(|a| a.div_euclid(86_400_000_000).rem_euclid(7) as f64 / 6.0)
            .collect(),
        num(batch, "passenger_count", 1.0)?
            .into_iter()
            .map(|v| v / 4.0)
            .collect(),
        num(batch, "ratecode_id", 1.0)?
            .into_iter()
            .map(|v| v / 6.0)
            .collect(),
    ];
    for zones in [&pu, &du] {
        cols.extend((0..8).map(|j| zones.iter().map(|&z| emb[z][j]).collect()));
    }
    debug_assert!(cols.iter().all(|c| c.len() == n));
    Ok(cols
        .into_iter()
        .map(|c| Arc::new(Float64Array::from(c)) as ArrayRef)
        .collect())
}

/// `np.round(y, 6)`: `rint(y * 1e6) / 1e6`.
fn round6(y: f64) -> f64 {
    (y * 1e6).round_ties_even() / 1e6
}

fn as_f64(out: &ArrayRef) -> Vec<f64> {
    match out.data_type() {
        DataType::Float32 => out
            .as_any()
            .downcast_ref::<Float32Array>()
            .map(|a| a.values().iter().map(|&v| v as f64).collect())
            .unwrap_or_default(),
        _ => out.as_primitive::<Float64Type>().values().to_vec(),
    }
}

fn score(model: &str, embedding: &str, precision: &str, trips: &str, out: &str) -> Res<()> {
    let kernel = kernel(model, precision)?;
    let emb: Vec<Vec<f64>> = serde_json::from_reader(File::open(embedding)?)?;
    if emb.len() != 300 || emb.iter().any(|r| r.len() != 8) {
        return Err("Score's embedding is 300 rows of 8".into());
    }
    let schema = Arc::new(Schema::new(vec![
        Field::new("vendor_id", DataType::Int64, true),
        Field::new("pu_location_id", DataType::Int64, true),
        Field::new("fare_amount", DataType::Float64, true),
        Field::new("predicted_tip", DataType::Float64, true),
    ]));
    let mut writer = FileWriter::try_new(File::create(out)?, &schema)?;
    let mut scratch = Scratch::default();
    let (mut rows, mut seconds) = (0usize, 0.0f64);
    for batch in FileReader::try_new(File::open(trips)?, None)? {
        let batch = batch?;
        let x = features(&batch, &emb)?;
        let t = Instant::now();
        let y = kernel.forward(&x, &mut scratch)?;
        seconds += t.elapsed().as_secs_f64();
        rows += batch.num_rows();
        let tip: Float64Array = as_f64(&y).into_iter().map(|v| Some(round6(v))).collect();
        writer.write(&RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::clone(column(&batch, "vendor_id")?),
                Arc::clone(column(&batch, "pu_location_id")?),
                Arc::clone(column(&batch, "fare_amount")?),
                Arc::new(tip),
            ],
        )?)?;
    }
    writer.finish()?;
    println!("{{\"rows\": {rows}, \"precision\": \"{precision}\", \"network_s\": {seconds:.4}}}");
    Ok(())
}

fn time(
    model: &str,
    precision: &str,
    features: &str,
    rows: usize,
    reps: usize,
    out: &str,
) -> Res<()> {
    let kernel = kernel(model, precision)?;
    let mut columns: Vec<Vec<f64>> = Vec::new();
    for batch in FileReader::try_new(File::open(features)?, None)? {
        let batch = batch?;
        columns.resize(batch.num_columns(), Vec::new());
        for (c, col) in batch.columns().iter().enumerate() {
            columns[c].extend_from_slice(col.as_primitive::<Float64Type>().values());
        }
    }
    if columns.first().map_or(0, Vec::len) < rows {
        return Err(format!("the features have fewer than {rows} rows").into());
    }
    let x: Vec<ArrayRef> = columns
        .iter()
        .map(|c| Arc::new(Float64Array::from(c[..rows].to_vec())) as ArrayRef)
        .collect();
    let mut scratch = Scratch::default();
    let mut times = Vec::with_capacity(reps);
    let mut y = kernel.forward(&x, &mut scratch)?;
    for _ in 0..reps {
        let t = Instant::now();
        y = kernel.forward(&x, &mut scratch)?;
        times.push(t.elapsed().as_secs_f64());
    }
    times.sort_by(f64::total_cmp);
    let schema = Arc::new(Schema::new(vec![Field::new(
        "y",
        y.data_type().clone(),
        true,
    )]));
    let mut writer = FileWriter::try_new(File::create(out)?, &schema)?;
    writer.write(&RecordBatch::try_new(schema, vec![y])?)?;
    writer.finish()?;
    println!(
        "{{\"rows\": {rows}, \"precision\": \"{precision}\", \"reps\": {reps}, \"best_s\": {:.6}, \"median_s\": {:.6}}}",
        times[0],
        times[times.len() / 2]
    );
    Ok(())
}
