//! The tensor plan (e.3): detect the format, parse the header, one `Split` per tensor.
//!
//! Header bytes are read with `std::fs` here, not through the reactor. A reactor read needs a
//! `Buffer`, a `Buffer` can only come from an `Allocator`, and no allocator exists at
//! construction: the arena reaches a source only through the `&dyn Allocator` that `read`
//! receives. The document's d.1 says the headers come through `reactor.read_file_opt`, which
//! cannot be written; that is reported as an escalation and this is the plan-time metadata
//! read d.1 already grants local paths for size and listing. No payload byte is read here.

use moruna_kernel::{MorunaError, Result, SourceSchema, Split, SplitId};

use super::Entry;
use crate::util::plan_err;

/// The bytes read speculatively to find a header (e.3).
pub(crate) const PROBE_BYTES: u64 = 64 * 1024;

/// Which of the two formats a file is.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum Format {
    Safetensors,
    Npy,
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
        return Err(plan_err(format!(
            "{}: MRB1 is not a tensor file format since 0.7; write safetensors or .npy",
            path.display()
        )));
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
        "{}: not a safetensors or .npy file (extension {extension:?}, no known magic)",
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
