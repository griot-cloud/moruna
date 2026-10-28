//! A batch as one Arrow IPC stream, each way (16 e.5).

use moruna_kernel::arrow::buffer::Buffer;
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{MorunaError, Result, ipc};

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
/// several is refused (16 e.5: one in, one out). The bytes are the function's, so every message
/// is checked before arrow-ipc reads it, and a malformed stream is the kernel's error naming the
/// fault, never a panic (contracts CT-I13, 17 f.4).
pub fn read_one(bytes: Vec<u8>) -> Result<RecordBatch> {
    let mut batches = ipc::read_stream(&Buffer::from_vec(bytes))
        .map_err(|e| kernel(format!("the output is not a valid Arrow IPC stream: {e}")))?;
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

#[cfg(test)]
mod tests {
    use moruna_kernel::arrow::ipc::MetadataVersion;
    use moruna_testkit::ipc::{corruptions, panics, small, stream, streams, tally, unknown_type};

    use super::*;

    /// SB-T25: a function's output whose schema names a type id no Arrow type has is the
    /// kernel's error naming the field, not a panic in the IPC reader's schema converter.
    #[test]
    fn sb_t25_unknown_type_refused() {
        let bytes = unknown_type(&stream(&[small()], MetadataVersion::V5));
        let err = read_one(bytes).expect_err("an unknown type is refused");
        assert!(matches!(err, MorunaError::Kernel { .. }), "{err:?}");
        assert!(err.to_string().contains("field \"i\""), "{err}");
    }

    /// SB-T25: every corruption and truncation of valid output streams is an error or a batch.
    #[test]
    fn sb_t25_malformed_output_never_panics() {
        let mut failed = Vec::new();
        for (label, valid) in streams() {
            let cases = corruptions(&valid, 0x5B25_0000 ^ valid.len() as u64, 300);
            for case in panics(cases, read_one) {
                failed.push(format!("{label}, {case}"));
            }
        }
        assert!(failed.is_empty(), "{}", tally(&failed));
    }
}
