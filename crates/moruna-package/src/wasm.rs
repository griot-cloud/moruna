//! A component's `moruna.json` custom section (16 e.5, f.3).
//!
//! Only the component's own, top-level sections are read and written: a section inside a nested
//! core module is the module's. The binary format is walked by hand (a header, then sections of
//! an id byte, an unsigned LEB128 size and the content), which is all this needs.

use crate::{MANIFEST_NAME, Manifest, PackageError};

/// `\0asm`.
const MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6d];
/// The component model's version and layer (`0x0d 0x00`, layer 1).
const COMPONENT_VERSION: [u8; 4] = [0x0d, 0x00, 0x01, 0x00];
/// The custom section's id.
const CUSTOM: u8 = 0;

fn bad(reason: impl Into<String>) -> PackageError {
    PackageError::format("wasm", reason)
}

fn read_leb(bytes: &[u8], at: &mut usize) -> Result<u64, PackageError> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes
            .get(*at)
            .ok_or_else(|| bad("the component ends inside a section header"))?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            return Err(bad("a section size does not fit 64 bits"));
        }
    }
}

fn write_leb(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// One top-level section: its id, the range of the whole section, and for a custom section its
/// name and the range of its data.
struct Section {
    id: u8,
    whole: std::ops::Range<usize>,
    custom: Option<(String, std::ops::Range<usize>)>,
}

fn sections(bytes: &[u8]) -> Result<Vec<Section>, PackageError> {
    if bytes.len() < 8 || bytes[..4] != MAGIC {
        return Err(bad("not a WebAssembly binary"));
    }
    if bytes[4..8] != COMPONENT_VERSION {
        return Err(bad(
            "a core module, not a component: a .wasm package is a component of the moruna:kernel \
             world",
        ));
    }
    let mut out = Vec::new();
    let mut at = 8;
    while at < bytes.len() {
        let start = at;
        let id = bytes[at];
        at += 1;
        let size = read_leb(bytes, &mut at)? as usize;
        let end = at
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| bad("a section runs past the end of the component"))?;
        let custom = if id == CUSTOM {
            let mut name_at = at;
            let len = read_leb(bytes, &mut name_at)? as usize;
            let name_end = name_at
                .checked_add(len)
                .filter(|e| *e <= end)
                .ok_or_else(|| bad("a custom section's name runs past its section"))?;
            let name = std::str::from_utf8(&bytes[name_at..name_end])
                .map_err(|_| bad("a custom section's name is not UTF-8"))?
                .to_string();
            Some((name, name_end..end))
        } else {
            None
        };
        out.push(Section {
            id,
            whole: start..end,
            custom,
        });
        at = end;
    }
    Ok(out)
}

/// The component's `moruna.json` section, `None` when it has none; more than one is refused.
pub fn wasm_manifest(component: &[u8]) -> Result<Option<Vec<u8>>, PackageError> {
    let mut found = None;
    for section in sections(component)? {
        if let Some((name, data)) = section.custom
            && name == MANIFEST_NAME
        {
            if found.is_some() {
                return Err(PackageError::format(
                    MANIFEST_NAME,
                    "the component has more than one moruna.json section",
                ));
            }
            found = Some(component[data].to_vec());
        }
    }
    Ok(found)
}

/// The component with every `moruna.json` section removed and `manifest` appended as one
/// (16 f.3).
pub fn embed_manifest(component: &[u8], manifest: &Manifest) -> Result<Vec<u8>, PackageError> {
    let mut out = Vec::with_capacity(component.len() + 4096);
    out.extend_from_slice(&component[..8.min(component.len())]);
    for section in sections(component)? {
        let ours = matches!(&section.custom, Some((name, _)) if name == MANIFEST_NAME);
        if !ours {
            out.extend_from_slice(&component[section.whole]);
        }
        let _ = section.id;
    }
    let text = manifest.to_json();
    let mut content = Vec::with_capacity(text.len() + 16);
    write_leb(MANIFEST_NAME.len() as u64, &mut content);
    content.extend_from_slice(MANIFEST_NAME.as_bytes());
    content.extend_from_slice(text.as_bytes());
    out.push(CUSTOM);
    write_leb(content.len() as u64, &mut out);
    out.extend_from_slice(&content);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb_round_trips() {
        for value in [
            0u64,
            1,
            127,
            128,
            300,
            1 << 20,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            let mut out = Vec::new();
            write_leb(value, &mut out);
            let mut at = 0;
            assert_eq!(read_leb(&out, &mut at).unwrap(), value);
            assert_eq!(at, out.len());
        }
        let mut at = 0;
        assert!(read_leb(&[0x80], &mut at).is_err());
        let mut at = 0;
        assert!(read_leb(&[0xff; 11], &mut at).is_err());
    }
}
