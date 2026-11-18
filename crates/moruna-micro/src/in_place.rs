//! A record a task put, read where it lies: the batch an image stage hands on when it keeps
//! the task's answer in its window instead of copying it into the run's arena
//! ([`crate::layout::keep_in_place`]). Apart from the kernel's calls, so every target tests it;
//! `rings::payload_over` maps the record's pages and holds them for the batch.

use std::ptr::NonNull;
use std::sync::Arc;

use moruna_kernel::MorunaError;
use moruna_kernel::arrow::array::{ArrayData, ArrayRef, make_array};
use moruna_kernel::arrow::buffer::{BooleanBuffer, Buffer as ArrowBuffer, NullBuffer};
use moruna_kernel::arrow::record_batch::{RecordBatch, RecordBatchOptions};

use crate::layout::PAGE;

/// The record `record` holds (as `moruna_kernel::ipc::encode_framing` lays one out), decoded
/// where it lies: no byte is copied, every array points into `record`, and `record`'s memory
/// is kept until the last of the batch's buffers is dropped.
///
/// Each buffer of the batch is an Arrow allocation of its own, over its own bytes of the
/// record, so the batch counts its bytes once. Arrow counts a buffer as the whole allocation it
/// is a slice of (`Buffer::capacity`), and a table's bytes are Arrow's count (`Payload::bytes`,
/// contracts CT-I3): its morsel's charge, its stage's `bytes_out`, the growth reads are cut by
/// and the controller's amplification. Decoded as slices of the one record, an answer of `k`
/// buffers counted `k` times the record's length, where the same answer copied into the arena
/// counted its bytes: Enrich's reports said 14.8 to 18.7 bytes out per byte in for an output
/// 1.17 times its input, and Score's 0.22 or 0.73 by how many of a run's answers were kept
/// (6.3h, 6.2 run 3).
pub fn decode(record: ArrowBuffer) -> moruna_kernel::Result<RecordBatch> {
    let decoded = moruna_kernel::ipc::decode(record, PAGE as usize)?;
    let columns: Vec<ArrayRef> = decoded
        .columns()
        .iter()
        .map(|column| make_array(counted_once(&column.to_data())))
        .collect();
    let options = RecordBatchOptions::new().with_row_count(Some(decoded.num_rows()));
    RecordBatch::try_new_with_options(decoded.schema(), columns, &options)
        .map_err(|e| MorunaError::Plan(format!("a record read in place: {e}")))
}

/// `data` over the same bytes, with its validity, each of its buffers and its children's each
/// an allocation of its own.
fn counted_once(data: &ArrayData) -> ArrayData {
    let nulls = data.nulls().map(|nulls| {
        let bits = BooleanBuffer::new(own(nulls.buffer()), nulls.offset(), nulls.len());
        NullBuffer::new(bits)
    });
    let builder = ArrayData::builder(data.data_type().clone())
        .len(data.len())
        .offset(data.offset())
        .nulls(nulls)
        .buffers(data.buffers().iter().map(own).collect())
        .child_data(data.child_data().iter().map(counted_once).collect());
    // SAFETY: the type, the length, the offset, the validity, every buffer's bytes and the
    // children are those of `data`, an array the decoder built and checked; only what owns
    // each buffer changed, so what held of `data` holds of this, with no second pass over
    // the rows.
    #[allow(unsafe_code)]
    unsafe {
        builder.build_unchecked()
    }
}

/// `buffer`'s bytes as an Arrow allocation of exactly them, which keeps `buffer`, and so
/// whatever `buffer` keeps, for as long as it or any slice of it lives.
fn own(buffer: &ArrowBuffer) -> ArrowBuffer {
    let bytes = buffer.as_slice();
    let at = NonNull::from(bytes).cast::<u8>();
    // SAFETY: `at` is the first of the `bytes.len()` bytes of `buffer`, which stay allocated
    // and which nothing writes while `buffer` lives (an Arrow buffer is read only); the new
    // allocation's owner is a clone of `buffer`, dropped only with the allocation's last user.
    #[allow(unsafe_code)]
    unsafe {
        ArrowBuffer::from_custom_allocation(at, bytes.len(), Arc::new(buffer.clone()))
    }
}
