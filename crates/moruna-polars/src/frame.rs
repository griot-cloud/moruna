//! Polars as syntax for a Rust kernel: a function from a `DataFrame` to a `DataFrame` as an
//! Moruna [`Kernel`] (05 f.9, MH 4.9).
//!
//! The batch crosses into Polars column by column through the Arrow C Data Interface
//! ([`crate::ffi`]), so fixed-width columns are handed over by pointer, and the result crosses
//! back the same way. Polars keeps strings as views and lists as large lists; the columns that
//! come back in those spellings are cast to the plain Arrow types a declaration names
//! (`string`, `binary`, `list`), which is the one place this path copies, and only those columns.

use std::sync::Arc;

use moruna_kernel::arrow::array::ArrayRef;
use moruna_kernel::arrow::compute::kernels::cast;
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::record_batch::{RecordBatch, RecordBatchOptions};
use moruna_kernel::declare::Declared;
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError, NoState,
    Payload, PayloadKind, PayloadSpec, SourceSchema, TierPref,
};
use polars::prelude::{Column, DataFrame, IntoColumn, PlSmallStr, PolarsError, PolarsResult};

use crate::ffi;

/// A record batch as a Polars frame, by pointer (the C Data Interface, per column).
pub fn dataframe_from_batch(batch: &RecordBatch) -> PolarsResult<DataFrame> {
    let mut columns: Vec<Column> = Vec::with_capacity(batch.num_columns());
    for (field, array) in batch.schema().fields().iter().zip(batch.columns()) {
        let series = ffi::arrow_to_series(PlSmallStr::from_str(field.name()), array)?;
        columns.push(series.into_column());
    }
    DataFrame::new(batch.num_rows(), columns)
}

/// The type a Polars result column is handed back as: views and large variants as the plain
/// types, recursively through lists.
pub fn plain_type(dt: &DataType) -> DataType {
    match dt {
        DataType::Utf8View | DataType::LargeUtf8 => DataType::Utf8,
        DataType::BinaryView | DataType::LargeBinary => DataType::Binary,
        DataType::LargeList(item) | DataType::List(item) | DataType::ListView(item) => {
            DataType::List(Arc::new(Field::new(
                item.name(),
                plain_type(item.data_type()),
                item.is_nullable(),
            )))
        }
        other => other.clone(),
    }
}

/// A Polars frame as a record batch, by pointer, with the plain types of [`plain_type`].
pub fn batch_from_dataframe(frame: &DataFrame) -> PolarsResult<RecordBatch> {
    let mut fields = Vec::with_capacity(frame.width());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(frame.width());
    for column in frame.columns() {
        let series = column.as_materialized_series();
        let array = ffi::series_to_arrow(series)?;
        let wanted = plain_type(array.data_type());
        let array = if &wanted == array.data_type() {
            array
        } else {
            cast::cast(&array, &wanted)
                .map_err(|e| PolarsError::ComputeError(e.to_string().into()))?
        };
        fields.push(Field::new(series.name().as_str(), wanted, true));
        arrays.push(array);
    }
    RecordBatch::try_new_with_options(
        Arc::new(Schema::new(fields)),
        arrays,
        &RecordBatchOptions::new().with_row_count(Some(frame.height())),
    )
    .map_err(|e| PolarsError::ComputeError(e.to_string().into()))
}

/// A function over Polars frames as a stateless kernel.
pub struct FrameKernel<F> {
    name: String,
    f: F,
    declared: Declared,
    hints: KernelHints,
}

/// Wrap `f` as a kernel named `name` (its fingerprint is `Fingerprint::compute` over
/// `"polars:" + name`, contracts e.6; a Rust author changes the name when the function changes
/// meaning, as for any Rust kernel).
pub fn frame_kernel<F>(name: &str, f: F) -> FrameKernel<F>
where
    F: Fn(DataFrame) -> PolarsResult<DataFrame> + Send + Sync + 'static,
{
    FrameKernel {
        name: name.to_string(),
        f,
        declared: Declared::default(),
        hints: KernelHints::default(),
    }
}

impl<F> FrameKernel<F> {
    /// Declare the kernel's schemas, which makes it checkable (MH 4.9).
    pub fn with_declared(mut self, declared: Declared) -> Self {
        self.declared = declared;
        self
    }

    /// Set the hints the controller reads before the probe.
    pub fn with_hints(mut self, hints: KernelHints) -> Self {
        self.hints = hints;
        self
    }
}

fn to_moruna(e: PolarsError) -> MorunaError {
    MorunaError::Kernel {
        stage: moruna_kernel::StageId::MAX,
        seq: moruna_kernel::Seq::MAX,
        msg: format!("polars: {e}"),
        features: None,
    }
}

impl<F> Kernel for FrameKernel<F>
where
    F: Fn(DataFrame) -> PolarsResult<DataFrame> + Send + Sync + 'static,
{
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute(
            &format!("polars:{}", self.name),
            self.declared.canonical_json().as_bytes(),
        )
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        self.hints.clone()
    }

    fn declared(&self) -> Declared {
        self.declared.clone()
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    /// Opaque, like a Python kernel: the frame function says nothing about its output until it
    /// runs, so the answer is the input's schema and the sink takes the real one from the first
    /// payload (12 f.1). The declaration is what `moruna check` compares.
    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        self.accepts().check(input)?;
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        let Payload::Table(batch, _) = &input else {
            return Err(MorunaError::Plan(
                "a Polars frame kernel takes a table, and was given a tensor".into(),
            ));
        };
        let frame = dataframe_from_batch(batch).map_err(to_moruna)?;
        let out = (self.f)(frame).map_err(to_moruna)?;
        let batch = batch_from_dataframe(&out).map_err(to_moruna)?;
        drop(input);
        Payload::table(batch)
    }
}
