//! The image filesystem server (SDD 27): every task's root filesystem.
//!
//! It starts with an ordered list of block services and layer ids, lowest first (the
//! slot `layers`), looks each service up in the device manager's directory, and
//! registers one DMA memory object, the page cache's slots, with every layer's
//! driver, so the cache is pinned once and never per request. It mounts each layer
//! by reading its header, and from then on reads a page only when something touches
//! it, checks it against its hash, and keeps it in the cache every client shares.
//! `map` hands out a read-only memory object over a file's pages: pages the cache
//! holds are copied into it, the rest are read from the device straight into it, and
//! the object is kept so a second mapping of the same pages is the same memory.
//!
//! Each connection is one task, with its own open files and its own scratch
//! directory at `/tmp`. The server ends when its last client and listener have gone.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{
    collections::{BTreeMap, VecDeque},
    format,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::cell::RefCell;

use block_protocol::{Info, block};
use imagefs_protocol::{
    Bytes, Dirent, FILES_MAX, MAP_MAX, READ_MAX, READDIR_BYTES, Stat, Stats, imagefs, kind, open,
};
use moruna_imagefs::{
    cache::{Cache, Disk, Fetch},
    error::Error as FsError,
    format::{Kind, PAGE, PAGE_U64},
    fs::{Place, Tree},
    map,
    overlay::{Found, Overlay},
    scratch::Scratch,
    sha256,
    volume::{FileMap, Pages},
};
use moruna_svc::{
    CallError, Channel, Console, Context, DEADLINE_INFINITE, Dispatcher, Encoder, Mapping, Memory,
    Rights, Wire, deadline_in, map as mapflags, println, start,
};

moruna_svc::main!(run);

/// The page cache's slots: 16 MiB.
const CACHE_PAGES: u32 = 4096;
/// The mappings kept for the next task that maps the same pages.
const MAP_CACHE_BYTES: u64 = 64 << 20;
const MAP_CACHE_ENTRIES: usize = 256;
/// Requests in flight to one device at once: within a channel's queue.
const IN_FLIGHT: usize = 32;
const SECONDS: u64 = 1_000_000_000;
/// How long a device has to answer a read.
const DEVICE_WAIT: u64 = 60 * SECONDS;
/// The first line of the `layers` slot's text.
const LAYERS_HEADER: &str = "moruna-layers 1";

struct Device {
    service: String,
    id: String,
    client: block::Client,
    info: Info,
    /// The cache's slots, registered with this device's driver.
    slots: u32,
}

/// The layers' devices and the cache's slots.
struct BlockDisk {
    devices: Vec<Device>,
    /// The slots, as the server reads them.
    view: Mapping<'static>,
    _slab: Memory,
    requests: u64,
}

/// One transfer: `pages` layer pages from `layer_page` into the registered buffer
/// from its page `at`.
#[derive(Clone, Copy)]
struct Span {
    at: u64,
    layer_page: u64,
    pages: u64,
}

fn io(what: &str, e: CallError<block::Error>) -> FsError {
    FsError::Io(format!("{what}: {e:?}"))
}

impl BlockDisk {
    /// The most pages one request moves on `layer`'s device.
    fn max_pages(&self, layer: usize) -> u64 {
        self.devices
            .get(layer)
            .map_or(1, |d| (d.info.max_sectors as u64 * 512 / PAGE_U64).max(1))
    }

    /// Reads every span into registration `registration` of `layer`'s driver, with at
    /// most `IN_FLIGHT` requests outstanding, and returns when all are answered.
    fn transfer(&mut self, layer: usize, registration: u32, spans: &[Span]) -> Result<(), FsError> {
        let device = self
            .devices
            .get_mut(layer)
            .ok_or_else(|| FsError::Io(format!("no device for layer {layer}")))?;
        let mut waiting: VecDeque<u32> = VecDeque::new();
        let deadline = deadline_in(DEVICE_WAIT);
        let mut sent = 0u64;
        let answer = |device: &mut Device, txid: u32| -> Result<(), FsError> {
            let mut reply = device
                .client
                .core()
                .reply(txid, deadline)
                .map_err(|e| io(&device.service, e.typed()))?;
            reply
                .decoder()
                .finish()
                .map_err(|_| FsError::Io(format!("{}: a malformed answer", device.service)))
        };
        for span in spans {
            if waiting.len() >= IN_FLIGHT
                && let Some(txid) = waiting.pop_front()
            {
                answer(device, txid)?;
            }
            let mut e = Encoder::new();
            let fields = registration
                .encode(&mut e)
                .and_then(|()| (span.at as u32).encode(&mut e))
                .and_then(|()| (span.layer_page * 8).encode(&mut e))
                .and_then(|()| ((span.pages * 8) as u32).encode(&mut e));
            fields.map_err(|e| FsError::Io(format!("encoding a read: {e:?}")))?;
            let txid = device
                .client
                .core()
                .send(6, e)
                .map_err(|e| io(&device.service, e.typed()))?;
            waiting.push_back(txid);
            sent += 1;
        }
        while let Some(txid) = waiting.pop_front() {
            answer(device, txid)?;
        }
        self.requests += sent;
        Ok(())
    }
}

