//! The object a tensor payload crosses into Python as (e.2).
//!
//! It implements the two halves of the Python DLPack protocol, `__dlpack__` and
//! `__dlpack_device__`, so `torch.from_dlpack`, `jax.dlpack.from_dlpack`, `cupy.from_dlpack` and
//! `numpy.from_dlpack` all accept it. Nothing is copied: the capsule describes the same bytes and
//! keeps the [`ManagedTensor`] (and so the arena buffer behind it) alive until the consumer's
//! deleter runs.

use std::sync::Mutex;

use amoru_kernel::{DeviceId, ManagedTensor, Tier};
use pyo3::exceptions::{PyBufferError, PyRuntimeError};
use pyo3::prelude::*;

/// DLPack `DLDeviceType` values for the tiers a tensor can be exported from.
const KDL_CPU: i32 = 1;
const KDL_CUDA: i32 = 2;
const KDL_CUDA_HOST: i32 = 3;

/// A tensor payload as Python sees it: an object with `__dlpack__` and `__dlpack_device__`.
///
/// The tensor is handed over on the first `__dlpack__` call, which is what the protocol expects
/// (a capsule is consumed once); a second call raises `BufferError`.
#[pyclass(frozen, name = "Tensor", module = "amoru")]
pub struct PyTensor {
    tensor: Mutex<Option<ManagedTensor>>,
    device: (i32, i32),
}

impl PyTensor {
    /// Wrap a tensor for export in the tier its payload declares. `Err` for a tier with no
    /// resident bytes, which cannot cross.
    ///
    /// The tier is the payload's rather than the tensor's because the payload is what carries the
    /// truthful tier (CT-I2): the two agree for every tensor built through the safe constructors.
    pub fn new(tensor: ManagedTensor, tier: Tier) -> amoru_kernel::Result<PyTensor> {
        let device = dl_device(tier)?;
        Ok(PyTensor {
            tensor: Mutex::new(Some(tensor)),
            device,
        })
    }
}

/// The DLPack `(device_type, device_id)` pair for a tier (e.2). Every `Tier` variant is named
/// (CT-I11); `Disk` and `Remote` have no bytes to point at.
fn dl_device(tier: Tier) -> amoru_kernel::Result<(i32, i32)> {
    match tier {
        Tier::Host => Ok((KDL_CPU, 0)),
        Tier::PinnedHost => Ok((KDL_CUDA_HOST, 0)),
        Tier::Device(DeviceId(id)) => Ok((KDL_CUDA, i32::from(id))),
        Tier::Disk(_) => Err(amoru_kernel::AmoruError::Staging(
            "a tensor on Disk has no bytes to cross into Python".into(),
        )),
        Tier::Remote(_, _) => Err(amoru_kernel::AmoruError::Unsupported("rdma")),
    }
}

#[pymethods]
impl PyTensor {
    /// The DLPack capsule for this tensor; the tensor is handed over to the consumer.
    ///
    /// Every keyword of the protocol is accepted. `stream` and `copy` are ignored because the
    /// adapter never copies on export and never owns a stream (a device payload is produced by
    /// the placement engine, which has already synchronised it); `max_version` and `dl_device` are
    /// ignored because this producer speaks exactly one version, DLPack 1.x, on the device the
    /// payload is already on.
    #[pyo3(signature = (*, stream = None, max_version = None, dl_device = None, copy = None))]
    fn __dlpack__(
        &self,
        py: Python<'_>,
        stream: Option<Bound<'_, PyAny>>,
        max_version: Option<Bound<'_, PyAny>>,
        dl_device: Option<Bound<'_, PyAny>>,
        copy: Option<bool>,
    ) -> PyResult<Py<PyAny>> {
        let _ = (stream, max_version, dl_device, copy);
        let mut slot = self
            .tensor
            .lock()
            .map_err(|_| PyRuntimeError::new_err("amoru.Tensor lock was poisoned"))?;
        let Some(tensor) = slot.take() else {
            return Err(PyBufferError::new_err(
                "this amoru.Tensor has already been consumed by __dlpack__",
            ));
        };
        Ok(tensor.into_dlpack().into_pyobject(py)?.unbind())
    }

    /// The DLPack `(device_type, device_id)` of the bytes.
    fn __dlpack_device__(&self) -> (i32, i32) {
        self.device
    }

    fn __repr__(&self) -> String {
        format!(
            "amoru.Tensor(device_type={}, device_id={})",
            self.device.0, self.device.1
        )
    }
}
