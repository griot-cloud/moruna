//! What each standard kernel does to a batch, and what it does to a schema (MH 4.9).

use std::collections::HashSet;
use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, Date32Array, LargeStringArray, ListArray,
    PrimitiveArray, StringArray, StringBuilder, UInt32Array, new_null_array,
};
use moruna_kernel::arrow::compute::kernels::{boolean, cast, filter, take, zip};
use moruna_kernel::arrow::datatypes::{
    DataType, Field, Schema, SchemaRef, TimeUnit, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType,
};
use moruna_kernel::arrow::record_batch::{RecordBatch, RecordBatchOptions};
use moruna_kernel::arrow::row::{RowConverter, SortField};
use moruna_kernel::{MorunaError, Result};
use sha2::Digest;

use crate::expr::{Expr, Literal};

pub(crate) fn plan(kernel: &str, msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Plan(format!("moruna.std.{kernel}: {msg}"))
}

fn arrow(kernel: &str) -> impl Fn(moruna_kernel::arrow::error::ArrowError) -> MorunaError + '_ {
    move |e| plan(kernel, e)
}

fn field<'a>(kernel: &str, schema: &'a Schema, name: &str) -> Result<(usize, &'a Field)> {
    let index = schema
        .index_of(name)
        .map_err(|_| plan(kernel, format!("the input has no column `{name}`")))?;
    Ok((index, schema.field(index)))
}

fn batch_of(
    kernel: &str,
    schema: SchemaRef,
    columns: Vec<ArrayRef>,
    rows: usize,
) -> Result<RecordBatch> {
    RecordBatch::try_new_with_options(
        schema,
        columns,
        &RecordBatchOptions::new().with_row_count(Some(rows)),
    )
    .map_err(arrow(kernel))
}

/// One step of a projection: `cast`, `rename`, `select` or `drop` (MH 4.9). A chain of them is
/// one projection, which is what makes `select` after `cast` one stage.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Cast the named columns; `strict` refuses a value that does not fit instead of nulling it.
    Cast {
        /// Column to target type, in name order.
        columns: Vec<(String, DataType)>,
        /// Error on a value that does not convert.
        strict: bool,
    },
    /// Rename columns, old to new.
    Rename(Vec<(String, String)>),
    /// Keep these columns, in this order.
    Select(Vec<String>),
    /// Remove these columns.
    Drop(Vec<String>),
}

impl Step {
    fn name(&self) -> &'static str {
        match self {
            Step::Cast { .. } => "cast",
            Step::Rename(_) => "rename",
            Step::Select(_) => "select",
            Step::Drop(_) => "drop",
        }
    }
}

/// An output column of a projection: which input column, the casts on the way, its name.
#[derive(Clone, Debug)]
struct Planned {
    source: usize,
    casts: Vec<(DataType, bool)>,
    name: String,
    ty: DataType,
    nullable: bool,
}

/// Walk the steps over `input` symbolically: the output columns and, for each, the casts it
/// takes. A column dropped by a later step is never cast, which is the point of fusing.
fn plan_projection(steps: &[Step], input: &Schema) -> Result<Vec<Planned>> {
    let mut cols: Vec<Planned> = input
        .fields()
        .iter()
        .enumerate()
        .map(|(i, f)| Planned {
            source: i,
            casts: Vec::new(),
            name: f.name().clone(),
            ty: f.data_type().clone(),
            nullable: f.is_nullable(),
        })
        .collect();
    for step in steps {
        let kernel = step.name();
        let find = |cols: &[Planned], name: &str| {
            cols.iter()
                .position(|c| c.name == name)
                .ok_or_else(|| plan(kernel, format!("the input has no column `{name}`")))
        };
        match step {
            Step::Cast { columns, strict } => {
                for (name, to) in columns {
                    let at = find(&cols, name)?;
                    if !cast::can_cast_types(&cols[at].ty, to) {
                        return Err(plan(
                            kernel,
                            format!(
                                "column `{name}` cannot be cast from {} to {to}",
                                cols[at].ty
                            ),
                        ));
                    }
                    cols[at].casts.push((to.clone(), *strict));
                    cols[at].ty = to.clone();
                }
            }
            Step::Rename(pairs) => {
                for (old, new) in pairs {
                    let at = find(&cols, old)?;
                    cols[at].name = new.clone();
                }
                let mut seen = HashSet::new();
                for c in &cols {
                    if !seen.insert(c.name.clone()) {
                        return Err(plan(
                            kernel,
                            format!("two columns would be named `{}`", c.name),
                        ));
                    }
                }
            }
            Step::Select(names) => {
                let mut kept = Vec::with_capacity(names.len());
                for name in names {
                    kept.push(cols[find(&cols, name)?].clone());
                }
                cols = kept;
            }
            Step::Drop(names) => {
                for name in names {
                    find(&cols, name)?;
                }
                cols.retain(|c| !names.contains(&c.name));
            }
        }
    }
    Ok(cols)
}

