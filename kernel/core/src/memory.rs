//! Memory objects, device memory regions and pins (SDD 20, f.5; SDD 25, f.3 and f.4).
//!
//! A memory object is a set of zeroed pages allocated when it is created. Mapping it
//! into a process maps those same pages, so two processes that map one object share
//! its bytes with no copy: the basis of the data plane. A device memory region is a
//! range of physical MMIO pages OSTD hands out only if no RAM and nothing OSTD itself
//! uses lies in it. A pin hands a device the pages of a memory object made for DMA.

use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;

use moruna_abi::{Error, PAGE_SIZE, charge, limits};
use ostd::{
    io::IoMem,
    mm::{
        FrameAllocOptions, HasDaddr, UFrame, USegment, VmIo,
        dma::{DmaStream, FromAndToDevice},
    },
    sync::SpinLock,
};

use crate::{
    account::{Account, Charge},
    object::{QUARANTINE_MAX, Resource},
};

const PAGE: usize = PAGE_SIZE as usize;

// The fixed charges cover the records they stand for (SDD 20 f.18).
const _: () = assert!(core::mem::size_of::<MemoryObject>() as u64 <= charge::OBJECT);
const _: () = assert!(core::mem::size_of::<UFrame>() as u64 <= charge::PAGE_RECORD);
const _: () = assert!(core::mem::size_of::<DeviceMemory>() as u64 <= charge::OBJECT);
const _: () = assert!(core::mem::size_of::<Pin>() as u64 <= charge::OBJECT);
const _: () =
    assert!(core::mem::size_of::<DmaStream<FromAndToDevice>>() as u64 + 8 <= charge::PIN_PAGE);

pub struct MemoryObject {
    frames: Vec<UFrame>,
    size: u64,
    /// Made with `memory_flags::DMA`: only such an object may be pinned for a device.
    dma: bool,
    /// KE1: one physical run for a contiguous DMA object.
    run: Option<USegment>,
    /// Its pages and its record, charged to the account it was made in until it is
    /// freed, whoever holds it by then (SDD 20 f.18).
    _charge: Charge,
    /// Stream B1: the pages of a pager's object or a snapshot, which arrive after it is
    /// made; `frames` is empty then.
    lazy: Option<alloc::boxed::Box<Lazy>>,
}

impl MemoryObject {
    /// Allocates `size` bytes, rounded up to whole pages, all zero, charged to
    /// `account`.
    pub fn new(size: u64, account: &Arc<Account>) -> Result<MemoryObject, Error> {
        MemoryObject::with_dma(size, false, account)
    }

    /// Allocates `size` bytes as [`MemoryObject::new`] does; `dma` marks it as memory a
    /// holder of a DMA resource may hand to a device.
    pub fn with_dma(size: u64, dma: bool, account: &Arc<Account>) -> Result<MemoryObject, Error> {
        if size == 0 {
            return Err(Error::InvalidArgs);
        }
        if size > limits::MEMORY_MAX_BYTES {
            return Err(Error::Limit);
        }
        let pages = size.div_ceil(PAGE_SIZE) as usize;
        // Charged before anything is allocated, so a refusal allocates nothing.
        let charge = Charge::new(account, charge::memory(pages as u64))?;
        let frames = MemoryObject::large_frames(pages)?;
        Ok(MemoryObject {
            frames,
            size: pages as u64 * PAGE_SIZE,
            dma,
            run: None,
            _charge: charge,
            // stream B1 hook
            lazy: None,
        })
    }

    /// Allocates `size` bytes for DMA, rounded up to whole pages, all zero, as one
    /// physically contiguous run (`memory_flags::CONTIGUOUS`, ABI 2.4).
    pub fn contiguous(size: u64, account: &Arc<Account>) -> Result<MemoryObject, Error> {
        if size == 0 {
            return Err(Error::InvalidArgs);
        }
        if size > moruna_abi::memory_flags::CONTIGUOUS_MAX_BYTES {
            return Err(Error::Limit);
        }
        let pages = size.div_ceil(PAGE_SIZE) as usize;
        // Charge all pages, metadata and object BEFORE allocating the physical run.
        let charge = Charge::new(account, charge::memory(pages as u64))?;
        let mut options = FrameAllocOptions::new();
        options.zeroed(true);
        let run: USegment = options
            .alloc_segment(pages)
            .map_err(|_| Error::NoMemory)?
            .into();
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(pages)
            .map_err(|_| Error::NoMemory)?;
        frames.extend(run.clone());
        Ok(MemoryObject {
            frames,
            size: pages as u64 * PAGE_SIZE,
            dma: true,
            run: Some(run),
            _charge: charge,
            // stream B1 hook
            lazy: None,
        })
    }

