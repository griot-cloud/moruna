//! CT-T19 tensor_from_buffer: `ManagedTensor::from_buffer` over an `MRB1` body gives
//! `data_ptr == buf.host_ptr() + offset` with the shape and dtype as given; a short buffer or a
//! misaligned offset is a `Convert` or `Io` error, never a panic. Proves d.4.

mod common;

use moruna_kernel::mrb1::{Header, record_len};
use moruna_kernel::{MorunaError, DType, ManagedTensor, Payload, Tier};
use common::FakeAllocator;

const PAGE: u64 = 4096;

/// A DLPack tensor with the given descriptor, over bytes this test leaks, the way a foreign
/// producer would hand one over.
fn dlpack_with(
    dtype: dlpark::ffi::DLDataType,
    device: dlpark::ffi::DLDevice,
    shape: Vec<i64>,
    strides: Vec<i64>,
) -> dlpark::versioned::Dlpack {
    let data = Box::new(vec![0u8; 4096]);
    let ptr = data.as_ptr().cast_mut().cast();
    let builder = dlpark::Builder::new(data, dlpark::metadata::CopiedSlice::new(shape, strides));
    // SAFETY: test-only; the boxed bytes keep the tensor's data alive until the deleter runs,
    // and 4096 bytes cover every descriptor this test builds.
    let builder = unsafe { builder.data(ptr) }.dtype(dtype).device(device);
    builder
        .try_build::<dlpark::ffi::DLManagedTensorVersioned>()
        .expect("a DLPack tensor")
}

