//! The NumPy `.npy` header (e.3).
//!
//! The header is a Python dict literal, not JSON, so it is parsed here by hand. Only the three
//! keys the format defines are read: `descr`, `fortran_order` and `shape`. A Fortran-ordered
//! file is a `Plan` error at `new`: the tensor would be non-contiguous (contracts b).

use moruna_kernel::{MorunaError, DType, Result};

use super::Entry;
use crate::tensor::plan::read_prefix;
use crate::util::plan_err;

const MAGIC: &[u8] = b"\x93NUMPY";

/// The one tensor a `.npy` file holds (e.3).
pub(crate) fn plan(path: &std::path::Path, len: u64, prefix: &[u8]) -> Result<Vec<Entry>> {
    let bad = |msg: String| MorunaError::Io {
        op: "npy",
        target: path.display().to_string(),
        msg,
    };
    if prefix.len() < 10 || !prefix.starts_with(MAGIC) {
        return Err(bad(
            "the file does not start with the NumPy magic".to_string()
        ));
    }
    let major = prefix[6];
    let (header_len, header_start) = match major {
        1 => (u64::from(u16::from_le_bytes([prefix[8], prefix[9]])), 10u64),
        2 | 3 => {
            if prefix.len() < 12 {
                return Err(bad(
                    "the file is shorter than a version 2 header".to_string()
                ));
            }
            let mut n = [0u8; 4];
            n.copy_from_slice(&prefix[8..12]);
            (u64::from(u32::from_le_bytes(n)), 12u64)
        }
        other => return Err(bad(format!("unknown .npy major version {other}"))),
    };
    let data_offset = header_start
        .checked_add(header_len)
        .ok_or_else(|| bad(format!("header length {header_len} overflows")))?;
    if data_offset > len {
        return Err(bad(format!(
            "header claims {header_len} bytes but the file is {len} bytes"
        )));
    }
    let header: Vec<u8> = if (prefix.len() as u64) >= data_offset {
        prefix[header_start as usize..data_offset as usize].to_vec()
    } else {
        read_prefix(path, header_start, header_len)?
    };
    let text =
        std::str::from_utf8(&header).map_err(|e| bad(format!("the header is not UTF-8: {e}")))?;

    let descr = field(text, "descr").ok_or_else(|| bad("the header has no descr".to_string()))?;
    let descr = descr.trim().trim_matches(|c| c == '\'' || c == '"');
    let dtype = dtype_of(descr).ok_or_else(|| {
        plan_err(format!(
            "{}: dtype {descr} has no Moruna DType (contracts d.4), or is not little endian",
            path.display()
        ))
    })?;
    let fortran = field(text, "fortran_order")
        .ok_or_else(|| bad("the header has no fortran_order".to_string()))?;
    if fortran.trim() != "False" {
        return Err(plan_err(format!(
            "{}: the array is in Fortran order, which is not contiguous (contracts b)",
            path.display()
        )));
    }
    let shape_text =
        field(text, "shape").ok_or_else(|| bad("the header has no shape".to_string()))?;
    let shape = parse_shape(shape_text)
        .ok_or_else(|| bad(format!("the shape {shape_text} does not parse")))?;

    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("array")
        .to_string();
    Ok(vec![Entry {
        path: path.to_path_buf(),
        name,
        dtype,
        shape,
        data_offset,
        file_len: len,
    }])
}

/// The text of one key of the Python dict literal, up to the comma that ends its value.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    for quote in ['\'', '"'] {
        let needle = format!("{quote}{key}{quote}:");
        if let Some(start) = text.find(&needle) {
            let rest = &text[start + needle.len()..];
            let mut depth = 0usize;
            for (i, c) in rest.char_indices() {
                match c {
                    '(' | '[' => depth += 1,
                    ')' | ']' => depth = depth.saturating_sub(1),
                    ',' if depth == 0 => return Some(&rest[..i]),
                    '}' if depth == 0 => return Some(&rest[..i]),
                    _ => {}
                }
            }
            return Some(rest);
        }
    }
    None
}

/// A Python tuple of non-negative integers, `()` included (a rank 0 array).
fn parse_shape(text: &str) -> Option<Vec<i64>> {
    let inner = text.trim().strip_prefix('(')?.strip_suffix(')')?;
    let mut out = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(part.parse::<i64>().ok().filter(|d| *d >= 0)?);
    }
    Some(out)
}

/// The `DType` a NumPy type string names. Big-endian (`>`) types are rejected: the runtime
/// reads bytes as they lie and never byte-swaps.
fn dtype_of(descr: &str) -> Option<DType> {
    let (order, rest) = descr.split_at_checked(1)?;
    if order == ">" {
        return None;
    }
    let code = if order == "<" || order == "|" || order == "=" {
        rest
    } else {
        descr
    };
    Some(match code {
        "i1" | "b" => DType::I8,
        "i2" => DType::I16,
        "i4" => DType::I32,
        "i8" => DType::I64,
        "u1" => DType::U8,
        "u2" => DType::U16,
        "u4" => DType::U32,
        "u8" => DType::U64,
        "f2" => DType::F16,
        "f4" => DType::F32,
        "f8" => DType::F64,
        "b1" | "?" => DType::Bool,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str =
        "{'descr': '<f4', 'fortran_order': False, 'shape': (3, 2), }                ";

    #[test]
    fn fields_are_found_and_parsed() {
        assert_eq!(field(HEADER, "descr"), Some(" '<f4'"));
        assert_eq!(field(HEADER, "fortran_order"), Some(" False"));
        assert_eq!(field(HEADER, "shape"), Some(" (3, 2)"));
        assert_eq!(field(HEADER, "missing"), None);
        assert_eq!(parse_shape(" (3, 2)"), Some(vec![3, 2]));
        assert_eq!(parse_shape("()"), Some(Vec::new()));
        assert_eq!(parse_shape("(5,)"), Some(vec![5]));
        assert_eq!(parse_shape("(-1,)"), None);
        assert_eq!(parse_shape("3, 2"), None);
        assert_eq!(field("{\"descr\": \"<f8\"}", "descr"), Some(" \"<f8\""));
    }

    #[test]
    fn dtypes_map_by_code_and_reject_big_endian() {
        assert_eq!(dtype_of("<f4"), Some(DType::F32));
        assert_eq!(dtype_of("|b1"), Some(DType::Bool));
        assert_eq!(dtype_of("=i8"), Some(DType::I64));
        assert_eq!(dtype_of("f8"), Some(DType::F64));
        assert_eq!(dtype_of(">f4"), None);
        assert_eq!(dtype_of("<U8"), None);
        assert_eq!(dtype_of(""), None);
    }
}
