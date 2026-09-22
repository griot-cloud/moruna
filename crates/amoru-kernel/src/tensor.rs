//! The DLPack-backed tensor wrapper (contracts d.4, `from_buffer`). `unsafe` is permitted here
//! for DLPack pointer handling (section l).
//!
//! The wrapper owns a `dlpark::versioned::Dlpack` (DLPack 1.x `DLManagedTensorVersioned`) and
//! adds tier tracking and Amoru's contiguity checks. Handles are reference counted internally
//! so a zero-copy view of the bytes (`Payload::as_column`, `BufferView::of_tensor`) can keep
//! the tensor alive without the tensor being `Clone`.

use std::sync::{Arc, Mutex};

use dlpark::ffi::{DLDataType, DLDataTypeCode, DLDevice, DLDeviceType, DLManagedTensorVersioned};
use dlpark::metadata::CopiedSlice;
use dlpark::{Builder, DlpackFlags};

use crate::buffer::Buffer;
use crate::error::AmoruError;
use crate::ids::DeviceId;
use crate::payload::DType;
use crate::tier::Tier;

/// The DLPack handle this wrapper exchanges: dlpark's owning handle for
/// `DLManagedTensorVersioned` (DLPack 1.x).
pub type Dlpack = dlpark::versioned::Dlpack;

/// A hook run around a tensor's DLPack deleter: it receives the deleter as a closure and must
/// call it, after doing whatever the producer needs first (a Python producer's deleter must
/// run with the interpreter attached; the adapters SDD decides that). Set per tensor with
/// `ManagedTensor::set_deleter_hook`; the deleter runs on whichever thread drops the tensor (g).
pub type DeleterHook = Arc<dyn Fn(&mut dyn FnMut()) + Send + Sync>;

struct Inner {
    dlpack: Option<Dlpack>,
    tier: Tier,
    dtype: DType,
    shape: Vec<i64>,
    strides: Option<Vec<i64>>,
    hook: Mutex<Option<DeleterHook>>,
}

// SAFETY: the DLPack contract requires the deleter to be callable from any thread, and every
// field other than the hook is immutable after construction; the hook sits behind a `Mutex`.
unsafe impl Send for Inner {}
// SAFETY: as above; shared access only reads immutable metadata or the locked hook.
unsafe impl Sync for Inner {}

impl Drop for Inner {
    fn drop(&mut self) {
        let dlpack = self.dlpack.take();
        let hook = self
            .hook
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match (dlpack, hook) {
            (Some(d), Some(hook)) => {
                let mut slot = Some(d);
                hook(&mut || {
                    slot.take();
                });
                drop(slot);
            }
            (Some(d), None) => drop(d),
            (None, _) => {}
        }
    }
}

/// A DLPack-backed tensor with ownership. Wraps `dlpark`'s versioned managed tensor;
/// the wrapper adds tier tracking and Amoru's contiguity checks.
pub struct ManagedTensor {
    inner: Arc<Inner>,
}

impl ManagedTensor {
    /// Element type.
    pub fn dtype(&self) -> DType {
        self.inner.dtype
    }

    /// Shape; empty for a zero-dimensional tensor.
    pub fn shape(&self) -> &[i64] {
        &self.inner.shape
    }

    /// `None` means contiguous row-major.
    pub fn strides(&self) -> Option<&[i64]> {
        self.inner.strides.as_deref()
    }

    /// True when the strides are absent or equal the row-major strides for the shape (b).
    pub fn is_contiguous(&self) -> bool {
        match &self.inner.strides {
            None => true,
            Some(strides) => {
                if self.inner.shape.contains(&0) {
                    return true;
                }
                strides.as_slice() == row_major_strides(&self.inner.shape).as_slice()
            }
        }
    }

    /// Product of the shape; 1 for a zero-dimensional tensor.
    pub fn element_count(&self) -> u64 {
        element_count(&self.inner.shape)
    }

    /// `element_count × item_size`, ignoring strides (f.1).
    pub fn byte_len(&self) -> u64 {
        self.element_count() * self.inner.dtype.item_size() as u64
    }

    /// The tier the bytes are in.
    pub fn tier(&self) -> Tier {
        self.inner.tier
    }

    /// Raw data pointer plus byte offset, as DLPack defines them.
    pub fn data_ptr(&self) -> (*mut u8, u64) {
        match &self.inner.dlpack {
            Some(d) => {
                let t = d.tensor();
                (t.data.cast::<u8>(), t.byte_offset)
            }
            None => (std::ptr::null_mut(), 0),
        }
    }

    /// Install the hook that runs around this tensor's DLPack deleter (g). A hook set while
    /// zero-copy views of the tensor exist runs when the last of them drops.
    pub fn set_deleter_hook(&self, hook: DeleterHook) {
        let mut slot = self.inner.hook.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(hook);
    }