impl Disk for BlockDisk {
    fn fetch(&mut self, reads: &[Fetch]) -> Result<(), FsError> {
        let mut sorted = reads.to_vec();
        sorted.sort_by_key(|r| (r.layer, r.page));
        let mut i = 0;
        while i < sorted.len() {
            let layer = sorted[i].layer;
            let max = self.max_pages(layer);
            let mut spans: Vec<Span> = Vec::new();
            while i < sorted.len() && sorted[i].layer == layer {
                let r = sorted[i];
                match spans.last_mut() {
                    Some(s)
                        if s.layer_page + s.pages == r.page
                            && s.at + s.pages == r.slot as u64
                            && s.pages < max =>
                    {
                        s.pages += 1
                    }
                    _ => spans.push(Span {
                        at: r.slot as u64,
                        layer_page: r.page,
                        pages: 1,
                    }),
                }
                i += 1;
            }
            let registration = self
                .devices
                .get(layer)
                .ok_or_else(|| FsError::Io(format!("no device for layer {layer}")))?
                .slots;
            self.transfer(layer, registration, &spans)?;
        }
        Ok(())
    }

    fn copy(&mut self, slot: u32, offset: usize, out: &mut [u8]) -> Result<(), FsError> {
        read_fast(&self.view, slot as usize * PAGE + offset, out)
    }

    fn device_pages(&mut self, layer: usize) -> Result<u64, FsError> {
        self.devices
            .get(layer)
            .map(|d| d.info.sectors * 512 / PAGE_U64)
            .ok_or_else(|| FsError::Io(format!("no device for layer {layer}")))
    }
}

/// Copies out of a mapping eight bytes an access where it can.
fn read_fast(view: &Mapping<'_>, mut at: usize, out: &mut [u8]) -> Result<(), FsError> {
    let fail = |e: moruna_svc::Error| FsError::Io(format!("reading the cache: {}", e.name()));
    let mut i = 0;
    while i < out.len() && !at.is_multiple_of(8) {
        out[i] = view.read8(at).map_err(fail)?;
        i += 1;
        at += 1;
    }
    while out.len() - i >= 8 {
        let v = view.read64(at).map_err(fail)?;
        out[i..i + 8].copy_from_slice(&v.to_le_bytes());
        i += 8;
        at += 8;
    }
    while i < out.len() {
        out[i] = view.read8(at).map_err(fail)?;
        i += 1;
        at += 1;
    }
    Ok(())
}

/// Copies one page into a mapping, eight bytes an access.
fn write_page(view: &Mapping<'_>, at: usize, page: &[u8; PAGE]) -> Result<(), FsError> {
    for (i, word) in page.as_chunks::<8>().0.iter().enumerate() {
        view.write64(at + i * 8, u64::from_le_bytes(*word))
            .map_err(|e| FsError::Io(format!("filling a mapping: {}", e.name())))?;
    }
    Ok(())
}

/// A mapping kept for the next task that maps the same pages.
struct Mapped {
    /// Layer, node, first page, pages.
    key: (usize, u32, u64, u64),
    memory: Memory,
    bytes: u64,
    used: u64,
}

struct Shared {
    cache: Cache<BlockDisk>,
    overlay: Overlay,
    out: Console,
    maps: Vec<Mapped>,
    map_bytes: u64,
    tick: u64,
    map_hits: u64,
    map_pages_read: u64,
    map_pages_copied: u64,
    served: u64,
}

