//! The engine's arena on granted memory objects (`architecture/data-os.md` §2, §3): stream
//! C's module. Only stream C writes this file.
//!
//! On the Moruna kernel the arena's host regions are DMA memory objects charged to the
//! engine's own account (the run's), made when the run starts and mapped once, with large
//! pages and every page present (`map::HUGE | map::POPULATE`, contract C4). Until stream A
//! answers those flags the objects are mapped plainly, and their pages still never fault: the
//! kernel allocates a memory object's pages, zeroed, when it is made, and fills every page
//! table entry when it is mapped. Nothing is given back while the run lasts, so a page the
//! arena reuses is never zeroed again.
//!
//! [`Objects`] is the arena's [`HostBacking`]. [`Pins`] gives one device the arena's pages:
//! each region is pinned through the device's DMA resource once, the first time an operation
//! names it, and its pages' device addresses kept as runs, so a read lands by DMA in the
//! arena buffer the reactor was handed (contract C2) and no call is made per request.
//!
//! [`Placed`] is the engine's `Allocator`: the arena, with each of the scheduler's workers
//! placed on a core the first time it allocates (contract C5). A place is never a function:
//! any worker still runs any function's batch, as the scheduler decides.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use moruna_arena::{Granted, HostBacking};
use moruna_kernel::{AllocStats, Allocator, Buffer, MorunaError, Tier};
use moruna_sys::{Error, Handle, Mapping, Memory, PAGE_SIZE, Pin, Thread, limits, map};

use crate::blk::engine::Translate;

/// One region the arena was given.
struct Region {
    id: u64,
    base: u64,
    bytes: u64,
    memory: Arc<Memory>,
}

/// What keeps a region mapped: dropped when the arena lets the region go.
struct Keep {
    _mapping: Mapping<'static>,
    base: u64,
    regions: Weak<Mutex<Vec<Region>>>,
}

impl Drop for Keep {
    fn drop(&mut self) {
        if let Some(regions) = self.regions.upgrade() {
            lock(&regions).retain(|r| r.base != self.base);
        }
    }
}

/// How the arena's memory was mapped, for the run's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mapped {
    /// Regions made.
    pub regions: u64,
    /// Their bytes.
    pub bytes: u64,
    /// Regions mapped with large pages and every page present.
    pub huge: u64,
    /// Regions mapped plainly because the kernel refused the flags.
    pub plain: u64,
}

/// The arena's memory on the Moruna kernel: DMA memory objects, at most
/// `limits::MEMORY_MAX_BYTES` each.
pub struct Objects {
    regions: Arc<Mutex<Vec<Region>>>,
    made: AtomicU64,
    bytes: AtomicU64,
    huge: AtomicU64,
    plain: AtomicU64,
    max: u64,
}

impl Objects {
    /// A backing that makes regions on demand.
    pub fn new() -> Arc<Objects> {
        Objects::with_max(limits::MEMORY_MAX_BYTES)
    }

    /// A backing whose regions are at most `max` bytes.
    pub fn with_max(max: u64) -> Arc<Objects> {
        Arc::new(Objects {
            regions: Arc::default(),
            made: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            huge: AtomicU64::new(0),
            plain: AtomicU64::new(0),
            max,
        })
    }

    /// How the regions were mapped.
    pub fn mapped(&self) -> Mapped {
        Mapped {
            regions: self.made.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            huge: self.huge.load(Ordering::Relaxed),
            plain: self.plain.load(Ordering::Relaxed),
        }
    }

    /// The region holding `addr`: its id, base and length. A region's id is never another's,
    /// though a later region may be mapped where a released one was.
    fn find(&self, addr: u64) -> Option<(u64, u64, u64)> {
        let regions = lock(&self.regions);
        let i = regions.partition_point(|r| r.base <= addr).checked_sub(1)?;
        let r = &regions[i];
        (addr < r.base + r.bytes).then_some((r.id, r.base, r.bytes))
    }

