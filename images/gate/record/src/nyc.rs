//! The four NYC functions over a record: the columns read in place, the core of
//! `moruna_gate_functions::nyc` run on them, the output assembled as the Python function builds
//! it (`moruna_gate_functions::batch` is the same over arrow-rs).

use alloc::string::String;

use moruna_gate_functions::nyc as core;

use crate::decode::Batch;
use crate::encode::OutArray;
use crate::types::{LField, LType};
use crate::{Error, Output};

/// An input column passed through: its field (nullable, as `pa.record_batch` makes it) and its
/// buffers, borrowed.
fn pass<'a>(out: &mut Output<'a>, batch: &Batch<'a>, name: &str) -> Result<(), Error> {
    let (f, a) = batch.column(name)?;
    let field = LField {
        name: String::from(name),
        nullable: true,
        ty: f.ty.clone(),
    };
    out.push(field, OutArray::borrowed(a));
    Ok(())
}

pub fn enrich<'a>(batch: &Batch<'a>) -> Result<Output<'a>, Error> {
    let e = core::enrich(
        batch.ints("pickup_at")?,
        batch.ints("dropoff_at")?,
        batch.floats("trip_distance")?,
        batch.floats("fare_amount")?,
        batch.floats("tip_amount")?,
    )?;
    let mut out = Output::new(e.keep.len());
    // `append_column` keeps the input's fields as they are.
    for (f, a) in batch.fields.iter().zip(&batch.columns) {
        out.push(f.clone(), OutArray::take(a, &f.ty, &e.keep)?);
    }
    out.push(
        LField::new("duration_s", LType::F64),
        OutArray::fixed(e.duration_s, None),
    );
    out.push(LField::new("mph", LType::F64), OutArray::fixed(e.mph, None));
    out.push(
        LField::new("fare_per_mile", LType::F64),
        OutArray::fixed(e.fare_per_mile, None),
    );
    out.push(
        LField::new("tip_share", LType::F64),
        OutArray::fixed(e.tip_share, Some(e.tip_valid)),
    );
    Ok(out)
}

pub fn audit<'a>(batch: &Batch<'a>) -> Result<Output<'a>, Error> {
    let (expected, flagged) = core::audit(
        batch.ints("pickup_at")?,
        batch.ints("dropoff_at")?,
        batch.floats("trip_distance")?,
        batch.floats("fare_amount")?,
        batch.floats("ratecode_id")?,
    )?;
    let mut out = Output::new(batch.rows);
    pass(&mut out, batch, "vendor_id")?;
    pass(&mut out, batch, "pu_location_id")?;
    pass(&mut out, batch, "fare_amount")?;
    out.push(
        LField::new("expected_fare", LType::F64),
        OutArray::fixed(expected, None),
    );
    out.push(
        LField::new("flagged", LType::Bool),
        OutArray::bools(&flagged),
    );
    Ok(out)
}

pub fn occupancy<'a>(batch: &Batch<'a>) -> Result<Output<'a>, Error> {
    let o = core::occupancy(
        batch.ints("pickup_at")?,
        batch.ints("dropoff_at")?,
        batch.int64("pu_location_id")?,
    )?;
    let (zone, minute, count, ty) = match o {
        core::Occupied::Int {
            zone,
            minute_of_day,
            car_minutes,
        } => (
            OutArray::fixed(zone, None),
            OutArray::fixed(minute_of_day, None),
            car_minutes,
            LType::I64,
        ),
        core::Occupied::Float {
            zone,
            minute_of_day,
            car_minutes,
        } => (
            OutArray::fixed(zone, None),
            OutArray::fixed(minute_of_day, None),
            car_minutes,
            LType::F64,
        ),
    };
    let mut out = Output::new(count.len());
    out.push(LField::new("zone", ty.clone()), zone);
    out.push(LField::new("minute_of_day", ty), minute);
    out.push(
        LField::new("car_minutes", LType::I64),
        OutArray::fixed(count, None),
    );
    Ok(out)
}

pub fn score<'a>(batch: &Batch<'a>) -> Result<Output<'a>, Error> {
    let t = core::Trips {
        pickup: batch.ints("pickup_at")?,
        dropoff: batch.ints("dropoff_at")?,
        trip_distance: batch.floats("trip_distance")?,
        passenger_count: batch.floats("passenger_count")?,
        ratecode_id: batch.floats("ratecode_id")?,
        pu_location_id: batch.int64("pu_location_id")?,
        do_location_id: batch.int64("do_location_id")?,
    };
    let y = core::score(&t);
    let mut out = Output::new(batch.rows);
    pass(&mut out, batch, "vendor_id")?;
    pass(&mut out, batch, "pu_location_id")?;
    pass(&mut out, batch, "fare_amount")?;
    out.push(
        LField::new("predicted_tip", LType::F64),
        OutArray::fixed(y, None),
    );
    Ok(out)
}