impl Shared {
    /// A refusal for the client, with what went wrong with a layer or a device said
    /// on the console first.
    fn fail(&self, e: FsError) -> imagefs::Error {
        match e {
            FsError::NotFound => imagefs::Error::NotFound,
            FsError::NotDirectory => imagefs::Error::NotDirectory,
            FsError::IsDirectory => imagefs::Error::IsDirectory,
            FsError::ReadOnly => imagefs::Error::ReadOnly,
            FsError::NoSpace => imagefs::Error::NoSpace,
            FsError::Exists => imagefs::Error::Exists,
            FsError::NotEmpty => imagefs::Error::NotEmpty,
            FsError::BadPath => imagefs::Error::BadPath,
            FsError::Loop => imagefs::Error::Loop,
            FsError::BadRange => imagefs::Error::BadRange,
            FsError::TooLarge => imagefs::Error::TooLarge,
            FsError::Unsupported => imagefs::Error::Unsupported,
            FsError::Corrupt(why) => {
                println!(self.out, "KC2 imagefs: refused a corrupt page: {}", why);
                imagefs::Error::Corrupt
            }
            FsError::Io(why) => {
                println!(self.out, "KC2 imagefs: a device failed: {}", why);
                imagefs::Error::Io
            }
        }
    }

    fn stats(&self) -> Stats {
        let c = self.cache.stats();
        Stats {
            hits: c.hits,
            misses: c.misses,
            pages_read: c.pages_read + self.map_pages_read,
            block_requests: self.cache.disk_ref().requests,
            evictions: c.evictions,
            corrupt: c.corrupt,
            resident: self.cache.resident() as u64,
            capacity: self.cache.capacity() as u64,
            map_hits: self.map_hits,
            map_pages_read: self.map_pages_read,
            map_pages_copied: self.map_pages_copied,
        }
    }

    /// The kept mapping of `key`, as a new read-only handle.
    fn kept(&mut self, key: (usize, u32, u64, u64)) -> Option<Memory> {
        self.tick += 1;
        let tick = self.tick;
        let m = self.maps.iter_mut().find(|m| m.key == key)?;
        m.used = tick;
        let handle = m
            .memory
            .0
            .duplicate(Rights::READ | Rights::MAP | Rights::DUPLICATE | Rights::TRANSFER)
            .ok()?;
        self.map_hits += 1;
        Some(Memory(handle))
    }

    fn keep(&mut self, key: (usize, u32, u64, u64), memory: Memory, bytes: u64) {
        if bytes > MAP_CACHE_BYTES {
            return;
        }
        while self.maps.len() >= MAP_CACHE_ENTRIES || self.map_bytes + bytes > MAP_CACHE_BYTES {
            let Some(oldest) = (0..self.maps.len()).min_by_key(|i| self.maps[*i].used) else {
                break;
            };
            let gone = self.maps.swap_remove(oldest);
            self.map_bytes -= gone.bytes;
        }
        self.tick += 1;
        self.map_bytes += bytes;
        self.maps.push(Mapped {
            key,
            memory,
            bytes,
            used: self.tick,
        });
    }

    /// A new mapping of `pages` pages of `file` from file page `first`.
    fn build_map(&mut self, file: &FileMap, first: u64, pages: u64) -> Result<Memory, FsError> {
        let layer = file.layer;
        let bytes = pages * PAGE_U64;
        let memory = Memory::create_dma(bytes).map_err(|_| FsError::TooLarge)?;
        let view = memory
            .map(0, bytes, mapflags::WRITE)
            .map_err(|e| FsError::Io(format!("mapping a new mapping: {}", e.name())))?;
        let max = self.cache.disk().max_pages(layer);
        let plan = map::plan(&self.cache, file, first, pages, max)?;
        for (at, lp) in &plan.copies {
            let mut page = [0u8; PAGE];
            self.cache.read(layer, *lp, 0, &mut page)?;
            write_page(&view, *at as usize * PAGE, &page)?;
        }
        self.map_pages_copied += plan.copies.len() as u64;
        if !plan.runs.is_empty() {
            let mut hashes: Vec<[u8; 32]> = Vec::new();
            for run in &plan.runs {
                for p in 0..run.pages {
                    hashes.push(self.cache.expected(layer, run.layer_page + p)?);
                }
            }
            let handle = memory
                .0
                .duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER)
                .map_err(|e| FsError::Io(format!("a handle for the driver: {}", e.name())))?;
            let disk = self.cache.disk();
            let device = disk
                .devices
                .get_mut(layer)
                .ok_or_else(|| FsError::Io(format!("no device for layer {layer}")))?;
            let (registration, _) = device
                .client
                .register(Memory(handle))
                .map_err(|e| io(&device.service, e))?;
            let spans: Vec<Span> = plan
                .runs
                .iter()
                .map(|r| Span {
                    at: r.at,
                    layer_page: r.layer_page,
                    pages: r.pages,
                })
                .collect();
            let read = disk.transfer(layer, registration, &spans);
            let device = &mut disk.devices[layer];
            let ended = device
                .client
                .unregister(registration)
                .map_err(|e| io(&device.service, e));
            read?;
            ended?;
            let mut i = 0;
            let mut page = [0u8; PAGE];
            for run in &plan.runs {
                for p in 0..run.pages {
                    read_fast(&view, (run.at + p) as usize * PAGE, &mut page)?;
                    if sha256::digest(&page) != hashes[i] {
                        return Err(FsError::Corrupt(format!(
                            "layer {layer}: page {} read into a mapping does not match its hash",
                            run.layer_page + p
                        )));
                    }
                    i += 1;
                }
                self.map_pages_read += run.pages;
            }
        }
        drop(view);
        Ok(memory)
    }
}

