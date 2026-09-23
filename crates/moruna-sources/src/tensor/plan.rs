//! The tensor plan (e.3): detect the format, parse the header, one `Split` per tensor.
//!
//! Header bytes are read with `std::fs` here, not through the reactor. A reactor read needs a
//! `Buffer`, a `Buffer` can only come from an `Allocator`, and no allocator exists at
//! construction: the arena reaches a source only through the `&dyn Allocator` that `read`
//! receives. The document's d.1 says the headers come through `reactor.read_file_opt`, which
//! cannot be written; that is reported as an escalation and this is the plan-time metadata
//! read d.1 already grants local paths for size and listing. No payload byte is read here.

use moruna_kernel::{DType, MorunaError, Result, SourceSchema, Split, SplitId};

use super::Entry;
use crate::util::plan_err;

/// The bytes read speculatively to find a header (e.3).
pub(crate) const PROBE_BYTES: u64 = 64 * 1024;

/// Which of the three formats a file is.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum Format {
    Safetensors,
    Npy,
    Amb1,
}

/// Read at most `len` bytes of `path` from `offset`; a short read at end of file is fine.
pub(crate) fn read_prefix(path: &std::path::Path, offset: u64, len: u64) -> Result<Vec<u8>> {
    use std::io::{Read, Seek};
    let io = |e: std::io::Error| MorunaError::Io {
        op: "read_file",
        target: path.display().to_string(),
        msg: e.to_string(),
    };
    let mut file = std::fs::File::open(path).map_err(io)?;
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset)).map_err(io)?;
    }
    let mut out = vec![0u8; len as usize];
    let mut filled = 0usize;
    while filled < out.len() {
        match file.read(&mut out[filled..]).map_err(io)? {
            0 => break,
            n => filled += n,
        }
    }
    out.truncate(filled);
    Ok(out)
}

/// The length of a local file.
pub(crate) fn file_len(path: &std::path::Path) -> Result<u64> {
    std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| MorunaError::Io {
            op: "metadata",
            target: path.display().to_string(),
            msg: e.to_string(),
        })
}

