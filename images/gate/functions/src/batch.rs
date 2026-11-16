//! The functions as 0.6.6 called them: one Arrow `RecordBatch` in, one out.
//!
//! Each reads its columns by name, refuses a type the Python function could not take, runs the
//! core and builds the batch the Python function builds: the same column names, types, order
//! and nulls. Every output field is nullable, as `pa.record_batch` and `append_column` make
//! them.

use std::format;
use std::string::{String, ToString};
use std::sync::Arc;
use std::vec;
use std::vec::Vec;

use arrow::array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, PrimitiveArray, RecordBatch,
    UInt32Array,
};
use arrow::buffer::{BooleanBuffer, NullBuffer};
use arrow::datatypes::{
    ArrowPrimitiveType, DataType, Field, Float64Type, Int64Type, Schema, TimeUnit,
    TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType,
};

use crate::cols::{Bits, BitsBuf, Col};
use crate::error::{Error, Result};
use crate::nyc;

fn column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a ArrayRef> {
    batch
        .column_by_name(name)
        .ok_or_else(|| Error::Schema(format!("the batch has no column {name:?}")))
}

fn bits(array: &dyn Array) -> Result<Option<Bits<'_>>> {
    match array.nulls() {
        Some(n) if n.null_count() > 0 => Ok(Some(Bits::new(
            n.buffer().as_slice(),
            n.offset(),
            array.len(),
        )?)),
        _ => Ok(None),
    }
}

fn prim<T: ArrowPrimitiveType>(a: &ArrayRef) -> Option<&PrimitiveArray<T>> {
    a.as_any().downcast_ref::<PrimitiveArray<T>>()
}

/// An `int64` column, or a timestamp column as its raw values (`cast(pa.int64())`).
pub fn ints<'a>(batch: &'a RecordBatch, name: &str) -> Result<Col<'a, i64>> {
    let a = column(batch, name)?;
    let values: &[i64] = match a.data_type() {
        DataType::Int64 => prim::<Int64Type>(a).map(|p| &p.values()[..]),
        DataType::Timestamp(TimeUnit::Second, _) => {
            prim::<TimestampSecondType>(a).map(|p| &p.values()[..])
        }
        DataType::Timestamp(TimeUnit::Millisecond, _) => {
            prim::<TimestampMillisecondType>(a).map(|p| &p.values()[..])
        }
        DataType::Timestamp(TimeUnit::Microsecond, _) => {
            prim::<TimestampMicrosecondType>(a).map(|p| &p.values()[..])
        }
        DataType::Timestamp(TimeUnit::Nanosecond, _) => {
            prim::<TimestampNanosecondType>(a).map(|p| &p.values()[..])
        }
        _ => None,
    }
    .ok_or_else(|| {
        Error::Schema(format!(
            "column {name:?} is {}, not int64 or a timestamp",
            a.data_type()
        ))
    })?;
    Ok(Col::new(values, bits(a.as_ref())?))
}

/// A `double` column.
pub fn floats<'a>(batch: &'a RecordBatch, name: &str) -> Result<Col<'a, f64>> {
    let a = column(batch, name)?;
    let p = prim::<Float64Type>(a).ok_or_else(|| {
        Error::Schema(format!("column {name:?} is {}, not double", a.data_type()))
    })?;
    Ok(Col::new(&p.values()[..], bits(a.as_ref())?))
}

fn nulls(b: BitsBuf) -> Option<NullBuffer> {
    (b.nulls > 0).then(|| NullBuffer::new(BooleanBuffer::new(b.bytes.into(), 0, b.len)))
}

fn f64_array(values: Vec<f64>, valid: Option<BitsBuf>) -> ArrayRef {
    Arc::new(Float64Array::new(values.into(), valid.and_then(nulls)))
}

fn i64_array(values: Vec<i64>) -> ArrayRef {
    Arc::new(Int64Array::new(values.into(), None))
}

/// A batch of `columns`, each field nullable.
fn record(columns: Vec<(&str, ArrayRef)>) -> Result<RecordBatch> {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(n, a)| Field::new(n.to_string(), a.data_type().clone(), true))
        .collect();
    let arrays = columns.into_iter().map(|(_, a)| a).collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
        .map_err(|e| Error::Schema(e.to_string()))
}