/// What a connection has open.
enum Open {
    Image {
        place: Place,
        file: Option<FileMap>,
        listing: Option<Vec<Dirent>>,
    },
    Scratch {
        id: u32,
        write: bool,
        listing: Option<Vec<Dirent>>,
    },
}

/// One task's connection.
struct Task {
    shared: Rc<RefCell<Shared>>,
    scratch: Scratch,
    files: BTreeMap<u32, Open>,
    next: u32,
}

fn to_kind(k: Kind) -> u8 {
    match k {
        Kind::Directory => kind::DIRECTORY,
        Kind::Regular => kind::REGULAR,
        Kind::Symlink | Kind::Whiteout => kind::SYMLINK,
    }
}

fn stat_of(s: moruna_imagefs::fs::Stat) -> Stat {
    Stat {
        kind: to_kind(s.kind),
        mode: s.mode,
        size: s.size,
        ino: s.ino,
    }
}

impl Task {
    fn file(&mut self, file: u32) -> Result<&mut Open, imagefs::Error> {
        self.files.get_mut(&file).ok_or(imagefs::Error::BadFile)
    }

    /// A directory's entries, listed once and kept with the open file.
    fn listing(&mut self, file: u32) -> Result<Vec<Dirent>, imagefs::Error> {
        let shared = self.shared.clone();
        let mut sh = shared.borrow_mut();
        let sh = &mut *sh;
        let scratch = &self.scratch;
        let open = self.files.get_mut(&file).ok_or(imagefs::Error::BadFile)?;
        let (place, slot) = match open {
            Open::Image { place, listing, .. } => (place.clone(), listing),
            Open::Scratch { id, listing, .. } => (Place::Scratch(*id), listing),
        };
        if let Some(l) = slot {
            return Ok(l.clone());
        }
        if !place.is_directory(scratch) {
            return Err(imagefs::Error::NotDirectory);
        }
        let tree = Tree {
            overlay: &sh.overlay,
            scratch,
        };
        let entries = tree.list(&mut sh.cache, &place).map_err(|e| sh.fail(e))?;
        let list: Vec<Dirent> = entries
            .into_iter()
            .map(|(name, k, ino)| (Bytes(name), to_kind(k), ino))
            .collect();
        *slot = Some(list.clone());
        Ok(list)
    }
}

