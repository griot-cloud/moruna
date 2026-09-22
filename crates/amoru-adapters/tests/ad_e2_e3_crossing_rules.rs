//! The export and import rules (e.2, e.3) and the tensor half of AD-I2.
//!
//! AD-T1 and AD-T2 prove the table rows of those tables. These are the rest: a tensor crosses by
//! DLPack and comes back the same way, a returned tensor that already points into the arena is
//! not copied and one that does not is copied once, and every row of e.3 that is an error says
//! what came back.

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

mod common;

use std::sync::Arc;

use amoru_adapters::python::cross::export;
use amoru_adapters::python::tensor_obj::PyTensor;
use amoru_kernel::{Allocator, AmoruError, DType, Kernel, ManagedTensor, NoState, Payload, Tier};
use amoru_testkit::FakeAllocator;
use pyo3::prelude::*;

use common::CountingAllocator;

/// A tensor of `i64`s whose bytes the arena owns.
fn arena_tensor(alloc: &FakeAllocator, values: &[i64]) -> Payload {
    let mut buffer = alloc.buffer(values.len() * 8, alloc.host_tier());
    for (index, value) in values.iter().enumerate() {
        buffer[index * 8..(index + 1) * 8].copy_from_slice(&value.to_le_bytes());
    }
    let tensor = ManagedTensor::from_buffer(buffer, 0, DType::I64, vec![values.len() as i64])
        .expect("a tensor over an arena buffer");
    Payload::tensor(tensor).expect("a resident tensor")
}

fn tensor_values(payload: &Payload) -> Vec<i64> {
    let Payload::Tensor(tensor, _) = payload else {
        panic!("expected a tensor payload");
    };
    let bytes = amoru_adapters::python::cross::host_tensor_bytes(tensor).expect("host bytes");
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|chunk| i64::from_le_bytes(*chunk))
        .collect()
}

const TENSOR_KERNELS: &str = r#"
import numpy

def passthrough(tensor):
    return numpy.from_dlpack(tensor)

def doubled(tensor):
    return numpy.from_dlpack(tensor) * 2
"#;

/// e.2 and e.3: a tensor crosses by DLPack, and a kernel that hands the same bytes back is not
/// copied at the boundary (AD-I2).
#[test]
fn a_tensor_that_comes_back_unchanged_is_not_copied() {
    let counting = CountingAllocator::new(FakeAllocator::new());
    let alloc: Arc<dyn Allocator> = counting.clone();
    let kernel = common::stateless_kernel(
        TENSOR_KERNELS,
        "passthrough",
        "ad_e2_passthrough",
        Arc::clone(&alloc),
    );
    let input = arena_tensor(counting.fake(), &[1, 2, 3, 4, 5]);
    let mut state = NoState;
    let output = kernel.apply(&mut state, input).expect("the kernel runs");
    assert_eq!(counting.boundary_copies(), 0, "arena bytes were copied");
    assert_eq!(output.tier(), Tier::Host);
    assert_eq!(tensor_values(&output), vec![1, 2, 3, 4, 5]);
}

/// AD-I2: a returned tensor the arena does not own is copied into it exactly once.
#[test]
fn a_new_tensor_is_copied_into_the_arena_once() {
    let counting = CountingAllocator::new(FakeAllocator::new());
    let alloc: Arc<dyn Allocator> = counting.clone();
    let kernel = common::stateless_kernel(
        TENSOR_KERNELS,
        "doubled",
        "ad_e2_doubled",
        Arc::clone(&alloc),
    );
    let input = arena_tensor(counting.fake(), &[1, 2, 3, 4, 5]);
    let mut state = NoState;
    let output = kernel.apply(&mut state, input).expect("the kernel runs");
    assert_eq!(counting.boundary_copies(), 1);
    assert_eq!(counting.boundary_bytes(), 5 * 8);
    assert_eq!(kernel.stats().boundary_copies, 1);
    assert_eq!(tensor_values(&output), vec![2, 4, 6, 8, 10]);
}

/// e.2: the object handed to Python carries the DLPack protocol, reports the tier's device, and
/// hands its bytes over exactly once.
#[test]
fn the_tensor_object_speaks_dlpack_once() {
    let fake = FakeAllocator::new();
    let payload = arena_tensor(&fake, &[9, 9]);
    Python::attach(|py| {
        let object = export(py, payload).expect("a tensor exports");
        let device: (i32, i32) = object
            .call_method0("__dlpack_device__")
            .expect("__dlpack_device__")
            .extract()
            .expect("a pair of ints");
        assert_eq!(device, (1, 0), "a Host tensor is DLPack's CPU device");
        assert!(
            object
                .repr()
                .expect("a repr")
                .to_string()
                .contains("amoru.Tensor")
        );
        let first = object.call_method0("__dlpack__");
        assert!(first.is_ok(), "the first __dlpack__ hands the tensor over");
        let second = object.call_method0("__dlpack__");
        assert!(
            second.is_err(),
            "a tensor can only be handed over once; the second call must raise"
        );
    });
}

