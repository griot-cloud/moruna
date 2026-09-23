//! The expression plugin a kernel becomes (f.5).

use std::sync::Arc;

use moruna_kernel::arrow::array::ArrayRef;
use moruna_kernel::arrow::datatypes::{Field, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{MorunaError, Kernel, NoState, Payload};
use polars::prelude::{PlSmallStr, PolarsError, PolarsResult, Series};

use crate::ffi;

/// Host `kernel` inside Polars as an expression plugin (d.1).
///
/// The returned closure is what a Polars plugin entry point calls: the input series in, one
/// output series out.
pub fn polars_plugin<K: Kernel>(kernel: K) -> impl Fn(&[Series]) -> PolarsResult<Series> {
    move |inputs: &[Series]| run_kernel(&kernel, inputs)
}

/// One invocation of the bridge: series in, the kernel's first output column out.
///
/// The kernel arrives as a trait object rather than as a type parameter, so there is one copy of
/// the bridge for every kernel rather than one per kernel type: one thing to read and one thing
/// to test.
pub fn run_kernel(kernel: &dyn Kernel, inputs: &[Series]) -> PolarsResult<Series> {
    if inputs.is_empty() {
        return Err(PolarsError::ComputeError(
            "an Moruna kernel plugin needs at least one input series".into(),
        ));
    }
    let mut fields = Vec::with_capacity(inputs.len());
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(inputs.len());
    for series in inputs {
        let array = ffi::series_to_arrow(series)?;
        fields.push(Field::new(
            series.name().to_string(),
            array.data_type().clone(),
            array.null_count() > 0,
        ));
        columns.push(array);
    }
    let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
        .map_err(|e| PolarsError::ComputeError(e.to_string().into()))?;
    let payload = Payload::table(batch).map_err(moruna)?;
    let mut state = NoState;
    let output = kernel.apply(&mut state, payload).map_err(moruna)?;
    let Payload::Table(out, _) = output else {
        return Err(PolarsError::ComputeError(
            "an Moruna kernel returned a tensor, which a Polars expression cannot carry".into(),
        ));
    };
    let Some(column) = out.columns().first() else {
        return Err(PolarsError::ComputeError(
            "an Moruna kernel returned no columns".into(),
        ));
    };
    let name = out
        .schema()
        .fields()
        .first()
        .map(|field| PlSmallStr::from_str(field.name()))
        .unwrap_or_else(|| PlSmallStr::from_static("moruna"));
    ffi::arrow_to_series(name, column)
}

fn moruna(error: MorunaError) -> PolarsError {
    PolarsError::ComputeError(error.to_string().into())
}