impl imagefs::Server for Task {
    fn open(
        &mut self,
        _: &mut Context<'_>,
        path: Bytes,
        flags: u32,
        mode: u32,
    ) -> Result<(u32, Stat), imagefs::Error> {
        if flags & !open::ALL != 0 {
            return Err(imagefs::Error::Unsupported);
        }
        if self.files.len() >= FILES_MAX {
            return Err(imagefs::Error::TooManyFiles);
        }
        let shared = self.shared.clone();
        let mut sh = shared.borrow_mut();
        let sh = &mut *sh;
        let write = flags & (open::WRITE | open::TRUNCATE) != 0;
        let found = {
            let tree = Tree {
                overlay: &sh.overlay,
                scratch: &self.scratch,
            };
            tree.resolve(&mut sh.cache, &path.0, flags & open::NOFOLLOW == 0)
        };
        let place = match found {
            Ok(place) => {
                if flags & open::CREATE != 0 && flags & open::EXCLUSIVE != 0 {
                    return Err(imagefs::Error::Exists);
                }
                place
            }
            Err(FsError::NotFound) if flags & open::CREATE != 0 => {
                let parent = {
                    let tree = Tree {
                        overlay: &sh.overlay,
                        scratch: &self.scratch,
                    };
                    tree.parent(&mut sh.cache, &path.0)
                };
                match parent.map_err(|e| sh.fail(e))? {
                    (Place::Scratch(dir), name) => Place::Scratch(
                        self.scratch
                            .create(dir, &name, mode)
                            .map_err(|e| sh.fail(e))?,
                    ),
                    _ => return Err(imagefs::Error::ReadOnly),
                }
            }
            Err(e) => return Err(sh.fail(e)),
        };
        let directory = place.is_directory(&self.scratch);
        if matches!(place, Place::Image(Found::Symlink { .. })) {
            // O_NOFOLLOW on a symbolic link.
            return Err(imagefs::Error::Loop);
        }
        if directory && write {
            return Err(imagefs::Error::IsDirectory);
        }
        if !directory && flags & open::DIRECTORY != 0 {
            return Err(imagefs::Error::NotDirectory);
        }
        let open = match place {
            Place::Scratch(id) => {
                if flags & open::TRUNCATE != 0 {
                    self.scratch.truncate(id, 0).map_err(|e| sh.fail(e))?;
                }
                self.scratch.open(id).map_err(|e| sh.fail(e))?;
                Open::Scratch {
                    id,
                    write,
                    listing: None,
                }
            }
            _ if write => return Err(imagefs::Error::ReadOnly),
            place => Open::Image {
                place,
                file: None,
                listing: None,
            },
        };
        let stat = match &open {
            Open::Image { place, .. } => place.stat(&self.scratch),
            Open::Scratch { id, .. } => Place::Scratch(*id).stat(&self.scratch),
        }
        .map_err(|e| sh.fail(e))?;
        let number = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.files.insert(number, open);
        Ok((number, stat_of(stat)))
    }

    fn stat(
        &mut self,
        _: &mut Context<'_>,
        path: Bytes,
        follow: bool,
    ) -> Result<Stat, imagefs::Error> {
        let shared = self.shared.clone();
        let mut sh = shared.borrow_mut();
        let sh = &mut *sh;
        let tree = Tree {
            overlay: &sh.overlay,
            scratch: &self.scratch,
        };
        let place = tree
            .resolve(&mut sh.cache, &path.0, follow)
            .map_err(|e| sh.fail(e))?;
        place
            .stat(&self.scratch)
            .map(stat_of)
            .map_err(|e| sh.fail(e))
    }

    fn fstat(&mut self, _: &mut Context<'_>, file: u32) -> Result<Stat, imagefs::Error> {
        let place = match self.file(file)? {
            Open::Image { place, .. } => place.clone(),
            Open::Scratch { id, .. } => Place::Scratch(*id),
        };
        let shared = self.shared.clone();
        let sh = shared.borrow();
        place
            .stat(&self.scratch)
            .map(stat_of)
            .map_err(|e| sh.fail(e))
    }

    fn readdir(
        &mut self,
        _: &mut Context<'_>,
        file: u32,
        cookie: u32,
    ) -> Result<(Vec<Dirent>, u32), imagefs::Error> {
        let list = self.listing(file)?;
        let mut out = Vec::new();
        let mut bytes = 0;
        let mut at = cookie as usize;
        while let Some(entry) = list.get(at) {
            let size = 24 + entry.0.0.len();
            if bytes + size > READDIR_BYTES {
                break;
            }
            bytes += size;
            out.push(entry.clone());
            at += 1;
        }
        Ok((out, at as u32))
    }

    fn readlink(&mut self, _: &mut Context<'_>, path: Bytes) -> Result<Bytes, imagefs::Error> {
        let shared = self.shared.clone();
        let mut sh = shared.borrow_mut();
        let sh = &mut *sh;
        let tree = Tree {
            overlay: &sh.overlay,
            scratch: &self.scratch,
        };
        tree.readlink(&mut sh.cache, &path.0)
            .map(Bytes)
            .map_err(|e| sh.fail(e))
    }