    /// Export as a DLPack capsule the caller owns; consumes self. The exported managed tensor
    /// describes the same bytes and keeps this tensor (and so the arena buffer or Arrow array
    /// behind it, and its deleter hook) alive until the consumer calls the deleter.
    pub fn into_dlpack(self) -> Dlpack {
        let (data, byte_offset) = self.data_ptr();
        let dtype = dl_dtype(self.inner.dtype);
        let device = match device_of(self.inner.tier) {
            Ok(d) => d,
            // A resident tensor always has a device (checked at construction).
            Err(_) => DLDevice::CPU,
        };
        let shape = self.inner.shape.clone();
        let strides = match &self.inner.strides {
            Some(s) => s.clone(),
            None => row_major_strides(&shape),
        };
        let flags = match &self.inner.dlpack {
            Some(d) => d.flags(),
            None => DlpackFlags::empty(),
        };
        let builder = Builder::new(Box::new(self), CopiedSlice::new(shape, strides));
        // SAFETY: the context (this tensor) keeps `data` valid until the exported deleter
        // runs; dtype, shape, strides and byte offset are those of the tensor.
        let builder = unsafe { builder.data(data.cast()) }
            .byte_offset(byte_offset)
            .dtype(dtype)
            .device(device);
        // SAFETY: the flags are copied verbatim from the tensor being exported, so no new
        // ownership claim (`IS_COPIED`) is asserted.
        let builder = unsafe { builder.flags_unchecked(flags) };
        // SAFETY: `shape` and `strides` have the same length by construction, and the rank fits
        // an `i32` because every constructor of this type checks it.
        unsafe { builder.build_unchecked::<DLManagedTensorVersioned>() }
    }

    /// Import from DLPack; the tier is read from the DLPack device field.
    pub fn from_dlpack(t: Dlpack) -> crate::Result<Self> {
        let version = t.version();
        if version.major != dlpark::ffi::DLPACK_MAJOR_VERSION {
            return Err(AmoruError::Plan(format!(
                "unsupported DLPack major version {} (this build speaks {})",
                version.major,
                dlpark::ffi::DLPACK_MAJOR_VERSION
            )));
        }
        let tensor = t.tensor();
        let tier = tier_of(tensor.device)?;
        let Some(dtype) = dtype_of(tensor.dtype) else {
            return Err(AmoruError::Plan(format!(
                "unsupported DLPack dtype code {} bits {} lanes {}",
                tensor.dtype.code.0, tensor.dtype.bits, tensor.dtype.lanes
            )));
        };
        let shape = t
            .shape()
            .map_err(|e| AmoruError::Plan(format!("invalid DLPack shape: {e}")))?
            .to_vec();
        if shape.iter().any(|d| *d < 0) {
            return Err(AmoruError::Plan(format!(
                "negative DLPack dimension in {shape:?}"
            )));
        }
        let strides = t
            .strides()
            .map_err(|e| AmoruError::Plan(format!("invalid DLPack strides: {e}")))?
            .map(<[i64]>::to_vec);
        if let Some(s) = &strides
            && s.len() != shape.len()
        {
            return Err(AmoruError::Plan(format!(
                "DLPack strides rank {} differs from shape rank {}",
                s.len(),
                shape.len()
            )));
        }
        Ok(ManagedTensor {
            inner: Arc::new(Inner {
                dlpack: Some(t),
                tier,
                dtype,
                shape,
                strides,
                hook: Mutex::new(None),
            }),
        })
    }

    /// Wrap the bytes of an arena buffer, starting at `byte_offset`, as a contiguous
    /// row-major tensor of `dtype` and `shape`; the buffer is owned by the tensor and
    /// released when it drops. Checks `byte_offset + element_count × item_size ≤ len`
    /// and alignment; safe. Tier = the buffer's. This is how a staged tensor record
    /// comes back from disk (placement f.6) without `unsafe` outside this crate.
    pub fn from_buffer(
        buf: Buffer,
        byte_offset: u64,
        dtype: DType,
        shape: Vec<i64>,
    ) -> crate::Result<Self> {
        let io = |msg: String| AmoruError::Io {
            op: "from_buffer",
            target: "buffer".into(),
            msg,
        };
        if shape.iter().any(|d| *d < 0) {
            return Err(io(format!("negative dimension in shape {shape:?}")));
        }
        let item = dtype.item_size() as u64;
        let needed = element_count(&shape)
            .checked_mul(item)
            .and_then(|n| n.checked_add(byte_offset));
        let Some(needed) = needed else {
            return Err(io(format!("shape {shape:?} overflows")));
        };
        if needed > buf.len() as u64 {
            return Err(io(format!(
                "buffer of {} bytes is short of the {needed} bytes the tensor needs",
                buf.len()
            )));
        }
        if !byte_offset.is_multiple_of(item) {
            return Err(io(format!(
                "byte offset {byte_offset} is not a multiple of the item size {item}"
            )));
        }
        let tier = buf.tier();
        let base = match tier {
            Tier::PinnedHost | Tier::Host => buf.host_ptr(),
            Tier::Device(_) => buf.device_ptr().map(|p| p as usize as *mut u8),
            Tier::Disk(_) => None,
            Tier::Remote(_, _) => None,
        };
        let Some(base) = base else {
            return Err(AmoruError::Staging(format!(
                "from_buffer: {tier:?} buffer is not resident"
            )));
        };
        if !(base as usize + byte_offset as usize).is_multiple_of(dtype.item_size()) {
            return Err(io(format!(
                "data address is not aligned to the item size {item}"
            )));
        }
        // SAFETY: `buf` owns `len` bytes in `tier` (CT-I2) and is moved into the tensor's
        // context, so the bytes outlive the tensor; the bounds were checked above.
        unsafe { Self::over_owner(buf, base, byte_offset, dtype, shape, tier) }
    }

