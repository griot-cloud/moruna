//! A Moruna image archive (system design 3E and 5C; PM, items 5.4 and 3.6): an image as the one
//! file a node registers, fetches and runs in place.
//!
//! The image's layers are converted on the client, once, into the layer files Moruna's image
//! filesystem serves ([`crate::layerfile`]: sealed, every file's data from a page boundary), and
//! the archive is those files after a small index:
//!
//! ```text
//! 0      MAGIC "MRNIMAG1"
//! 8      the index's length in bytes, u32 little-endian
//! 12     zero, u32
//! 16     the index: JSON, {"moruna_image_archive": 1, "record": RECORD, "layers": [SPAN...]}
//! ...    zeros to the next page
//! PAGE*k each layer file, lowest first, one after another, each whole pages
//! ```
//!
//! `RECORD` is the image's record ([`crate::record::Record`]: its manifest and config byte for
//! byte, and each layer's digest and the seal of its file); each `SPAN` is
//! `{"offset": BYTES, "pages": N}`, where that layer's file lies. Nothing else is in the file:
//! the first layer starts at the first page after the index, each next one where the one below
//! it ends, and the file ends where the last one does, so every byte is accounted for and one
//! image is one file, the same bytes every time ([`write`]).
//!
//! On a node nothing is converted: [`Archive::parse`] reads the index and checks every layer's
//! header against its seal and its span, and the image filesystem mounts each span of the
//! archive's memory object as a layer, in place. The archive's SHA-256 is what the operator
//! signs and the node verifies before any of this (3E).

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use moruna_imagefs::format::{Header, PAGE};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::digest::Digest;
use crate::error::{Error, Result};
use crate::record::{Record, Verified};
use crate::source::ImageSource;

/// An archive's first eight bytes.
pub const MAGIC: [u8; 8] = *b"MRNIMAG1";
/// The index's format.
pub const VERSION: u32 = 1;
/// Where the index starts: after the magic, its length and a zero word.
pub const INDEX_AT: usize = 16;
/// The longest index read: a manifest and config of any real image fit many times over.
pub const INDEX_MAX: usize = 1 << 20;

const PAGE_U64: u64 = PAGE as u64;

/// Where one layer file lies in the archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    /// Its first byte's offset: a whole number of pages.
    pub offset: u64,
    /// Its length in pages.
    pub pages: u64,
}

impl Span {
    /// Where it ends.
    pub fn end(&self) -> u64 {
        self.offset + self.pages * PAGE_U64
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    moruna_image_archive: u32,
    record: Record,
    layers: Vec<Span>,
}

/// One layer of a parsed archive: where its file lies, and its seal, checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer {
    /// Where its file lies.
    pub span: Span,
    /// Its seal: the SHA-256 of its header page, which the image filesystem checks again when
    /// it mounts the layer.
    pub seal: [u8; 32],
}

/// An archive whose index, layer headers and image identity [`Archive::parse`] checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Archive {
    /// The image's record, as the index carries it.
    pub record: Record,
    /// The image: its manifest's digest and its config.
    pub image: Verified,
    /// Its layers, lowest first.
    pub layers: Vec<Layer>,
}

fn refuse(why: impl Into<String>) -> Error {
    Error::Format(why.into())
}

/// The first page boundary at or after `at`.
fn page_up(at: u64) -> u64 {
    at.div_ceil(PAGE_U64) * PAGE_U64
}

/// Whether `bytes` begin as an archive does: what a node looks at to tell an image function from
/// a native one (PM, item 3.6: content decides).
pub fn is_archive(bytes: &[u8]) -> bool {
    bytes.len() >= MAGIC.len() && bytes[..MAGIC.len()] == MAGIC
}

