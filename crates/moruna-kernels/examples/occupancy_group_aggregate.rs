//! Occupancy's counting step through `moruna.group_aggregate@1` (work item 4.4; system design
//! 4G.4). The trips come as Arrow IPC files of `pickup_at`, `dropoff_at`, `pu_location_id` and
//! `fare_amount` (`bench/vm/group_aggregate_measure.py to-ipc` writes them from the normalised
//! Parquet), so this needs no Parquet reader. Each batch of trips is expanded exactly as
//! `bench/vm/workloads.py`'s `occupancy` does (one row per minute a trip ran, at most 241), and
//! the expanded rows are counted by zone and minute of day through the kernel.
//!
//! ```text
//! occupancy_group_aggregate check TRIPS.arrow OUT_DIR INSTANCES
//!     Per path (dense, hash): INSTANCES partial instances through the Kernel contract, batch i
//!     to instance i % INSTANCES, then one final instance merging their partial results.
//!     Writes OUT_DIR/PATH/partial/part-I.arrow and OUT_DIR/PATH/final/part-0.arrow.
//! occupancy_group_aggregate count TRIPS.arrow REPEATS
//!     The counting step alone, one instance, dense against hash, on rows expanded beforehand.
//! occupancy_group_aggregate floatsum REPEATS TRIPS.arrow...
//!     The cost of an exact float sum (each expanded row's share of its fare) against a binned
//!     one, a plain one and none, on the dense and the hash path, over every file in turn into
//!     one state per variant (a month, or a year as twelve months).
//! ```
//!
//! Every timing is one line of JSON on standard output.

// Every fallible call returns the contract's `MorunaError`, whose size is the contract's.
#![allow(clippy::result_large_err)]

use std::fs::File;
use std::sync::Arc;
use std::time::Instant;

use moruna_kernel::arrow::array::{ArrayRef, AsArray, Float64Array, Int64Array, RecordBatch};
use moruna_kernel::arrow::datatypes::{
    DataType, Field, Float64Type, Int64Type, Schema, TimestampMicrosecondType,
};
use moruna_kernel::arrow::ipc::reader::FileReader;
use moruna_kernel::arrow::ipc::writer::FileWriter;
use moruna_kernel::{InitCtx, Kernel, KernelState, Payload};
use moruna_kernels::group_aggregate::{FloatSum, GroupAggregate, Mode};
use moruna_testkit::FakeAllocator;
use serde_json::json;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// A minute in microseconds.
const MINUTE_US: i64 = 60_000_000;

fn read_ipc(path: &str) -> Res<Vec<RecordBatch>> {
    let reader = FileReader::try_new(File::open(path)?, None)?;
    Ok(reader.collect::<Result<Vec<_>, _>>()?)
}

fn write_ipc(path: &std::path::Path, batches: &[RecordBatch]) -> Res<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let Some(first) = batches.first() else {
        return Ok(());
    };
    let mut w = FileWriter::try_new(File::create(path)?, &first.schema())?;
    for b in batches {
        w.write(b)?;
    }
    w.finish()?;
    Ok(())
}

/// One batch of trips as Occupancy expands it: `zone` and `minute_of_day` for every minute a
/// trip ran, and, when asked, the trip's fare divided by its minutes.
fn expand(trips: &RecordBatch, with_value: bool) -> Res<RecordBatch> {
    let col = |name: &str| {
        trips
            .column_by_name(name)
            .ok_or_else(|| format!("the trips have no `{name}`"))
    };
    let pickup = col("pickup_at")?.as_primitive::<TimestampMicrosecondType>();
    let dropoff = col("dropoff_at")?.as_primitive::<TimestampMicrosecondType>();
    let zone = col("pu_location_id")?.as_primitive::<Int64Type>();
    let fare = col("fare_amount")?.as_primitive::<Float64Type>();
    let mut zones = Vec::new();
    let mut minutes = Vec::new();
    let mut values = Vec::new();
    for i in 0..trips.num_rows() {
        // numpy's `//` and `%` floor, as `div_euclid` and `rem_euclid` do for these divisors.
        let a = pickup.value(i).div_euclid(MINUTE_US);
        let b = dropoff.value(i).div_euclid(MINUTE_US);
        let n = (b - a).clamp(0, 240) + 1;
        let share = fare.value(i) / n as f64;
        for m in a..a + n {
            zones.push(zone.value(i));
            minutes.push(m.rem_euclid(1440));
            if with_value {
                values.push(share);
            }
        }
    }
    let mut fields = vec![
        Field::new("zone", DataType::Int64, false),
        Field::new("minute_of_day", DataType::Int64, false),
    ];
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(zones)),
        Arc::new(Int64Array::from(minutes)),
    ];
    if with_value {
        fields.push(Field::new("fare_minute", DataType::Float64, false));
        columns.push(Arc::new(Float64Array::from(values)));
    }
    Ok(RecordBatch::try_new(
        Arc::new(Schema::new(fields)),
        columns,
    )?)
}