/// Which format a file is, by extension and then by magic (e.3).
pub(crate) fn detect(path: &std::path::Path, prefix: &[u8]) -> Result<Format> {
    if prefix.starts_with(b"MRB1") {
        return Ok(Format::Amb1);
    }
    if prefix.starts_with(b"\x93NUMPY") {
        return Ok(Format::Npy);
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "safetensors" {
        return Ok(Format::Safetensors);
    }
    Err(plan_err(format!(
        "{}: not a safetensors, .npy or MRB1 file (extension {extension:?}, no known magic)",
        path.display()
    )))
}

/// Every tensor of every path, in path order then file order (e.3).
pub(crate) fn plan(cfg: &super::TensorSourceConfig) -> Result<Vec<Entry>> {
    if cfg.paths.is_empty() {
        return Err(plan_err("a TensorSource needs at least one path"));
    }
    let mut out = Vec::new();
    for path in &cfg.paths {
        let text = path.to_string_lossy();
        if !crate::util::is_local(&text) {
            return Err(plan_err(format!(
                "{text}: TensorSource reads local files only in v1; an object URL is a plan error"
            )));
        }
        let len = file_len(path)?;
        let prefix = read_prefix(path, 0, PROBE_BYTES.min(len.max(1)))?;
        let mut entries = match detect(path, &prefix)? {
            Format::Safetensors => super::safetensors::plan(path, len, &prefix)?,
            Format::Npy => super::npy::plan(path, len, &prefix)?,
            Format::Amb1 => amb1_plan(path, len, &prefix)?,
        };
        if let Some(wanted) = &cfg.tensors {
            entries.retain(|e| wanted.iter().any(|w| w == &e.name));
            for name in wanted {
                if !entries.iter().any(|e| &e.name == name) && cfg.paths.len() == 1 {
                    return Err(plan_err(format!(
                        "{text}: no tensor named {name} in the file"
                    )));
                }
            }
        }
        for entry in &entries {
            if entry.data_offset + entry.bytes() > len {
                return Err(plan_err(format!(
                    "{text}: tensor {} claims bytes to {} but the file is {len} bytes",
                    entry.name,
                    entry.data_offset + entry.bytes()
                )));
            }
        }
        out.extend(entries);
    }
    if out.is_empty() {
        return Err(plan_err("no tensor matched the configuration"));
    }
    Ok(out)
}

/// The `MRB1` header (contracts e.4).
///
/// `mrb1::Header::read` validates that the buffer holds the payload too, so it cannot parse a
/// header out of a 64 KiB probe of a large file. The fields are read here and then handed
/// straight back to the contracts writer: a header that does not reproduce the file's own
/// bytes exactly, padding included, is rejected. The contracts crate stays the authority on
/// the layout, the magic, the version and the offset rules; this is reported as an escalation.
fn amb1_plan(path: &std::path::Path, len: u64, prefix: &[u8]) -> Result<Vec<Entry>> {
    use moruna_kernel::mrb1::Header;
    let bad = |msg: String| MorunaError::Io {
        op: "mrb1",
        target: path.display().to_string(),
        msg,
    };
    if prefix.len() < 16 {
        return Err(bad(format!("file of {len} bytes is shorter than a header")));
    }
    let Some(dtype) = DType::from_code(prefix[6]) else {
        return Err(bad(format!("unknown dtype code {}", prefix[6])));
    };
    let ndim = prefix[7] as usize;
    if ndim > moruna_kernel::mrb1::MAX_NDIM {
        return Err(bad(format!(
            "ndim {ndim} exceeds {}",
            moruna_kernel::mrb1::MAX_NDIM
        )));
    }
    let end = Header::header_end(ndim) as usize;
    if prefix.len() < end {
        return Err(bad(format!(
            "file of {len} bytes is short of the {end}-byte header"
        )));
    }
    let mut shape = Vec::with_capacity(ndim);
    for i in 0..ndim {
        let mut d = [0u8; 8];
        d.copy_from_slice(&prefix[8 + 8 * i..16 + 8 * i]);
        shape.push(i64::from_le_bytes(d));
    }
    let mut offset = [0u8; 8];
    offset.copy_from_slice(&prefix[8 + 8 * ndim..16 + 8 * ndim]);
    let header = Header {
        dtype,
        shape,
        data_offset: u64::from_le_bytes(offset),
    };
    let width = usize::try_from(header.data_offset).unwrap_or(usize::MAX);
    let mut rebuilt = vec![0u8; width];
    header.write(&mut rebuilt)?;
    let Some(actual) = prefix.get(..width) else {
        return Err(bad(format!(
            "file of {len} bytes is short of the {width}-byte header and its padding"
        )));
    };
    if rebuilt != actual {
        return Err(bad(
            "the header does not match the bytes the MRB1 writer would produce".to_string(),
        ));
    }
    if len < header.payload_end() {
        return Err(bad(format!(
            "file of {len} bytes is short of the {} bytes the header claims",
            header.payload_end()
        )));
    }
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("tensor")
        .to_string();
    Ok(vec![Entry {
        path: path.to_path_buf(),
        name,
        dtype: header.dtype,
        shape: header.shape,
        data_offset: header.data_offset,
        file_len: len,
    }])
}

/// One `Split` per entry, ids in plan order (e.3).
pub(crate) fn splits(entries: &[Entry]) -> Vec<Split> {
    entries
        .iter()
        .enumerate()
        .map(|(i, entry)| Split {
            id: i as SplitId,
            rows: entry.rows(),
            uncompressed_bytes: entry.bytes(),
            estimated: false,
            column_bytes: Vec::new(),
            null_counts: Vec::new(),
            sub_splittable: !entry.shape.is_empty(),
        })
        .collect()
}

/// The schema a tensor source declares: the dtype every entry shares and the shape of the
/// first, with a variable batch dimension (contracts d.4).
pub(crate) fn schema(entries: &[Entry]) -> Result<SourceSchema> {
    let first = entries
        .first()
        .ok_or_else(|| plan_err("no tensor matched the configuration"))?;
    for entry in entries {
        if entry.dtype != first.dtype {
            return Err(plan_err(format!(
                "tensors {} and {} have different dtypes ({:?} and {:?}); one source has one schema",
                first.name, entry.name, first.dtype, entry.dtype
            )));
        }
    }
    let mut shape = first.shape.clone();
    if let Some(leading) = shape.first_mut() {
        *leading = -1;
    }
    Ok(SourceSchema::Tensor {
        dtype: first.dtype,
        shape,
    })
}