    /// Build a tensor over bytes kept alive by `owner`.
    ///
    /// # Safety
    /// `data + byte_offset` must address `element_count(shape) × item_size` bytes in `tier`
    /// for as long as `owner` lives.
    pub(crate) unsafe fn over_owner<C: Send + 'static>(
        owner: C,
        data: *mut u8,
        byte_offset: u64,
        dtype: DType,
        shape: Vec<i64>,
        tier: Tier,
    ) -> crate::Result<Self> {
        let device = device_of(tier)?;
        if i32::try_from(shape.len()).is_err() {
            return Err(AmoruError::Plan(format!(
                "tensor rank {} does not fit DLPack",
                shape.len()
            )));
        }
        let strides = row_major_strides(&shape);
        let builder = Builder::new(Box::new(owner), CopiedSlice::new(shape.clone(), strides));
        // SAFETY: the caller guarantees the bytes and `owner` keeps them alive until the
        // deleter runs.
        let builder = unsafe { builder.data(data.cast()) }
            .byte_offset(byte_offset)
            .dtype(dl_dtype(dtype))
            .device(device);
        let dlpack = builder
            .try_build::<DLManagedTensorVersioned>()
            .map_err(|e| AmoruError::Plan(format!("dlpack: {e}")))?;
        Ok(ManagedTensor {
            inner: Arc::new(Inner {
                dlpack: Some(dlpack),
                tier,
                dtype,
                shape,
                strides: None,
                hook: Mutex::new(None),
            }),
        })
    }

    /// A second handle to the same tensor, for zero-copy views that must keep it alive.
    pub(crate) fn share(&self) -> ManagedTensor {
        ManagedTensor {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl std::fmt::Debug for ManagedTensor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedTensor")
            .field("dtype", &self.inner.dtype)
            .field("shape", &self.inner.shape)
            .field("strides", &self.inner.strides)
            .field("tier", &self.inner.tier)
            .finish()
    }
}

fn element_count(shape: &[i64]) -> u64 {
    shape.iter().map(|d| *d as u64).product()
}

fn row_major_strides(shape: &[i64]) -> Vec<i64> {
    let mut strides = vec![0i64; shape.len()];
    let mut stride = 1i64;
    for i in (0..shape.len()).rev() {
        strides[i] = stride;
        stride = stride.saturating_mul(shape[i].max(1));
    }
    strides
}

/// The DLPack dtype for an Amoru dtype.
pub(crate) fn dl_dtype(dtype: DType) -> DLDataType {
    let (code, bits) = match dtype {
        DType::I8 => (DLDataTypeCode::INT, 8),
        DType::I16 => (DLDataTypeCode::INT, 16),
        DType::I32 => (DLDataTypeCode::INT, 32),
        DType::I64 => (DLDataTypeCode::INT, 64),
        DType::U8 => (DLDataTypeCode::UINT, 8),
        DType::U16 => (DLDataTypeCode::UINT, 16),
        DType::U32 => (DLDataTypeCode::UINT, 32),
        DType::U64 => (DLDataTypeCode::UINT, 64),
        DType::F16 => (DLDataTypeCode::FLOAT, 16),
        DType::BF16 => (DLDataTypeCode::BFLOAT, 16),
        DType::F32 => (DLDataTypeCode::FLOAT, 32),
        DType::F64 => (DLDataTypeCode::FLOAT, 64),
        DType::Bool => (DLDataTypeCode::BOOL, 8),
    };
    DLDataType {
        code,
        bits,
        lanes: 1,
    }
}