impl Archive {
    /// The archive that is `bytes`, every byte of them, checked: its magic and index, each
    /// layer's span (page-aligned, one after another from the first page after the index, the
    /// file ending where the last ends), each layer's header (a layer file's, of as many pages
    /// as its span, sealed as the record says), and the record itself ([`Record::verify`]: the
    /// manifest names the config and the layers it holds). Every refusal says what is wrong.
    pub fn parse(bytes: &[u8]) -> Result<Archive> {
        if !is_archive(bytes) {
            return Err(refuse(
                "not a Moruna image archive: its first eight bytes are not MRNIMAG1",
            ));
        }
        if bytes.len() < INDEX_AT {
            return Err(refuse("the archive ends inside its header"));
        }
        let length = u32::from_le_bytes(bytes[8..12].try_into().expect("four bytes")) as usize;
        if bytes[12..16] != [0; 4] {
            return Err(refuse("the archive's header has a nonzero reserved word"));
        }
        if length > INDEX_MAX || INDEX_AT + length > bytes.len() {
            return Err(refuse(format!(
                "the archive's index is {length} bytes, more than the {} it holds or the \
                 {INDEX_MAX} an index may be",
                bytes.len().saturating_sub(INDEX_AT)
            )));
        }
        let index: Index = serde_json::from_slice(&bytes[INDEX_AT..INDEX_AT + length])
            .map_err(|e| refuse(format!("the archive's index is not one: {e}")))?;
        if index.moruna_image_archive != VERSION {
            return Err(refuse(format!(
                "an archive of version {}; this build reads {VERSION}",
                index.moruna_image_archive
            )));
        }
        let record = index.record;
        if record.moruna_image_record != crate::record::VERSION {
            return Err(refuse(format!(
                "an image record of version {}; this build reads {}",
                record.moruna_image_record,
                crate::record::VERSION
            )));
        }
        if index.layers.len() != record.layers.len() {
            return Err(refuse(format!(
                "the index places {} layer files for a record of {} layers",
                index.layers.len(),
                record.layers.len()
            )));
        }
        // Every byte accounted for: zeros to the first layer, the layers end to end, nothing
        // after the last.
        let index_end = (INDEX_AT + length) as u64;
        let mut at = page_up(index_end);
        if bytes[index_end as usize..(at as usize).min(bytes.len())]
            .iter()
            .any(|b| *b != 0)
        {
            return Err(refuse(
                "the archive holds bytes between its index and its first layer",
            ));
        }
        let mut layers = Vec::with_capacity(index.layers.len());
        for (i, span) in index.layers.iter().enumerate() {
            if span.offset != at {
                return Err(refuse(format!(
                    "layer {i} is placed at {}, and the archive's next page is {at}",
                    span.offset
                )));
            }
            if span.pages == 0
                || span
                    .pages
                    .checked_mul(PAGE_U64)
                    .and_then(|len| span.offset.checked_add(len))
                    .is_none_or(|end| end > bytes.len() as u64)
            {
                return Err(refuse(format!(
                    "layer {i} is {} pages at {}, past the archive's {} bytes",
                    span.pages,
                    span.offset,
                    bytes.len()
                )));
            }
            let start = span.offset as usize;
            let (header, geometry) = Header::parse(&bytes[start..start + PAGE])
                .map_err(|e| refuse(format!("layer {i}'s header: {e:?}")))?;
            if geometry.pages != span.pages {
                return Err(refuse(format!(
                    "layer {i}'s header says it is {} pages, and the index gives it {}",
                    geometry.pages, span.pages
                )));
            }
            let seal = header
                .seal()
                .map_err(|e| refuse(format!("layer {i}'s header: {e:?}")))?;
            layers.push(Layer { span: *span, seal });
            at = span.end();
        }
        if at != bytes.len() as u64 {
            return Err(refuse(format!(
                "the archive is {} bytes, and its last layer ends at {at}",
                bytes.len()
            )));
        }
        let seals: Vec<[u8; 32]> = layers.iter().map(|l| l.seal).collect();
        let digest = Digest::of(record.manifest.as_bytes());
        let image = record.verify(&digest.to_string(), &seals)?;
        Ok(Archive {
            record,
            image,
            layers,
        })
    }
}

/// An archive written: where, its SHA-256 (what is signed and registered), its length, and the
/// image's manifest digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    /// Where it is.
    pub path: PathBuf,
    /// The SHA-256 of all its bytes.
    pub sha256: [u8; 32],
    /// Its length in bytes.
    pub size: u64,
    /// The image's manifest digest.
    pub image: Digest,
}