pub(crate) fn projection_schema(steps: &[Step], input: &Schema) -> Result<SchemaRef> {
    let planned = plan_projection(steps, input)?;
    Ok(Arc::new(Schema::new(
        planned
            .iter()
            .map(|c| Field::new(c.name.clone(), c.ty.clone(), c.nullable))
            .collect::<Vec<_>>(),
    )))
}

pub(crate) fn project(steps: &[Step], batch: &RecordBatch) -> Result<RecordBatch> {
    let planned = plan_projection(steps, &batch.schema())?;
    let mut columns = Vec::with_capacity(planned.len());
    for c in &planned {
        let mut array = batch.column(c.source).clone();
        for (to, strict) in &c.casts {
            let options = cast::CastOptions {
                safe: !strict,
                ..Default::default()
            };
            array = cast::cast_with_options(&array, to, &options).map_err(arrow("cast"))?;
        }
        columns.push(array);
    }
    let schema = Arc::new(Schema::new(
        planned
            .iter()
            .map(|c| Field::new(c.name.clone(), c.ty.clone(), c.nullable))
            .collect::<Vec<_>>(),
    ));
    batch_of("select", schema, columns, batch.num_rows())
}

/// `fill_null`: each named column's nulls replaced with a literal cast to the column's type.
pub(crate) fn fill_check(values: &[(String, Literal)], input: &Schema) -> Result<()> {
    for (name, literal) in values {
        let (_, f) = field("fill_null", input, name)?;
        literal.scalar_of(f.data_type())?;
    }
    Ok(())
}

pub(crate) fn fill(values: &[(String, Literal)], batch: &RecordBatch) -> Result<RecordBatch> {
    let schema = batch.schema();
    let mut columns = batch.columns().to_vec();
    for (name, literal) in values {
        let (at, f) = field("fill_null", &schema, name)?;
        let column = &columns[at];
        if column.null_count() == 0 {
            continue;
        }
        let scalar = literal.scalar_of(f.data_type())?;
        let present = boolean::is_not_null(column.as_ref()).map_err(arrow("fill_null"))?;
        columns[at] = zip::zip(&present, column, &scalar).map_err(arrow("fill_null"))?;
    }
    batch_of("fill_null", schema, columns, batch.num_rows())
}

/// `filter`: the rows where the predicate is true.
pub(crate) fn filter(expr: &Expr, batch: &RecordBatch) -> Result<RecordBatch> {
    let mask = expr.eval(batch)?;
    filter::filter_record_batch(batch, &mask).map_err(arrow("filter"))
}

/// `fill_null` then `filter`, as one stage: the predicate reads the filled values of the
/// columns it names, the rows are filtered, and only the surviving rows are filled.
pub(crate) fn fill_then_filter(
    values: &[(String, Literal)],
    expr: &Expr,
    batch: &RecordBatch,
) -> Result<RecordBatch> {
    let read: Vec<String> = expr.columns().into_iter().map(|c| c.name).collect();
    let needed: Vec<(String, Literal)> = values
        .iter()
        .filter(|(name, _)| read.contains(name))
        .cloned()
        .collect();
    let mask = if needed.is_empty() {
        expr.eval(batch)?
    } else {
        expr.eval(&fill(&needed, batch)?)?
    };
    let kept = filter::filter_record_batch(batch, &mask).map_err(arrow("filter"))?;
    fill(values, &kept)
}