/// e.2: a pinned tensor reports DLPack's pinned host device, and a tensor with no resident bytes
/// cannot cross at all (CT-I11: every tier is named).
#[test]
fn the_tensor_object_names_every_tier() {
    let pinned = FakeAllocator::new().pinned(true);
    let payload = arena_tensor(&pinned, &[1]);
    Python::attach(|py| {
        let object = export(py, payload).expect("a pinned tensor exports");
        let device: (i32, i32) = object
            .call_method0("__dlpack_device__")
            .expect("__dlpack_device__")
            .extract()
            .expect("a pair of ints");
        assert_eq!(
            device,
            (3, 0),
            "a PinnedHost tensor is DLPack's CUDA host device"
        );
    });

    let fake = FakeAllocator::new();
    let Payload::Tensor(tensor, _) = arena_tensor(&fake, &[1]) else {
        panic!("a tensor");
    };
    // A tier with no resident bytes cannot cross, and the reserved remote tier is refused rather
    // than wildcarded (CT-I11).
    assert!(matches!(
        PyTensor::new(
            tensor,
            Tier::Disk(amoru_kernel::SegmentRef {
                segment: 0,
                offset: 0,
                len: 8,
            })
        ),
        Err(AmoruError::Staging(_))
    ));
    let Payload::Tensor(tensor, _) = arena_tensor(&fake, &[1]) else {
        panic!("a tensor");
    };
    assert!(matches!(
        PyTensor::new(
            tensor,
            Tier::Remote(
                amoru_kernel::LOCAL_NODE,
                amoru_kernel::RemoteRef {
                    addr: 0,
                    rkey: 0,
                    len: 8,
                },
            )
        ),
        Err(AmoruError::Unsupported("rdma"))
    ));
    let Payload::Tensor(tensor, _) = arena_tensor(&fake, &[1]) else {
        panic!("a tensor");
    };
    assert!(PyTensor::new(tensor, Tier::Device(amoru_kernel::DeviceId(1))).is_ok());
}

const RETURNS: &str = r#"
import pyarrow

def returns_none(batch):
    return None

def returns_an_int(batch):
    return 7

def returns_a_two_chunk_table(batch):
    return pyarrow.Table.from_batches([batch, batch])

def returns_a_one_chunk_table(batch):
    return pyarrow.Table.from_batches([batch])
"#;

fn apply_returning(attribute: &str, module: &str) -> amoru_kernel::Result<Payload> {
    let fake = FakeAllocator::new();
    let alloc: Arc<dyn Allocator> = Arc::new(fake.clone());
    let kernel = common::stateless_kernel(RETURNS, attribute, module, alloc);
    let batch = common::arena_i64_batch(&fake, &[1, 2]);
    let input = Payload::table(batch).expect("the batch is arena owned");
    let mut state = NoState;
    kernel.apply(&mut state, input)
}

/// e.3: every row of the import table that is an error names what came back.
#[test]
fn the_import_rules_name_what_came_back() {
    let none = apply_returning("returns_none", "ad_e3_none").expect_err("None is an error");
    let AmoruError::Kernel { msg, .. } = &none else {
        panic!("not a Kernel error: {none:?}");
    };
    assert_eq!(msg, "kernel returned None");

    let integer = apply_returning("returns_an_int", "ad_e3_int").expect_err("an int is an error");
    let AmoruError::Kernel { msg, .. } = &integer else {
        panic!("not a Kernel error: {integer:?}");
    };
    assert!(msg.contains("int"), "the message must name the type: {msg}");

    let chunks = apply_returning("returns_a_two_chunk_table", "ad_e3_chunks")
        .expect_err("more than one chunk is an error");
    let AmoruError::Kernel { msg, .. } = &chunks else {
        panic!("not a Kernel error: {chunks:?}");
    };
    assert!(
        msg.starts_with("return a RecordBatch or a single-chunk Table"),
        "the message must say what to return instead: {msg}"
    );

    let one_chunk = apply_returning("returns_a_one_chunk_table", "ad_e3_one_chunk")
        .expect("a single chunk table is a record batch");
    assert_eq!(one_chunk.rows(), 2);
}

const AWKWARD_RETURNS: &str = r#"
import numpy
import pyarrow

def nulls_and_nesting(batch):
    text = pyarrow.array(["a", None, "c"], type=pyarrow.string())
    lists = pyarrow.array([[1, 2], None, [3]], type=pyarrow.list_(pyarrow.int32()))
    return pyarrow.record_batch([text, lists], names=["text", "lists"])

def a_stride(batch):
    return numpy.arange(10)[::2]

def nothing_at_all(batch):
    return numpy.zeros(0, dtype=numpy.int64)

class BrokenExport:
    def __arrow_c_array__(self, requested_schema=None):
        raise RuntimeError("no capsules here")

def broken_export(batch):
    return BrokenExport()
"#;