    /// The memory object of region `id` and its bytes, while the arena holds it.
    fn memory(&self, id: u64) -> Option<(Arc<Memory>, u64)> {
        lock(&self.regions)
            .iter()
            .find(|r| r.id == id)
            .map(|r| (Arc::clone(&r.memory), r.bytes))
    }

    /// Whether the arena still holds region `id`.
    fn holds(&self, id: u64) -> bool {
        lock(&self.regions).iter().any(|r| r.id == id)
    }
}

impl HostBacking for Objects {
    fn region_max(&self) -> u64 {
        self.max
    }

    fn region(&self, bytes: u64) -> moruna_kernel::Result<Granted> {
        let bytes = bytes.next_multiple_of(PAGE_SIZE);
        let refused = |what: &str, e: Error| MorunaError::Config {
            name: "budget.host",
            msg: format!(
                "{what} a memory object of {bytes} bytes for the arena: {}",
                e.name()
            ),
        };
        let memory = Memory::create_dma(bytes).map_err(|e| refused("making", e))?;
        let (mapping, huge) =
            match memory.map_detached(0, bytes, map::WRITE | map::HUGE | map::POPULATE) {
                Ok(m) => (m, true),
                // Stream A's flags are refused until it lands (data-os-streams.md).
                Err(Error::InvalidArgs | Error::BadSyscall) => (
                    memory
                        .map_detached(0, bytes, map::WRITE)
                        .map_err(|e| refused("mapping", e))?,
                    false,
                ),
                Err(e) => return Err(refused("mapping", e)),
            };
        let base = mapping.addr();
        let id = self.made.fetch_add(1, Ordering::Relaxed);
        {
            let mut regions = lock(&self.regions);
            let at = regions.partition_point(|r| r.base < base);
            regions.insert(
                at,
                Region {
                    id,
                    base,
                    bytes,
                    memory: Arc::new(memory),
                },
            );
        }
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        if huge {
            self.huge.fetch_add(1, Ordering::Relaxed);
        } else {
            self.plain.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Granted {
            base: base as *mut u8,
            bytes,
            huge_pages: huge,
            keep: Box::new(Keep {
                _mapping: mapping,
                base,
                regions: Arc::downgrade(&self.regions),
            }),
        })
    }
}

/// A run of a region's pages a device sees as one block: from byte `offset` of the region,
/// `len` bytes at device address `addr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// Its first byte in the region.
    pub offset: u64,
    /// Its device address.
    pub addr: u64,
    /// Its bytes.
    pub len: u64,
}

/// The runs of a region whose page `i` is at device address `pages[i]`.
pub fn runs_of(pages: &[u64]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (i, &addr) in pages.iter().enumerate() {
        let offset = i as u64 * PAGE_SIZE;
        match runs.last_mut() {
            Some(last) if last.addr + last.len == addr && last.offset + last.len == offset => {
                last.len += PAGE_SIZE;
            }
            _ => runs.push(Run {
                offset,
                addr,
                len: PAGE_SIZE,
            }),
        }
    }
    runs
}

/// Appends the device ranges of bytes `[offset, offset + len)` of a region with `runs`, each
/// at most `u32::MAX` bytes; `false` when the runs do not cover them.
pub fn emit(runs: &[Run], offset: u64, len: u64, out: &mut Vec<(u64, u32)>) -> bool {
    let mut at = offset;
    let end = offset + len;
    let Some(mut i) = runs.partition_point(|r| r.offset <= at).checked_sub(1) else {
        return len == 0;
    };
    while at < end {
        let Some(run) = runs.get(i) else {
            return false;
        };
        if at < run.offset || at >= run.offset + run.len {
            return false;
        }
        let n = (run.offset + run.len - at)
            .min(end - at)
            .min(u64::from(u32::MAX));
        out.push((run.addr + (at - run.offset), n as u32));
        at += n;
        if at == run.offset + run.len {
            i += 1;
        }
    }
    true
}