/// The key bytes of each row, for `dedupe`.
pub(crate) fn row_keys(keys: &[String], batch: &RecordBatch) -> Result<Vec<Vec<u8>>> {
    let schema = batch.schema();
    let mut fields = Vec::with_capacity(keys.len());
    let mut columns = Vec::with_capacity(keys.len());
    for key in keys {
        let (at, f) = field("dedupe", &schema, key)?;
        fields.push(SortField::new(f.data_type().clone()));
        columns.push(batch.column(at).clone());
    }
    let converter = RowConverter::new(fields).map_err(arrow("dedupe"))?;
    let rows = converter
        .convert_columns(&columns)
        .map_err(arrow("dedupe"))?;
    Ok(rows.iter().map(|r| r.as_ref().to_vec()).collect())
}

pub(crate) fn dedupe_check(keys: &[String], input: &Schema) -> Result<()> {
    for key in keys {
        let (_, f) = field("dedupe", input, key)?;
        if !RowConverter::supports_fields(&[SortField::new(f.data_type().clone())]) {
            return Err(plan(
                "dedupe",
                format!("column `{key}` of type {} cannot be a key", f.data_type()),
            ));
        }
    }
    Ok(())
}

/// `dedupe`: the rows whose key has not been seen, in this batch or an earlier one.
pub(crate) fn dedupe(
    keys: &[String],
    seen: &mut HashSet<Vec<u8>>,
    batch: &RecordBatch,
) -> Result<RecordBatch> {
    let rows = row_keys(keys, batch)?;
    let mask: BooleanArray = rows.into_iter().map(|k| Some(seen.insert(k))).collect();
    filter::filter_record_batch(batch, &mask).map_err(arrow("dedupe"))
}

/// A hash algorithm of `moruna.std.hash` and of `mask(mode="hash")`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Algo {
    /// SHA-256.
    Sha256,
    /// BLAKE3.
    Blake3,
}

impl Algo {
    pub(crate) fn parse(kernel: &str, s: &str) -> Result<Algo> {
        match s {
            "sha256" => Ok(Algo::Sha256),
            "blake3" => Ok(Algo::Blake3),
            other => Err(plan(
                kernel,
                format!("unknown algo `{other}` (use sha256 or blake3)"),
            )),
        }
    }