/// Occupancy's count, on the dense path (zones 0 to 299, minutes 0 to 1439, the 432,000 cells
/// of 4G.4) or the hash path, with `extra` aggregates.
fn occupancy(dense: bool, extra: serde_json::Value) -> Res<GroupAggregate> {
    let mut aggregates = vec![json!({"op": "count", "as": "car_minutes"})];
    if let serde_json::Value::Array(more) = extra {
        aggregates.extend(more);
    }
    let mut args = json!({"keys": ["zone", "minute_of_day"], "aggregates": aggregates});
    if dense {
        args["domain"] = json!({"zone": [0, 299], "minute_of_day": [0, 1439]});
    }
    Ok(GroupAggregate::new(&args)?)
}

fn ctx(instance: usize) -> InitCtx {
    InitCtx {
        instance,
        device: None,
        alloc: Arc::new(FakeAllocator::new()),
    }
}

fn feed(k: &GroupAggregate, st: &mut dyn KernelState, b: &RecordBatch) -> Res<()> {
    k.apply(st, Payload::table(b.clone())?)?;
    Ok(())
}

fn finish(k: &GroupAggregate, st: &mut dyn KernelState) -> Res<Option<RecordBatch>> {
    Ok(match k.finish(st)? {
        Some(Payload::Table(b, _)) => Some(b),
        Some(_) => return Err("finish gave a tensor".into()),
        None => None,
    })
}

fn check(trips: &str, out: &str, instances: usize) -> Res<()> {
    let expanded = read_ipc(trips)?
        .iter()
        .map(|b| expand(b, false))
        .collect::<Res<Vec<_>>>()?;
    let rows: usize = expanded.iter().map(RecordBatch::num_rows).sum();
    let mut finals = Vec::new();
    for (label, dense) in [("dense", true), ("hash", false)] {
        let single = occupancy(dense, json!([]))?;
        let partial = single.in_mode(Mode::Partial, instances)?;
        let merge = single.in_mode(Mode::Final, 1)?;
        let mut states = (0..instances)
            .map(|i| partial.init(&ctx(i)))
            .collect::<Result<Vec<_>, _>>()?;
        for (i, b) in expanded.iter().enumerate() {
            feed(&partial, states[i % instances].as_mut(), b)?;
        }
        let mut last = merge.init(&ctx(0))?;
        let mut partial_rows = 0;
        for (i, st) in states.iter_mut().enumerate() {
            if let Some(p) = finish(&partial, st.as_mut())? {
                partial_rows += p.num_rows();
                write_ipc(
                    &std::path::Path::new(out).join(format!("{label}/partial/part-{i}.arrow")),
                    std::slice::from_ref(&p),
                )?;
                feed(&merge, last.as_mut(), &p)?;
            }
        }
        let result = finish(&merge, last.as_mut())?.ok_or("no rows")?;
        write_ipc(
            &std::path::Path::new(out).join(format!("{label}/final/part-0.arrow")),
            std::slice::from_ref(&result),
        )?;
        println!(
            "{}",
            json!({"check": label, "rows_in": rows, "instances": instances,
                   "partial_rows": partial_rows, "rows_out": result.num_rows(),
                   "path": format!("{:?}", single.path())})
        );
        finals.push(result);
    }
    if finals[0] != finals[1] {
        return Err("the dense and hash paths' results differ".into());
    }
    println!("{}", json!({"check": "dense equals hash", "ok": true}));
    Ok(())
}

/// One instance over `batches`, timed from the first `apply` to the end of `finish`.
fn timed(k: &GroupAggregate, batches: &[RecordBatch]) -> Res<(f64, usize)> {
    let mut st = k.init(&ctx(0))?;
    let t = Instant::now();
    for b in batches {
        feed(k, st.as_mut(), b)?;
    }
    let out = finish(k, st.as_mut())?;
    Ok((t.elapsed().as_secs_f64(), out.map_or(0, |b| b.num_rows())))
}