struct Pinned {
    runs: Vec<Run>,
    _pin: Pin,
    _memory: Arc<Memory>,
}

/// The arena's pages as one device reaches them. A region is pinned when an operation first
/// names it; the pins of regions the arena has let go are dropped at the next pin, and their
/// pages released from the device's quarantine (no operation can name a released region).
pub struct Pins {
    objects: Arc<Objects>,
    dma: Arc<Handle>,
    pinned: RwLock<std::collections::HashMap<u64, Pinned>>,
    calls: AtomicU64,
}

impl Pins {
    /// The arena's regions as the devices `dma` covers reach them, each pinned when an
    /// operation first names it.
    pub fn new(objects: Arc<Objects>, dma: Arc<Handle>) -> Pins {
        Pins {
            objects,
            dma,
            pinned: RwLock::default(),
            calls: AtomicU64::new(0),
        }
    }

    /// Pins made: one per region, never one per request.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// Pins region `id`, and drops the pins of regions the arena no longer holds.
    fn pin(&self, id: u64) -> Result<(), String> {
        let (memory, bytes) = self
            .objects
            .memory(id)
            .ok_or("an arena region let go while an operation named it")?;
        let mut pinned = self.pinned.write().unwrap_or_else(|e| e.into_inner());
        let before = pinned.len();
        pinned.retain(|held, _| self.objects.holds(*held));
        if pinned.len() < before {
            let _ = moruna_sys::dma_release(&self.dma);
        }
        if pinned.contains_key(&id) {
            return Ok(());
        }
        let mut pages = vec![0u64; (bytes / PAGE_SIZE) as usize];
        self.calls.fetch_add(1, Ordering::Relaxed);
        let (pin, n) = Pin::create(&self.dma, &memory.0, true, &mut pages)
            .map_err(|e| format!("pinning an arena region for a disk: {}", e.name()))?;
        pages.truncate(n);
        pinned.insert(
            id,
            Pinned {
                runs: runs_of(&pages),
                _pin: pin,
                _memory: memory,
            },
        );
        Ok(())
    }

    fn lookup(&self, id: u64, offset: u64, len: u64, out: &mut Vec<(u64, u32)>) -> Option<bool> {
        let pinned = self.pinned.read().unwrap_or_else(|e| e.into_inner());
        pinned.get(&id).map(|p| emit(&p.runs, offset, len, out))
    }
}

impl Translate for Pins {
    fn runs(&self, addr: u64, len: u64, out: &mut Vec<(u64, u32)>) -> Result<(), String> {
        let (id, base, bytes) = self
            .objects
            .find(addr)
            .ok_or_else(|| format!("{addr:#x} is not in the arena, which alone a disk reaches"))?;
        if addr + len > base + bytes {
            return Err(format!(
                "{len} bytes at {addr:#x} are not in one arena region"
            ));
        }
        let found = match self.lookup(id, addr - base, len, out) {
            Some(found) => found,
            None => {
                self.pin(id)?;
                self.lookup(id, addr - base, len, out).unwrap_or(false)
            }
        };
        if found {
            Ok(())
        } else {
            Err(format!("{len} bytes at {addr:#x} are not all pinned"))
        }
    }
}

/// The name the scheduler gives worker `n`'s thread.
pub const WORKER_PREFIX: &str = "moruna-worker-";

/// The worker a thread named `name` is, if it is one.
pub fn worker_index(name: &str) -> Option<u32> {
    name.strip_prefix(WORKER_PREFIX)?.parse().ok()
}

/// Places the calling thread on CPU `cpu` (`thread_set_cpu`, stream A). The engine's threads
/// are Linux threads with no handle of their own, so the call names the calling thread with
/// the raw handle 0.
pub fn pin_calling_thread(cpu: u32) -> Result<(), Error> {
    let me = std::mem::ManuallyDrop::new(Thread(Handle::from_raw(0)));
    moruna_sys::datapath::thread_set_cpu(&me, Some(cpu))
}