    fn read(
        &mut self,
        _: &mut Context<'_>,
        file: u32,
        offset: u64,
        len: u32,
    ) -> Result<Bytes, imagefs::Error> {
        if len > READ_MAX {
            return Err(imagefs::Error::TooLarge);
        }
        let shared = self.shared.clone();
        let mut sh = shared.borrow_mut();
        let sh = &mut *sh;
        let open = self.files.get_mut(&file).ok_or(imagefs::Error::BadFile)?;
        let (place, map) = match open {
            Open::Scratch { id, .. } => {
                return self
                    .scratch
                    .read(*id, offset, len as usize)
                    .map(Bytes)
                    .map_err(|e| sh.fail(e));
            }
            Open::Image { place, file, .. } => (place, file),
        };
        let Place::Image(Found::File { layer, node, .. }) = place else {
            return Err(imagefs::Error::IsDirectory);
        };
        if map.is_none() {
            *map = Some(
                sh.overlay
                    .file(&mut sh.cache, *layer, node)
                    .map_err(|e| sh.fail(e))?,
            );
        }
        let Some(map) = map.as_ref() else {
            return Err(imagefs::Error::Io);
        };
        if offset >= map.size {
            return Ok(Bytes(Vec::new()));
        }
        let n = (len as u64).min(map.size - offset) as usize;
        let first = offset / PAGE_U64;
        let last = (offset + n as u64 - 1) / PAGE_U64;
        let pages: Vec<u64> = (first..=last)
            .filter_map(|p| map.layer_page(p).flatten())
            .collect();
        sh.cache
            .prefetch(map.layer, &pages)
            .map_err(|e| sh.fail(e))?;
        let mut out = vec![0u8; n];
        let mut done = 0usize;
        while done < n {
            let at = offset + done as u64;
            let within = (at % PAGE_U64) as usize;
            let take = (PAGE - within).min(n - done);
            if let Some(Some(lp)) = map.layer_page(at / PAGE_U64) {
                sh.cache
                    .read(map.layer, lp, within, &mut out[done..done + take])
                    .map_err(|e| sh.fail(e))?;
            }
            done += take;
        }
        Ok(Bytes(out))
    }

    fn map(
        &mut self,
        _: &mut Context<'_>,
        file: u32,
        offset: u64,
        len: u64,
    ) -> Result<Memory, imagefs::Error> {
        let shared = self.shared.clone();
        let mut sh = shared.borrow_mut();
        let sh = &mut *sh;
        let open = self.files.get_mut(&file).ok_or(imagefs::Error::BadFile)?;
        let (place, map) = match open {
            Open::Scratch { id, .. } => {
                return Err(if self.scratch.info(*id).is_ok_and(|i| i.directory) {
                    imagefs::Error::IsDirectory
                } else {
                    imagefs::Error::Unsupported
                });
            }
            Open::Image { place, file, .. } => (place, file),
        };
        let Place::Image(Found::File { layer, index, node }) = place else {
            return Err(imagefs::Error::IsDirectory);
        };
        if len > MAP_MAX {
            return Err(imagefs::Error::TooLarge);
        }
        if !offset.is_multiple_of(PAGE_U64) || len == 0 {
            return Err(imagefs::Error::BadRange);
        }
        if map.is_none() {
            *map = Some(
                sh.overlay
                    .file(&mut sh.cache, *layer, node)
                    .map_err(|e| sh.fail(e))?,
            );
        }
        let Some(map) = map.as_ref() else {
            return Err(imagefs::Error::Io);
        };
        let first = offset / PAGE_U64;
        let pages = len.div_ceil(PAGE_U64);
        if first + pages > map.pages() {
            return Err(imagefs::Error::BadRange);
        }
        let key = (*layer, *index, first, pages);
        if let Some(memory) = sh.kept(key) {
            return Ok(memory);
        }
        let memory = sh.build_map(map, first, pages).map_err(|e| sh.fail(e))?;
        let given = memory
            .0
            .duplicate(Rights::READ | Rights::MAP | Rights::DUPLICATE | Rights::TRANSFER)
            .map_err(|_| imagefs::Error::Io)?;
        sh.keep(key, memory, pages * PAGE_U64);
        Ok(Memory(given))
    }

    fn write(
        &mut self,
        _: &mut Context<'_>,
        file: u32,
        offset: u64,
        data: Bytes,
    ) -> Result<u32, imagefs::Error> {
        let Open::Scratch {
            id, write: true, ..
        } = self.file(file)?
        else {
            return Err(imagefs::Error::BadFile);
        };
        let id = *id;
        let shared = self.shared.clone();
        let sh = shared.borrow();
        self.scratch
            .write(id, offset, &data.0)
            .map(|n| n as u32)
            .map_err(|e| sh.fail(e))
    }

    fn truncate(
        &mut self,
        _: &mut Context<'_>,
        file: u32,
        size: u64,
    ) -> Result<(), imagefs::Error> {
        let Open::Scratch {
            id, write: true, ..
        } = self.file(file)?
        else {
            return Err(imagefs::Error::BadFile);
        };
        let id = *id;
        let shared = self.shared.clone();
        let sh = shared.borrow();
        self.scratch.truncate(id, size).map_err(|e| sh.fail(e))
    }