    /// Whether this memory is still charged to exactly this original Account.
    pub fn charged_to(&self, account: &Arc<Account>) -> bool {
        self._charge.matches(account)
    }

    /// Whether the object was made for DMA.
    pub fn is_dma(&self) -> bool {
        self.dma
    }

    /// A memory object holding a copy of `bytes`, charged to `account`.
    pub fn from_bytes(bytes: &[u8], account: &Arc<Account>) -> Result<MemoryObject, Error> {
        let object = MemoryObject::new(bytes.len().max(1) as u64, account)?;
        object.write(0, bytes)?;
        Ok(object)
    }

    /// The size in bytes, a whole number of pages.
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn frames(&self) -> &[UFrame] {
        &self.frames
    }

    /// Copies `buf.len()` bytes starting at `offset` out of the object.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::OutOfRange)?;
        if end > self.size {
            return Err(Error::OutOfRange);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset as usize + done;
            let (page, within) = (at / PAGE, at % PAGE);
            let n = (PAGE - within).min(buf.len() - done);
            // stream B1 hook: a lazy object's page.
            self.page(page, false)?
                .read_bytes(within, &mut buf[done..done + n])
                .map_err(|_| Error::Fault)?;
            done += n;
        }
        Ok(())
    }

    /// Copies `buf` into the object at `offset`.
    pub fn write(&self, offset: u64, buf: &[u8]) -> Result<(), Error> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::OutOfRange)?;
        if end > self.size {
            return Err(Error::OutOfRange);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset as usize + done;
            let (page, within) = (at / PAGE, at % PAGE);
            let n = (PAGE - within).min(buf.len() - done);
            // stream B1 hook: a lazy object's page, its own once written.
            self.page(page, true)?
                .write_bytes(within, &buf[done..done + n])
                .map_err(|_| Error::Fault)?;
            done += n;
        }
        Ok(())
    }
}

// ---- stream A (data path, `architecture/data-os-streams.md`): stream A's additions to
// ---- `MemoryObject` (large frames) go in this block.

/// The pages of a large page (2 MiB).
const RUN_PAGES: usize = ostd::mm::HUGE_PAGE_SIZE / PAGE;

impl MemoryObject {
    /// `pages` zeroed frames for an object, in aligned 2 MiB runs while a whole run
    /// remains, so that `map::HUGE` maps them as large pages (data-os §2, C4); single
    /// frames for the rest, and for a run the allocator has no aligned space for. The
    /// same pages as single frames, charged the same.
    pub fn large_frames(pages: usize) -> Result<Vec<UFrame>, Error> {
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(pages)
            .map_err(|_| Error::NoMemory)?;
        let mut options = FrameAllocOptions::new();
        options.zeroed(true);
        moruna_datapath::frames::fill(
            &mut frames,
            pages,
            RUN_PAGES,
            || options.alloc_segment(RUN_PAGES).ok().map(USegment::from),
            || {
                options
                    .alloc_frame()
                    .map(Into::into)
                    .map_err(|_| Error::NoMemory)
            },
        )?;
        Ok(frames)
    }
}

// ---- stream B1 (personality, `architecture/data-os-streams.md`): stream B1's additions to
// ---- `MemoryObject` (snapshots, pager-backed objects) go in this block.
//
// A lazy object's pages arrive after it is made (kernel-is-the-machine §4.1 b and c): a
// pager's when its server supplies them, a snapshot's when it first writes them (until
// then it reads its parent's, or zero). A clone shares pages between two snapshots: each
// copies a shared page when it writes it, and the page stays charged to the account that
// held it until neither shares it.

use alloc::{boxed::Box, collections::BTreeMap};
use core::sync::atomic::{AtomicU8, Ordering};

use moruna_abi::{write_u32, write_u64};
use ostd::sync::{Waiter, Waker};

use crate::{
    channel::{Endpoint, Message},
    process::Thread,
};

/// What a lazy object charges for a page it holds: the page and its record.
const LAZY_PAGE: u64 = charge::PAGE + 4 * charge::PAGE_RECORD;
/// What it charges for a page it shares: the record.
const LAZY_RECORD: u64 = 4 * charge::PAGE_RECORD;
/// The longest chain of snapshots; reading a page walks it.
const SNAPSHOT_DEPTH: u32 = 8;
/// The largest pager's object (`moruna_sys::personality::PAGER_MAX_BYTES`).
pub const PAGER_MAX_BYTES: u64 = 1 << 40;
/// The pager's request (`moruna_sys::personality::pager_request`).
mod request {
    pub const KIND: usize = 0;
    pub const KEY: usize = 8;
    pub const OFFSET: usize = 16;
    pub const LEN: usize = 24;
    pub const SIZE: usize = 32;
    pub const FAULT: u32 = 1;
}