#[test]
fn ct_t19_tensor_from_buffer() {
    let alloc = FakeAllocator::new();

    // The staging path of placement f.6: an MRB1 record in one buffer, the tensor built over
    // the record's payload at its `data_offset`.
    for dtype in DType::ALL {
        let shape = vec![3i64, 4];
        let data_offset = Header::data_offset_for(shape.len(), PAGE).expect("a data offset");
        let header = Header {
            dtype,
            shape: shape.clone(),
            data_offset,
        };
        let total = record_len(shape.len(), header.payload_len(), PAGE).expect("a record length");
        let mut buffer = alloc.buffer(total as usize, Tier::Host);
        header.write(&mut buffer).expect("write the header");
        let base = buffer.host_ptr().expect("a host buffer");

        let tensor = ManagedTensor::from_buffer(buffer, data_offset, dtype, shape.clone())
            .expect("a tensor over the record's payload");

        let (data, offset) = tensor.data_ptr();
        assert_eq!(
            data.wrapping_add(offset as usize),
            base.wrapping_add(data_offset as usize),
            "{dtype:?}"
        );
        assert_eq!(tensor.dtype(), dtype);
        assert_eq!(tensor.shape(), shape.as_slice());
        assert_eq!(
            tensor.strides(),
            None,
            "a tensor over a buffer is contiguous row-major"
        );
        assert!(tensor.is_contiguous());
        assert_eq!(tensor.element_count(), 12);
        assert_eq!(tensor.byte_len(), 12 * dtype.item_size() as u64);
        assert_eq!(tensor.tier(), Tier::Host);
        assert!(format!("{tensor:?}").contains("ManagedTensor"));
    }

    // The buffer is owned by the tensor and released when it drops.
    let before = alloc.in_use(Tier::Host);
    {
        let buffer = alloc.buffer(4096, Tier::Host);
        let tensor = ManagedTensor::from_buffer(buffer, 0, DType::F64, vec![8])
            .expect("a tensor over a buffer");
        assert!(alloc.in_use(Tier::Host) > before);
        drop(tensor);
    }
    assert_eq!(
        alloc.in_use(Tier::Host),
        before,
        "the tensor released its buffer"
    );

    // A short buffer is an error, not a panic.
    let buffer = alloc.buffer(16, Tier::Host);
    let err = ManagedTensor::from_buffer(buffer, 0, DType::F64, vec![8])
        .expect_err("a short buffer must be refused");
    assert!(
        matches!(
            err,
            MorunaError::Io {
                op: "from_buffer",
                ..
            }
        ),
        "got {err}"
    );
    assert!(err.to_string().contains("short"));

    // So is a payload that would start past the end of the buffer.
    let buffer = alloc.buffer(64, Tier::Host);
    let err = ManagedTensor::from_buffer(buffer, 64, DType::I32, vec![4])
        .expect_err("an offset at the end must be refused");
    assert!(
        matches!(
            err,
            MorunaError::Io {
                op: "from_buffer",
                ..
            }
        ),
        "got {err}"
    );

    // A misaligned byte offset is an error, not a panic.
    let buffer = alloc.buffer(4096, Tier::Host);
    let err = ManagedTensor::from_buffer(buffer, 2, DType::I32, vec![4])
        .expect_err("a misaligned offset must be refused");
    assert!(
        matches!(
            err,
            MorunaError::Io {
                op: "from_buffer",
                ..
            }
        ),
        "got {err}"
    );
    assert!(err.to_string().contains("item size"));

    // A negative dimension is an error.
    let buffer = alloc.buffer(4096, Tier::Host);
    let err = ManagedTensor::from_buffer(buffer, 0, DType::I32, vec![-1])
        .expect_err("a negative dimension must be refused");
    assert!(
        matches!(
            err,
            MorunaError::Io {
                op: "from_buffer",
                ..
            }
        ),
        "got {err}"
    );

    // A buffer that is not resident has no bytes to wrap.
    let segment = moruna_kernel::SegmentRef {
        segment: 1,
        offset: PAGE,
        len: 32,
    };
    let staged = common::tagged_buffer(64, Tier::Disk(segment));
    let err = ManagedTensor::from_buffer(staged, 0, DType::I32, Vec::new())
        .expect_err("a staged buffer has no resident bytes");
    assert!(matches!(err, MorunaError::Staging(_)), "got {err}");
    // and neither does one on another node.
    let remote = moruna_kernel::RemoteRef {
        addr: 1,
        rkey: 2,
        len: 32,
    };
    let elsewhere = common::tagged_buffer(64, Tier::Remote(moruna_kernel::LOCAL_NODE, remote));
    let err = ManagedTensor::from_buffer(elsewhere, 0, DType::I32, Vec::new())
        .expect_err("a remote buffer has no resident bytes");
    assert!(matches!(err, MorunaError::Staging(_)), "got {err}");
    // A device buffer does, and its tensor carries the device tier.
    let device = common::tagged_buffer(32, Tier::Device(moruna_kernel::DeviceId(2)));
    let tensor = ManagedTensor::from_buffer(device, 0, DType::I32, vec![8])
        .expect("a tensor over a device buffer");
    assert_eq!(tensor.tier(), Tier::Device(moruna_kernel::DeviceId(2)));

    // A zero-dimensional tensor has one element (b) and still round trips through DLPack.
    let buffer = alloc.buffer(8, Tier::Host);
    let scalar =
        ManagedTensor::from_buffer(buffer, 0, DType::F64, Vec::new()).expect("a scalar tensor");
    assert_eq!(scalar.element_count(), 1);
    assert_eq!(scalar.byte_len(), 8);
    let (data, _) = scalar.data_ptr();
    let exported = scalar.into_dlpack();
    let imported = ManagedTensor::from_dlpack(exported).expect("import the exported tensor");
    assert_eq!(imported.dtype(), DType::F64);
    assert_eq!(imported.shape(), &[] as &[i64]);
    assert_eq!(imported.tier(), Tier::Host);
    let (again, offset) = imported.data_ptr();
    assert_eq!(again.wrapping_add(offset as usize), data);
    assert_eq!(
        Payload::tensor(imported).expect("tensor payload").bytes(),
        8
    );

    // A capsule of a foreign DLPack major version is refused, never read past its deleter
    // (d.4, and the DLPack spec's own rule).
    let foreign = dlpack_with(
        dlpark::ffi::DLDataType::of::<f32>(),
        dlpark::ffi::DLDevice::CPU,
        vec![2i64],
        vec![1i64],
    );
    let raw = foreign.into_raw();
    // SAFETY: test-only; `raw` came from `into_raw` and is handed straight back to a
    // `ManagedBox`, which calls dlpark's own deleter, so only the version field changes.
    let foreign = unsafe {
        (*raw).version.major = dlpark::ffi::DLPACK_MAJOR_VERSION + 1;
        dlpark::ManagedBox::new_unchecked(raw)
    };
    let err = ManagedTensor::from_dlpack(foreign).expect_err("a foreign major version");
    let expected = dlpark::ffi::DLPACK_MAJOR_VERSION;
    assert!(
        matches!(
            err,
            MorunaError::Convert(moruna_kernel::ConvertError::Version { found, expected: want })
                if found == expected + 1 && want == expected
        ),
        "got {err}"
    );

    // A DLPack tensor this build cannot describe is refused by name, never assumed.
    let err = ManagedTensor::from_dlpack(dlpack_with(
        dlpark::ffi::DLDataType {
            code: dlpark::ffi::DLDataTypeCode::COMPLEX,
            bits: 64,
            lanes: 1,
        },
        dlpark::ffi::DLDevice::CPU,
        vec![2i64],
        vec![1i64],
    ))
    .expect_err("an unmapped dtype must be refused");
    assert!(matches!(err, MorunaError::Plan(_)), "got {err}");
    assert!(err.to_string().contains("dtype"));

    let err = ManagedTensor::from_dlpack(dlpack_with(
        dlpark::ffi::DLDataType::of::<f32>(),
        dlpark::ffi::DLDevice {
            device_type: dlpark::ffi::DLDeviceType::ROCM,
            device_id: 0,
        },
        vec![2i64],
        vec![1i64],
    ))
    .expect_err("an unsupported device must be refused");
    assert!(matches!(err, MorunaError::Plan(_)), "got {err}");

    let err = ManagedTensor::from_dlpack(dlpack_with(
        dlpark::ffi::DLDataType::of::<f32>(),
        dlpark::ffi::DLDevice::CPU,
        vec![-2i64],
        vec![1i64],
    ))
    .expect_err("a negative dimension must be refused");
    assert!(matches!(err, MorunaError::Plan(_)), "got {err}");

    let err = ManagedTensor::from_dlpack(dlpack_with(
        dlpark::ffi::DLDataType::of::<f32>(),
        dlpark::ffi::DLDevice::CPU,
        vec![2i64, 2],
        vec![2i64, 1],
    ))
    .expect("a strided tensor is carried, not refused")
    .strides()
    .map(<[i64]>::to_vec);
    assert_eq!(
        err,
        Some(vec![2, 1]),
        "explicit strides are kept as the producer gave them"
    );

    // A tensor imported with explicit row-major strides is contiguous, and exporting it keeps
    // them, so a consumer sees the same layout it gave.
    let imported = ManagedTensor::from_dlpack(dlpack_with(
        dlpark::ffi::DLDataType::of::<f32>(),
        dlpark::ffi::DLDevice::CPU,
        vec![2i64, 3],
        vec![3i64, 1],
    ))
    .expect("import a row-major tensor");
    assert!(imported.is_contiguous());
    let exported = imported.into_dlpack();
    let again = ManagedTensor::from_dlpack(exported).expect("import what was exported");
    assert_eq!(again.shape(), [2, 3]);
    assert_eq!(again.strides(), Some([3i64, 1].as_slice()));

    // The deleter hook runs around the tensor's own deleter (g).
    let ran = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&ran);
    let buffer = alloc.buffer(32, Tier::Host);
    let tensor = ManagedTensor::from_buffer(buffer, 0, DType::I64, vec![4]).expect("a tensor");
    tensor.set_deleter_hook(std::sync::Arc::new(move |delete: &mut dyn FnMut()| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        delete();
    }));
    let held = alloc.in_use(Tier::Host);
    drop(tensor);
    assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        alloc.in_use(Tier::Host),
        held - 32,
        "the hook still released the buffer"
    );
}
