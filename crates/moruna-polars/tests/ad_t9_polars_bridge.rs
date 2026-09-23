//! AD-T9 polars_bridge (AD-I6, S7): the `normalise` kernel runs as a Polars expression plugin
//! and inside Moruna, on the same input, with identical output.
//!
//! Tagged "(integration, closes in wave 1)" in 05-adapters section k. It needs no component
//! outside this wave, so it runs rather than being skipped.

#![cfg(feature = "polars")]
#![allow(clippy::result_large_err)]

mod common;

use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{Kernel, NoState, Payload};
use moruna_polars::polars_plugin;
use polars::prelude::{IntoSeries, NewChunkedArray, PlSmallStr, Series, StringChunked};

use common::{Normalise, ROWS};

/// The same rows, as Polars sees them.
fn input_series() -> Series {
    let values: Vec<Option<&str>> = ROWS.to_vec();
    StringChunked::from_iter_options(PlSmallStr::from_static("text"), values.into_iter())
        .into_series()
}

/// The same rows, as Moruna sees them.
fn input_payload() -> Payload {
    let column: ArrayRef = Arc::new(StringArray::from(ROWS.to_vec()));
    let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, true)]));
    let batch = RecordBatch::try_new(schema, vec![column]).expect("one column");
    Payload::table(batch).expect("host buffers")
}

fn through_polars() -> Vec<Option<String>> {
    let plugin = polars_plugin(Normalise);
    let out = plugin(&[input_series()]).expect("the plugin runs");
    let text = out.str().expect("the plugin returns text");
    (0..out.len())
        .map(|index| text.get(index).map(str::to_string))
        .collect()
}

fn through_moruna() -> Vec<Option<String>> {
    let mut state = NoState;
    let out = Normalise
        .apply(&mut state, input_payload())
        .expect("the kernel runs");
    let Payload::Table(batch, _) = out else {
        panic!("normalise returns a table");
    };
    let column = batch.column(0);
    let values = column
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

#[test]
fn ad_t9_polars_bridge() {
    let expected = common::expected();
    let inside_polars = through_polars();
    let inside_moruna = through_moruna();
    assert_eq!(
        inside_moruna, expected,
        "the kernel does not produce what normalise_text produces"
    );
    assert_eq!(
        inside_polars, inside_moruna,
        "the same kernel produced different output in Polars and in Moruna (S7)"
    );
}

/// AD-I6: the bridge carries the kernel's own column name out, so nothing about the kernel is
/// rewritten on the way through Polars.
#[test]
fn ad_t9_polars_bridge_keeps_the_kernels_column_name() {
    let plugin = polars_plugin(Normalise);
    let out = plugin(&[input_series()]).expect("the plugin runs");
    assert_eq!(out.name().as_str(), "text_normalised");
}

/// AD-I6: the bridge carries the kernel's answer or the kernel's refusal, and invents neither.
#[test]
fn ad_t9_polars_bridge_carries_refusals() {
    use polars::prelude::{NamedFrom, PolarsError};

    let plugin = polars_plugin(Normalise);
    assert!(
        matches!(plugin(&[]), Err(PolarsError::ComputeError(_))),
        "a plugin with no input series has nothing to give the kernel"
    );

    let numbers = Series::new(PlSmallStr::from_static("n"), &[1_i64, 2]);
    assert!(
        matches!(
            moruna_polars::run_kernel(&common::TensorOut, std::slice::from_ref(&numbers)),
            Err(PolarsError::ComputeError(_))
        ),
        "a tensor cannot be a Series"
    );
    assert!(
        matches!(
            moruna_polars::run_kernel(&common::NoColumns, std::slice::from_ref(&numbers)),
            Err(PolarsError::ComputeError(_))
        ),
        "a batch with no columns has no Series to return"
    );

    let refused = moruna_polars::run_kernel(&common::Failing, std::slice::from_ref(&numbers))
        .expect_err("the kernel refuses");
    assert!(
        refused.to_string().contains("this kernel always refuses"),
        "the bridge must carry the kernel's own words: {refused}"
    );
}