fn apply_awkward(attribute: &str, module: &str, rows: &[i64]) -> amoru_kernel::Result<Payload> {
    let counting = CountingAllocator::new(FakeAllocator::new());
    let alloc: Arc<dyn Allocator> = counting.clone();
    let kernel = common::stateless_kernel(AWKWARD_RETURNS, attribute, module, alloc);
    let batch = common::arena_i64_batch(counting.fake(), rows);
    let input = Payload::table(batch).expect("the batch is arena owned");
    let mut state = NoState;
    kernel.apply(&mut state, input)
}

/// f.3: the boundary copy walks the whole array, null bitmaps and child arrays included, so a
/// batch with nulls and a list column lands in the arena intact.
#[test]
fn a_batch_with_nulls_and_a_nested_column_is_copied_whole() {
    let counting = CountingAllocator::new(FakeAllocator::new());
    let alloc: Arc<dyn Allocator> = counting.clone();
    let kernel = common::stateless_kernel(
        AWKWARD_RETURNS,
        "nulls_and_nesting",
        "ad_f3_nesting",
        Arc::clone(&alloc),
    );
    let batch = common::arena_i64_batch(counting.fake(), &[1, 2, 3]);
    let input = Payload::table(batch).expect("the batch is arena owned");
    let mut state = NoState;
    let output = kernel.apply(&mut state, input).expect("the kernel runs");
    assert_eq!(counting.boundary_copies(), 1);
    let Payload::Table(out, tier) = &output else {
        panic!("a table");
    };
    assert_eq!(*tier, Tier::Host);
    assert_eq!(out.num_rows(), 3);
    assert_eq!(out.column(0).null_count(), 1, "the null bitmap crossed");
    assert_eq!(out.column(1).null_count(), 1, "the list's nulls crossed");
    // Every buffer of the copy, children included, is now the arena's.
    for column in out.columns() {
        let data = column.to_data();
        assert!(
            data.buffers()
                .iter()
                .all(|buffer| counting.contains(buffer.as_ptr())),
            "a buffer of the copy is still outside the arena"
        );
    }
}

/// AD-I2: a device tensor is handed on, never copied.
#[test]
fn a_device_tensor_is_never_copied() {
    let counting = CountingAllocator::new(FakeAllocator::new());
    let alloc: Arc<dyn Allocator> = counting.clone();
    let device = Tier::Device(amoru_kernel::DeviceId(0));
    let buffer = counting.fake().buffer(64, device);
    let tensor = ManagedTensor::from_buffer(buffer, 0, DType::I64, vec![8])
        .expect("a tensor over device memory");
    let landed = amoru_adapters::python::copy::land(
        amoru_adapters::python::cross::Imported::Tensor(tensor),
        &alloc,
    )
    .expect("a device tensor lands without a copy");
    assert_eq!(landed.copied_bytes, 0);
    assert_eq!(landed.payload.tier(), device);
    assert_eq!(counting.boundary_copies(), 0);
}

/// f.3: the adapter copies bytes, it does not repack them, so a tensor whose strides are not row
/// major is an error rather than a silent gather.
#[test]
fn a_tensor_with_strides_cannot_be_copied() {
    let error = apply_awkward("a_stride", "ad_f3_stride", &[1, 2])
        .expect_err("a strided tensor cannot be copied");
    assert!(
        matches!(error, AmoruError::Convert(_)),
        "expected a Convert error, got {error:?}"
    );
}

/// f.3: a tensor with no elements crosses, and nothing is counted as copied.
#[test]
fn an_empty_tensor_crosses() {
    let output =
        apply_awkward("nothing_at_all", "ad_f3_empty", &[1, 2]).expect("an empty tensor crosses");
    assert_eq!(output.rows(), 0);
    assert_eq!(output.bytes(), 0);
}

/// e.3: a failed C Data Interface export is an error naming what Python said, never a copy by
/// some other route (section l, anti-patterns).
#[test]
fn a_broken_arrow_export_is_an_error() {
    let error = apply_awkward("broken_export", "ad_e3_broken", &[1, 2])
        .expect_err("a broken export is an error");
    let AmoruError::Kernel { msg, .. } = &error else {
        panic!("not a Kernel error: {error:?}");
    };
    assert!(
        msg.contains("Arrow C Data Interface") && msg.contains("no capsules here"),
        "the message must name both sides: {msg}"
    );
}

/// f.3: a tensor that is not on the host has no bytes for the adapter to read.
#[test]
fn a_device_tensor_has_no_host_bytes() {
    let fake = FakeAllocator::new();
    let buffer = fake.buffer(64, Tier::Device(amoru_kernel::DeviceId(0)));
    let tensor = ManagedTensor::from_buffer(buffer, 0, DType::I64, vec![8])
        .expect("a tensor over device memory");
    assert!(matches!(
        amoru_adapters::python::cross::host_tensor_bytes(&tensor),
        Err(AmoruError::Staging(_))
    ));
}
