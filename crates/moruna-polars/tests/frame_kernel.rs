//! CK-T8, the Rust half (MH 4.9): a function from a Polars frame to a Polars frame is a
//! kernel through `moruna-polars`, its fixed-width columns cross by pointer, the view and large
//! types Polars keeps come back as the plain ones, and its declaration is what it declares.

#![cfg(feature = "polars")]
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, AsArray, Int64Array, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int64Type, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::declare::{ColumnDecl, Declared, SchemaDecl};
use moruna_kernel::{Kernel, KernelHints, KernelKind, NoState, Payload, SourceSchema};
use moruna_polars::{batch_from_dataframe, dataframe_from_batch, frame::plain_type, frame_kernel};
use polars::prelude::{DataFrame, IntoColumn, PolarsError};

fn batch() -> RecordBatch {
    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("n", DataType::Int64, true),
            Field::new("s", DataType::Utf8, true),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![Some(1), None, Some(3)])) as ArrayRef,
            Arc::new(StringArray::from(vec![Some("a"), Some("b"), None])),
        ],
    )
    .expect("batch")
}

#[test]
fn a_fixed_width_column_crosses_both_ways_by_pointer() {
    let input = batch();
    let before = input.column(0).to_data().buffers()[0].as_ptr();
    let frame = dataframe_from_batch(&input).expect("into polars");
    assert_eq!(frame.height(), 3);
    let back = batch_from_dataframe(&frame).expect("out of polars");
    assert_eq!(
        back.column(0).to_data().buffers()[0].as_ptr(),
        before,
        "int64 values cross the C data interface without a copy"
    );
    assert_eq!(back.schema().field(1).data_type(), &DataType::Utf8);
    let strings: Vec<Option<&str>> = back.column(1).as_string::<i32>().iter().collect();
    assert_eq!(strings, vec![Some("a"), Some("b"), None]);
}

#[test]
fn a_frame_function_is_a_kernel() {
    let declared = Declared {
        input: Some(SchemaDecl::Subset(vec![ColumnDecl::new(
            "n",
            DataType::Int64,
        )])),
        output: Some(SchemaDecl::Relative {
            adds: vec![ColumnDecl::new("twice", DataType::Int64)],
            drops: Vec::new(),
            changes: Vec::new(),
        }),
    };
    let kernel = frame_kernel("twice", |mut frame: DataFrame| {
        let n = frame.column("n")?.as_materialized_series().clone();
        let mut twice = &n * 2;
        twice.rename("twice".into());
        frame.with_column(twice.into_column())?;
        Ok(frame)
    })
    .with_declared(declared.clone())
    .with_hints(KernelHints {
        expected_amplification: Some(1.0),
        ..Default::default()
    });
    assert_eq!(kernel.declared(), declared);
    assert_eq!(kernel.hints().expected_amplification, Some(1.0));
    assert!(matches!(kernel.kind(), KernelKind::Stateless));
    let schema = SourceSchema::Table(batch().schema());
    assert!(matches!(
        kernel.output_schema(&schema),
        Ok(SourceSchema::Table(_))
    ));
    assert!(
        kernel
            .init(&moruna_kernel::InitCtx {
                instance: 0,
                device: None,
                alloc: Arc::new(moruna_testkit_free::NoAlloc),
            })
            .is_ok()
    );
    let out = kernel
        .apply(&mut NoState, Payload::table(batch()).expect("payload"))
        .expect("apply");
    let Payload::Table(out, _) = out else {
        panic!("a table")
    };
    let expected = declared
        .output
        .as_ref()
        .expect("output")
        .resolve(&batch().schema())
        .expect("resolve");
    assert!(
        expected.compare(&out.schema()).is_empty(),
        "{:?}",
        out.schema()
    );
    let twice: Vec<Option<i64>> = out
        .column_by_name("twice")
        .expect("twice")
        .as_primitive::<Int64Type>()
        .iter()
        .collect();
    assert_eq!(twice, vec![Some(2), None, Some(6)]);
    // The fingerprint follows the name and the declaration.
    let other = frame_kernel("twice", Ok);
    assert_ne!(kernel.fingerprint(), other.fingerprint());
}

#[test]
fn a_polars_error_is_a_kernel_error() {
    let kernel = frame_kernel("fails", |_| Err(PolarsError::ComputeError("no".into())));
    let err = kernel
        .apply(&mut NoState, Payload::table(batch()).expect("payload"))
        .expect_err("fails");
    assert!(err.to_string().contains("polars: "), "{err}");
}

#[test]
fn plain_types() {
    let item = Arc::new(Field::new("item", DataType::Utf8View, true));
    assert_eq!(plain_type(&DataType::LargeUtf8), DataType::Utf8);
    assert_eq!(plain_type(&DataType::BinaryView), DataType::Binary);
    assert_eq!(
        plain_type(&DataType::LargeList(item)),
        DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
    );
    assert_eq!(plain_type(&DataType::Int8), DataType::Int8);
}

/// An allocator the test never allocates from; `init` only stores it.
mod moruna_testkit_free {
    pub struct NoAlloc;
    impl moruna_kernel::Allocator for NoAlloc {
        fn alloc(
            &self,
            _bytes: usize,
            _tier: moruna_kernel::Tier,
        ) -> moruna_kernel::Result<moruna_kernel::Buffer> {
            Err(moruna_kernel::MorunaError::Unsupported("test"))
        }
        fn page_bytes(&self) -> usize {
            4096
        }
        fn is_pinned(&self) -> bool {
            false
        }
        fn stats(&self) -> moruna_kernel::AllocStats {
            moruna_kernel::AllocStats::default()
        }
    }
}
