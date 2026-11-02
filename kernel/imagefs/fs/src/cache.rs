//! The page cache (SDD 27, f.5): verified layer pages, shared by every client of the
//! server, keyed by layer and page, bounded, and evicting the least recently used page.
//!
//! A page is read from its device only when something touches it, and believed only
//! after it matches its hash: the hash of a content page is in a tree page, which is
//! itself a cached page checked against its parent, up to the top page, which is
//! checked against the root the header carries; the header checks itself. So a cold
//! read of one page costs that page and the tree pages on its path not already cached.
//!
//! The cache does not own memory or devices: a [`Disk`] holds the slots (on the
//! kernel, pages of one DMA memory object every layer's driver has registered) and
//! moves pages from the layers into them.

use alloc::{collections::BTreeMap, format, vec::Vec};

use crate::error::{Error, Result};
use crate::format::{HASH_BYTES, Header, PAGE, Parent};
use crate::sha256;
use crate::volume::{Layer, Pages};

/// A page to read: page `page` of layer `layer` into slot `slot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fetch {
    pub layer: usize,
    pub page: u64,
    pub slot: u32,
}

/// The slots and the devices behind the cache.
pub trait Disk {
    /// Reads every page `reads` names into its slot, and returns once all are in.
    fn fetch(&mut self, reads: &[Fetch]) -> Result<()>;
    /// Copies `out.len()` bytes from `offset` of slot `slot`.
    fn copy(&mut self, slot: u32, offset: usize, out: &mut [u8]) -> Result<()>;
    /// Layer `layer`'s device length, in pages.
    fn device_pages(&mut self, layer: usize) -> Result<u64>;
}

/// What the cache has done, for the protocol's `stats` and the tests that count it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Reads of a page that was cached.
    pub hits: u64,
    /// Reads of a page that was not, each followed by a device read.
    pub misses: u64,
    /// Pages read from the devices, headers included.
    pub pages_read: u64,
    /// Pages dropped to make room.
    pub evictions: u64,
    /// Pages that did not match their hash.
    pub corrupt: u64,
}

const NIL: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Slot {
    key: Option<(usize, u64)>,
    /// Towards the most recently used.
    prev: u32,
    /// Towards the least recently used.
    next: u32,
}

pub struct Cache<D: Disk> {
    disk: D,
    layers: Vec<Layer>,
    slots: Vec<Slot>,
    index: BTreeMap<(usize, u64), u32>,
    free: Vec<u32>,
    /// The most recently used slot, and the least.
    head: u32,
    tail: u32,
    stats: Stats,
}

