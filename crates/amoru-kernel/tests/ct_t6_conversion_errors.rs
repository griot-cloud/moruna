//! CT-T6 conversion_errors: a nullable column is `HasNulls` at run time and `Plan` at plan time;
//! Boolean is `NotNumeric`; a strided tensor is `NotContiguous`; rank 3 is `Rank(3)`.
//! Proves CT-I5.

mod common;

use std::sync::Arc;

use amoru_kernel::{
    AmoruError, ConvertError, DType, ManagedTensor, Payload, PayloadKind, PayloadSpec,
    SourceSchema, Tier, TierPref,
};
use arrow::array::{ArrayRef, BooleanArray, Int32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::FakeAllocator;

fn batch(field: Field, column: ArrayRef) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![field]));
    RecordBatch::try_new(schema, vec![column]).expect("batch")
}

fn tensor_spec() -> PayloadSpec {
    PayloadSpec {
        kind: PayloadKind::Tensor,
        tier: TierPref::Any,
    }
}

#[test]
fn ct_t6_conversion_errors() {
    let alloc = FakeAllocator::new();

    // A nullable column with nulls: HasNulls at run time.
    let column: ArrayRef = Arc::new(Int32Array::from(vec![Some(1), None, Some(3)]));
    let nullable = batch(Field::new("c", DataType::Int32, true), column);
    let schema = SourceSchema::Table(nullable.schema());
    let payload = Payload::table(nullable).expect("table payload");
    let err = payload
        .as_tensor(Some("c"))
        .expect_err("nulls must be refused");
    assert!(
        matches!(err, AmoruError::Convert(ConvertError::HasNulls(ref name)) if name == "c"),
        "expected HasNulls, got {err}"
    );

    // and Plan at plan time, on the declared nullability alone (f.5).
    let err = tensor_spec()
        .check(&schema)
        .expect_err("a nullable field must be refused");
    assert!(
        matches!(err, AmoruError::Plan(_)),
        "expected Plan, got {err}"
    );
    assert!(err.to_string().contains("nullable"));

    // A nullable field that happens to hold no nulls still fails at plan time, deliberately.
    let column: ArrayRef = Arc::new(Int32Array::from(vec![Some(1), Some(2)]));
    let no_nulls = batch(Field::new("c", DataType::Int32, true), column);
    let err = tensor_spec()
        .check(&SourceSchema::Table(no_nulls.schema()))
        .expect_err("a nullable field must be refused even with no nulls");
    assert!(matches!(err, AmoruError::Plan(_)));

    // Boolean: NotNumeric, because an Arrow bitmap is not one byte per value (e.3).
    let column: ArrayRef = Arc::new(BooleanArray::from(vec![true, false]));
    let booleans = batch(Field::new("b", DataType::Boolean, false), column);
    let schema = SourceSchema::Table(booleans.schema());
    let payload = Payload::table(booleans).expect("table payload");
    let err = payload
        .as_tensor(Some("b"))
        .expect_err("a bitmap must be refused");
    assert!(
        matches!(err, AmoruError::Convert(ConvertError::NotNumeric(ref name)) if name == "b"),
        "expected NotNumeric, got {err}"
    );
    assert!(tensor_spec().check(&schema).is_err());

    // A strided tensor: NotContiguous.
    let buffer = alloc.buffer(8 * 4, Tier::Host);
    let contiguous = ManagedTensor::from_buffer(buffer, 0, DType::F32, vec![2, 4])
        .expect("tensor over a buffer");
    assert!(contiguous.is_contiguous());
    let strided = strided_tensor();
    assert!(!strided.is_contiguous());
    let err = Payload::tensor(strided)
        .expect("tensor payload")
        .as_column("c")
        .expect_err("a strided tensor must be refused");
    assert!(
        matches!(err, AmoruError::Convert(ConvertError::NotContiguous)),
        "expected NotContiguous, got {err}"
    );

    // Rank 3: Rank(3), at run time and at plan time.
    let buffer = alloc.buffer(2 * 3 * 4 * 4, Tier::Host);
    let cube = ManagedTensor::from_buffer(buffer, 0, DType::F32, vec![2, 3, 4])
        .expect("tensor over a buffer");
    let err = Payload::tensor(cube)
        .expect("tensor payload")
        .as_column("c")
        .expect_err("rank 3 must be refused");
    assert!(
        matches!(err, AmoruError::Convert(ConvertError::Rank(3))),
        "expected Rank(3), got {err}"
    );
    let table_spec = PayloadSpec {
        kind: PayloadKind::Table,
        tier: TierPref::Any,
    };
    let rank3 = SourceSchema::Tensor {
        dtype: DType::F32,
        shape: vec![2, 3, 4],
    };
    assert!(table_spec.check(&rank3).is_err());

    // A dtype with no Arrow form cannot become a column, at plan time or at run time.
    let bf16 = SourceSchema::Tensor {
        dtype: DType::BF16,
        shape: vec![4],
    };
    assert!(table_spec.check(&bf16).is_err());
    let buffer = alloc.buffer(4 * 2, Tier::Host);
    let tensor = ManagedTensor::from_buffer(buffer, 0, DType::BF16, vec![4]).expect("tensor");
    let err = Payload::tensor(tensor)
        .expect("tensor payload")
        .as_column("c")
        .expect_err("BF16 must be refused");
    assert!(
        matches!(err, AmoruError::Convert(ConvertError::NotNumeric(_))),
        "got {err}"
    );

    // A payload that is not resident cannot be converted: Staging, not Convert (h).
    let batch = common::mixed_batch(2);
    let segment = amoru_kernel::SegmentRef {
        segment: 0,
        offset: 4096,
        len: 16,
    };
    // SAFETY: test-only; the payload is tagged Disk to reach the non-resident path.
    let staged = unsafe { Payload::table_in(batch, Tier::Disk(segment)) };
    let err = staged
        .as_tensor(Some("i"))
        .expect_err("a staged payload has no bytes");
    assert!(
        matches!(err, AmoruError::Staging(_)),
        "expected Staging, got {err}"
    );

    // The mixed dtype case of `as_tensor(None)` (e.3).
    let ints: ArrayRef = Arc::new(Int32Array::from_iter_values(0..4));
    let floats: ArrayRef = Arc::new(arrow::array::Float64Array::from_iter_values(
        (0..4).map(f64::from),
    ));
    let schema = Arc::new(Schema::new(vec![
        Field::new("i", DataType::Int32, false),
        Field::new("f", DataType::Float64, false),
    ]));
    let mixed = RecordBatch::try_new(schema, vec![ints, floats]).expect("batch");
    let spec_schema = SourceSchema::Table(mixed.schema());
    let err = Payload::table(mixed)
        .expect("table payload")
        .as_tensor(None)
        .expect_err("mixed dtypes must be refused");
    assert!(
        matches!(err, AmoruError::Convert(ConvertError::MixedDTypes)),
        "got {err}"
    );
    assert!(tensor_spec().check(&spec_schema).is_err());

    // A conversion asked of the wrong payload kind is a plan error, not a silent success.
    let table = Payload::table(common::mixed_batch(2)).expect("table payload");
    let err = table
        .as_column("i")
        .expect_err("as_column on a table must be refused");
    assert!(matches!(err, AmoruError::Plan(_)), "got {err}");
    let buffer = alloc.buffer(16, Tier::Host);
    let tensor = ManagedTensor::from_buffer(buffer, 0, DType::F32, vec![4]).expect("a tensor");
    let payload = Payload::tensor(tensor).expect("tensor payload");
    let err = payload
        .as_tensor(None)
        .expect_err("as_tensor on a tensor must be refused");
    assert!(matches!(err, AmoruError::Plan(_)), "got {err}");
    let err = payload
        .as_tensor(Some("c"))
        .expect_err("as_tensor on a tensor must be refused");
    assert!(matches!(err, AmoruError::Plan(_)), "got {err}");

    // A column that is not in the batch is named.
    let table = Payload::table(common::mixed_batch(2)).expect("table payload");
    let err = table
        .as_tensor(Some("absent"))
        .expect_err("an unknown column must be refused");
    assert!(err.to_string().contains("absent"));

    // A batch with no columns at all has nothing to convert.
    let empty = arrow::record_batch::RecordBatch::new_empty(Arc::new(Schema::empty()));
    let err = Payload::table(empty)
        .expect("table payload")
        .as_tensor(None)
        .expect_err("a batch with no columns must be refused");
    assert!(
        matches!(err, AmoruError::Convert(ConvertError::NotNumeric(_))),
        "got {err}"
    );

    // A tensor whose bytes are on a device is not host addressable, so it has no column.
    let device = common::tagged_buffer(16, Tier::Device(amoru_kernel::DeviceId(0)));
    let tensor = ManagedTensor::from_buffer(device, 0, DType::F32, vec![4]).expect("a tensor");
    let err = Payload::tensor(tensor)
        .expect("tensor payload")
        .as_column("c")
        .expect_err("a device tensor has no host column");
    assert!(matches!(err, AmoruError::Staging(_)), "got {err}");

    // A nullable item type inside a FixedSizeList fails at plan time (f.5).
    let item = Arc::new(Field::new("item", DataType::Float32, true));
    let nullable_items = Arc::new(Schema::new(vec![Field::new(
        "v",
        DataType::FixedSizeList(item, 3),
        false,
    )]));
    let err = tensor_spec()
        .check(&SourceSchema::Table(nullable_items))
        .expect_err("nullable list items must be refused");
    assert!(matches!(err, AmoruError::Plan(_)), "got {err}");

    // A FixedSizeList of width 0 is not a numeric column (h).
    let item = Arc::new(Field::new("item", DataType::Float32, false));
    let zero_width = Arc::new(Schema::new(vec![Field::new(
        "v",
        DataType::FixedSizeList(item, 0),
        false,
    )]));
    assert!(
        tensor_spec()
            .check(&SourceSchema::Table(zero_width))
            .is_err()
    );

    // The specs that do pass (f.5).
    let either = PayloadSpec {
        kind: PayloadKind::Either,
        tier: TierPref::Host,
    };
    assert!(either.check(&rank3).is_ok());
    let tensor_schema = SourceSchema::Tensor {
        dtype: DType::F32,
        shape: vec![-1, 8],
    };
    assert!(tensor_spec().check(&tensor_schema).is_ok());
    assert!(
        table_spec
            .check(&SourceSchema::Table(contiguous_schema()))
            .is_ok()
    );
    assert!(
        tensor_spec()
            .check(&SourceSchema::Table(contiguous_schema()))
            .is_ok()
    );
}

fn contiguous_schema() -> arrow::datatypes::SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("a", DataType::Float32, false),
        Field::new("b", DataType::Float32, false),
    ]))
}

/// A tensor whose strides are explicit and not row-major, built through DLPack so the wrapper
/// sees exactly what a foreign producer would hand it.
fn strided_tensor() -> ManagedTensor {
    let data = Box::new(vec![0f32; 12]);
    let ptr = data.as_ptr().cast_mut().cast();
    let builder = dlpark::Builder::new(
        data,
        dlpark::metadata::CopiedSlice::new(vec![2i64, 3], vec![1i64, 2]),
    );
    // SAFETY: test-only; the boxed vector keeps the bytes alive until the deleter runs, and
    // the shape and strides describe elements inside it.
    let builder = unsafe { builder.data(ptr) }
        .dtype(dlpark::ffi::DLDataType::of::<f32>())
        .device(dlpark::ffi::DLDevice::CPU);
    let dlpack = builder
        .try_build::<dlpark::ffi::DLManagedTensorVersioned>()
        .expect("a strided DLPack tensor");
    ManagedTensor::from_dlpack(dlpack).expect("import a strided tensor")
}