impl Written {
    /// The SHA-256 as 64 lowercase hex digits.
    pub fn sha256_hex(&self) -> String {
        self.sha256.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// A writer that hashes and counts what passes.
struct Hashing<W> {
    inner: W,
    hash: Sha256,
    size: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn io_at<'a>(path: &'a Path, what: &'static str) -> impl Fn(io::Error) -> Error + 'a {
    move |source| Error::Io {
        what: format!("{what} {}", path.display()),
        source,
    }
}

/// The image `reference` names in `source`, converted ([`crate::layerfile::convert`]) and
/// written as the archive `out`, which must not exist; the layer files are made beside it and
/// removed. The same image is the same bytes every time.
pub fn write(source: &dyn ImageSource, reference: &str, out: &Path) -> Result<Written> {
    if out.symlink_metadata().is_ok() {
        return Err(Error::Io {
            what: format!("making {}", out.display()),
            source: io::Error::from(io::ErrorKind::AlreadyExists),
        });
    }
    let parts = out.with_extension("layers");
    let result = write_with(source, reference, out, &parts);
    let _ = fs::remove_dir_all(&parts);
    // What this call made of `out`, which was not there, goes with its failure.
    if result.is_err() {
        let _ = fs::remove_file(out);
    }
    result
}

fn write_with(
    source: &dyn ImageSource,
    reference: &str,
    out: &Path,
    parts: &Path,
) -> Result<Written> {
    let converted = crate::layerfile::convert(source, reference, parts)?;
    let (image, manifest) = crate::derive::manifest_bytes(source, reference)?;
    let config = crate::derive::config_bytes(source, &image, &manifest)?;
    let seals: Vec<[u8; 32]> = converted.layers.iter().map(|l| l.id.bytes()).collect();
    let record = Record::new(&manifest, &config, &seals)?;
    // The index's length depends on the offsets, which depend on its length: placed after the
    // longest index these offsets could make, then laid out again until it holds still.
    let mut first = PAGE_U64;
    let index = loop {
        let mut at = first;
        let mut spans = Vec::with_capacity(converted.layers.len());
        for layer in &converted.layers {
            spans.push(Span {
                offset: at,
                pages: layer.pages,
            });
            at += layer.pages * PAGE_U64;
        }
        let index = serde_json::to_vec(&Index {
            moruna_image_archive: VERSION,
            record: record.clone(),
            layers: spans,
        })
        .expect("an index of strings and numbers serializes");
        let wanted = page_up((INDEX_AT + index.len()) as u64);
        if wanted == first {
            break index;
        }
        first = wanted;
    };
    if index.len() > INDEX_MAX {
        return Err(refuse(format!(
            "the image's index is {} bytes, more than the {INDEX_MAX} an archive's may be",
            index.len()
        )));
    }
    let file = File::options()
        .write(true)
        .create_new(true)
        .open(out)
        .map_err(io_at(out, "making"))?;
    let mut writer = Hashing {
        inner: BufWriter::new(file),
        hash: Sha256::new(),
        size: 0,
    };
    let fail = io_at(out, "writing");
    writer.write_all(&MAGIC).map_err(&fail)?;
    writer
        .write_all(&(index.len() as u32).to_le_bytes())
        .map_err(&fail)?;
    writer.write_all(&[0; 4]).map_err(&fail)?;
    writer.write_all(&index).map_err(&fail)?;
    let pad = first as usize - INDEX_AT - index.len();
    writer.write_all(&vec![0; pad]).map_err(&fail)?;
    for layer in &converted.layers {
        let mut file = File::open(&layer.path).map_err(io_at(&layer.path, "reading"))?;
        let copied = io::copy(&mut (&mut file).take(layer.pages * PAGE_U64), &mut writer)
            .map_err(io_at(&layer.path, "copying"))?;
        if copied != layer.pages * PAGE_U64 {
            return Err(refuse(format!(
                "the layer file {} is {copied} bytes, not the {} pages it was made",
                layer.path.display(),
                layer.pages
            )));
        }
    }
    writer.flush().map_err(&fail)?;
    Ok(Written {
        path: out.to_path_buf(),
        sha256: writer.hash.finalize().into(),
        size: writer.size,
        image,
    })
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