    fn hex(&self, bytes: &[u8]) -> String {
        let digest: [u8; 32] = match self {
            Algo::Sha256 => sha2::Sha256::digest(bytes).into(),
            Algo::Blake3 => *blake3::hash(bytes).as_bytes(),
        };
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Each value of a column as text, `None` for null: the column cast to `Utf8`, except binary
/// columns, whose bytes are taken as they are.
fn texts(kernel: &str, column: &ArrayRef) -> Result<Vec<Option<Vec<u8>>>> {
    match column.data_type() {
        DataType::Binary => Ok(column
            .as_binary::<i32>()
            .iter()
            .map(|v| v.map(<[u8]>::to_vec))
            .collect()),
        DataType::LargeBinary => Ok(column
            .as_binary::<i64>()
            .iter()
            .map(|v| v.map(<[u8]>::to_vec))
            .collect()),
        _ => {
            let text = cast::cast(column, &DataType::Utf8).map_err(arrow(kernel))?;
            Ok(text
                .as_string::<i32>()
                .iter()
                .map(|v| v.map(|s| s.as_bytes().to_vec()))
                .collect())
        }
    }
}

fn check_textual(kernel: &str, input: &Schema, columns: &[String]) -> Result<()> {
    for name in columns {
        let (_, f) = field(kernel, input, name)?;
        let binary = matches!(f.data_type(), DataType::Binary | DataType::LargeBinary);
        if !binary && !cast::can_cast_types(f.data_type(), &DataType::Utf8) {
            return Err(plan(
                kernel,
                format!("column `{name}` of type {} has no text form", f.data_type()),
            ));
        }
    }
    Ok(())
}

pub(crate) fn appended(
    kernel: &str,
    input: &Schema,
    output: &str,
    ty: DataType,
) -> Result<SchemaRef> {
    if input.index_of(output).is_ok() {
        return Err(plan(
            kernel,
            format!("the output column `{output}` already exists"),
        ));
    }
    let mut fields: Vec<Field> = input.fields().iter().map(|f| f.as_ref().clone()).collect();
    fields.push(Field::new(output, ty, true));
    Ok(Arc::new(Schema::new_with_metadata(
        fields,
        input.metadata().clone(),
    )))
}

pub(crate) fn hash_schema(columns: &[String], output: &str, input: &Schema) -> Result<SchemaRef> {
    check_textual("hash", input, columns)?;
    appended("hash", input, output, DataType::Utf8)
}

/// `hash`: a hex digest per row over the named columns, each encoded as a presence byte, then
/// for a present value its length as a little-endian u64 and its bytes (MH 4.9).
pub(crate) fn hash(
    columns: &[String],
    algo: Algo,
    output: &str,
    batch: &RecordBatch,
) -> Result<RecordBatch> {
    let schema = hash_schema(columns, output, &batch.schema())?;
    let mut texts_by_column = Vec::with_capacity(columns.len());
    for name in columns {
        let (at, _) = field("hash", &batch.schema(), name)?;
        texts_by_column.push(texts("hash", batch.column(at))?);
    }
    let mut builder = StringBuilder::with_capacity(batch.num_rows(), batch.num_rows() * 64);
    let mut encoded = Vec::new();
    for row in 0..batch.num_rows() {
        encoded.clear();
        for column in &texts_by_column {
            match &column[row] {
                None => encoded.push(0u8),
                Some(bytes) => {
                    encoded.push(1u8);
                    encoded.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                    encoded.extend_from_slice(bytes);
                }
            }
        }
        builder.append_value(algo.hex(&encoded));
    }
    let mut out = batch.columns().to_vec();
    out.push(Arc::new(builder.finish()));
    batch_of("hash", schema, out, batch.num_rows())
}

/// How `mask` hides a value.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MaskMode {
    /// Every character becomes `*`.
    Redact,
    /// All but the last `keep` characters become `*`.
    Partial(usize),
    /// The value becomes null; any type.
    Null,
    /// The value becomes its SHA-256, in hex.
    Hash,
}

pub(crate) fn mask_check(columns: &[String], mode: MaskMode, input: &Schema) -> Result<()> {
    for name in columns {
        let (_, f) = field("mask", input, name)?;
        let textual = matches!(f.data_type(), DataType::Utf8 | DataType::LargeUtf8);
        if mode != MaskMode::Null && !textual {
            return Err(plan(
                "mask",
                format!(
                    "column `{name}` is {}, and only a string column can be masked this way",
                    f.data_type()
                ),
            ));
        }
    }
    Ok(())
}

fn masked(mode: MaskMode, value: &str) -> String {
    match mode {
        MaskMode::Redact => "*".repeat(value.chars().count()),
        MaskMode::Partial(keep) => {
            let n = value.chars().count();
            let hidden = n.saturating_sub(keep);
            let mut out = "*".repeat(hidden);
            out.extend(value.chars().skip(hidden));
            out
        }
        MaskMode::Hash => Algo::Sha256.hex(value.as_bytes()),
        MaskMode::Null => String::new(),
    }
}

/// `mask`: the named columns hidden in place, their types unchanged.
pub(crate) fn mask(columns: &[String], mode: MaskMode, batch: &RecordBatch) -> Result<RecordBatch> {
    let schema = batch.schema();
    mask_check(columns, mode, &schema)?;
    let mut out = batch.columns().to_vec();
    for name in columns {
        let (at, f) = field("mask", &schema, name)?;
        let column = &out[at];
        out[at] = match (mode, f.data_type()) {
            (MaskMode::Null, dt) => new_null_array(dt, column.len()),
            (_, DataType::LargeUtf8) => Arc::new(
                column
                    .as_string::<i64>()
                    .iter()
                    .map(|v| v.map(|s| masked(mode, s)))
                    .collect::<LargeStringArray>(),
            ),
            _ => Arc::new(
                column
                    .as_string::<i32>()
                    .iter()
                    .map(|v| v.map(|s| masked(mode, s)))
                    .collect::<StringArray>(),
            ),
        };
    }
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|f| {
            let mut f = f.as_ref().clone();
            if mode == MaskMode::Null && columns.contains(f.name()) {
                f = f.with_nullable(true);
            }
            f
        })
        .collect();
    batch_of(
        "mask",
        Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone())),
        out,
        batch.num_rows(),
    )
}

