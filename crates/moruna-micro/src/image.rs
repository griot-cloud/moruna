//! The image an `image` stage runs on, as the kernel attached it (SDD 30 f.12): the record the
//! launcher wrote beside its layers, which the engine is granted as the data handle `image`,
//! and the seals of the layers the personality's image filesystem mounted, which the
//! personality's `layers` (`moruna.linux` 3.0) says. `moruna_image::record::Record::verify`
//! checks one against the other and against the entry's reference; this module only fetches
//! them, once a process.

use std::sync::OnceLock;

#[path = "image_layers.rs"]
mod layer_query;
use moruna_sys::{Memory, map};

use crate::layout::PAGE;

/// The largest record read: a manifest and a config are a few kilobytes.
const RECORD_MAX: u64 = 256 * 1024;
const HELLO_NS: u64 = 120_000_000_000;

/// What the kernel attached.
#[derive(Debug)]
pub struct Attached {
    /// The record's bytes (`moruna_image::record::Record`).
    pub record: Vec<u8>,
    /// The seals of the layers the image's tasks start on, lowest first.
    pub seals: Vec<[u8; 32]>,
}

/// The attached image, read the first time a stage asks; every stage after is told the same,
/// or the same refusal.
pub fn attached() -> Result<&'static Attached, String> {
    static ATTACHED: OnceLock<Result<Attached, String>> = OnceLock::new();
    ATTACHED
        .get_or_init(|| {
            Ok(Attached {
                record: record()?,
                seals: seals()?,
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The data handle `image`: its text, which ends at its first zero byte (a file of the boot's,
/// padded to whole pages).
fn record() -> Result<Vec<u8>, String> {
    let memory = Memory(crate::start::take("image").ok_or_else(|| {
        format!(
            "the engine was granted no `image`, the record of the image the kernel attached, so \
             no image's identity can be checked (it holds {:?})",
            crate::start::slots()
        )
    })?);
    let mut out = Vec::new();
    let mut page = vec![0u8; PAGE as usize];
    let mut at = 0;
    while at < RECORD_MAX {
        let mapping = memory.map(at, PAGE, map::READ).map_err(|e| {
            format!(
                "the image's record has no zero byte in its {at} bytes (mapping on: {})",
                e.name()
            )
        })?;
        mapping
            .read(0, &mut page)
            .map_err(|e| format!("reading the image's record: {}", e.name()))?;
        match page.iter().position(|b| *b == 0) {
            Some(end) => {
                out.extend_from_slice(&page[..end]);
                return Ok(out);
            }
            None => out.extend_from_slice(&page),
        }
        at += PAGE;
    }
    Err(format!(
        "the image's record is larger than {RECORD_MAX} bytes"
    ))
}

/// The personality's `layers`, on a connection of its own.
fn seals() -> Result<Vec<[u8; 32]>, String> {
    let listener = crate::task::personality()?;
    layer_query::query(listener, moruna_svc::deadline_in(HELLO_NS))
}
