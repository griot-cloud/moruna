//! Layers attached in place (system design 3E and 5C; item 3.6): an image's layer files lying
//! end to end in one read-only region, a Moruna image archive's memory object, each mounted
//! where it lies.
//!
//! Nothing is converted and the region is never copied whole: an [`AttachedDisk`] is the
//! [`Disk`] behind a [`Cache`] whose "devices" are spans of the region, so a page is read
//! from the region into a cache slot only when something touches it, and believed only once
//! it matches its hash, as a block device's page is. [`attach`] mounts every span, lowest
//! first, and refuses one whose header's seal is not the one the archive's index gave it.
//!
//! A file's pages that lie one after another in a layer are mapped where they lie, with no copy
//! ([`in_place`]): each is checked against its hash first, once, since nothing writes the
//! region.

use alloc::{format, vec, vec::Vec};

use crate::cache::{Cache, Disk, Fetch};
use crate::error::{Error, Result};
use crate::format::PAGE;
use crate::overlay::Overlay;

const PAGE_U64: u64 = PAGE as u64;

/// Where one layer file lies in the region, and the seal its header must have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Its first byte's offset in the region: a whole number of pages.
    pub offset: u64,
    /// Its length in pages.
    pub pages: u64,
    /// The SHA-256 of its header page.
    pub seal: [u8; 32],
}

/// The region's spans as the disks of a cache; `R` is the region's bytes, read in place (on
/// the kernel, a read-only mapping of the archive's memory object).
pub struct AttachedDisk<R: AsRef<[u8]>> {
    region: R,
    spans: Vec<Span>,
    slots: Vec<u8>,
    /// Each layer's pages checked in place, a bit a page ([`in_place`]).
    checked: Vec<Vec<u64>>,
}

impl<R: AsRef<[u8]>> AttachedDisk<R> {
    /// The spans of `region`, with `slots` cache slots; refused when a span is not whole pages
    /// inside the region, or overlaps the one before it.
    pub fn new(region: R, spans: Vec<Span>, slots: u32) -> Result<AttachedDisk<R>> {
        let len = region.as_ref().len() as u64;
        let mut at = 0;
        for (i, span) in spans.iter().enumerate() {
            let end = span
                .pages
                .checked_mul(PAGE_U64)
                .and_then(|bytes| span.offset.checked_add(bytes));
            match end {
                Some(end)
                    if span.pages > 0
                        && span.offset.is_multiple_of(PAGE_U64)
                        && span.offset >= at
                        && end <= len =>
                {
                    at = end;
                }
                _ => {
                    return Err(Error::Corrupt(format!(
                        "layer {i}: {} pages at {} is not whole pages of the region's {len} \
                         bytes after the layer below it",
                        span.pages, span.offset
                    )));
                }
            }
        }
        let checked = spans
            .iter()
            .map(|s| vec![0u64; s.pages.div_ceil(64) as usize])
            .collect();
        Ok(AttachedDisk {
            region,
            spans,
            slots: vec![0; slots as usize * PAGE],
            checked,
        })
    }

    /// The region.
    pub fn region(&self) -> &R {
        &self.region
    }

    fn is_checked(&self, layer: usize, page: u64) -> bool {
        self.checked[layer][(page / 64) as usize] & (1 << (page % 64)) != 0
    }

    fn check(&mut self, layer: usize, page: u64) {
        self.checked[layer][(page / 64) as usize] |= 1 << (page % 64);
    }

    /// The spans.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Page `page` of layer `layer`, as it lies in the region: for a mapping filled straight
    /// from it, whose pages the caller checks against their hashes.
    pub fn page(&self, layer: usize, page: u64) -> Result<&[u8]> {
        let span = self
            .spans
            .get(layer)
            .filter(|span| page < span.pages)
            .ok_or_else(|| Error::Io(format!("layer {layer} has no page {page}")))?;
        let at = (span.offset + page * PAGE_U64) as usize;
        Ok(&self.region.as_ref()[at..at + PAGE])
    }
}

