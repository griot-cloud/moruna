// SPDX-License-Identifier: MPL-2.0

//! Where frames come from: pages of chunks, memory objects the core holds and the
//! personality maps in its region. A frame shown to the program is `process_map` of its
//! chunk and its offset there.
//!
//! - Chunks grow with what is held: half of it, from 2 MiB to 64 MiB, or the size of a
//!   larger segment.
//! - A chunk is a bitmap of its pages. One page is found next-fit, a run first-fit.
//! - A chunk that empties is given back to the core, unless it is the only empty one.
//! - Pages a chunk has never handed out are still zero from the core, so they are not
//!   zeroed again.

use alloc::{sync::Arc, vec, vec::Vec};

use super::super::{PAGE_SIZE, vm_space::calls::Object};
use crate::{Error, Result, sync::raw::RawLock};

const MIN_CHUNK: usize = 2 << 20;
const MAX_CHUNK: usize = 64 << 20;

/// A chunk's object, held by the arena and by every frame of it.
#[derive(Debug)]
pub(crate) struct Chunk {
    object: Object,
}

impl Chunk {
    /// Where its first page is in the personality.
    pub(crate) fn base(&self) -> usize {
        self.object.addr()
    }

    /// Its object's handle value.
    pub(crate) fn raw(&self) -> u32 {
        self.object.raw()
    }

    fn pages(&self) -> usize {
        self.object.len() / PAGE_SIZE
    }
}

struct Slot {
    chunk: Arc<Chunk>,
    /// A bit per page, set while the page is a frame.
    used_bits: Vec<u64>,
    used: usize,
    /// Pages from here on were never handed out.
    fresh: usize,
    /// Where the next search for one page starts.
    hint: usize,
}

impl Slot {
    fn new(chunk: Chunk) -> Slot {
        let pages = chunk.pages();
        Slot {
            chunk: Arc::new(chunk),
            used_bits: vec![0; pages.div_ceil(64)],
            used: 0,
            fresh: 0,
            hint: 0,
        }
    }

    fn is_used(&self, page: usize) -> bool {
        self.used_bits[page / 64] & (1 << (page % 64)) != 0
    }

    fn set(&mut self, first: usize, count: usize, used: bool) {
        for page in first..first + count {
            if used {
                self.used_bits[page / 64] |= 1 << (page % 64);
            } else {
                self.used_bits[page / 64] &= !(1 << (page % 64));
            }
        }
    }

    /// The first page of `count` free ones, next-fit for one page and first-fit for more.
    fn find(&self, count: usize) -> Option<usize> {
        let pages = self.chunk.pages();
        if self.used + count > pages {
            return None;
        }
        if count == 1 {
            let words = self.used_bits.len();
            let start = self.hint / 64;
            for k in 0..words {
                let w = (start + k) % words;
                let free = !self.used_bits[w];
                if free != 0 {
                    let page = w * 64 + free.trailing_zeros() as usize;
                    if page < pages {
                        return Some(page);
                    }
                }
            }
            return None;
        }
        let mut run = 0;
        let mut page = 0;
        while page < pages {
            if run == 0 && page.is_multiple_of(64) && self.used_bits[page / 64] == !0 {
                page += 64;
                continue;
            }
            if self.is_used(page) {
                run = 0;
            } else {
                run += 1;
                if run == count {
                    return Some(page + 1 - count);
                }
            }
            page += 1;
        }
        None
    }

    /// Takes `count` pages from `first`; returns how many of them may hold old bytes.
    fn take(&mut self, first: usize, count: usize) -> usize {
        self.set(first, count, true);
        self.used += count;
        self.hint = first + count;
        let stale = self.fresh.saturating_sub(first).min(count);
        self.fresh = self.fresh.max(first + count);
        stale
    }
}

struct Arena {
    /// By address.
    slots: Vec<Slot>,
    /// The bytes the chunks hold.
    bytes: usize,
}

static ARENA: RawLock<Arena> = RawLock::new(Arena {
    slots: Vec::new(),
    bytes: 0,
});

/// `count` contiguous pages: their chunk, the first page's index in it, and how many pages
/// from there may hold old bytes (the rest are zero).
pub(crate) fn alloc(count: usize) -> Result<(Arc<Chunk>, usize, usize)> {
    if count == 0 {
        return Err(Error::InvalidArgs);
    }
    let len = count.checked_mul(PAGE_SIZE).ok_or(Error::Overflow)?;
    let mut arena = ARENA.lock();
    // The newest chunk first: it is where the last pages came from.
    for slot in arena.slots.iter_mut().rev() {
        if let Some(first) = slot.find(count) {
            let stale = slot.take(first, count);
            return Ok((slot.chunk.clone(), first, stale));
        }
    }
    if len as u64 > moruna_abi::limits::MEMORY_MAX_BYTES {
        return Err(Error::NoMemory);
    }
    let size = (arena.bytes / 2)
        .clamp(MIN_CHUNK, MAX_CHUNK)
        .max(len.next_multiple_of(MIN_CHUNK))
        .min(moruna_abi::limits::MEMORY_MAX_BYTES as usize);
    let mut slot = Slot::new(Chunk {
        object: Object::create(size)?,
    });
    arena.bytes += size;
    let stale = slot.take(0, count);
    let chunk = slot.chunk.clone();
    let at = arena
        .slots
        .partition_point(|s| s.chunk.base() < chunk.base());
    arena.slots.insert(at, slot);
    Ok((chunk, 0, stale))
}

/// Gives back page `page` of `chunk`.
pub(crate) fn free(chunk: &Chunk, page: usize) {
    let gone = {
        let mut arena = ARENA.lock();
        let at = arena
            .slots
            .binary_search_by_key(&chunk.base(), |s| s.chunk.base())
            .expect("a frame's chunk is not the arena's");
        let slot = &mut arena.slots[at];
        debug_assert!(slot.is_used(page), "a frame freed twice");
        slot.set(page, 1, false);
        slot.used -= 1;
        slot.hint = slot.hint.min(page);
        let empty = slot.used == 0;
        let other_empty = arena
            .slots
            .iter()
            .enumerate()
            .any(|(i, s)| i != at && s.used == 0);
        if empty && other_empty {
            let slot = arena.slots.remove(at);
            arena.bytes -= slot.chunk.object.len();
            Some(slot)
        } else {
            None
        }
    };
    // The chunk goes back to the core once its last frame is gone, outside the lock.
    drop(gone);
}

/// The chunks held now and their bytes.
pub(crate) fn held() -> (usize, usize) {
    let arena = ARENA.lock();
    (arena.slots.len(), arena.bytes)
}