/// The Amoru dtype for a DLPack dtype; `None` when there is no one-to-one match.
pub(crate) fn dtype_of(dt: DLDataType) -> Option<DType> {
    if dt.lanes != 1 {
        return None;
    }
    let out = match (dt.code, dt.bits) {
        (DLDataTypeCode::INT, 8) => DType::I8,
        (DLDataTypeCode::INT, 16) => DType::I16,
        (DLDataTypeCode::INT, 32) => DType::I32,
        (DLDataTypeCode::INT, 64) => DType::I64,
        (DLDataTypeCode::UINT, 8) => DType::U8,
        (DLDataTypeCode::UINT, 16) => DType::U16,
        (DLDataTypeCode::UINT, 32) => DType::U32,
        (DLDataTypeCode::UINT, 64) => DType::U64,
        (DLDataTypeCode::FLOAT, 16) => DType::F16,
        (DLDataTypeCode::BFLOAT, 16) => DType::BF16,
        (DLDataTypeCode::FLOAT, 32) => DType::F32,
        (DLDataTypeCode::FLOAT, 64) => DType::F64,
        (DLDataTypeCode::BOOL, 8) => DType::Bool,
        _ => return None,
    };
    Some(out)
}

/// The DLPack device for a resident tier.
fn device_of(tier: Tier) -> crate::Result<DLDevice> {
    match tier {
        Tier::Host => Ok(DLDevice::CPU),
        Tier::PinnedHost => Ok(DLDevice {
            device_type: DLDeviceType::CUDAHOST,
            device_id: 0,
        }),
        Tier::Device(DeviceId(id)) => Ok(DLDevice::cuda(i32::from(id))),
        Tier::Disk(_) => Err(AmoruError::Staging("tensor is not resident (Disk)".into())),
        Tier::Remote(_, _) => Err(AmoruError::Unsupported("rdma")),
    }
}

/// The tier for a DLPack device.
fn tier_of(device: DLDevice) -> crate::Result<Tier> {
    match device.device_type {
        DLDeviceType::CPU => Ok(Tier::Host),
        DLDeviceType::CUDAHOST => Ok(Tier::PinnedHost),
        DLDeviceType::CUDA => u8::try_from(device.device_id)
            .map(|id| Tier::Device(DeviceId(id)))
            .map_err(|_| {
                AmoruError::Plan(format!(
                    "DLPack device id {} out of range",
                    device.device_id
                ))
            }),
        other => Err(AmoruError::Plan(format!(
            "unsupported DLPack device type {}",
            other.0
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtype_mapping_is_one_to_one() {
        for d in [
            DType::I8,
            DType::I16,
            DType::I32,
            DType::I64,
            DType::U8,
            DType::U16,
            DType::U32,
            DType::U64,
            DType::F16,
            DType::BF16,
            DType::F32,
            DType::F64,
            DType::Bool,
        ] {
            assert_eq!(dtype_of(dl_dtype(d)), Some(d));
        }
        assert_eq!(
            dtype_of(DLDataType {
                code: DLDataTypeCode::COMPLEX,
                bits: 64,
                lanes: 1
            }),
            None
        );
        assert_eq!(
            dtype_of(DLDataType {
                code: DLDataTypeCode::FLOAT,
                bits: 32,
                lanes: 4
            }),
            None
        );
    }

    #[test]
    fn device_mapping() {
        assert_eq!(tier_of(DLDevice::CPU).ok(), Some(Tier::Host));
        assert_eq!(
            tier_of(DLDevice::cuda(3)).ok(),
            Some(Tier::Device(DeviceId(3)))
        );
        assert!(tier_of(DLDevice::cuda(300)).is_err());
        assert!(
            tier_of(DLDevice {
                device_type: DLDeviceType::ROCM,
                device_id: 0
            })
            .is_err()
        );
        let pinned = device_of(Tier::PinnedHost).unwrap();
        assert_eq!(pinned.device_type, DLDeviceType::CUDAHOST);
        assert_eq!(tier_of(pinned).ok(), Some(Tier::PinnedHost));
        let seg = crate::tier::SegmentRef {
            segment: 0,
            offset: 0,
            len: 0,
        };
        assert!(matches!(
            device_of(Tier::Disk(seg)),
            Err(AmoruError::Staging(_))
        ));
        let r = crate::tier::RemoteRef {
            addr: 0,
            rkey: 0,
            len: 0,
        };
        assert!(matches!(
            device_of(Tier::Remote(crate::ids::LOCAL_NODE, r)),
            Err(AmoruError::Unsupported("rdma"))
        ));
    }

    #[test]
    fn strides_and_counts() {
        assert_eq!(row_major_strides(&[2, 3, 4]), vec![12, 4, 1]);
        assert_eq!(row_major_strides(&[]), Vec::<i64>::new());
        assert_eq!(element_count(&[]), 1);
        assert_eq!(element_count(&[0, 5]), 0);
    }
}
