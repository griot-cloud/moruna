//! The safetensors header (e.3).
//!
//! `safetensors::SafeTensors::read_metadata` refuses any buffer that is not the whole file
//! (it ends with `buffer_end + 8 + header_len != buffer_len`), so a source cannot use it to
//! plan a model file without reading the model. The header is JSON and is parsed here with
//! `serde_json` over the header bytes alone; the tensor entries are exactly the crate's own
//! `dtype`, `shape` and `data_offsets` fields. Reported as an escalation.

use amoru_kernel::{AmoruError, DType, Result};

use super::Entry;
use crate::tensor::plan::read_prefix;
use crate::util::plan_err;

/// The length prefix in front of a safetensors header.
const LEN_BYTES: u64 = 8;

/// Every tensor of a safetensors file, in data-offset order (e.3).
pub(crate) fn plan(path: &std::path::Path, len: u64, prefix: &[u8]) -> Result<Vec<Entry>> {
    let bad = |msg: String| AmoruError::Io {
        op: "safetensors",
        target: path.display().to_string(),
        msg,
    };
    if (prefix.len() as u64) < LEN_BYTES {
        return Err(bad(format!("file of {len} bytes has no header length")));
    }
    let mut n = [0u8; 8];
    n.copy_from_slice(&prefix[..8]);
    let header_len = u64::from_le_bytes(n);
    let data_start = LEN_BYTES
        .checked_add(header_len)
        .ok_or_else(|| bad(format!("header length {header_len} overflows")))?;
    if data_start > len {
        return Err(bad(format!(
            "header claims {header_len} bytes but the file is {len} bytes"
        )));
    }
    // The probe may not have reached the end of a long header; read the rest (e.3).
    let header: Vec<u8> = if (prefix.len() as u64) >= data_start {
        prefix[8..data_start as usize].to_vec()
    } else {
        read_prefix(path, LEN_BYTES, header_len)?
    };
    let text =
        std::str::from_utf8(&header).map_err(|e| bad(format!("the header is not UTF-8: {e}")))?;
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| bad(format!("the header is not JSON: {e}")))?;
    let object = json
        .as_object()
        .ok_or_else(|| bad("the header is not a JSON object".to_string()))?;

    let mut entries = Vec::new();
    for (name, value) in object {
        if name == "__metadata__" {
            continue;
        }
        let info = value
            .as_object()
            .ok_or_else(|| bad(format!("tensor {name} is not an object")))?;
        let dtype_text = info
            .get("dtype")
            .and_then(|v| v.as_str())
            .ok_or_else(|| bad(format!("tensor {name} has no dtype")))?;
        let dtype = dtype_of(dtype_text).ok_or_else(|| {
            plan_err(format!(
                "{}: tensor {name} has dtype {dtype_text}, which Amoru has no DType for (contracts d.4)",
                path.display()
            ))
        })?;
        let shape = info
            .get("shape")
            .and_then(|v| v.as_array())
            .ok_or_else(|| bad(format!("tensor {name} has no shape")))?
            .iter()
            .map(|d| {
                d.as_i64()
                    .ok_or_else(|| bad(format!("tensor {name} has a non-integer dimension")))
            })
            .collect::<Result<Vec<i64>>>()?;
        if shape.iter().any(|d| *d < 0) {
            return Err(bad(format!("tensor {name} has a negative dimension")));
        }
        let offsets = info
            .get("data_offsets")
            .and_then(|v| v.as_array())
            .ok_or_else(|| bad(format!("tensor {name} has no data_offsets")))?;
        let start = offsets
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| bad(format!("tensor {name} has no start offset")))?;
        let end = offsets
            .get(1)
            .and_then(|v| v.as_u64())
            .ok_or_else(|| bad(format!("tensor {name} has no end offset")))?;
        let entry = Entry {
            path: path.to_path_buf(),
            name: name.clone(),
            dtype,
            shape,
            data_offset: data_start + start,
            file_len: len,
        };
        if end < start || end - start != entry.bytes() {
            return Err(bad(format!(
                "tensor {name} spans {} bytes but its shape needs {}",
                end.saturating_sub(start),
                entry.bytes()
            )));
        }
        entries.push(entry);
    }
    entries.sort_by_key(|e| e.data_offset);
    if entries.is_empty() {
        return Err(bad("the header names no tensor".to_string()));
    }
    Ok(entries)
}

/// The `DType` a safetensors dtype string names; `None` for one Amoru has no element type for
/// (the 4-, 6- and 8-bit float types, and the complex types).
fn dtype_of(text: &str) -> Option<DType> {
    Some(match text {
        "BOOL" => DType::Bool,
        "U8" => DType::U8,
        "I8" => DType::I8,
        "I16" => DType::I16,
        "U16" => DType::U16,
        "F16" => DType::F16,
        "BF16" => DType::BF16,
        "I32" => DType::I32,
        "U32" => DType::U32,
        "F32" => DType::F32,
        "F64" => DType::F64,
        "I64" => DType::I64,
        "U64" => DType::U64,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_amoru_dtype_has_a_safetensors_name() {
        for dtype in DType::ALL {
            let name = format!("{dtype:?}").to_ascii_uppercase();
            assert_eq!(dtype_of(&name), Some(dtype), "{name}");
        }
        assert_eq!(dtype_of("F8_E5M2"), None);
    }
}
