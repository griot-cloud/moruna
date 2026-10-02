//! The allocator guard's gate, frame and counting (05 b, f.9 to f.12, AD-I8 to AD-I15; E13).
//!
//! Nothing here touches the interpreter: the hooks in `python::hooks` call [`allocate`] and the
//! `*_owned` family from inside CPython's and NumPy's allocators, and everything they do is
//! thread-local additions, atomics, and, on the gate's slow path only, one sample of the run's
//! sampler. Every function a hook calls is safe to run without the GIL, allocates nothing, and
//! takes no lock unless the gate has to measure the process.

use core::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use moruna_kernel::{AllocCounts, KernelAlloc, Sampler};

/// `guard.min_bytes` (05 i): the smallest request the gate examines. Every request inside a
/// frame is counted whatever its size; only these can be refused.
pub const GUARD_MIN_BYTES: u64 = 64 * 1024;

/// Index of Python objects in `KernelAlloc` (contracts e.5).
const PYTHON: usize = 0;
/// Index of NumPy data in `KernelAlloc`.
const NUMPY: usize = 1;

/// A guarded request the gate declined (05 b).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Bytes the request asked for.
    pub requested: u64,
    /// The process's memory when it was judged: a fresh measurement plus what was admitted since.
    pub in_use: u64,
    /// The ceiling it would have passed.
    pub ceiling: u64,
}

/// What the guard holds for one kernel now (05 d.1).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveCounts {
    /// NumPy blocks of at least `GUARD_MIN_BYTES` the kernel holds.
    pub live_bytes: u64,
    /// The most `live_bytes` has been.
    pub peak_bytes: u64,
    /// Requests the gate refused for this kernel.
    pub refusals: u64,
}

/// One per run (05 f.9): decides every guarded request.
pub struct MemoryGate {
    sampler: Arc<dyn Sampler>,
    ceiling: AtomicU64,
    base: AtomicU64,
    since: AtomicI64,
    measurements: AtomicU64,
    refusals: AtomicU64,
}

impl std::fmt::Debug for MemoryGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryGate")
            .field("ceiling", &self.ceiling.load(Ordering::Relaxed))
            .field("base", &self.base.load(Ordering::Relaxed))
            .field("since", &self.since.load(Ordering::Relaxed))
            .finish()
    }
}

fn signed(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

impl MemoryGate {
    /// A gate over the run's sampler, starting from `ceiling` and one measurement taken now. A
    /// sample that names a ceiling replaces `ceiling` (the ceiling in force, 03 f.2).
    pub fn new(sampler: Arc<dyn Sampler>, ceiling: u64) -> Arc<MemoryGate> {
        let sample = sampler.sample();
        let ceiling = if sample.ceiling_bytes > 0 {
            sample.ceiling_bytes
        } else {
            ceiling
        };
        Arc::new(MemoryGate {
            sampler,
            ceiling: AtomicU64::new(ceiling),
            base: AtomicU64::new(sample.anon_bytes),
            since: AtomicI64::new(0),
            measurements: AtomicU64::new(1),
            refusals: AtomicU64::new(0),
        })
    }

    /// f.9: admit `bytes` (counting them in `since`) or refuse them.
    pub fn admit(&self, bytes: u64) -> Result<(), Refusal> {
        let n = signed(bytes);
        let s = self.since.fetch_add(n, Ordering::AcqRel).saturating_add(n);
        let ceiling = self.ceiling.load(Ordering::Acquire);
        let estimate = self
            .base
            .load(Ordering::Acquire)
            .saturating_add(s.max(0) as u64);
        if ceiling == 0 || estimate <= ceiling {
            return Ok(());
        }
        // The slow path: undo the add and judge on a fresh measurement.
        self.since.fetch_sub(n, Ordering::AcqRel);
        let counted = self.since.load(Ordering::Acquire);
        let sample = self.sampler.sample();
        self.since.fetch_sub(counted, Ordering::AcqRel);
        self.base.store(sample.anon_bytes, Ordering::Release);
        if sample.ceiling_bytes > 0 {
            self.ceiling.store(sample.ceiling_bytes, Ordering::Release);
        }
        self.measurements.fetch_add(1, Ordering::Relaxed);
        let ceiling = self.ceiling.load(Ordering::Acquire);
        let in_use = sample
            .anon_bytes
            .saturating_add(self.since.load(Ordering::Acquire).max(0) as u64);
        if ceiling > 0 && in_use.saturating_add(bytes) > ceiling {
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return Err(Refusal {
                requested: bytes,
                in_use,
                ceiling,
            });
        }
        self.since.fetch_add(n, Ordering::AcqRel);
        Ok(())
    }

    /// Bytes an admitted request gave back without being allocated.
    pub fn credit(&self, bytes: u64) {
        self.since.fetch_sub(signed(bytes), Ordering::AcqRel);
    }

    /// How many times the gate measured the process (its first measurement included).
    pub fn measurements(&self) -> u64 {
        self.measurements.load(Ordering::Relaxed)
    }

    /// How many requests the gate refused.
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }
}