pub(crate) fn mask_schema(columns: &[String], mode: MaskMode, input: &Schema) -> Result<SchemaRef> {
    mask_check(columns, mode, input)?;
    let fields: Vec<Field> = input
        .fields()
        .iter()
        .map(|f| {
            let f = f.as_ref().clone();
            if mode == MaskMode::Null && columns.contains(f.name()) {
                f.with_nullable(true)
            } else {
                f
            }
        })
        .collect();
    Ok(Arc::new(Schema::new_with_metadata(
        fields,
        input.metadata().clone(),
    )))
}

pub(crate) fn explode_schema(column: &str, input: &Schema) -> Result<SchemaRef> {
    let (at, f) = field("explode", input, column)?;
    let DataType::List(item) = f.data_type() else {
        return Err(plan(
            "explode",
            format!("column `{column}` is {}, not a list", f.data_type()),
        ));
    };
    let mut fields: Vec<Field> = input.fields().iter().map(|f| f.as_ref().clone()).collect();
    fields[at] = Field::new(column, item.data_type().clone(), true);
    Ok(Arc::new(Schema::new_with_metadata(
        fields,
        input.metadata().clone(),
    )))
}

/// `explode`: one row per list element, the other columns repeated; a null or empty list
/// gives one row with a null element, as Polars does.
pub(crate) fn explode(column: &str, batch: &RecordBatch) -> Result<RecordBatch> {
    let schema = explode_schema(column, &batch.schema())?;
    let (at, _) = field("explode", &batch.schema(), column)?;
    let list: &ListArray = batch.column(at).as_list::<i32>();
    let mut parents: Vec<u32> = Vec::with_capacity(list.values().len());
    let mut elements: Vec<Option<u32>> = Vec::with_capacity(list.values().len());
    let offsets = list.value_offsets();
    for row in 0..list.len() {
        let (start, end) = (offsets[row] as u32, offsets[row + 1] as u32);
        if list.is_null(row) || start == end {
            parents.push(row as u32);
            elements.push(None);
        } else {
            for element in start..end {
                parents.push(row as u32);
                elements.push(Some(element));
            }
        }
    }
    let parents = UInt32Array::from(parents);
    let elements = UInt32Array::from(elements);
    let mut columns = Vec::with_capacity(batch.num_columns());
    for (i, c) in batch.columns().iter().enumerate() {
        columns.push(if i == at {
            take::take(list.values().as_ref(), &elements, None).map_err(arrow("explode"))?
        } else {
            take::take(c.as_ref(), &parents, None).map_err(arrow("explode"))?
        });
    }
    batch_of("explode", schema, columns, parents.len())
}

pub(crate) fn concat_schema(columns: &[String], output: &str, input: &Schema) -> Result<SchemaRef> {
    check_textual("concat_str", input, columns)?;
    appended("concat_str", input, output, DataType::Utf8)
}

