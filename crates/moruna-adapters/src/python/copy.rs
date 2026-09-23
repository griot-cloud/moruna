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

use moruna_kernel::arrow::array::{Array, ArrayData, ArrayRef, make_array};
use moruna_kernel::arrow::buffer::{BooleanBuffer, Buffer as ArrowBuffer, NullBuffer};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{Allocator, ManagedTensor, MorunaError, Payload, Result, Tier};

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
        Imported::Table(batch) => land_table(batch, alloc.as_ref())?,
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

/// The run's one host tier: `PinnedHost` when the arena is page locked, `Host` otherwise
/// (contracts e.1).
fn host_tier(alloc: &dyn Allocator) -> Tier {
    if alloc.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    }
}

fn land_table(batch: RecordBatch, alloc: &dyn Allocator) -> Result<Landed> {
    if batch_is_arena_owned(&batch, alloc) {
        return Ok(Landed {
            payload: Payload::table_with(batch, alloc)?,
            copied_bytes: 0,
        });
    }
    let tier = host_tier(alloc);
    let mut copied = 0u64;
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for column in batch.columns() {
        let data = copy_array_data(&column.to_data(), alloc, tier, &mut copied)?;
        columns.push(make_array(data));
    }
    let schema = batch.schema();
    let copy = RecordBatch::try_new_with_options(
        schema,
        columns,
        &moruna_kernel::arrow::record_batch::RecordBatchOptions::new()
            .with_row_count(Some(batch.num_rows())),
    )
    .map_err(|e| MorunaError::Plan(format!("boundary copy: {e}")))?;
    Ok(Landed {
        payload: Payload::table_with(copy, alloc)?,
        copied_bytes: copied,
    })
}

/// True when every buffer of every column already lies in a region this allocator owns, which is
/// the case when the kernel returned a slice of what it was given.
fn batch_is_arena_owned(batch: &RecordBatch, alloc: &dyn Allocator) -> bool {
    let mut any = false;
    for column in batch.columns() {
        if !data_is_arena_owned(&column.to_data(), alloc, &mut any) {
            return false;
        }
    }
    any
}

fn data_is_arena_owned(data: &ArrayData, alloc: &dyn Allocator, any: &mut bool) -> bool {
    if let Some(nulls) = data.nulls() {
        *any = true;
        if !alloc.contains(nulls.buffer().as_ptr()) {
            return false;
        }
    }
    for buffer in data.buffers() {
        *any = true;
        if !alloc.contains(buffer.as_ptr()) {
            return false;
        }
    }
    for child in data.child_data() {
        if !data_is_arena_owned(child, alloc, any) {
            return false;
        }
    }
    true
}

fn copy_array_data(
    data: &ArrayData,
    alloc: &dyn Allocator,
    tier: Tier,
    copied: &mut u64,
) -> Result<ArrayData> {
    let mut builder = ArrayData::builder(data.data_type().clone())
        .len(data.len())
        .offset(data.offset());
    if let Some(nulls) = data.nulls() {
        let bits = copy_buffer(nulls.buffer(), alloc, tier, copied)?;
        let boolean = BooleanBuffer::new(bits, nulls.offset(), nulls.len());
        builder = builder.nulls(Some(NullBuffer::new(boolean)));
    }
    for buffer in data.buffers() {
        builder = builder.add_buffer(copy_buffer(buffer, alloc, tier, copied)?);
    }
    for child in data.child_data() {
        builder = builder.add_child_data(copy_array_data(child, alloc, tier, copied)?);
    }
    builder
        .build()
        .map_err(|e| MorunaError::Plan(format!("boundary copy: rebuilding an array: {e}")))
}

/// Copy one Arrow buffer into an arena buffer of the same length.
fn copy_buffer(
    source: &ArrowBuffer,
    alloc: &dyn Allocator,
    tier: Tier,
    copied: &mut u64,
) -> Result<ArrowBuffer> {
    let bytes = source.as_slice();
    let mut buffer = alloc.alloc(bytes.len().max(1), tier)?;
    buffer[..bytes.len()].copy_from_slice(bytes);
    *copied += bytes.len() as u64;
    Ok(buffer
        .into_arrow_buffer()?
        .slice_with_length(0, bytes.len()))
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