/// One per Python kernel: whether refusal is on, its gate, and what it holds in NumPy blocks.
#[derive(Debug)]
pub struct KernelMemory {
    guarded: bool,
    gate: OnceLock<Arc<MemoryGate>>,
    live: AtomicI64,
    peak: AtomicU64,
    refusals: AtomicU64,
}

impl KernelMemory {
    /// The counts of a kernel whose decorator said `memory_guard=guarded`.
    pub fn new(guarded: bool) -> Arc<KernelMemory> {
        Arc::new(KernelMemory {
            guarded,
            gate: OnceLock::new(),
            live: AtomicI64::new(0),
            peak: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
        })
    }

    /// The run's gate; the first call wins.
    pub fn bind(&self, gate: Arc<MemoryGate>) {
        let _ = self.gate.set(gate);
    }

    /// Whether the decorator left refusal on.
    pub fn guarded(&self) -> bool {
        self.guarded
    }

    /// The gate that may refuse this kernel's requests: bound, and refusal on.
    fn refusing_gate(&self) -> Option<&MemoryGate> {
        if self.guarded {
            self.gate.get().map(Arc::as_ref)
        } else {
            None
        }
    }

    /// What the guard holds for this kernel now.
    pub fn live(&self) -> LiveCounts {
        LiveCounts {
            live_bytes: self.live.load(Ordering::Acquire).max(0) as u64,
            peak_bytes: self.peak.load(Ordering::Acquire),
            refusals: self.refusals.load(Ordering::Acquire),
        }
    }

    fn add_live(&self, delta: i64) {
        if delta == 0 {
            return;
        }
        let now = self
            .live
            .fetch_add(delta, Ordering::AcqRel)
            .saturating_add(delta);
        if now > 0 {
            self.peak.fetch_max(now as u64, Ordering::AcqRel);
        }
    }

    /// f.12: run `f` in a frame of this kernel on the calling thread.
    pub fn frame<R>(self: &Arc<Self>, f: impl FnOnce() -> R) -> (R, FrameEnd) {
        let frame = Frame {
            kernel: Arc::as_ptr(self),
            gate: self
                .refusing_gate()
                .map_or(core::ptr::null(), |g| g as *const MemoryGate),
            counts: Cell::new(KernelAlloc::NONE),
            held: Cell::new(0),
            refusal: Cell::new(None),
            inside: Cell::new(false),
        };
        let restore = Restore(FRAME.with(|slot| slot.replace(&frame as *const Frame)));
        let value = f();
        drop(restore);
        let mut counts = frame.counts.get();
        counts.measured = true;
        counts.refusal_on = !frame.gate.is_null();
        (
            value,
            FrameEnd {
                counts,
                refusal: frame.refusal.get(),
            },
        )
    }
}

/// What a frame leaves: the call's counts (Python objects and NumPy data) and its first refusal.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FrameEnd {
    /// The call's counts, `measured`, with `refusal_on` set.
    pub counts: KernelAlloc,
    /// The first refusal of the call, the diagnostic's.
    pub refusal: Option<Refusal>,
}

