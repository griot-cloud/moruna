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
    Array, ArrayRef, AsArray, BooleanArray, FixedSizeListArray, Float64Array, Int32Array,
    Int64Array, PrimitiveArray, RecordBatch, StringArray, StructArray, UInt32Array, UInt64Array,
};
use arrow::buffer::{BooleanBuffer, NullBuffer};
use arrow::datatypes::{
    ArrowPrimitiveType, DataType, Field, Float32Type, Float64Type, Int32Type, Int64Type, Schema,
    TimeUnit, TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType,
};

use crate::cols::{Bits, BitsBuf, Col, Offsets, Strs};
use crate::error::{Error, Result};
use crate::overture::{AttrsIn, BBox, Floats, Sources};
use crate::{fineweb, nyc, overture};

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

/// A string or binary column of either offset width.
pub fn strs<'a>(a: &'a ArrayRef, what: &str) -> Result<Strs<'a>> {
    let valid = bits(a.as_ref())?;
    let (offsets, data) = match a.data_type() {
        DataType::Utf8 => {
            let s = a.as_string::<i32>();
            (Offsets::Small(s.value_offsets()), s.value_data())
        }
        DataType::LargeUtf8 => {
            let s = a.as_string::<i64>();
            (Offsets::Large(s.value_offsets()), s.value_data())
        }
        DataType::Binary => {
            let s = a.as_binary::<i32>();
            (Offsets::Small(s.value_offsets()), s.value_data())
        }
        DataType::LargeBinary => {
            let s = a.as_binary::<i64>();
            (Offsets::Large(s.value_offsets()), s.value_data())
        }
        other => {
            return Err(Error::Schema(format!(
                "{what} is {other}, not a string or binary column"
            )));
        }
    };
    Ok(Strs {
        offsets,
        data,
        valid,
    })
}

fn child<'a>(s: &'a StructArray, name: &str, what: &str) -> Result<&'a ArrayRef> {
    s.column_by_name(name)
        .ok_or_else(|| Error::Schema(format!("{what} has no field {name:?}")))
}

fn floats_of<'a>(a: &'a ArrayRef, what: &str) -> Result<Floats<'a>> {
    let v = bits(a.as_ref())?;
    match a.data_type() {
        DataType::Float32 => Ok(Floats::F32(Col::new(
            &a.as_primitive::<Float32Type>().values()[..],
            v,
        ))),
        DataType::Float64 => Ok(Floats::F64(Col::new(
            &a.as_primitive::<Float64Type>().values()[..],
            v,
        ))),
        other => Err(Error::Schema(format!(
            "{what} is {other}, not floating point"
        ))),
    }
}

fn struct_col<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StructArray> {
    let a = column(batch, name)?;
    a.as_any().downcast_ref::<StructArray>().ok_or_else(|| {
        Error::Schema(format!(
            "column {name:?} is {}, not a struct",
            a.data_type()
        ))
    })
}

fn strings(values: Vec<Option<String>>) -> ArrayRef {
    Arc::new(StringArray::from(values))
}

/// `fw_minhash`: `id`, `url`, `minhash` (64 `uint32`), `bands` (8 `uint64`).
pub fn fw_minhash(batch: &RecordBatch) -> Result<RecordBatch> {
    let texts = column(batch, "text")?;
    let (sigs, keys) = fineweb::minhash(&strs(texts, "column \"text\"")?)?;
    let item = |t: DataType| Arc::new(Field::new("item", t, true));
    let minhash = FixedSizeListArray::try_new(
        item(DataType::UInt32),
        fineweb::PERMS as i32,
        Arc::new(UInt32Array::from(sigs)),
        None,
    )
    .map_err(|e| Error::Schema(e.to_string()))?;
    let bands = FixedSizeListArray::try_new(
        item(DataType::UInt64),
        fineweb::BANDS as i32,
        Arc::new(UInt64Array::from(keys)),
        None,
    )
    .map_err(|e| Error::Schema(e.to_string()))?;
    record(vec![
        ("id", column(batch, "id")?.clone()),
        ("url", column(batch, "url")?.clone()),
        ("minhash", Arc::new(minhash)),
        ("bands", Arc::new(bands)),
    ])
}