const _: () = assert!(
    core::mem::size_of::<MemoryObject>() as u64 + core::mem::size_of::<Lazy>() as u64
        <= charge::OBJECT
);

/// A page of an object as a mapping sees it.
pub enum Resolved {
    /// The object's own page: mapped as the area allows.
    Own(UFrame),
    /// A page it shares (its parent's, or one a clone shares): mapped read only, and
    /// copied when written.
    Shared(UFrame),
}

impl Resolved {
    pub fn frame(&self) -> &UFrame {
        match self {
            Resolved::Own(f) | Resolved::Shared(f) => f,
        }
    }
}

/// A page the two sides of a clone share, charged to the account that held it.
struct Kept {
    frame: UFrame,
    _charge: Charge,
}

enum Held {
    Own(UFrame, Charge),
    Shared(Arc<Kept>, Charge),
}

struct Lazy {
    pages: SpinLock<BTreeMap<usize, Held>>,
    source: Source,
    account: Arc<Account>,
    depth: u32,
}

enum Source {
    Pager(Pager),
    /// Pages not held read as `parent`'s from its page `first`, or as zero with none.
    Snapshot {
        parent: Option<Arc<MemoryObject>>,
        first: usize,
    },
}

struct Pager {
    /// The kernel's end of the server's channel.
    port: Arc<Endpoint>,
    key: u64,
    /// The threads waiting for each page, and whether its request is queued.
    waiting: SpinLock<BTreeMap<usize, Want>>,
}

#[derive(Default)]
struct Want {
    sent: bool,
    sleepers: Vec<Arc<Sleeper>>,
}

struct Sleeper {
    waker: Arc<Waker>,
    answer: AtomicU8,
}

const WAITING: u8 = 0;
const SUPPLIED: u8 = 1;
const REFUSED: u8 = 2;

/// A new page: zero, or a copy of `from`.
fn new_page(from: Option<&UFrame>) -> Result<UFrame, Error> {
    let mut options = FrameAllocOptions::new();
    options.zeroed(from.is_none());
    let frame: UFrame = options.alloc_frame().map_err(|_| Error::NoMemory)?.into();
    if let Some(from) = from {
        let mut bytes = [0u8; PAGE];
        from.read_bytes(0, &mut bytes).map_err(|_| Error::Fault)?;
        frame.write_bytes(0, &bytes).map_err(|_| Error::Fault)?;
    }
    Ok(frame)
}

impl MemoryObject {
    fn lazy(size: u64, source: Source, depth: u32, account: &Arc<Account>) -> Result<Self, Error> {
        Ok(MemoryObject {
            frames: Vec::new(),
            size,
            dma: false,
            run: None,
            _charge: Charge::new(account, charge::OBJECT)?,
            lazy: Some(Box::new(Lazy {
                pages: SpinLock::new(BTreeMap::new()),
                source,
                account: account.clone(),
                depth,
            })),
        })
    }