/// The thread-local record of one call (05 b, f.12).
struct Frame {
    kernel: *const KernelMemory,
    /// Null when the call's requests cannot be refused.
    gate: *const MemoryGate,
    counts: Cell<KernelAlloc>,
    /// What the call holds in NumPy blocks, for its peak.
    held: Cell<i64>,
    refusal: Cell<Option<Refusal>>,
    /// Set while the guard calls the allocator underneath.
    inside: Cell<bool>,
}

thread_local! {
    static FRAME: Cell<*const Frame> = const { Cell::new(core::ptr::null()) };
}

/// Puts the previous frame back, on return and on unwind.
struct Restore(*const Frame);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0;
        let _ = FRAME.try_with(|slot| slot.set(previous));
    }
}

/// The frame open on this thread, if any.
fn with_frame<R>(f: impl FnOnce(&Frame) -> R) -> Option<R> {
    let ptr = FRAME.try_with(Cell::get).ok()?;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: a non-null slot was set by `KernelMemory::frame` on this thread to a `Frame` on
    // that call's stack, and `Restore` replaces it before that stack frame is left (on return
    // and on unwind), so the pointer is valid for as long as it is in the slot. `Frame` is only
    // ever read through a shared reference and mutated through `Cell`s, on this thread.
    let frame = unsafe { &*ptr };
    Some(f(frame))
}

impl Frame {
    fn kernel(&self) -> &KernelMemory {
        // SAFETY: the frame holds `Arc::as_ptr` of the kernel whose `frame` method is on the
        // stack below it, and that call holds the `Arc`, so the kernel outlives the frame.
        unsafe { &*self.kernel }
    }

    fn gate(&self) -> Option<&MemoryGate> {
        if self.gate.is_null() {
            return None;
        }
        // SAFETY: `gate` points into the `Arc<MemoryGate>` the kernel holds in its `OnceLock`,
        // which is never replaced or dropped while the kernel lives (see `kernel`).
        Some(unsafe { &*self.gate })
    }

    fn count(&self, source: usize, bytes: u64) {
        let mut counts = self.counts.get();
        let c = counts.source_mut(source);
        c.bytes = c.bytes.saturating_add(bytes);
        c.requests += 1;
        c.largest = c.largest.max(bytes);
        self.counts.set(counts);
    }

    fn hold(&self, delta: i64) {
        let held = self.held.get().saturating_add(delta);
        self.held.set(held);
        if held > 0 {
            let mut counts = self.counts.get();
            counts.numpy.peak = counts.numpy.peak.max(held as u64);
            self.counts.set(counts);
        }
    }

    /// Ask the gate about a request when it is a guarded one: `Ok(true)` charged, `Ok(false)`
    /// not examined, `Err` refused (and recorded).
    fn examine(&self, source: usize, bytes: u64) -> Result<bool, ()> {
        let Some(gate) = self.gate() else {
            return Ok(false);
        };
        if bytes < GUARD_MIN_BYTES {
            return Ok(false);
        }
        match gate.admit(bytes) {
            Ok(()) => Ok(true),
            Err(refusal) => {
                let mut counts = self.counts.get();
                counts.source_mut(source).refused += 1;
                self.counts.set(counts);
                if self.refusal.get().is_none() {
                    self.refusal.set(Some(refusal));
                }
                self.kernel().refusals.fetch_add(1, Ordering::AcqRel);
                Err(())
            }
        }
    }

    fn call<T>(&self, underlying: impl FnOnce() -> *mut T) -> *mut T {
        let was = self.inside.replace(true);
        let p = underlying();
        self.inside.set(was);
        p
    }
}