/// `ov_attrs`: `id`, `name`, `sources`, `best_source`, `height_m`, `usage`, `volume_m3`.
pub fn ov_attrs(batch: &RecordBatch) -> Result<RecordBatch> {
    let names = struct_col(batch, "names")?;
    let src = column(batch, "sources")?;
    let (offsets, items) = match src.data_type() {
        DataType::List(_) => {
            let l = src.as_list::<i32>();
            (Offsets::Small(l.value_offsets()), l.values())
        }
        DataType::LargeList(_) => {
            let l = src.as_list::<i64>();
            (Offsets::Large(l.value_offsets()), l.values())
        }
        other => {
            return Err(Error::Schema(format!(
                "column \"sources\" is {other}, not a list"
            )));
        }
    };
    let items = items
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or_else(|| Error::Schema("sources' items are not structs".into()))?;
    let bbox = struct_col(batch, "bbox")?;
    let floors = column(batch, "num_floors")?;
    if floors.data_type() != &DataType::Int32 {
        return Err(Error::Schema(format!(
            "column \"num_floors\" is {}, not int32",
            floors.data_type()
        )));
    }
    let a = AttrsIn {
        rows: batch.num_rows(),
        names_valid: bits(names)?,
        primary: strs(child(names, "primary", "names")?, "names.primary")?,
        sources: Sources {
            offsets,
            valid: bits(src.as_ref())?,
            item_valid: bits(items)?,
            dataset: strs(child(items, "dataset", "sources")?, "sources.dataset")?,
            confidence: floats_of(child(items, "confidence", "sources")?, "sources.confidence")?,
        },
        height: floats(batch, "height")?,
        num_floors: Col::new(
            &floors.as_primitive::<Int32Type>().values()[..],
            bits(floors.as_ref())?,
        ),
        class: strs(column(batch, "class")?, "column \"class\"")?,
        subtype: strs(column(batch, "subtype")?, "column \"subtype\"")?,
        bbox: BBox {
            valid: bits(bbox)?,
            xmin: floats_of(child(bbox, "xmin", "bbox")?, "bbox.xmin")?,
            xmax: floats_of(child(bbox, "xmax", "bbox")?, "bbox.xmax")?,
            ymin: floats_of(child(bbox, "ymin", "bbox")?, "bbox.ymin")?,
            ymax: floats_of(child(bbox, "ymax", "bbox")?, "bbox.ymax")?,
        },
    };
    let o = overture::attrs(&a)?;
    let usage: Vec<Option<String>> = o.usage.iter().map(|u| Some(String::from(*u))).collect();
    record(vec![
        ("id", column(batch, "id")?.clone()),
        ("name", strings(o.name)),
        ("sources", Arc::new(Int32Array::from(o.sources))),
        ("best_source", strings(o.best_source)),
        ("height_m", Arc::new(Float64Array::from(o.height_m))),
        ("usage", strings(usage)),
        ("volume_m3", Arc::new(Float64Array::from(o.volume_m3))),
    ])
}

/// `ov_shape`: `id`, `class`, `subtype`, `height`, then the shape's columns.
pub fn ov_shape(batch: &RecordBatch) -> Result<RecordBatch> {
    let s = overture::shapes(&strs(column(batch, "geometry")?, "column \"geometry\"")?)?;
    let f = |v: Vec<f64>| -> ArrayRef { Arc::new(Float64Array::from(v)) };
    let i = |v: Vec<i32>| -> ArrayRef { Arc::new(Int32Array::from(v)) };
    let perimeter = if s.pairs == 0 {
        i64_array(vec![0; s.perimeter_m.len()])
    } else {
        f(s.perimeter_m)
    };
    record(vec![
        ("id", column(batch, "id")?.clone()),
        ("class", column(batch, "class")?.clone()),
        ("subtype", column(batch, "subtype")?.clone()),
        ("height", column(batch, "height")?.clone()),
        ("area_m2", f(s.area_m2)),
        ("perimeter_m", perimeter),
        ("lon", f(s.lon)),
        ("lat", f(s.lat)),
        ("vertices", i(s.vertices)),
        ("compactness", f(s.compactness)),
        ("tile_x", i(s.tile_x)),
        ("tile_y", i(s.tile_y)),
    ])
}

/// The function a job runs, by its harness name.
pub fn by_name(job: &str) -> Option<fn(&RecordBatch) -> Result<RecordBatch>> {
    Some(match job {
        "enrich" => enrich,
        "audit" => audit,
        "occupancy" => occupancy,
        "score" => score,
        "fw_minhash" => fw_minhash,
        "ov_attrs" => ov_attrs,
        "ov_shape" => ov_shape,
        _ => return None,
    })
}

/// The jobs [`by_name`] knows, for messages.
pub fn known() -> String {
    String::from("enrich, audit, occupancy, score, fw_minhash, ov_attrs, ov_shape")
}