fn count(trips: &str, repeats: usize) -> Res<()> {
    let expanded = read_ipc(trips)?
        .iter()
        .map(|b| expand(b, false))
        .collect::<Res<Vec<_>>>()?;
    let rows: usize = expanded.iter().map(RecordBatch::num_rows).sum();
    for r in 0..repeats {
        for (label, dense) in [("dense", true), ("hash", false)] {
            let k = occupancy(dense, json!([]))?;
            let (seconds, groups) = timed(&k, &expanded)?;
            println!(
                "{}",
                json!({"measure": "count", "path": label, "repeat": r, "rows_in": rows,
                       "batches": expanded.len(), "groups": groups, "seconds": seconds})
            );
        }
    }
    Ok(())
}

fn floatsum(repeats: usize, files: &[String]) -> Res<()> {
    let sum = json!([{"op": "sum", "column": "fare_minute", "as": "fare"}]);
    let mut variants: Vec<(String, GroupAggregate)> = Vec::new();
    for (path, dense) in [("dense", true), ("hash", false)] {
        variants.push((format!("{path}/count only"), occupancy(dense, json!([]))?));
        let with_sum = occupancy(dense, sum.clone())?;
        for (label, how) in [
            ("exact", FloatSum::Exact),
            ("binned", FloatSum::Binned),
            ("plain", FloatSum::Plain),
        ] {
            variants.push((format!("{path}/{label}"), with_sum.measure_float_sum(how)));
        }
    }
    for r in 0..repeats {
        let mut states = variants
            .iter()
            .map(|(_, k)| k.init(&ctx(0)))
            .collect::<Result<Vec<_>, _>>()?;
        let mut seconds = vec![0f64; variants.len()];
        let mut rows = 0usize;
        for file in files {
            let expanded = read_ipc(file)?
                .iter()
                .map(|b| expand(b, true))
                .collect::<Res<Vec<_>>>()?;
            rows += expanded.iter().map(RecordBatch::num_rows).sum::<usize>();
            for (v, (_, k)) in variants.iter().enumerate() {
                let t = Instant::now();
                for b in &expanded {
                    feed(k, states[v].as_mut(), b)?;
                }
                seconds[v] += t.elapsed().as_secs_f64();
            }
        }
        let mut results = Vec::new();
        for (v, (label, k)) in variants.iter().enumerate() {
            let t = Instant::now();
            let out = finish(k, states[v].as_mut())?.ok_or("no rows")?;
            seconds[v] += t.elapsed().as_secs_f64();
            let sum = out.column_by_name("fare").map(|c| {
                c.as_primitive::<Float64Type>()
                    .iter()
                    .map(|x| x.unwrap_or(0.0))
                    .collect::<Vec<_>>()
            });
            println!(
                "{}",
                json!({"measure": "floatsum", "variant": label, "repeat": r, "files": files.len(),
                       "rows_in": rows, "groups": out.num_rows(), "seconds": seconds[v]})
            );
            results.push((label.clone(), sum));
        }
        // How far the binned and plain sums are from the exact one, per group.
        for path in ["dense", "hash"] {
            let get = |l: &str| {
                results
                    .iter()
                    .find(|(label, _)| label == &format!("{path}/{l}"))
                    .and_then(|(_, s)| s.clone())
            };
            let (Some(exact), Some(binned), Some(plain)) =
                (get("exact"), get("binned"), get("plain"))
            else {
                continue;
            };
            let differ = |other: &[f64]| {
                exact
                    .iter()
                    .zip(other)
                    .filter(|(a, b)| a.to_bits() != b.to_bits())
                    .count()
            };
            let worst = |other: &[f64]| {
                exact
                    .iter()
                    .zip(other)
                    .map(|(a, b)| if *a == 0.0 { 0.0 } else { ((a - b) / a).abs() })
                    .fold(0f64, f64::max)
            };
            println!(
                "{}",
                json!({"measure": "floatsum accuracy", "path": path, "repeat": r,
                       "groups": exact.len(),
                       "binned_differs": differ(&binned), "binned_worst_rel": worst(&binned),
                       "plain_differs": differ(&plain), "plain_worst_rel": worst(&plain)})
            );
        }
    }
    Ok(())
}

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("check") if args.len() == 4 => check(&args[1], &args[2], args[3].parse()?),
        Some("count") if args.len() == 3 => count(&args[1], args[2].parse()?),
        Some("floatsum") if args.len() >= 3 => floatsum(args[1].parse()?, &args[2..]),
        _ => Err(
            "usage: check TRIPS OUT INSTANCES | count TRIPS REPEATS | floatsum REPEATS TRIPS..."
                .into(),
        ),
    }
}