    /// An object of `size` bytes (whole pages) whose pages the server at the peer of
    /// `port` supplies, asked with `key`.
    pub fn pager(
        size: u64,
        port: Arc<Endpoint>,
        key: u64,
        account: &Arc<Account>,
    ) -> Result<MemoryObject, Error> {
        if size == 0 || !size.is_multiple_of(PAGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        if size > PAGER_MAX_BYTES {
            return Err(Error::Limit);
        }
        let pager = Pager {
            port,
            key,
            waiting: SpinLock::new(BTreeMap::new()),
        };
        MemoryObject::lazy(size, Source::Pager(pager), 0, account)
    }

    /// A snapshot of `pages` pages of `parent` from its page `first`: it reads them
    /// until it writes them.
    pub fn snapshot(
        parent: &Arc<MemoryObject>,
        first: usize,
        pages: usize,
        account: &Arc<Account>,
    ) -> Result<MemoryObject, Error> {
        if pages == 0 {
            return Err(Error::InvalidArgs);
        }
        if first
            .checked_add(pages)
            .is_none_or(|end| end > parent.page_count())
        {
            return Err(Error::OutOfRange);
        }
        let depth = parent.lazy.as_ref().map_or(0, |l| l.depth) + 1;
        if depth > SNAPSHOT_DEPTH {
            return Err(Error::Limit);
        }
        let source = Source::Snapshot {
            parent: Some(parent.clone()),
            first,
        };
        MemoryObject::lazy(pages as u64 * PAGE_SIZE, source, depth, account)
    }

    fn page_count(&self) -> usize {
        (self.size / PAGE_SIZE) as usize
    }

    /// Whether its pages arrive after it is made: a pager's object or a snapshot.
    pub fn is_lazy(&self) -> bool {
        self.lazy.is_some()
    }

    /// Whether it is a snapshot: a private copy, which a clone copies on write.
    pub fn is_snapshot(&self) -> bool {
        self.lazy
            .as_ref()
            .is_some_and(|l| matches!(l.source, Source::Snapshot { .. }))
    }

    /// Page `index` as a mapping sees it, made the object's own first when `write`;
    /// `None` for a pager's page not supplied yet (the toucher waits with [`Self::want`]).
    pub fn resolve(&self, index: usize, write: bool) -> Result<Option<Resolved>, Error> {
        if index >= self.page_count() {
            return Err(Error::OutOfRange);
        }
        match &self.lazy {
            None => Ok(Some(Resolved::Own(self.frames[index].clone()))),
            Some(lazy) => lazy.resolve(index, write),
        }
    }

    /// The frame of page `index` for one of the kernel's own copies: `Fault` for a
    /// pager's page not supplied.
    fn page(&self, index: usize, write: bool) -> Result<UFrame, Error> {
        match self.resolve(index, write)? {
            Some(r) => Ok(r.frame().clone()),
            None => Err(Error::Fault),
        }
    }

    /// Waits until page `index` is held, asking its pager once: `Fault` when the pager
    /// refuses it or has gone, `BadState` when the thread ends meanwhile.
    pub fn want(&self, index: usize, thread: &Thread) -> Result<(), Error> {
        let Some(lazy) = &self.lazy else {
            return Ok(());
        };
        match &lazy.source {
            Source::Snapshot {
                parent: Some(p),
                first,
            } => p.want(first + index, thread),
            Source::Snapshot { parent: None, .. } => Ok(()),
            Source::Pager(pager) => pager.want(lazy, index, thread),
        }
    }

    fn pager_of(&self) -> Result<(&Lazy, &Pager), Error> {
        match self.lazy.as_deref() {
            Some(
                lazy @ Lazy {
                    source: Source::Pager(pager),
                    ..
                },
            ) => Ok((lazy, pager)),
            _ => Err(Error::WrongType),
        }
    }

    /// The pager supplies `count` pages from `first` with the pages of `source` (a plain
    /// object) from `source_first`; a page already held keeps its own.
    pub fn supply(
        &self,
        first: usize,
        count: usize,
        source: &MemoryObject,
        source_first: usize,
    ) -> Result<(), Error> {
        let (lazy, pager) = self.pager_of()?;
        if source.lazy.is_some() {
            return Err(Error::InvalidArgs);
        }
        let end = first.checked_add(count).ok_or(Error::OutOfRange)?;
        let source_end = source_first.checked_add(count).ok_or(Error::OutOfRange)?;
        if end > self.page_count() || source_end > source.frames.len() {
            return Err(Error::OutOfRange);
        }
        let mut done = Ok(());
        {
            let mut pages = lazy.pages.lock();
            for i in 0..count {
                if pages.contains_key(&(first + i)) {
                    continue;
                }
                match Charge::new(&lazy.account, LAZY_PAGE) {
                    Ok(c) => {
                        pages.insert(
                            first + i,
                            Held::Own(source.frames[source_first + i].clone(), c),
                        );
                    }
                    Err(e) => {
                        done = Err(e);
                        break;
                    }
                }
            }
        }
        // Whatever was supplied wakes its waiters, even when the charge ran out.
        pager.answer(first, end, SUPPLIED, |i| lazy.pages.lock().contains_key(&i));
        done
    }

    /// The pager refuses `count` pages from `first`: their waiters fault.
    pub fn refuse(&self, first: usize, count: usize) -> Result<(), Error> {
        let (_, pager) = self.pager_of()?;
        let end = first.checked_add(count).ok_or(Error::OutOfRange)?;
        if end > self.page_count() {
            return Err(Error::OutOfRange);
        }
        pager.answer(first, end, REFUSED, |_| true);
        Ok(())
    }

    /// A snapshot forgets the pages it wrote from `first`, `count` of them, which read as
    /// its parent's (or zero) again: `madvise(MADV_DONTNEED)`. Any other object keeps them.
    pub fn forget(&self, first: usize, count: usize) {
        if !self.is_snapshot() {
            return;
        }
        if let Some(lazy) = &self.lazy {
            let gone: Vec<Held> = {
                let mut pages = lazy.pages.lock();
                let keys: Vec<usize> = pages.range(first..first + count).map(|(k, _)| *k).collect();
                keys.iter().filter_map(|k| pages.remove(k)).collect()
            };
            drop(gone);
        }
    }

    /// The two sides of a clone of a private mapping of `pages` pages from `first`: the
    /// object the clone's source maps from now on, and the one the clone maps, both from
    /// their page 0 when new. A plain object becomes the parent of two new snapshots; a
    /// snapshot shares what it holds there with a new twin, so either side's write copies.
    pub fn fork(
        self: &Arc<Self>,
        first: usize,
        pages: usize,
        mine: &Arc<Account>,
        theirs: &Arc<Account>,
    ) -> Result<(Arc<MemoryObject>, usize, Arc<MemoryObject>), Error> {
        let Some(lazy) = &self.lazy else {
            let a = MemoryObject::snapshot(self, first, pages, mine)?;
            let b = MemoryObject::snapshot(self, first, pages, theirs)?;
            return Ok((Arc::new(a), 0, Arc::new(b)));
        };
        let Source::Snapshot {
            parent,
            first: parent_first,
        } = &lazy.source
        else {
            return Err(Error::InvalidArgs);
        };
        let source = Source::Snapshot {
            parent: parent.clone(),
            first: parent_first + first,
        };
        let twin = MemoryObject::lazy(pages as u64 * PAGE_SIZE, source, lazy.depth, theirs)?;
        let twin_lazy = twin.lazy.as_ref().ok_or(Error::BadState)?;
        let mut pages_mine = lazy.pages.lock();
        let keys: Vec<usize> = pages_mine
            .range(first..first + pages)
            .map(|(k, _)| *k)
            .collect();
        // Every record charged before anything changes, so a refusal changes nothing.
        let mut records = Vec::new();
        records
            .try_reserve_exact(2 * keys.len())
            .map_err(|_| Error::NoMemory)?;
        for _ in &keys {
            records.push(Charge::new(mine, LAZY_RECORD)?);
            records.push(Charge::new(theirs, LAZY_RECORD)?);
        }
        let mut pages_twin = twin_lazy.pages.lock();
        for k in keys {
            let (Some(record_twin), Some(record_mine)) = (records.pop(), records.pop()) else {
                break;
            };
            let kept = match pages_mine.remove(&k) {
                Some(Held::Own(frame, charge)) => Arc::new(Kept {
                    frame,
                    _charge: charge,
                }),
                Some(Held::Shared(kept, charge)) => {
                    pages_mine.insert(k, Held::Shared(kept.clone(), charge));
                    kept
                }
                None => continue,
            };
            if !matches!(pages_mine.get(&k), Some(Held::Shared(..))) {
                pages_mine.insert(k, Held::Shared(kept.clone(), record_mine));
            }
            pages_twin.insert(k - first, Held::Shared(kept, record_twin));
        }
        drop(pages_twin);
        drop(pages_mine);
        Ok((self.clone(), first, Arc::new(twin)))
    }

    /// The two sides of a clone of an area of zero pages `pages` long, which holds
    /// `held` (by page, charged to `mine` as zero pages): anonymous snapshots sharing
    /// them, copy-on-write.
    pub fn fork_zero(
        pages: usize,
        held: Vec<(usize, UFrame)>,
        mine: &Arc<Account>,
        theirs: &Arc<Account>,
    ) -> Result<(Arc<MemoryObject>, Arc<MemoryObject>), Error> {
        let anonymous = || Source::Snapshot {
            parent: None,
            first: 0,
        };
        let size = pages as u64 * PAGE_SIZE;
        let a = MemoryObject::lazy(size, anonymous(), 0, mine)?;
        let b = MemoryObject::lazy(size, anonymous(), 0, theirs)?;
        {
            let (la, lb) = (
                a.lazy.as_ref().ok_or(Error::BadState)?,
                b.lazy.as_ref().ok_or(Error::BadState)?,
            );
            let (mut pa, mut pb) = (la.pages.lock(), lb.pages.lock());
            for (index, frame) in held {
                let kept = Arc::new(Kept {
                    frame,
                    _charge: Charge::new(mine, LAZY_PAGE)?,
                });
                pa.insert(
                    index,
                    Held::Shared(kept.clone(), Charge::new(mine, LAZY_RECORD)?),
                );
                pb.insert(index, Held::Shared(kept, Charge::new(theirs, LAZY_RECORD)?));
            }
        }
        Ok((Arc::new(a), Arc::new(b)))
    }
}

impl Lazy {
    fn resolve(&self, index: usize, write: bool) -> Result<Option<Resolved>, Error> {
        let shared = {
            let mut pages = self.pages.lock();
            match pages.get(&index) {
                Some(Held::Own(f, _)) => return Ok(Some(Resolved::Own(f.clone()))),
                Some(Held::Shared(k, _)) if !write => {
                    return Ok(Some(Resolved::Shared(k.frame.clone())));
                }
                Some(Held::Shared(k, _)) if Arc::strong_count(k) == 1 => {
                    // The last of the clone's sides to hold it takes it as its own, with
                    // no copy, charged to its account instead. Its callers have taken the
                    // page out of every table first.
                    let charge = Charge::new(&self.account, LAZY_PAGE)?;
                    let Some(Held::Shared(k, record)) = pages.remove(&index) else {
                        return Err(Error::BadState);
                    };
                    match Arc::try_unwrap(k) {
                        Ok(kept) => {
                            let frame = kept.frame.clone();
                            pages.insert(index, Held::Own(kept.frame, charge));
                            drop(pages);
                            drop(record);
                            return Ok(Some(Resolved::Own(frame)));
                        }
                        Err(k) => {
                            let frame = k.frame.clone();
                            pages.insert(index, Held::Shared(k, record));
                            Some(frame)
                        }
                    }
                }
                Some(Held::Shared(k, _)) => Some(k.frame.clone()),
                None => None,
            }
        };
        let below = match (shared, &self.source) {
            (Some(f), _) => Some(f),
            (None, Source::Pager(_)) => return Ok(None),
            (None, Source::Snapshot { parent: None, .. }) => None,
            (
                None,
                Source::Snapshot {
                    parent: Some(p),
                    first,
                },
            ) => match p.resolve(first + index, false)? {
                None => return Ok(None),
                Some(r) if !write => return Ok(Some(Resolved::Shared(r.frame().clone()))),
                Some(r) => Some(r.frame().clone()),
            },
        };
        // A write, or a first read of a zero page: the object's own page now.
        let charge = Charge::new(&self.account, LAZY_PAGE)?;
        let frame = new_page(below.as_ref())?;
        let mut pages = self.pages.lock();
        if let Some(Held::Own(f, _)) = pages.get(&index) {
            // Another thread made it first.
            return Ok(Some(Resolved::Own(f.clone())));
        }
        let replaced = pages.insert(index, Held::Own(frame.clone(), charge));
        drop(pages);
        drop(replaced);
        Ok(Some(Resolved::Own(frame)))
    }
}

impl Pager {
    fn want(&self, lazy: &Lazy, index: usize, thread: &Thread) -> Result<(), Error> {
        let (waiter, waker) = Waiter::new_pair();
        let sleeper = Arc::new(Sleeper {
            waker: waker.clone(),
            answer: AtomicU8::new(WAITING),
        });
        {
            let mut waiting = self.waiting.lock();
            let want = waiting.entry(index).or_default();
            want.sleepers.try_reserve(1).map_err(|_| Error::NoMemory)?;
            want.sleepers.push(sleeper.clone());
        }
        // Supplied before this thread was counted: nothing to wait for.
        if lazy.pages.lock().contains_key(&index) {
            self.leave(index, &sleeper);
            return Ok(());
        }
        thread.set_waker(Some(waker.clone()));
        self.port.signaller().observe(&waker);
        let result = loop {
            match sleeper.answer.load(Ordering::Acquire) {
                SUPPLIED => break Ok(()),
                REFUSED => break Err(Error::Fault),
                _ => {}
            }
            // A page fault waits for its page through an interrupt; only an end stops it.
            if thread.dying() {
                break Err(Error::BadState);
            }
            if let Err(e) = self.send(lazy, index) {
                break Err(e);
            }
            waiter.wait();
        };
        self.port.signaller().unobserve(&waker);
        thread.set_waker(None);
        self.leave(index, &sleeper);
        result
    }