/// `concat_str`: the named columns as text, joined with `separator`; null when any is null.
pub(crate) fn concat_str(
    columns: &[String],
    separator: &str,
    output: &str,
    batch: &RecordBatch,
) -> Result<RecordBatch> {
    let schema = concat_schema(columns, output, &batch.schema())?;
    let mut parts = Vec::with_capacity(columns.len());
    for name in columns {
        let (at, _) = field("concat_str", &batch.schema(), name)?;
        parts.push(texts("concat_str", batch.column(at))?);
    }
    let mut builder = StringBuilder::new();
    for row in 0..batch.num_rows() {
        let mut value = Vec::new();
        let mut null = false;
        for (i, column) in parts.iter().enumerate() {
            match &column[row] {
                None => null = true,
                Some(bytes) => {
                    if i > 0 {
                        value.extend_from_slice(separator.as_bytes());
                    }
                    value.extend_from_slice(bytes);
                }
            }
        }
        if null {
            builder.append_null();
        } else {
            builder.append_value(String::from_utf8_lossy(&value));
        }
    }
    let mut out = batch.columns().to_vec();
    out.push(Arc::new(builder.finish()));
    batch_of("concat_str", schema, out, batch.num_rows())
}

/// A `date_trunc` unit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TruncUnit {
    /// January 1st.
    Year,
    /// The first of the month.
    Month,
    /// Midnight.
    Day,
    /// The hour.
    Hour,
    /// The minute.
    Minute,
    /// The second.
    Second,
}

impl TruncUnit {
    pub(crate) fn parse(s: &str) -> Result<TruncUnit> {
        Ok(match s {
            "year" => TruncUnit::Year,
            "month" => TruncUnit::Month,
            "day" => TruncUnit::Day,
            "hour" => TruncUnit::Hour,
            "minute" => TruncUnit::Minute,
            "second" => TruncUnit::Second,
            other => {
                return Err(plan(
                    "date_trunc",
                    format!(
                        "unknown unit `{other}` (use year, month, day, hour, minute or second)"
                    ),
                ));
            }
        })
    }
}

/// Days since 1970-01-01 of a civil date (Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of days since 1970-01-01 (Hinnant).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Truncate a day count to the year or the month.
fn trunc_days(days: i64, unit: TruncUnit) -> i64 {
    let (y, m, _) = civil_from_days(days);
    match unit {
        TruncUnit::Year => days_from_civil(y, 1, 1),
        TruncUnit::Month => days_from_civil(y, m, 1),
        TruncUnit::Day | TruncUnit::Hour | TruncUnit::Minute | TruncUnit::Second => days,
    }
}

/// Truncate a timestamp of `per_second` ticks per second; `None` when the result does not fit.
fn trunc_ticks(value: i64, per_second: i64, unit: TruncUnit) -> Option<i64> {
    let step = match unit {
        TruncUnit::Second => per_second,
        TruncUnit::Minute => per_second.checked_mul(60)?,
        TruncUnit::Hour => per_second.checked_mul(3_600)?,
        TruncUnit::Day | TruncUnit::Month | TruncUnit::Year => per_second.checked_mul(86_400)?,
    };
    let floored = value.div_euclid(step).checked_mul(step)?;
    match unit {
        TruncUnit::Year | TruncUnit::Month => {
            let days = value.div_euclid(step);
            trunc_days(days, unit).checked_mul(step)
        }
        TruncUnit::Day | TruncUnit::Hour | TruncUnit::Minute | TruncUnit::Second => Some(floored),
    }
}

pub(crate) fn trunc_schema(
    column: &str,
    output: Option<&str>,
    input: &Schema,
) -> Result<SchemaRef> {
    let (_, f) = field("date_trunc", input, column)?;
    match f.data_type() {
        DataType::Timestamp(_, _) | DataType::Date32 => {}
        other => {
            return Err(plan(
                "date_trunc",
                format!("column `{column}` is {other}, not a timestamp or a date32"),
            ));
        }
    }
    match output {
        None => Ok(Arc::new(input.clone())),
        Some(out) => appended("date_trunc", input, out, f.data_type().clone()),
    }
}