impl<R: AsRef<[u8]>> Disk for AttachedDisk<R> {
    fn fetch(&mut self, reads: &[Fetch]) -> Result<()> {
        let region = self.region.as_ref();
        for read in reads {
            let span = self
                .spans
                .get(read.layer)
                .ok_or_else(|| Error::Io(format!("no layer {}", read.layer)))?;
            if read.page >= span.pages {
                return Err(Error::Io(format!(
                    "layer {}: page {} is past its {} pages",
                    read.layer, read.page, span.pages
                )));
            }
            let at = (span.offset + read.page * PAGE_U64) as usize;
            let slot = read.slot as usize * PAGE;
            let into = self
                .slots
                .get_mut(slot..slot + PAGE)
                .ok_or_else(|| Error::Io(format!("no slot {}", read.slot)))?;
            into.copy_from_slice(&region[at..at + PAGE]);
        }
        Ok(())
    }

    fn copy(&mut self, slot: u32, offset: usize, out: &mut [u8]) -> Result<()> {
        let at = slot as usize * PAGE + offset;
        let from = self
            .slots
            .get(at..at + out.len())
            .ok_or_else(|| Error::Io(format!("no slot {slot}")))?;
        out.copy_from_slice(from);
        Ok(())
    }

    fn device_pages(&mut self, layer: usize) -> Result<u64> {
        self.spans
            .get(layer)
            .map(|span| span.pages)
            .ok_or_else(|| Error::Io(format!("no layer {layer}")))
    }
}

/// Where pages `first..first + pages` of layer `layer` lie in the region, in bytes from its
/// first: for a mapping that takes them where they lie, with no copy (item 3.6). Each page is
/// checked against its hash, from the layer's tables, before it is given, once: nothing writes
/// the region. Refused when a page is past its layer or does not match its hash.
pub fn in_place<R: AsRef<[u8]>>(
    cache: &mut Cache<AttachedDisk<R>>,
    layer: usize,
    first: u64,
    pages: u64,
) -> Result<u64> {
    let span = *cache
        .disk_ref()
        .spans
        .get(layer)
        .ok_or_else(|| Error::Io(format!("no layer {layer}")))?;
    if pages == 0 || first.checked_add(pages).is_none_or(|end| end > span.pages) {
        return Err(Error::BadRange);
    }
    for page in first..first + pages {
        if cache.disk_ref().is_checked(layer, page) {
            continue;
        }
        let expected = cache.expected(layer, page)?;
        if crate::sha256::digest(cache.disk_ref().page(layer, page)?) != expected {
            return Err(Error::Corrupt(format!(
                "layer {layer}: page {page}, mapped where it lies, does not match its hash"
            )));
        }
        cache.disk().check(layer, page);
    }
    Ok(span.offset + first * PAGE_U64)
}

/// `region`'s spans mounted as an image's layers, lowest first, over a cache of `slots` pages,
/// and their overlay: refused when a span is not one ([`AttachedDisk::new`]), its header is
/// not a layer file's of at most its pages, or its seal is not the span's.
pub fn attach<R: AsRef<[u8]>>(
    region: R,
    spans: Vec<Span>,
    slots: u32,
) -> Result<(Cache<AttachedDisk<R>>, Overlay)> {
    let seals: Vec<[u8; 32]> = spans.iter().map(|s| s.seal).collect();
    let mut cache = Cache::new(AttachedDisk::new(region, spans, slots)?, slots)?;
    for (i, seal) in seals.iter().enumerate() {
        cache.mount()?;
        let mounted = cache.layers()[i]
            .header
            .seal()
            .map_err(|e| Error::Corrupt(format!("layer {i}'s header: {}", e.0)))?;
        if mounted != *seal {
            return Err(Error::Corrupt(format!(
                "layer {i}: its header is sealed {}, and the archive's index gives it {}",
                hex(&mounted),
                hex(seal)
            )));
        }
    }
    let overlay = Overlay::new(cache.layers().to_vec());
    Ok((cache, overlay))
}

fn hex(seal: &[u8; 32]) -> alloc::string::String {
    seal.iter().map(|b| format!("{b:02x}")).collect()
}