impl<D: Disk> Cache<D> {
    /// A cache of `slots` pages over `disk`; refused with fewer than 16, which one
    /// read and the tree pages above it could need at once.
    pub fn new(disk: D, slots: u32) -> Result<Cache<D>> {
        if !(16..NIL).contains(&slots) {
            return Err(Error::TooLarge);
        }
        Ok(Cache {
            disk,
            layers: Vec::new(),
            slots: alloc::vec![
                Slot {
                    key: None,
                    prev: NIL,
                    next: NIL
                };
                slots as usize
            ],
            index: BTreeMap::new(),
            free: (0..slots).rev().collect(),
            head: NIL,
            tail: NIL,
            stats: Stats::default(),
        })
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    pub fn disk(&mut self) -> &mut D {
        &mut self.disk
    }

    pub fn disk_ref(&self) -> &D {
        &self.disk
    }

    /// Slots in the cache.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Pages cached now.
    pub fn resident(&self) -> usize {
        self.index.len()
    }

    /// Mounts the next layer: reads its header page, checks it and that its device
    /// holds the whole file, and returns its index.
    pub fn mount(&mut self) -> Result<usize> {
        let layer = self.layers.len();
        let slot = self.take()?;
        let fetched = self.disk.fetch(&[Fetch {
            layer,
            page: 0,
            slot,
        }]);
        self.stats.pages_read += 1;
        let mut page = [0u8; PAGE];
        let copied = fetched.and_then(|()| self.disk.copy(slot, 0, &mut page));
        self.free.push(slot);
        copied?;
        let (header, geometry) = Header::parse(&page).map_err(|e| {
            self.stats.corrupt += 1;
            Error::Corrupt(format!("layer {layer}: its header: {}", e.0))
        })?;
        let device = self.disk.device_pages(layer)?;
        if device < geometry.pages {
            return Err(Error::Corrupt(format!(
                "layer {layer}: the file is {} pages and its device holds {device}",
                geometry.pages
            )));
        }
        self.layers.push(Layer::new(layer, header, geometry));
        Ok(layer)
    }

    fn unlink(&mut self, s: u32) {
        let Slot { prev, next, .. } = self.slots[s as usize];
        if prev == NIL {
            self.head = next;
        } else {
            self.slots[prev as usize].next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.slots[next as usize].prev = prev;
        }
        self.slots[s as usize].prev = NIL;
        self.slots[s as usize].next = NIL;
    }

    fn push_front(&mut self, s: u32) {
        self.slots[s as usize].prev = NIL;
        self.slots[s as usize].next = self.head;
        if self.head != NIL {
            self.slots[self.head as usize].prev = s;
        }
        self.head = s;
        if self.tail == NIL {
            self.tail = s;
        }
    }

    /// A slot to read into: a free one, or the least recently used page's.
    fn take(&mut self) -> Result<u32> {
        if let Some(s) = self.free.pop() {
            return Ok(s);
        }
        let s = self.tail;
        if s == NIL {
            return Err(Error::TooLarge);
        }
        self.unlink(s);
        if let Some(key) = self.slots[s as usize].key.take() {
            self.index.remove(&key);
        }
        self.stats.evictions += 1;
        Ok(s)
    }

    fn layer(&self, layer: usize) -> Result<&Layer> {
        self.layers
            .get(layer)
            .ok_or_else(|| Error::Corrupt(format!("layer {layer} is not mounted")))
    }

    /// The slot holding the page, if it is cached; touches nothing.
    pub fn cached(&self, layer: usize, page: u64) -> Option<u32> {
        self.index.get(&(layer, page)).copied()
    }

    /// The hash page `page` of `layer` must have.
    pub fn expected(&mut self, layer: usize, page: u64) -> Result<[u8; 32]> {
        let l = self.layer(layer)?;
        match l.geometry.parent(page) {
            Some(Parent::Root) => Ok(l.header.root),
            Some(Parent::Tree { page: tree, slot }) => {
                let s = self.ensure(layer, tree)?;
                let mut hash = [0u8; 32];
                self.disk.copy(s, slot * HASH_BYTES, &mut hash)?;
                Ok(hash)
            }
            Some(Parent::Header) | None => Err(Error::Corrupt(format!(
                "layer {layer}: page {page} is not a page the tree covers"
            ))),
        }
    }

    /// Checks a slot just read against `expected`, and files it; a page that does not
    /// match is refused and its slot freed.
    fn admit(&mut self, layer: usize, page: u64, slot: u32, expected: &[u8; 32]) -> Result<()> {
        let mut bytes = [0u8; PAGE];
        if let Err(e) = self.disk.copy(slot, 0, &mut bytes) {
            self.free.push(slot);
            return Err(e);
        }
        if sha256::digest(&bytes) != *expected {
            self.free.push(slot);
            self.stats.corrupt += 1;
            return Err(Error::Corrupt(format!(
                "layer {layer}: page {page} does not match its hash"
            )));
        }
        self.slots[slot as usize].key = Some((layer, page));
        self.index.insert((layer, page), slot);
        self.push_front(slot);
        Ok(())
    }

    /// The slot holding page `page` of `layer`, read and checked if it was not cached;
    /// the page becomes the most recently used.
    pub fn ensure(&mut self, layer: usize, page: u64) -> Result<u32> {
        if let Some(s) = self.cached(layer, page) {
            self.stats.hits += 1;
            self.unlink(s);
            self.push_front(s);
            return Ok(s);
        }
        let expected = self.expected(layer, page)?;
        self.stats.misses += 1;
        let slot = self.take()?;
        if let Err(e) = self.disk.fetch(&[Fetch { layer, page, slot }]) {
            self.free.push(slot);
            return Err(e);
        }
        self.stats.pages_read += 1;
        self.admit(layer, page, slot, &expected)?;
        Ok(slot)
    }

    /// Reads into the cache, in one call to the disk, every page of `pages` it does
    /// not hold, so the device sees one batch rather than a page at a time. At most
    /// half the cache is read at once; a longer list is read in parts.
    pub fn prefetch(&mut self, layer: usize, pages: &[u64]) -> Result<()> {
        for part in pages.chunks((self.slots.len() / 2).max(1)) {
            let mut wanted: Vec<(u64, [u8; 32])> = Vec::new();
            for &page in part {
                if self.cached(layer, page).is_some() || wanted.iter().any(|(p, _)| *p == page) {
                    continue;
                }
                let hash = self.expected(layer, page)?;
                wanted.push((page, hash));
            }
            if wanted.is_empty() {
                continue;
            }
            let mut reads = Vec::with_capacity(wanted.len());
            for (page, _) in &wanted {
                reads.push(Fetch {
                    layer,
                    page: *page,
                    slot: self.take()?,
                });
            }
            self.stats.misses += reads.len() as u64;
            if let Err(e) = self.disk.fetch(&reads) {
                self.free.extend(reads.iter().map(|r| r.slot));
                return Err(e);
            }
            self.stats.pages_read += reads.len() as u64;
            let mut failed = None;
            for (read, (_, hash)) in reads.iter().zip(&wanted) {
                if let Err(e) = self.admit(layer, read.page, read.slot, hash) {
                    failed.get_or_insert(e);
                }
            }
            if let Some(e) = failed {
                return Err(e);
            }
        }
        Ok(())
    }
}

impl<D: Disk> Pages for Cache<D> {
    fn read(&mut self, layer: usize, page: u64, offset: usize, out: &mut [u8]) -> Result<()> {
        if offset.checked_add(out.len()).is_none_or(|end| end > PAGE) {
            return Err(Error::BadRange);
        }
        let slot = self.ensure(layer, page)?;
        self.disk.copy(slot, offset, out)
    }
}