/// f.10: a Python domain hook's request for `bytes`; `underlying` calls the allocator underneath.
pub fn allocate<T>(bytes: u64, underlying: impl FnOnce() -> *mut T) -> *mut T {
    let ptr = FRAME.try_with(Cell::get).unwrap_or(core::ptr::null());
    if ptr.is_null() {
        return underlying();
    }
    let decided = with_frame(|frame| {
        if frame.inside.get() {
            return None;
        }
        frame.count(PYTHON, bytes);
        Some(frame.examine(PYTHON, bytes))
    })
    .flatten();
    match decided {
        None => underlying(),
        Some(Err(())) => core::ptr::null_mut(),
        Some(Ok(charged)) => {
            let p = with_frame(|frame| frame.call(underlying)).unwrap_or(core::ptr::null_mut());
            if p.is_null() && charged {
                with_frame(|frame| frame.gate().map(|g| g.credit(bytes)));
            }
            p
        }
    }
}

/// The frame open on this thread when it is `owner`'s.
fn owner_frame<R>(owner: &KernelMemory, f: impl FnOnce(&Frame) -> R) -> Option<R> {
    with_frame(|frame| {
        if core::ptr::eq(frame.kernel, owner) {
            Some(f(frame))
        } else {
            None
        }
    })
    .flatten()
}

fn counted(bytes: u64) -> i64 {
    if bytes >= GUARD_MIN_BYTES {
        signed(bytes)
    } else {
        0
    }
}

/// f.11: a NumPy block of `bytes` for `owner`; `underlying` allocates it.
pub fn allocate_owned<T>(
    owner: &KernelMemory,
    bytes: u64,
    underlying: impl FnOnce() -> *mut T,
) -> *mut T {
    let charged = match owner_frame(owner, |frame| {
        frame.count(NUMPY, bytes);
        frame.examine(NUMPY, bytes)
    }) {
        None => false,
        Some(Err(())) => return core::ptr::null_mut(),
        Some(Ok(charged)) => charged,
    };
    let p = underlying();
    if p.is_null() {
        if charged {
            owner_frame(owner, |frame| frame.gate().map(|g| g.credit(bytes)));
        }
        return p;
    }
    owner.add_live(counted(bytes));
    owner_frame(owner, |frame| frame.hold(signed(bytes)));
    p
}

/// f.11: a NumPy block of `old` bytes of `owner` resized to `new`; `underlying` reallocates it.
/// A refusal returns null, which leaves the old block valid.
pub fn resize_owned<T>(
    owner: &KernelMemory,
    old: u64,
    new: u64,
    underlying: impl FnOnce() -> *mut T,
) -> *mut T {
    let growth = new.saturating_sub(old);
    let charged = match owner_frame(owner, |frame| {
        frame.count(NUMPY, growth);
        frame.examine(NUMPY, growth)
    }) {
        None => false,
        Some(Err(())) => return core::ptr::null_mut(),
        Some(Ok(charged)) => charged,
    };
    let p = underlying();
    if p.is_null() {
        if charged {
            owner_frame(owner, |frame| frame.gate().map(|g| g.credit(growth)));
        }
        return p;
    }
    owner.add_live(counted(new) - counted(old));
    owner_frame(owner, |frame| frame.hold(signed(new) - signed(old)));
    p
}

/// f.11: a NumPy block of `bytes` of `owner` freed, from any thread.
pub fn release_owned(owner: &KernelMemory, bytes: u64) {
    owner.add_live(-counted(bytes));
    owner_frame(owner, |frame| frame.hold(-signed(bytes)));
}

/// Arrow's counts for one call from the pool's statistics read before and after it (f.14):
/// `(bytes_allocated, max_memory, total_bytes_allocated, num_allocations)` each time.
pub fn arrow_counts(before: (u64, u64, u64, u64), after: (u64, u64, u64, u64)) -> AllocCounts {
    let (held_0, max_0, total_0, num_0) = before;
    let (held_1, max_1, total_1, num_1) = after;
    let mut peak = held_1.saturating_sub(held_0);
    if max_1 > max_0 {
        peak = peak.max(max_1.saturating_sub(held_0));
    }
    AllocCounts {
        bytes: total_1.saturating_sub(total_0),
        requests: num_1.saturating_sub(num_0),
        largest: 0,
        peak,
        refused: 0,
    }
}

#[cfg(test)]
mod tests;