    fn close(&mut self, _: &mut Context<'_>, file: u32) -> Result<(), imagefs::Error> {
        match self.files.remove(&file) {
            Some(Open::Scratch { id, .. }) => {
                self.scratch.close(id);
                Ok(())
            }
            Some(Open::Image { .. }) => Ok(()),
            None => Err(imagefs::Error::BadFile),
        }
    }

    fn mkdir(&mut self, _: &mut Context<'_>, path: Bytes, mode: u32) -> Result<(), imagefs::Error> {
        let shared = self.shared.clone();
        let mut sh = shared.borrow_mut();
        let sh = &mut *sh;
        let parent = {
            let tree = Tree {
                overlay: &sh.overlay,
                scratch: &self.scratch,
            };
            tree.parent(&mut sh.cache, &path.0)
        };
        match parent.map_err(|e| sh.fail(e))? {
            (Place::Scratch(dir), name) => self
                .scratch
                .mkdir(dir, &name, mode)
                .map(|_| ())
                .map_err(|e| sh.fail(e)),
            _ => Err(imagefs::Error::ReadOnly),
        }
    }

    fn remove(&mut self, _: &mut Context<'_>, path: Bytes) -> Result<(), imagefs::Error> {
        let shared = self.shared.clone();
        let mut sh = shared.borrow_mut();
        let sh = &mut *sh;
        let parent = {
            let tree = Tree {
                overlay: &sh.overlay,
                scratch: &self.scratch,
            };
            tree.parent(&mut sh.cache, &path.0)
        };
        match parent.map_err(|e| sh.fail(e))? {
            (Place::Scratch(dir), name) => self.scratch.remove(dir, &name).map_err(|e| sh.fail(e)),
            _ => Err(imagefs::Error::ReadOnly),
        }
    }

    fn quota(&mut self, _: &mut Context<'_>, bytes: u64) -> Result<(), imagefs::Error> {
        let shared = self.shared.clone();
        let sh = shared.borrow();
        self.scratch.set_quota(bytes).map_err(|e| sh.fail(e))
    }

    fn stats(&mut self, _: &mut Context<'_>) -> Result<Stats, imagefs::Error> {
        Ok(self.shared.borrow().stats())
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if let Ok(mut sh) = self.shared.try_borrow_mut() {
            sh.served += 1;
        }
    }
}

/// The `layers` slot's text: `moruna-layers 1`, then `layer SERVICE SHA256` lines,
/// lowest first.
fn layers() -> Result<Vec<(String, String)>, String> {
    let memory = Memory(start::take("layers").ok_or("no layers slot")?);
    let mapping = memory
        .map(0, PAGE_U64, 0)
        .map_err(|e| format!("mapping the layer list: {}", e.name()))?;
    let mut bytes = vec![0u8; PAGE];
    mapping
        .read(0, &mut bytes)
        .map_err(|e| format!("reading the layer list: {}", e.name()))?;
    let len = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let text = core::str::from_utf8(&bytes[..len]).map_err(|_| "the layer list is not text")?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    if lines.next().map(str::trim) != Some(LAYERS_HEADER) {
        return Err(format!("the layer list does not begin `{LAYERS_HEADER}`"));
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let ["layer", service, id] = fields.as_slice() else {
            return Err(format!("`{line}` is not `layer SERVICE SHA256`"));
        };
        if id.len() != 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(format!("`{id}` is not a sha256 in lower-case hex"));
        }
        if out.iter().any(|(s, i)| s == service || i == id) {
            return Err(format!("`{line}` names a device or a layer twice"));
        }
        out.push((service.to_string(), id.to_string()));
    }
    if out.is_empty() {
        return Err("the layer list names no layer".to_string());
    }
    Ok(out)
}

