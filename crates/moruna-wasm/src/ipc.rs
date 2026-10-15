//! A batch as one Arrow IPC stream, each way (16 e.5).

use moruna_kernel::arrow::buffer::Buffer;
use moruna_kernel::arrow::ipc::reader::StreamDecoder;
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{MorunaError, Result};

fn convert(what: &str, e: impl std::fmt::Display) -> MorunaError {
    MorunaError::Plan(format!("wasm: {what}: {e}"))
}

/// The batch as an IPC stream: the schema, the batch, the end marker.
pub fn write(batch: &RecordBatch) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(batch.get_array_memory_size() + 1024);
    let mut writer = StreamWriter::try_new(&mut out, &batch.schema())
        .map_err(|e| convert("encoding the input", e))?;
    writer
        .write(batch)
        .map_err(|e| convert("encoding the input", e))?;
    writer
        .finish()
        .map_err(|e| convert("encoding the input", e))?;
    drop(writer);
    Ok(out)
}

/// The one batch an IPC stream holds, decoded over the bytes without copying them; none or
/// several is refused (16 e.5: one in, one out).
pub fn read_one(bytes: Vec<u8>) -> Result<RecordBatch> {
    let mut buffer = Buffer::from_vec(bytes);
    let mut decoder = StreamDecoder::new();
    let mut batches = Vec::new();
    while !buffer.is_empty() {
        match decoder
            .decode(&mut buffer)
            .map_err(|e| kernel(format!("the output is not an Arrow IPC stream: {e}")))?
        {
            Some(batch) => batches.push(batch),
            None if buffer.is_empty() => break,
            None => {}
        }
    }
    decoder
        .finish()
        .map_err(|e| kernel(format!("the output stream ends inside a message: {e}")))?;
    match batches.len() {
        1 => Ok(batches.remove(0)),
        n => Err(kernel(format!(
            "the output stream holds {n} record batches and a call returns exactly one"
        ))),
    }
}

fn kernel(msg: String) -> MorunaError {
    MorunaError::Kernel {
        stage: moruna_adapters::UNKNOWN_STAGE,
        seq: moruna_adapters::UNKNOWN_SEQ,
        msg: format!("wasm: {msg}"),
        features: None,
    }
}