/// `enrich`: the input's columns for the kept rows, then `duration_s`, `mph`, `fare_per_mile`
/// and `tip_share`.
pub fn enrich(batch: &RecordBatch) -> Result<RecordBatch> {
    let e = nyc::enrich(
        ints(batch, "pickup_at")?,
        ints(batch, "dropoff_at")?,
        floats(batch, "trip_distance")?,
        floats(batch, "fare_amount")?,
        floats(batch, "tip_amount")?,
    )?;
    let keep = UInt32Array::from(e.keep);
    let kept = arrow::compute::take_record_batch(batch, &keep)
        .map_err(|err| Error::Schema(err.to_string()))?;
    let mut fields: Vec<Arc<Field>> = kept.schema().fields().iter().cloned().collect();
    let mut arrays: Vec<ArrayRef> = kept.columns().to_vec();
    let rows = keep.len();
    for (name, array) in [
        ("duration_s", f64_array(e.duration_s, None)),
        ("mph", f64_array(e.mph, None)),
        ("fare_per_mile", f64_array(e.fare_per_mile, None)),
        ("tip_share", f64_array(e.tip_share, Some(e.tip_valid))),
    ] {
        debug_assert_eq!(array.len(), rows);
        fields.push(Arc::new(Field::new(name, DataType::Float64, true)));
        arrays.push(array);
    }
    let schema = Schema::new_with_metadata(fields, kept.schema().metadata().clone());
    RecordBatch::try_new(Arc::new(schema), arrays).map_err(|err| Error::Schema(err.to_string()))
}

/// `audit`: `vendor_id`, `pu_location_id`, `fare_amount`, `expected_fare`, `flagged`.
pub fn audit(batch: &RecordBatch) -> Result<RecordBatch> {
    let (expected, flagged) = nyc::audit(
        ints(batch, "pickup_at")?,
        ints(batch, "dropoff_at")?,
        floats(batch, "trip_distance")?,
        floats(batch, "fare_amount")?,
        floats(batch, "ratecode_id")?,
    )?;
    record(vec![
        ("vendor_id", column(batch, "vendor_id")?.clone()),
        ("pu_location_id", column(batch, "pu_location_id")?.clone()),
        ("fare_amount", column(batch, "fare_amount")?.clone()),
        ("expected_fare", f64_array(expected, None)),
        ("flagged", Arc::new(BooleanArray::from(flagged))),
    ])
}

/// `occupancy`: `zone`, `minute_of_day`, `car_minutes`.
pub fn occupancy(batch: &RecordBatch) -> Result<RecordBatch> {
    let out = nyc::occupancy(
        ints(batch, "pickup_at")?,
        ints(batch, "dropoff_at")?,
        int64_only(batch, "pu_location_id")?,
    )?;
    let (zone, minute, count): (ArrayRef, ArrayRef, Vec<i64>) = match out {
        nyc::Occupied::Int {
            zone,
            minute_of_day,
            car_minutes,
        } => (i64_array(zone), i64_array(minute_of_day), car_minutes),
        nyc::Occupied::Float {
            zone,
            minute_of_day,
            car_minutes,
        } => (
            f64_array(zone, None),
            f64_array(minute_of_day, None),
            car_minutes,
        ),
    };
    record(vec![
        ("zone", zone),
        ("minute_of_day", minute),
        ("car_minutes", i64_array(count)),
    ])
}

/// An `int64` column, refusing a timestamp: `pu_location_id` is a zone number.
fn int64_only<'a>(batch: &'a RecordBatch, name: &str) -> Result<Col<'a, i64>> {
    let a = column(batch, name)?;
    if a.data_type() != &DataType::Int64 {
        return Err(Error::Schema(format!(
            "column {name:?} is {}, not int64",
            a.data_type()
        )));
    }
    ints(batch, name)
}

/// `score`: `vendor_id`, `pu_location_id`, `fare_amount`, `predicted_tip`.
pub fn score(batch: &RecordBatch) -> Result<RecordBatch> {
    let t = nyc::Trips {
        pickup: ints(batch, "pickup_at")?,
        dropoff: ints(batch, "dropoff_at")?,
        trip_distance: floats(batch, "trip_distance")?,
        passenger_count: floats(batch, "passenger_count")?,
        ratecode_id: floats(batch, "ratecode_id")?,
        pu_location_id: int64_only(batch, "pu_location_id")?,
        do_location_id: int64_only(batch, "do_location_id")?,
    };
    let y = nyc::score(&t);
    record(vec![
        ("vendor_id", column(batch, "vendor_id")?.clone()),
        ("pu_location_id", column(batch, "pu_location_id")?.clone()),
        ("fare_amount", column(batch, "fare_amount")?.clone()),
        ("predicted_tip", f64_array(y, None)),
    ])
}

/// The function a job runs, by its harness name.
pub fn by_name(job: &str) -> Option<fn(&RecordBatch) -> Result<RecordBatch>> {
    Some(match job {
        "enrich" => enrich,
        "audit" => audit,
        "occupancy" => occupancy,
        "score" => score,
        _ => return None,
    })
}

/// The jobs [`by_name`] knows, for messages.
pub fn known() -> String {
    String::from("enrich, audit, occupancy, score")
}
