//! AD-T10 datafusion_bridge (AD-I6, S7): the `normalise` kernel runs as a DataFusion
//! `ScalarUDF` and inside Amoru, on the same input, with identical output.
//!
//! Tagged "(integration, closes in wave 1)" in 05-adapters section k. It needs no component
//! outside this wave, so it runs rather than being skipped.

#![cfg(feature = "datafusion")]
#![allow(clippy::result_large_err)]

mod common;

use std::sync::Arc;

use amoru_datafusion::datafusion_udf;
use amoru_kernel::arrow::array::{Array, ArrayRef, StringArray};
use amoru_kernel::arrow::datatypes::{DataType, Field, Schema};
use amoru_kernel::arrow::record_batch::RecordBatch;
use amoru_kernel::{Kernel, NoState, Payload};
use datafusion::prelude::{SessionContext, col};

use common::{Normalise, ROWS};

fn input_batch() -> RecordBatch {
    let column: ArrayRef = Arc::new(StringArray::from(ROWS.to_vec()));
    let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, true)]));
    RecordBatch::try_new(schema, vec![column]).expect("one column")
}

fn strings(array: &ArrayRef) -> Vec<Option<String>> {
    let values = array
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("a text column");
    (0..values.len())
        .map(|index| {
            if values.is_null(index) {
                None
            } else {
                Some(values.value(index).to_string())
            }
        })
        .collect()
}

async fn through_datafusion() -> Vec<Option<String>> {
    let ctx = SessionContext::new();
    ctx.register_udf(datafusion_udf(Normalise, "amoru_normalise"));
    let frame = ctx
        .read_batch(input_batch())
        .expect("a one batch table")
        .select(vec![
            datafusion::prelude::Expr::ScalarFunction(
                datafusion::logical_expr::expr::ScalarFunction::new_udf(
                    Arc::new(datafusion_udf(Normalise, "amoru_normalise")),
                    vec![col("text")],
                ),
            )
            .alias("text_normalised"),
        ])
        .expect("the projection plans");
    let batches = frame.collect().await.expect("the projection runs");
    let mut out = Vec::new();
    for batch in &batches {
        out.extend(strings(batch.column(0)));
    }
    out
}

fn through_amoru() -> Vec<Option<String>> {
    let mut state = NoState;
    let payload = Payload::table(input_batch()).expect("host buffers");
    let out = Normalise
        .apply(&mut state, payload)
        .expect("the kernel runs");
    let Payload::Table(batch, _) = out else {
        panic!("normalise returns a table");
    };
    strings(batch.column(0))
}

#[test]
fn ad_t10_datafusion_bridge() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let expected = common::expected();
    let inside_datafusion = runtime.block_on(through_datafusion());
    let inside_amoru = through_amoru();
    assert_eq!(
        inside_amoru, expected,
        "the kernel does not produce what normalise_text produces"
    );
    assert_eq!(
        inside_datafusion, inside_amoru,
        "the same kernel produced different output in DataFusion and in Amoru (S7)"
    );
}

/// AD-I6: the bridge asks the kernel for its output type rather than deciding one itself.
#[test]
fn ad_t10_datafusion_bridge_asks_the_kernel_for_its_return_type() {
    let udf = datafusion_udf(Normalise, "amoru_normalise");
    assert_eq!(
        udf.return_type(&[DataType::Utf8]).expect("a return type"),
        DataType::Utf8
    );
}

/// AD-I6: the bridge carries the kernel's answer or the kernel's refusal, and invents neither.
#[test]
fn ad_t10_datafusion_bridge_carries_refusals() {
    use datafusion::logical_expr::{ColumnarValue, ScalarFunctionArgs, ScalarUDFImpl};
    use std::sync::Arc as StdArc;

    // The kernel's own plan time refusal reaches DataFusion as a plan error.
    let normalise = datafusion_udf(Normalise, "amoru_normalise");
    assert!(
        normalise.return_type(&[]).is_err(),
        "a kernel that needs a column must refuse a call with none"
    );

    // A kernel that answers with a tensor cannot be a scalar function, at plan time or at run
    // time.
    let tensor = datafusion_udf(common::TensorOut, "amoru_tensor");
    assert!(tensor.return_type(&[DataType::Int64]).is_err());

    // A kernel that answers with no columns has nothing to return.
    let empty = datafusion_udf(common::NoColumns, "amoru_empty");
    assert!(empty.return_type(&[DataType::Int64]).is_err());

    let column: ArrayRef =
        StdArc::new(amoru_kernel::arrow::array::Int64Array::from(vec![1_i64, 2]));
    let args = |name: &str| ScalarFunctionArgs {
        args: vec![ColumnarValue::Array(StdArc::clone(&column))],
        arg_fields: vec![StdArc::new(Field::new(name, DataType::Int64, false))],
        number_rows: 2,
        return_field: StdArc::new(Field::new("out", DataType::Int64, true)),
        config_options: StdArc::new(datafusion::config::ConfigOptions::default()),
    };
    let tensor_impl = amoru_datafusion::KernelUdf::new(common::TensorOut, "amoru_tensor");
    assert!(tensor_impl.invoke_with_args(args("n")).is_err());
    let empty_impl = amoru_datafusion::KernelUdf::new(common::NoColumns, "amoru_empty");
    assert!(empty_impl.invoke_with_args(args("n")).is_err());

    // Two bridged kernels are the same function when they carry the same name.
    assert_eq!(
        amoru_datafusion::KernelUdf::new(Normalise, "a"),
        amoru_datafusion::KernelUdf::new(Normalise, "a")
    );
    assert_ne!(
        amoru_datafusion::KernelUdf::new(Normalise, "a"),
        amoru_datafusion::KernelUdf::new(Normalise, "b")
    );
    assert!(
        format!("{:?}", amoru_datafusion::KernelUdf::new(Normalise, "a")).contains("KernelUdf")
    );
    assert_eq!(normalise.name(), "amoru_normalise");
}
