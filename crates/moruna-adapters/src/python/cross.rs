//! Crossing: handing a payload to Python and taking one back, by pointer (e.2, e.3).
//!
//! Arrow crosses through the C Data Interface and tensors through DLPack, in both directions.
//! Neither direction allocates anything of payload size, which is AD-I1; the one copy the adapter
//! is allowed is the boundary copy in [`super::copy`], and it happens after the crossing, not
//! during it.
//!
//! `unsafe` is permitted in this module (section l): the DLPack contract is what says a tensor's
//! bytes are where its `data` and `byte_offset` fields point, and reading them is the only way to
//! copy a tensor a kernel allocated outside the arena.

use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{MorunaError, Dlpack, ManagedTensor, Payload, Result, Tier};
use pyo3::prelude::*;
use pyo3::types::PyList;
use pyo3_arrow::PyRecordBatch;

use super::kernel::{kernel_error, py_err_message};
use super::tensor_obj::PyTensor;

/// What came back from a Python kernel, before the boundary copy has been considered (e.3).
pub enum Imported {
    /// A record batch, imported through the C Data Interface.
    Table(RecordBatch),
    /// A tensor, imported through DLPack.
    Tensor(ManagedTensor),
}

/// Hand a payload to Python (e.2). Nothing of payload size is allocated.
pub fn export<'py>(py: Python<'py>, input: Payload) -> Result<Bound<'py, PyAny>> {
    match input {
        Payload::Table(batch, tier) => match tier {
            Tier::Host | Tier::PinnedHost => {
                PyRecordBatch::new(batch).into_pyarrow(py).map_err(|e| {
                    kernel_error(format!("export to pyarrow: {}", py_err_message(py, &e)))
                })
            }
            // e.2: a device batch crosses as the Arrow C Device Interface, which neither
            // `arrow` 59 nor `pyo3-arrow` 0.19 implements. Reported; no device host exists (E1).
            Tier::Device(_) => Err(MorunaError::Unsupported("arrow-c-device")),
            Tier::Disk(_) => Err(MorunaError::Staging(
                "a batch on Disk has no bytes to cross into Python".into(),
            )),
            Tier::Remote(_, _) => Err(MorunaError::Unsupported("rdma")),
        },
        Payload::Tensor(tensor, tier) => {
            let obj = PyTensor::new(tensor, tier)?;
            let bound = Bound::new(py, obj)
                .map_err(|e| kernel_error(format!("export tensor: {}", py_err_message(py, &e))))?;
            Ok(bound.into_any())
        }
    }
}

/// Take a payload back from Python (e.3). The returned object is inspected in the order the
/// table in e.3 lists: a record batch, a single chunk table, a DLPack producer, `None`, anything
/// else.
pub fn import(py: Python<'_>, obj: &Bound<'_, PyAny>) -> Result<Imported> {
    if obj.is_none() {
        return Err(kernel_error("kernel returned None".into()));
    }
    if has_attr(obj, "__arrow_c_array__") {
        return import_batch(py, obj);
    }
    if has_attr(obj, "to_batches") {
        return import_single_chunk_table(py, obj);
    }
    if has_attr(obj, "__dlpack__") {
        return import_tensor(py, obj);
    }
    Err(kernel_error(format!(
        "kernel returned {}, which is neither a record batch, a single chunk table nor a DLPack producer",
        type_name(obj)
    )))
}

fn has_attr(obj: &Bound<'_, PyAny>, name: &str) -> bool {
    obj.hasattr(name).unwrap_or(false)
}

fn type_name(obj: &Bound<'_, PyAny>) -> String {
    match obj.get_type().name() {
        Ok(name) => name.to_string(),
        Err(_) => "an object of an unreadable type".to_string(),
    }
}

fn import_batch(py: Python<'_>, obj: &Bound<'_, PyAny>) -> Result<Imported> {
    // The C Data Interface import is a pointer hand off; a failure is an error, never a copy
    // through some other route (section l, anti-patterns).
    let batch: PyRecordBatch = obj.extract().map_err(|e| {
        kernel_error(format!(
            "the returned object's Arrow C Data Interface export failed: {}",
            py_err_message(py, &e)
        ))
    })?;
    Ok(Imported::Table(batch.into_inner()))
}

fn import_single_chunk_table(py: Python<'_>, obj: &Bound<'_, PyAny>) -> Result<Imported> {
    let batches = obj
        .call_method0("to_batches")
        .map_err(|e| kernel_error(format!("to_batches: {}", py_err_message(py, &e))))?;
    let list = batches
        .cast::<PyList>()
        .map_err(|_| kernel_error("to_batches did not return a list".into()))?;
    if list.len() != 1 {
        return Err(kernel_error(format!(
            "return a RecordBatch or a single-chunk Table; this table has {} chunks",
            list.len()
        )));
    }
    let only = list
        .get_item(0)
        .map_err(|e| kernel_error(format!("to_batches[0]: {}", py_err_message(py, &e))))?;
    import_batch(py, &only)
}

fn import_tensor(py: Python<'_>, obj: &Bound<'_, PyAny>) -> Result<Imported> {
    let capsule: Dlpack = obj
        .extract()
        .map_err(|e| kernel_error(format!("__dlpack__: {}", py_err_message(py, &e))))?;
    let tensor = ManagedTensor::from_dlpack(capsule)?;
    // AD-I5: the bytes belong to a Python object, so the DLPack deleter decrements a Python
    // reference count and must run attached, from whatever thread drops the tensor.
    tensor.set_deleter_hook(std::sync::Arc::new(|delete: &mut dyn FnMut()| {
        Python::attach(|_| delete());
    }));
    Ok(Imported::Tensor(tensor))
}

/// The contiguous bytes a host tensor occupies, for the boundary copy (f.3).
///
/// `Err` for a tensor that is not resident on the host or whose strides are not row major: the
/// adapter copies bytes, it does not repack them.
pub fn host_tensor_bytes(tensor: &ManagedTensor) -> Result<&[u8]> {
    if !tensor.tier().is_host() {
        return Err(MorunaError::Staging(format!(
            "host_tensor_bytes: {:?} is not a host tier",
            tensor.tier()
        )));
    }
    if !tensor.is_contiguous() {
        return Err(MorunaError::Convert(
            moruna_kernel::ConvertError::NotContiguous,
        ));
    }
    let (base, offset) = tensor.data_ptr();
    let len = tensor.byte_len() as usize;
    if len == 0 {
        return Ok(&[]);
    }
    let ptr = base.wrapping_add(offset as usize);
    if ptr.is_null() {
        return Err(MorunaError::Staging(
            "host_tensor_bytes: tensor data pointer is null".into(),
        ));
    }
    // SAFETY: DLPack's contract is that a tensor's elements start at `data + byte_offset` and,
    // for row major strides, span `element_count * item_size` bytes; both were checked above.
    // The borrow of `tensor` keeps the producer's memory alive for the returned slice, and no
    // `&mut` to those bytes exists while this shared reference is held.
    Ok(unsafe { std::slice::from_raw_parts(ptr.cast_const(), len) })
}