fn mount(out: &Console) -> Result<Shared, String> {
    let list = layers()?;
    let directory = start::take("directory").ok_or("no directory slot")?;
    let mut dir = devices_protocol::open_directory(Channel(directory), deadline_in(300 * SECONDS))
        .map_err(|e| format!("the directory: {e:?}"))?;
    dir.core().set_timeout(300 * SECONDS);
    let slab = Memory::create_dma(CACHE_PAGES as u64 * PAGE_U64)
        .map_err(|e| format!("the cache's memory: {}", e.name()))?;
    let mut devices = Vec::new();
    for (service, id) in list {
        let channel = dir
            .lookup(service.clone())
            .map_err(|e| format!("looking up {service}: {e:?}"))?;
        let mut client = block::Client::connect(channel, deadline_in(60 * SECONDS))
            .map_err(|e| format!("hello to {service}: {e:?}"))?;
        client.core().set_timeout(DEVICE_WAIT);
        let info = client
            .info()
            .map_err(|e| format!("{service}'s info: {e:?}"))?;
        if !info.read_only {
            return Err(format!(
                "{service} is writable; a layer's device is read only"
            ));
        }
        if info.sector_size != 512 {
            return Err(format!(
                "{service} has sectors of {} bytes",
                info.sector_size
            ));
        }
        let handle = slab
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER)
            .map_err(|e| format!("a handle to the cache: {}", e.name()))?;
        let (slots, pages) = client
            .register(Memory(handle))
            .map_err(|e| format!("registering the cache with {service}: {e:?}"))?;
        if pages != CACHE_PAGES {
            return Err(format!(
                "{service} registered {pages} of the cache's {CACHE_PAGES} pages"
            ));
        }
        devices.push(Device {
            service,
            id,
            client,
            info,
            slots,
        });
    }
    // Every device is found: the directory is no longer needed, and holding it would
    // keep the device manager waiting for its last client.
    drop(dir);
    let view = slab
        .map_detached(0, CACHE_PAGES as u64 * PAGE_U64, 0)
        .map_err(|e| format!("mapping the cache: {}", e.name()))?;
    let disk = BlockDisk {
        devices,
        view,
        _slab: slab,
        requests: 0,
    };
    let mut cache = Cache::new(disk, CACHE_PAGES).map_err(|e| format!("the cache: {e:?}"))?;
    let mut described = Vec::new();
    for i in 0..cache.disk().devices.len() {
        cache.mount().map_err(|e| format!("layer {i}: {e:?}"))?;
        let pages = cache.layers()[i].geometry.pages;
        let d = &cache.disk().devices[i];
        described.push(format!(
            "{} ({}..., {} pages)",
            d.service,
            &d.id[..12],
            pages
        ));
    }
    println!(
        out,
        "KC2 imagefs: up: {} layers, lowest first: {}; a cache of {} pages",
        described.len(),
        described.join(", "),
        CACHE_PAGES
    );
    let overlay = Overlay::new(cache.layers().to_vec());
    Ok(Shared {
        cache,
        overlay,
        out: Console(
            out.0
                .duplicate(Rights::WRITE)
                .map_err(|e| e.name().to_string())?,
        ),
        maps: Vec::new(),
        map_bytes: 0,
        tick: 0,
        map_hits: 0,
        map_pages_read: 0,
        map_pages_copied: 0,
        served: 0,
    })
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let shared = match mount(&out) {
        Ok(s) => Rc::new(RefCell::new(s)),
        Err(e) => {
            println!(out, "KC2 imagefs: FAIL: {}", e);
            return 1;
        }
    };
    let mut dispatcher = Dispatcher::new();
    let mut listeners = 0;
    for i in 0..start::count() {
        let Some((_, _, slot)) = start::entry(i) else {
            continue;
        };
        if !slot.starts_with("listen") {
            continue;
        }
        let Some(handle) = start::take(slot) else {
            continue;
        };
        let s = shared.clone();
        let factory = imagefs::factory(move || Task {
            shared: s.clone(),
            scratch: Scratch::new(),
            files: BTreeMap::new(),
            next: 1,
        });
        if let Err(e) = dispatcher.listen(Channel(handle), factory) {
            println!(
                out,
                "KC2 imagefs: FAIL: listening on {}: {}",
                slot,
                e.name()
            );
            return 1;
        }
        listeners += 1;
    }
    if listeners == 0 {
        println!(out, "KC2 imagefs: FAIL: no listen slot");
        return 1;
    }
    while dispatcher.channels() > 0 {
        if let Err(e) = dispatcher.turn(DEADLINE_INFINITE) {
            println!(out, "KC2 imagefs: FAIL: wait: {}", e.name());
            return 1;
        }
    }
    let sh = shared.borrow();
    let s = sh.stats();
    println!(
        out,
        "KC2 imagefs: served {} connections until every client left: {} hits, {} misses, {} pages read in {} requests, {} evictions, {} mappings kept and {} served again",
        sh.served,
        s.hits,
        s.misses,
        s.pages_read,
        s.block_requests,
        s.evictions,
        sh.maps.len(),
        s.map_hits
    );
    0
}