thread_local! {
    /// Whether this thread has been considered for a place.
    static CONSIDERED: Cell<bool> = const { Cell::new(false) };
}

/// The engine's `Allocator`: `arena`, placing each scheduler worker on CPU `worker % cpus` at
/// its first allocation, once, and never again. A refused placement stops every later attempt.
pub struct Placed {
    arena: Arc<dyn Allocator>,
    cpus: u32,
    pin: fn(u32) -> Result<(), Error>,
    placed: AtomicU64,
    refused: AtomicBool,
}

impl Placed {
    /// `arena` with workers placed over `cpus` CPUs by `pin`.
    pub fn new(
        arena: Arc<dyn Allocator>,
        cpus: usize,
        pin: fn(u32) -> Result<(), Error>,
    ) -> Placed {
        Placed {
            arena,
            cpus: cpus.max(1) as u32,
            pin,
            placed: AtomicU64::new(0),
            refused: AtomicBool::new(false),
        }
    }

    /// Workers placed.
    pub fn placed(&self) -> u64 {
        self.placed.load(Ordering::Relaxed)
    }

    /// Whether the kernel refused a placement.
    pub fn refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }

    fn place(&self) {
        if CONSIDERED.with(|c| c.replace(true)) || self.refused.load(Ordering::Relaxed) {
            return;
        }
        let Some(worker) = std::thread::current().name().and_then(worker_index) else {
            return;
        };
        match (self.pin)(worker % self.cpus) {
            Ok(()) => {
                self.placed.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                if !self.refused.swap(true, Ordering::Relaxed) {
                    tracing::info!(target: "arena.place", why = e.name(), "workers are not placed");
                }
            }
        }
    }
}

impl Allocator for Placed {
    fn alloc(&self, bytes: usize, tier: Tier) -> moruna_kernel::Result<Buffer> {
        self.place();
        self.arena.alloc(bytes, tier)
    }

    fn page_bytes(&self) -> usize {
        self.arena.page_bytes()
    }

    fn stats(&self) -> AllocStats {
        self.arena.stats()
    }

    fn available(&self, tier: Tier) -> Option<u64> {
        self.arena.available(tier)
    }

