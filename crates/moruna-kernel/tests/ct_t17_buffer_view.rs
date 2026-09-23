//! CT-T17 buffer_view: `BufferView::of_arrow` over an arrow buffer sliced from
//! `into_arrow_buffer` reports the arena's tier and pointer; over a heap buffer it returns
//! `Staging`; `of_tensor` matches `data_ptr`; dropping the view while the source lives changes
//! nothing; dropping the source while the view lives keeps the bytes valid (the owner is held).
//! Proves d.3.

mod common;

use std::sync::Arc;

use moruna_kernel::{MorunaError, BufferView, DType, ManagedTensor, Tier};
use common::FakeAllocator;

#[test]
fn ct_t17_buffer_view() {
    let alloc = FakeAllocator::new();

    // A view over an arrow buffer that came from an arena buffer: the arena's tier and the
    // buffer's own pointer.
    let mut buffer = alloc.buffer(256, Tier::PinnedHost);
    buffer[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    let base = buffer.host_ptr().expect("a host buffer");
    let arrow = buffer
        .into_arrow_buffer()
        .expect("an arrow buffer over the arena");
    let view = BufferView::of_arrow(&arrow, &alloc).expect("a view over an arena buffer");
    assert_eq!(view.tier(), Tier::PinnedHost);
    assert_eq!(view.host_ptr(), Some(base.cast_const()));
    assert_eq!(view.device_ptr(), None);
    assert_eq!(view.len(), 256);
    assert!(!view.is_empty());
    assert_eq!(
        view.as_host_slice().expect("host bytes")[..8],
        [1, 2, 3, 4, 5, 6, 7, 8]
    );

    // and over a slice of it: the same tier, the slice's own pointer and length.
    let slice = arrow.slice_with_length(64, 32);
    let sliced = BufferView::of_arrow(&slice, &alloc).expect("a view over a slice");
    assert_eq!(sliced.tier(), Tier::PinnedHost);
    assert_eq!(sliced.len(), 32);
    assert_eq!(sliced.host_ptr(), Some(base.wrapping_add(64).cast_const()));

    // A sub-range of a view is a view of the same bytes.
    let sub = view.slice(8, 16);
    assert_eq!(sub.len(), 16);
    assert_eq!(sub.host_ptr(), Some(base.wrapping_add(8).cast_const()));
    assert_eq!(sub.tier(), Tier::PinnedHost);

    // Dropping the view while the source lives changes nothing.
    drop(sub);
    drop(sliced);
    assert_eq!(arrow.as_slice()[..8], [1, 2, 3, 4, 5, 6, 7, 8]);

    // Dropping the source while the view lives keeps the bytes valid: the view holds an owner.
    drop(arrow);
    assert_eq!(
        view.as_host_slice().expect("host bytes")[..8],
        [1, 2, 3, 4, 5, 6, 7, 8]
    );
    drop(view);

    // A buffer that is not the arena's is refused by name.
    let heap = arrow::buffer::Buffer::from(vec![9u8; 32]);
    let err = BufferView::of_arrow(&heap, &alloc).expect_err("a heap buffer is not the arena's");
    assert!(
        matches!(err, MorunaError::Staging(_)),
        "expected Staging, got {err}"
    );
    assert!(err.to_string().contains("not an arena buffer"));

    // `of_tensor` covers the tensor's whole contiguous range and matches `data_ptr`.
    let buffer = alloc.buffer(64 * 4, Tier::Host);
    let base = buffer.host_ptr().expect("a host buffer");
    let tensor =
        Arc::new(ManagedTensor::from_buffer(buffer, 0, DType::F32, vec![8, 8]).expect("a tensor"));
    let view = BufferView::of_tensor(&tensor).expect("a view over a tensor");
    let (data, offset) = tensor.data_ptr();
    assert_eq!(
        view.host_ptr(),
        Some(data.wrapping_add(offset as usize).cast_const())
    );
    assert_eq!(view.host_ptr(), Some(base.cast_const()));
    assert_eq!(view.len() as u64, tensor.byte_len());
    assert_eq!(view.tier(), Tier::Host);

    // The tensor may go; the view keeps its bytes alive.
    drop(tensor);
    assert_eq!(view.as_host_slice().expect("host bytes").len(), 256);
    assert!(format!("{view:?}").contains("BufferView"));

    // A view of an `Arc<Buffer>` keeps that buffer alive too.
    let buffer = Arc::new(alloc.buffer(32, Tier::Host));
    let view = buffer.view();
    assert_eq!(view.len(), 32);
    assert_eq!(view.tier(), Tier::Host);
    drop(buffer);
    assert_eq!(view.as_host_slice().expect("host bytes").len(), 32);

    // A view of bytes on a device reports a device pointer and no host pointer, and one of a
    // staged or remote payload reports neither (d.3).
    let device = common::tagged_buffer(64, Tier::Device(moruna_kernel::DeviceId(1)));
    let tensor = Arc::new(
        ManagedTensor::from_buffer(device, 0, DType::F32, vec![16]).expect("a device tensor"),
    );
    let view = BufferView::of_tensor(&tensor).expect("a view over a device tensor");
    assert!(view.device_ptr().is_some());
    assert_eq!(view.host_ptr(), None);
    assert!(view.as_host_slice().is_none());
    assert_eq!(view.tier(), Tier::Device(moruna_kernel::DeviceId(1)));

    // An empty view has an empty slice, not a null one.
    let buffer = Arc::new(alloc.buffer(0, Tier::Host));
    let empty = buffer.view().slice(0, 0);
    assert!(empty.is_empty());
    assert_eq!(empty.as_host_slice(), Some(&[] as &[u8]));

    // A non-contiguous tensor has no single byte range, so it has no view.
    let strided = strided_tensor();
    let err = BufferView::of_tensor(&Arc::new(strided)).expect_err("a strided tensor has no view");
    assert!(
        matches!(err, MorunaError::Convert(_)),
        "expected Convert, got {err}"
    );
}

/// A tensor with explicit, non-row-major strides, built through DLPack.
fn strided_tensor() -> ManagedTensor {
    let data = Box::new(vec![0f32; 12]);
    let ptr = data.as_ptr().cast_mut().cast();
    let builder = dlpark::Builder::new(
        data,
        dlpark::metadata::CopiedSlice::new(vec![2i64, 3], vec![1i64, 2]),
    );
    // SAFETY: test-only; the boxed vector keeps the bytes alive until the deleter runs.
    let builder = unsafe { builder.data(ptr) }
        .dtype(dlpark::ffi::DLDataType::of::<f32>())
        .device(dlpark::ffi::DLDevice::CPU);
    let dlpack = builder
        .try_build::<dlpark::ffi::DLManagedTensorVersioned>()
        .expect("a strided DLPack tensor");
    ManagedTensor::from_dlpack(dlpack).expect("import a strided tensor")
}
