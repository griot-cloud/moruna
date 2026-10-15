//! The boundary copy (f.3, AD-I2).
//!
//! A Python kernel's output is whatever pyarrow, NumPy or Torch allocated for it, and those
//! bytes are outside the arena. Downstream the morsel is moved by DMA, which needs arena buffers,
//! so a returned host payload whose buffers the arena does not own is copied once, here, into the
//! arena, and counted. A returned device tensor is never copied: the placement engine, not the
//! adapter, decides what happens to device bytes.
//!
//! Nothing in this module attaches to the interpreter. AD-I4 is the reason: an attachment is
//! never held across a call into the arena, and every allocation below is such a call.

use std::sync::Arc;

use moruna_kernel::{Allocator, ManagedTensor, MorunaError, Payload, Result, Tier};

use crate::land::host_tier;

use super::cross::{Imported, host_tensor_bytes};

/// The outcome of the boundary step: the payload the runtime gets, and the bytes copied (0 when
/// the kernel returned arena memory or a device tensor).
pub struct Landed {
    /// The payload, resident in the arena when it needed to be copied there.
    pub payload: Payload,
    /// Bytes copied at the boundary; 0 when no copy was needed.
    pub copied_bytes: u64,
}

/// Bring an imported object into the arena if it is not there already (f.3).
///
/// Counts the copy in `AllocStats::boundary_copies_total` through
/// [`Allocator::note_boundary_copy`], once, here, where the copy happens.
pub fn land(imported: Imported, alloc: &Arc<dyn Allocator>) -> Result<Landed> {
    let landed = match imported {
        Imported::Table(batch) => {
            let (payload, copied_bytes) = crate::land::table(batch, alloc.as_ref())?;
            Landed {
                payload,
                copied_bytes,
            }
        }
        Imported::Tensor(tensor) => land_tensor(tensor, alloc.as_ref())?,
    };
    if landed.copied_bytes > 0 {
        alloc.note_boundary_copy(landed.copied_bytes);
        tracing::debug!(
            target: "adapter.boundary_copy",
            bytes = landed.copied_bytes,
            "copied a kernel's output into the arena"
        );
    }
    Ok(landed)
}

fn land_tensor(tensor: ManagedTensor, alloc: &dyn Allocator) -> Result<Landed> {
    let resident_on_host = match tensor.tier() {
        Tier::Host | Tier::PinnedHost => true,
        // AD-I2: a device tensor is never copied by the adapter.
        Tier::Device(_) => false,
        Tier::Disk(_) => {
            return Err(MorunaError::Staging(
                "a kernel returned a tensor on Disk, which has no resident bytes".into(),
            ));
        }
        Tier::Remote(_, _) => return Err(MorunaError::Unsupported("rdma")),
    };
    if !resident_on_host {
        return Ok(Landed {
            payload: Payload::tensor(tensor)?,
            copied_bytes: 0,
        });
    }
    let (base, offset) = tensor.data_ptr();
    let start = base.wrapping_add(offset as usize).cast_const();
    if alloc.contains(start) {
        return Ok(Landed {
            payload: Payload::tensor(tensor)?,
            copied_bytes: 0,
        });
    }
    let bytes = host_tensor_bytes(&tensor)?;
    let tier = host_tier(alloc);
    let mut buffer = alloc.alloc(bytes.len().max(1), tier)?;
    buffer[..bytes.len()].copy_from_slice(bytes);
    let copied_bytes = bytes.len() as u64;
    let dtype = tensor.dtype();
    let shape = tensor.shape().to_vec();
    drop(tensor);
    let copy = ManagedTensor::from_buffer(buffer, 0, dtype, shape)?;
    Ok(Landed {
        payload: Payload::tensor(copy)?,
        copied_bytes,
    })
}