    fn contains(&self, ptr: *const u8) -> bool {
        self.arena.contains(ptr)
    }

    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        self.arena.tier_of(ptr)
    }

    fn note_payload_copy(&self, bytes: u64) {
        self.arena.note_payload_copy(bytes)
    }

    fn note_boundary_copy(&self, bytes: u64) {
        self.arena.note_boundary_copy(bytes)
    }

    fn is_pinned(&self) -> bool {
        self.arena.is_pinned()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u64 = PAGE_SIZE;

    static PLACED_ON: Mutex<Vec<u32>> = Mutex::new(Vec::new());

    fn record(cpu: u32) -> Result<(), Error> {
        PLACED_ON.lock().unwrap().push(cpu);
        Ok(())
    }

    fn refuse(_: u32) -> Result<(), Error> {
        Err(Error::BadSyscall)
    }

    fn arena() -> Arc<dyn Allocator> {
        moruna_arena::Arena::new(moruna_arena::ArenaConfig {
            host_bytes: 4 << 20,
            huge_pages: moruna_kernel::Guarantee::Absent,
            memlock: moruna_kernel::Guarantee::Absent,
            ..moruna_arena::ArenaConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn a_worker_is_placed_once_at_its_first_allocation_and_no_other_thread_is() {
        assert_eq!(worker_index("moruna-worker-12"), Some(12));
        assert_eq!(worker_index("moruna-worker-"), None);
        assert_eq!(worker_index("moruna-blk-disk.in.a"), None);
        let placed = Arc::new(Placed::new(arena(), 4, record));
        let threads: Vec<_> = (0..6)
            .map(|i| {
                let placed = Arc::clone(&placed);
                std::thread::Builder::new()
                    .name(format!("moruna-worker-{i}"))
                    .spawn(move || {
                        for _ in 0..3 {
                            drop(placed.alloc(64 * 1024, Tier::Host).unwrap());
                        }
                    })
                    .unwrap()
            })
            .chain(std::iter::once({
                let placed = Arc::clone(&placed);
                std::thread::Builder::new()
                    .name("moruna-reactor".into())
                    .spawn(move || drop(placed.alloc(10, Tier::Host).unwrap()))
                    .unwrap()
            }))
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(placed.placed(), 6);
        assert!(!placed.refused());
        let mut on = PLACED_ON.lock().unwrap().clone();
        on.sort();
        assert_eq!(on, vec![0, 0, 1, 1, 2, 3], "worker n on CPU n % 4");
        // The rest is the arena's.
        assert_eq!(placed.page_bytes(), 4096);
        assert!(placed.available(Tier::Host).unwrap() > 0);
        let b = placed.alloc(100, Tier::Host).unwrap();
        assert!(placed.contains(b.as_ptr()));
        assert_eq!(placed.tier_of(b.as_ptr()), Some(Tier::Host));
        placed.note_payload_copy(1);
        placed.note_boundary_copy(2);
        assert!(placed.stats().host_in_use > 0);
        assert!(!placed.is_pinned());
    }

    #[test]
    fn a_refused_placement_is_not_tried_again() {
        let placed = Arc::new(Placed::new(arena(), 0, refuse));
        for i in 0..3 {
            let placed = Arc::clone(&placed);
            std::thread::Builder::new()
                .name(format!("moruna-worker-{i}"))
                .spawn(move || drop(placed.alloc(10, Tier::Host).unwrap()))
                .unwrap()
                .join()
                .unwrap();
        }
        assert!(placed.refused());
        assert_eq!(placed.placed(), 0);
    }

    #[test]
    fn contiguous_pages_are_one_run() {
        let runs = runs_of(&[10 * P, 11 * P, 12 * P, 40 * P, 41 * P, 7 * P]);
        assert_eq!(
            runs,
            vec![
                Run {
                    offset: 0,
                    addr: 10 * P,
                    len: 3 * P
                },
                Run {
                    offset: 3 * P,
                    addr: 40 * P,
                    len: 2 * P
                },
                Run {
                    offset: 5 * P,
                    addr: 7 * P,
                    len: P
                },
            ]
        );
        assert!(runs_of(&[]).is_empty());
    }

    #[test]
    fn a_range_is_emitted_across_runs() {
        let runs = runs_of(&[10 * P, 11 * P, 40 * P, 41 * P]);
        let mut out = Vec::new();
        assert!(emit(&runs, 100, 2 * P, &mut out));
        assert_eq!(
            out,
            vec![(10 * P + 100, (2 * P - 100) as u32), (40 * P, 100)]
        );
        out.clear();
        assert!(emit(&runs, 0, 4 * P, &mut out));
        assert_eq!(out, vec![(10 * P, 2 * P as u32), (40 * P, 2 * P as u32)]);
        out.clear();
        assert!(emit(&runs, 3 * P, 0, &mut out));
        assert!(out.is_empty());
        assert!(!emit(&runs, 3 * P, 2 * P, &mut out), "past the region");
        assert!(!emit(&[], 0, 1, &mut out));
        assert!(emit(&[], 0, 0, &mut out));
    }

    #[test]
    fn a_run_longer_than_a_descriptor_takes_is_split() {
        let pages: Vec<u64> = (0..(1u64 << 20) + 2).map(|i| (1 << 40) + i * P).collect();
        let runs = runs_of(&pages);
        assert_eq!(runs.len(), 1);
        let mut out = Vec::new();
        assert!(emit(&runs, 0, (1u64 << 32) + P, &mut out));
        assert_eq!(out.len(), 2);
        assert_eq!(u64::from(out[0].1), u64::from(u32::MAX));
        assert_eq!(
            out.iter().map(|o| u64::from(o.1)).sum::<u64>(),
            (1u64 << 32) + P
        );
    }
}
