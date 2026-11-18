//! A record a task put, read where it lies: the batch an image stage hands on when it keeps
//! the task's answer in its window instead of copying it into the run's arena
//! ([`crate::layout::keep_in_place`]). Apart from the kernel's calls, so every target tests it;
//! `rings::payload_over` maps the record's pages and holds them for the batch.

use moruna_kernel::arrow::buffer::Buffer as ArrowBuffer;
use moruna_kernel::arrow::record_batch::RecordBatch;

use crate::layout::PAGE;

/// The record `record` holds (as `moruna_kernel::ipc::encode_framing` lays one out), decoded
/// where it lies: no byte is copied, every array points into `record`, and `record`'s memory
/// is kept until the last of the batch's buffers is dropped.
pub fn decode(record: ArrowBuffer) -> moruna_kernel::Result<RecordBatch> {
    moruna_kernel::ipc::decode(record, PAGE as usize)
}