    /// Queues the request for `index` on the pager's channel if it is not queued yet
    /// and the queue has room (else a read of the pager's wakes the waiter to try
    /// again). `Fault` once the pager has gone.
    fn send(&self, lazy: &Lazy, index: usize) -> Result<(), Error> {
        let mut waiting = self.waiting.lock();
        let Some(want) = waiting.get_mut(&index) else {
            return Ok(());
        };
        if want.sent {
            return Ok(());
        }
        let channel = &self.port.channel;
        let peer = self.port.peer();
        let mut inner = channel.inner.lock();
        if !inner.open[peer] {
            return Err(Error::Fault);
        }
        if inner.queues[peer].len() >= limits::CHANNEL_DEPTH {
            return Ok(());
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(request::SIZE)
            .map_err(|_| Error::NoMemory)?;
        bytes.resize(request::SIZE, 0);
        write_u32(&mut bytes, request::KIND, request::FAULT);
        write_u64(&mut bytes, request::KEY, self.key);
        write_u64(&mut bytes, request::OFFSET, index as u64 * PAGE_SIZE);
        write_u64(&mut bytes, request::LEN, PAGE_SIZE);
        // Charged to the object's account while it is queued.
        let charge = Charge::new(&lazy.account, Message::cost(request::SIZE, 0))?;
        inner.queues[peer]
            .try_reserve(1)
            .map_err(|_| Error::NoMemory)?;
        inner.queues[peer].push_back(Message {
            bytes,
            handles: Vec::new(),
            endpoints: 0,
            charge: Some(charge),
        });
        channel.refresh(&inner);
        want.sent = true;
        Ok(())
    }

    fn leave(&self, index: usize, sleeper: &Arc<Sleeper>) {
        let mut waiting = self.waiting.lock();
        if let Some(want) = waiting.get_mut(&index) {
            want.sleepers.retain(|s| !Arc::ptr_eq(s, sleeper));
            if want.sleepers.is_empty() {
                waiting.remove(&index);
            }
        }
    }

    /// Answers the waiters of the pages `first .. end` that `answered` names.
    fn answer(&self, first: usize, end: usize, answer: u8, answered: impl Fn(usize) -> bool) {
        let gone: Vec<Want> = {
            let mut waiting = self.waiting.lock();
            let keys: Vec<usize> = waiting
                .range(first..end)
                .map(|(k, _)| *k)
                .filter(|k| answered(*k))
                .collect();
            keys.iter().filter_map(|k| waiting.remove(k)).collect()
        };
        for want in gone {
            for s in want.sleepers {
                s.answer.store(answer, Ordering::Release);
                s.waker.wake_up();
            }
        }
    }
}

/// The largest device memory region one call creates.
pub const DEVICE_MEMORY_MAX: u64 = 1 << 30;

pub struct DeviceMemory {
    pub io: IoMem,
    pub size: u64,
    range: Range<u64>,
    /// Its record: the pages are the device's, not RAM.
    _charge: Charge,
}

fn overlaps(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

/// Every range the kernel has acquired from OSTD, and the ranges live device memory
/// regions hold now. OSTD hands a range out once and never takes it back (its `IoMem`
/// is never recycled), so the kernel keeps what it acquired and hands out slices of it
/// again once no live region holds them: a driver that ends leaves its windows
/// claimable by the next one (SDD 25, f.3).
struct Claims {
    acquired: Vec<(Range<u64>, IoMem)>,
    live: Vec<Range<u64>>,
}

static CLAIMS: SpinLock<Claims> = SpinLock::new(Claims {
    acquired: Vec::new(),
    live: Vec::new(),
});

impl DeviceMemory {
    /// Claims the physical range `paddr .. paddr + size`, both page aligned, its record
    /// charged to `account`. A range a live region holds, or one that only partly
    /// overlaps a range acquired before, is refused (`AccessDenied`).
    pub fn new(paddr: u64, size: u64, account: &Arc<Account>) -> Result<DeviceMemory, Error> {
        if size == 0 || !paddr.is_multiple_of(PAGE_SIZE) || !size.is_multiple_of(PAGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        if size > DEVICE_MEMORY_MAX {
            return Err(Error::Limit);
        }
        let end = paddr.checked_add(size).ok_or(Error::InvalidArgs)?;
        let range = paddr..end;
        let start = usize::try_from(paddr).map_err(|_| Error::InvalidArgs)?;
        let end = usize::try_from(end).map_err(|_| Error::InvalidArgs)?;
        let charge = Charge::new(account, charge::OBJECT)?;
        let mut claims = CLAIMS.lock();
        if claims.live.iter().any(|l| overlaps(l, &range)) {
            return Err(Error::AccessDenied);
        }
        claims.live.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let io = match claims.acquired.iter().find(|(a, _)| overlaps(a, &range)) {
            Some((a, io)) if a.start <= range.start && range.end <= a.end => {
                let from = (range.start - a.start) as usize;
                io.slice(from..from + size as usize)
            }
            Some(_) => return Err(Error::AccessDenied),
            None => {
                claims
                    .acquired
                    .try_reserve(1)
                    .map_err(|_| Error::NoMemory)?;
                // OSTD refuses a range that holds RAM or overlaps what it uses itself:
                // that is the kernel's guard on physical memory (KK-I7).
                let io = IoMem::acquire(start..end).map_err(|_| Error::AccessDenied)?;
                claims.acquired.push((range.clone(), io.clone()));
                io
            }
        };
        claims.live.push(range.clone());
        Ok(DeviceMemory {
            io,
            size,
            range,
            _charge: charge,
        })
    }
}

impl Drop for DeviceMemory {
    fn drop(&mut self) {
        let mut claims = CLAIMS.lock();
        if let Some(i) = claims.live.iter().position(|l| *l == self.range) {
            claims.live.swap_remove(i);
        }
    }
}

/// A pin: the pages of a memory object handed to the devices a DMA resource covers.
/// When its last handle closes, the pages go into that resource's quarantine rather
/// than back to the allocator, because the device may still be writing them; they
/// leave it when a holder of the resource calls `dma_release`, after it has stopped
/// the device (SDD 25, f.4).
pub struct Pin {
    domain: Arc<Resource>,
    pinned: Option<Pinned>,
}

/// The pages of a memory object as a device sees them: one DMA mapping per page
/// (through the IOMMU when OSTD has one; the physical address otherwise), and their
/// record's charge, which stays with them in a quarantine until `dma_release`.
/// Dropping it ends the mappings and lets the pages go.
pub struct Pinned {
    _memory: Arc<MemoryObject>,
    _streams: Vec<DmaStream<FromAndToDevice>>,
    _charge: Charge,
}

impl Pin {
    /// Pins `memory` for `domain`'s devices, its record charged to `account`; returns
    /// the pin and the device address of each page, in order.
    pub fn new(
        domain: Arc<Resource>,
        memory: Arc<MemoryObject>,
        account: &Arc<Account>,
    ) -> Result<(Pin, Vec<u64>), Error> {
        if !memory.is_dma() {
            return Err(Error::InvalidArgs);
        }
        if domain.quarantine.lock().len() >= QUARANTINE_MAX {
            return Err(Error::Limit);
        }
        let pages = memory.frames().len();
        let charge = Charge::new(account, charge::OBJECT + pages as u64 * charge::PIN_PAGE)?;
        let mut streams = Vec::new();
        streams
            .try_reserve_exact(pages)
            .map_err(|_| Error::NoMemory)?;
        let mut addresses = Vec::new();
        addresses
            .try_reserve_exact(pages)
            .map_err(|_| Error::NoMemory)?;
        // x86_64 keeps DMA coherent with the caches.
        if let Some(run) = &memory.run {
            // One mapping, so the device sees the run as one run of addresses.
            let stream = DmaStream::map(run.clone(), true).map_err(|_| Error::NoMemory)?;
            let base = stream.daddr() as u64;
            addresses.extend((0..pages as u64).map(|i| base + i * PAGE_SIZE));
            streams.push(stream);
        } else {
            for frame in memory.frames() {
                let stream = DmaStream::map(USegment::from(frame.clone()), true)
                    .map_err(|_| Error::NoMemory)?;
                addresses.push(stream.daddr() as u64);
                streams.push(stream);
            }
        }
        let pin = Pin {
            domain,
            pinned: Some(Pinned {
                _memory: memory,
                _streams: streams,
                _charge: charge,
            }),
        };
        Ok((pin, addresses))
    }

    /// Ends the pin at once, with no quarantine: for a pin no device has been told of.
    pub fn abandon(mut self) {
        self.pinned = None;
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        if let Some(pinned) = self.pinned.take() {
            let mut quarantine = self.domain.quarantine.lock();
            if quarantine.try_reserve(1).is_ok() {
                quarantine.push(pinned);
            } else {
                // No memory to remember the pin: keeping the pages mapped for ever is
                // the only choice that cannot hand a device's write to a new owner.
                ostd::early_println!(
                    "kk: a DMA quarantine could not grow; a pin's pages stay allocated"
                );
                core::mem::forget(pinned);
            }
        }
    }
}

/// Empties a DMA resource's quarantine; returns how many pins it held.
pub fn release_quarantine(domain: &Resource) -> usize {
    let released = core::mem::take(&mut *domain.quarantine.lock());
    let n = released.len();
    // Dropped outside the lock: ending a DMA mapping takes OSTD's own locks.
    drop(released);
    n
}