/// `date_trunc`: a timestamp or date truncated to `unit`, in UTC, in place or as a new column.
pub(crate) fn date_trunc(
    column: &str,
    unit: TruncUnit,
    output: Option<&str>,
    batch: &RecordBatch,
) -> Result<RecordBatch> {
    let schema = trunc_schema(column, output, &batch.schema())?;
    let input = batch.schema();
    let (at, f) = field("date_trunc", &input, column)?;
    let array = batch.column(at);
    macro_rules! ticks {
        ($t:ty, $per:expr, $tz:expr) => {{
            let typed = array.as_primitive::<$t>();
            let out: PrimitiveArray<$t> = typed
                .iter()
                .map(|v| v.and_then(|v| trunc_ticks(v, $per, unit)))
                .collect();
            Arc::new(out.with_timezone_opt($tz.clone())) as ArrayRef
        }};
    }
    let truncated: ArrayRef = match f.data_type() {
        DataType::Timestamp(TimeUnit::Second, tz) => ticks!(TimestampSecondType, 1, tz),
        DataType::Timestamp(TimeUnit::Millisecond, tz) => {
            ticks!(TimestampMillisecondType, 1_000, tz)
        }
        DataType::Timestamp(TimeUnit::Microsecond, tz) => {
            ticks!(TimestampMicrosecondType, 1_000_000, tz)
        }
        DataType::Timestamp(TimeUnit::Nanosecond, tz) => {
            ticks!(TimestampNanosecondType, 1_000_000_000, tz)
        }
        _ => {
            let typed: &Date32Array = array.as_primitive();
            Arc::new(
                typed
                    .iter()
                    .map(|v| v.and_then(|d| i32::try_from(trunc_days(i64::from(d), unit)).ok()))
                    .collect::<Date32Array>(),
            )
        }
    };
    let mut columns = batch.columns().to_vec();
    match output {
        None => columns[at] = truncated,
        Some(_) => columns.push(truncated),
    }
    batch_of("date_trunc", schema, columns, batch.num_rows())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trips_and_truncates() {
        for days in [-719_162i64, -1, 0, 1, 19_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        // 2024-03-15 is day 19797.
        assert_eq!(civil_from_days(19_797), (2024, 3, 15));
        assert_eq!(
            civil_from_days(trunc_days(19_797, TruncUnit::Month)),
            (2024, 3, 1)
        );
        assert_eq!(
            civil_from_days(trunc_days(19_797, TruncUnit::Year)),
            (2024, 1, 1)
        );
        assert_eq!(trunc_days(19_797, TruncUnit::Day), 19_797);
        // 1969-12-31T23:59:59 truncates to the day before the epoch, not to the epoch.
        assert_eq!(trunc_ticks(-1, 1, TruncUnit::Day), Some(-86_400));
        assert_eq!(trunc_ticks(3_725, 1, TruncUnit::Hour), Some(3_600));
        assert_eq!(trunc_ticks(3_725, 1, TruncUnit::Minute), Some(3_720));
        assert_eq!(
            trunc_ticks(3_725_500, 1_000, TruncUnit::Second),
            Some(3_725_000)
        );
        assert_eq!(
            trunc_ticks(i64::MIN + 1, 1_000_000_000, TruncUnit::Year),
            None
        );
    }

    #[test]
    fn masking_modes() {
        assert_eq!(masked(MaskMode::Redact, "abc\u{e9}"), "****");
        assert_eq!(masked(MaskMode::Partial(2), "abcdef"), "****ef");
        assert_eq!(masked(MaskMode::Partial(10), "ab"), "ab");
        assert_eq!(masked(MaskMode::Hash, "").len(), 64);
        assert_eq!(masked(MaskMode::Null, "x"), "");
    }

    #[test]
    fn algorithms_differ_and_are_hex() {
        let a = Algo::Sha256.hex(b"x");
        let b = Algo::Blake3.hex(b"x");
        assert_ne!(a, b);
        assert_eq!(
            a,
            "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
        );
        assert!(Algo::parse("hash", "md5").is_err());
        assert!(TruncUnit::parse("week").is_err());
    }
}
