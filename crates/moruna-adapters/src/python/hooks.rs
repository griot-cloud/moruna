//! The allocator guard's hooks (05 f.10, f.11, f.14, e.5; E13): CPython's allocator domains
//! through PEP 445, NumPy's data allocator through NEP 49, and Arrow's default pool read around
//! a call. Every hook hands its decision to `crate::guard`, which needs no interpreter.

use core::ffi::{c_char, c_uint, c_void};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Once, OnceLock};

use moruna_kernel::AllocCounts;
use pyo3::ffi;
use pyo3::prelude::*;

use crate::guard::{self, KernelMemory};

// ---------------------------------------------------------------------------------------------
// f.10 CPython's allocator domains.

/// An allocator saved from `PyMem_GetAllocator`: plain function pointers and the context they
/// take, written once before the hook that reads it is installed.
struct Saved(ffi::PyMemAllocatorEx);

// SAFETY: the saved struct is a copy of CPython's allocator: function pointers and a context
// pointer CPython itself shares between every thread (PEP 445 requires a domain's allocator to
// be thread-safe), and it is never written after it is published through the `OnceLock`.
unsafe impl Send for Saved {}
// SAFETY: as for `Send`: read-only after publication.
unsafe impl Sync for Saved {}

static RAW: OnceLock<Saved> = OnceLock::new();
static MEM: OnceLock<Saved> = OnceLock::new();
static OBJ: OnceLock<Saved> = OnceLock::new();
static INSTALLED: Once = Once::new();

macro_rules! domain_hooks {
    ($saved:ident, $malloc:ident, $calloc:ident) => {
        extern "C" fn $malloc(ctx: *mut c_void, size: usize) -> *mut c_void {
            let Some(underlying) = $saved.get().and_then(|s| s.0.malloc) else {
                return core::ptr::null_mut();
            };
            guard::allocate(size as u64, || underlying(ctx, size))
        }

        extern "C" fn $calloc(ctx: *mut c_void, nelem: usize, elsize: usize) -> *mut c_void {
            let Some(underlying) = $saved.get().and_then(|s| s.0.calloc) else {
                return core::ptr::null_mut();
            };
            match nelem.checked_mul(elsize) {
                // An overflowing product is the allocator underneath's to refuse.
                None => underlying(ctx, nelem, elsize),
                Some(bytes) => guard::allocate(bytes as u64, || underlying(ctx, nelem, elsize)),
            }
        }
    };
}

domain_hooks!(RAW, raw_malloc, raw_calloc);
domain_hooks!(MEM, mem_malloc, mem_calloc);
domain_hooks!(OBJ, obj_malloc, obj_calloc);

type Malloc = extern "C" fn(*mut c_void, usize) -> *mut c_void;
type Calloc = extern "C" fn(*mut c_void, usize, usize) -> *mut c_void;

fn wrap(
    _py: Python<'_>,
    domain: ffi::PyMemAllocatorDomain,
    saved: &'static OnceLock<Saved>,
    malloc: Malloc,
    calloc: Calloc,
) {
    let mut underneath = ffi::PyMemAllocatorEx {
        ctx: core::ptr::null_mut(),
        malloc: None,
        calloc: None,
        realloc: None,
        free: None,
    };
    // SAFETY: PEP 445: `PyMem_GetAllocator` fills the struct it is given; the interpreter is
    // attached (`_py`).
    unsafe { ffi::PyMem_GetAllocator(domain, &mut underneath) };
    let ours = ffi::PyMemAllocatorEx {
        // The same context, so a thread that reads the struct while it is replaced calls either
        // function with the right one (f.10).
        ctx: underneath.ctx,
        malloc: Some(malloc),
        calloc: Some(calloc),
        // Never wrapped: every block is freed and resized by the allocator it came from.
        realloc: underneath.realloc,
        free: underneath.free,
    };
    if saved.set(Saved(underneath)).is_err() {
        return;
    }
    let mut ours = ours;
    // SAFETY: PEP 445 permits installing, after initialisation, an allocator that calls
    // through to the one it replaces, which `ours` does for every operation; the saved
    // allocator it calls is published above, before this makes `ours` reachable.
    unsafe { ffi::PyMem_SetAllocator(domain, &mut ours) };
}

/// f.10: install the three domain hooks, once per process, never removed.
pub(crate) fn install_python_hooks(py: Python<'_>) {
    INSTALLED.call_once(|| {
        wrap(
            py,
            ffi::PyMemAllocatorDomain::PYMEM_DOMAIN_RAW,
            &RAW,
            raw_malloc,
            raw_calloc,
        );
        wrap(
            py,
            ffi::PyMemAllocatorDomain::PYMEM_DOMAIN_MEM,
            &MEM,
            mem_malloc,
            mem_calloc,
        );
        wrap(
            py,
            ffi::PyMemAllocatorDomain::PYMEM_DOMAIN_OBJ,
            &OBJ,
            obj_malloc,
            obj_calloc,
        );
        tracing::info!(target: "adapter.guard", "Python allocator domain hooks installed");
    });
}

// ---------------------------------------------------------------------------------------------
// f.11 NumPy's data allocator (NEP 49).

/// NumPy's `PyDataMemAllocator` (version 1, `ndarraytypes.h`).
#[repr(C)]
#[derive(Copy, Clone)]
struct DataMemAllocator {
    ctx: *mut c_void,
    malloc: Option<unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void>,
    calloc: Option<unsafe extern "C" fn(*mut c_void, usize, usize) -> *mut c_void>,
    realloc: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void>,
    free: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, usize)>,
}

/// NumPy's `PyDataMem_Handler` (version 1).
#[repr(C)]
struct DataMemHandler {
    name: [c_char; 127],
    version: u8,
    allocator: DataMemAllocator,
}

/// One kernel's handler: what the capsule points at, and what the hooks' context points at.
#[repr(C)]
struct KernelHandler {
    handler: DataMemHandler,
    owner: Arc<KernelMemory>,
    underneath: DataMemAllocator,
}

/// e.5: the header in front of every block the guard hands NumPy.
const HEADER: usize = 16;
const CAPSULE_NAME: &core::ffi::CStr = c"mem_handler";
const HANDLER_NAME: &[u8] = b"moruna_guard";
/// NEP 49 is in NumPy's C API from feature version 0x0f (NumPy 1.22).
const NEP49_FEATURE_VERSION: c_uint = 0x0f;

/// The two NumPy C API entries the guard uses, read once per process.
struct NumpyApi {
    set_handler: unsafe extern "C" fn(*mut ffi::PyObject) -> *mut ffi::PyObject,
    default: DataMemAllocator,
}

// SAFETY: a function pointer into NumPy's extension module and a copy of its default
// allocator's function pointers and context, which NumPy shares between every thread; both
// are read-only after publication.
unsafe impl Send for NumpyApi {}
// SAFETY: as for `Send`.
unsafe impl Sync for NumpyApi {}

static NUMPY: OnceLock<Option<NumpyApi>> = OnceLock::new();

/// f.11: NumPy's C API when NumPy is loaded; `None` while it is not (asked again next call) and
/// for good once it is known to be too old.
fn numpy_api(py: Python<'_>) -> Option<&'static NumpyApi> {
    if let Some(found) = NUMPY.get() {
        return found.as_ref();
    }
    let modules = py.import("sys").and_then(|s| s.getattr("modules")).ok()?;
    let module = [
        "numpy._core._multiarray_umath",
        "numpy.core._multiarray_umath",
    ]
    .into_iter()
    .find_map(|name| modules.get_item(name).ok())?;
    let found = read_numpy_api(&module);
    if found.is_none() {
        tracing::warn!(
            target: "adapter.guard",
            "this NumPy has no NEP 49 allocator handler (NumPy 1.22 or later); its requests are neither counted nor guarded"
        );
    }
    NUMPY.get_or_init(|| found).as_ref()
}

fn read_numpy_api(module: &Bound<'_, PyAny>) -> Option<NumpyApi> {
    let capsule = module.getattr("_ARRAY_API").ok()?;
    // SAFETY: `_ARRAY_API` is NumPy's C API capsule, whose pointer is its table of entries
    // (`__multiarray_api.h`); `PyCapsule_GetPointer` with a null name is how NumPy's own
    // `_import_array` reads it, and it returns null (with an exception set) on anything else.
    let table = unsafe { ffi::PyCapsule_GetPointer(capsule.as_ptr(), core::ptr::null()) }
        as *const *const c_void;
    if table.is_null() {
        // SAFETY: clear the exception `PyCapsule_GetPointer` set; attached.
        unsafe { ffi::PyErr_Clear() };
        return None;
    }
    // SAFETY: slot 211 is `PyArray_GetNDArrayCFeatureVersion`, `unsigned int (void)`, in every
    // NumPy that has the table.
    let version = unsafe {
        let f: unsafe extern "C" fn() -> c_uint = core::mem::transmute(*table.add(211));
        f()
    };
    if version < NEP49_FEATURE_VERSION {
        return None;
    }
    // SAFETY: from feature version 0x0f, slot 304 is `PyDataMem_SetHandler`,
    // `PyObject *(PyObject *)`, and slot 306 is the address of `PyDataMem_DefaultHandler`, a
    // `PyObject *` capsule named "mem_handler" over a version 1 `PyDataMem_Handler`.
    unsafe {
        let set_handler: unsafe extern "C" fn(*mut ffi::PyObject) -> *mut ffi::PyObject =
            core::mem::transmute(*table.add(304));
        let default_capsule = *(*table.add(306) as *const *mut ffi::PyObject);
        let handler = ffi::PyCapsule_GetPointer(default_capsule, CAPSULE_NAME.as_ptr())
            as *const DataMemHandler;
        if handler.is_null() {
            ffi::PyErr_Clear();
            return None;
        }
        if (*handler).version != 1 {
            return None;
        }
        Some(NumpyApi {
            set_handler,
            default: (*handler).allocator,
        })
    }
}

/// Write the header and return the address NumPy receives.
///
/// # Safety
/// `base` is null or the start of a block of at least `HEADER` bytes, 16-byte aligned.
unsafe fn handed(base: *mut c_void, size: usize) -> *mut c_void {
    if base.is_null() {
        return base;
    }
    // SAFETY: the caller's contract: `base` heads at least `HEADER` bytes, aligned for `u64`.
    unsafe {
        let header = base as *mut u64;
        header.write(size as u64);
        header.add(1).write(0);
        (base as *mut u8).add(HEADER) as *mut c_void
    }
}

/// The block's start, the size its header records, and its class capacity (0: not a cached
/// class, so the C heap's to free).
///
/// # Safety
/// `ptr` was returned by `handed` or `handed_classed`.
unsafe fn block_of(ptr: *mut c_void) -> (*mut c_void, u64, usize) {
    // SAFETY: the caller's contract: `HEADER` bytes before `ptr` are the header written for it.
    unsafe {
        let base = (ptr as *mut u8).sub(HEADER) as *mut c_void;
        let header = base as *const u64;
        (base, header.read(), header.add(1).read() as usize)
    }
}

/// [`handed`] for a block of a cache class: the header records its capacity too.
///
/// # Safety
/// As for `handed`, with `base` heading at least `capacity` bytes.
unsafe fn handed_classed(base: *mut c_void, size: usize, capacity: usize) -> *mut c_void {
    // SAFETY: the caller's contract, as for `handed`.
    unsafe {
        let p = handed(base, size);
        if !p.is_null() {
            (base as *mut u64).add(1).write(capacity as u64);
        }
        p
    }
}

// ---------------------------------------------------------------------------------------------
// f.11 (2026-11-14): a freed NumPy block of CACHE_MIN bytes or more is kept for the next request
// of its class instead of going back to the C heap.
//
// F8.9 (12 l) fixes glibc's mmap threshold at 256 KiB so the process stays under small ceilings,
// which hands every block that large back to the system on `free`: a kernel that takes and frees
// arrays of a few mebibytes morsel after morsel had each one faulted in and zeroed again. On the
// benchmark VM that was the larger part of the kernel's time in the process, 35% of Occupancy's
// CPU against 22% for the plain loop, whose arrays the C heap keeps. The cache is Moruna's, in
// the hooks it already owns, so it is the same on every C library: glibc, musl (the engine on
// the microkernel), macOS. It holds at most a sixteenth of the run's ceiling (256 MiB with no
// run bound), and every block goes back to the C heap when the run ends.

/// The smallest block kept: below it the C heap keeps freed blocks itself.
const CACHE_MIN: usize = 256 << 10;
/// Classes per power of two: a block is allocated at the next of 1, 1.25, 1.5 and 1.75 times a
/// power of two, so a freed one serves any request of its class and wastes at most a quarter.
const STEPS: usize = 4;
/// Classes up to 2^63.
const CLASSES: usize = 64 * STEPS;
/// What the cache may hold when no run is bound.
const CACHE_UNBOUND: u64 = 256 << 20;

/// The class of a block of `total` bytes and the capacity it is allocated at; `None` below
/// `CACHE_MIN`.
fn class_of(total: usize) -> Option<(usize, usize)> {
    if total < CACHE_MIN {
        return None;
    }
    let power = (usize::BITS - 1 - total.leading_zeros()) as usize;
    let base = 1usize << power;
    (0..=STEPS).find_map(|step| {
        let capacity = base.checked_add((base / STEPS) * step)?;
        (capacity >= total).then_some((power * STEPS + step, capacity))
    })
}

/// The capacity of class `class`.
fn capacity_of(class: usize) -> usize {
    let base = 1usize << (class / STEPS);
    base + (base / STEPS) * (class % STEPS)
}

/// What the cache may hold for a kernel: a sixteenth of its run's ceiling.
fn cache_limit(owner: &KernelMemory) -> u64 {
    owner
        .run_ceiling()
        .map_or(CACHE_UNBOUND, |ceiling| ceiling / 16)
}

/// Freed blocks by class, as addresses, and the bytes they hold together.
struct BlockCache {
    classes: Box<[std::sync::Mutex<Vec<usize>>]>,
    held: std::sync::atomic::AtomicU64,
    hits: std::sync::atomic::AtomicU64,
    kept: std::sync::atomic::AtomicU64,
}

/// What the NumPy block cache has done in this process (f.11): requests it served from a freed
/// block, blocks it kept on `free`, and the bytes it holds now.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct NumpyCacheStats {
    /// Requests served from a block the cache held.
    pub hits: u64,
    /// Freed blocks the cache kept instead of giving them back to the C heap.
    pub kept: u64,
    /// Bytes the cache holds now.
    pub held_bytes: u64,
}

/// The NumPy block cache's figures for this process.
pub fn numpy_cache_stats() -> NumpyCacheStats {
    let cache = cache();
    NumpyCacheStats {
        hits: cache.hits.load(Ordering::Relaxed),
        kept: cache.kept.load(Ordering::Relaxed),
        held_bytes: cache.held.load(Ordering::Acquire),
    }
}

impl BlockCache {
    fn new() -> BlockCache {
        BlockCache {
            classes: (0..CLASSES)
                .map(|_| std::sync::Mutex::new(Vec::new()))
                .collect(),
            held: std::sync::atomic::AtomicU64::new(0),
            hits: std::sync::atomic::AtomicU64::new(0),
            kept: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// A block of `class` the cache holds, taken out of it.
    fn take(&self, class: usize) -> Option<*mut c_void> {
        let base = self.classes[class]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()?;
        self.held
            .fetch_sub(capacity_of(class) as u64, Ordering::AcqRel);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(base as *mut c_void)
    }

    /// Keep `base`, a block of `class`, if the cache stays within `limit`; false when it is the
    /// caller's to free.
    fn put(&self, class: usize, base: *mut c_void, limit: u64) -> bool {
        let capacity = capacity_of(class) as u64;
        let room = self
            .held
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held + capacity <= limit).then_some(held + capacity)
            })
            .is_ok();
        if room {
            self.classes[class]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(base as usize);
            self.kept.fetch_add(1, Ordering::Relaxed);
        }
        room
    }

    /// Every block the cache holds, with its capacity, taken out of it.
    fn drain(&self) -> Vec<(*mut c_void, usize)> {
        let mut out = Vec::new();
        for (class, blocks) in self.classes.iter().enumerate() {
            let taken = std::mem::take(&mut *blocks.lock().unwrap_or_else(|e| e.into_inner()));
            for base in taken {
                self.held
                    .fetch_sub(capacity_of(class) as u64, Ordering::AcqRel);
                out.push((base as *mut c_void, capacity_of(class)));
            }
        }
        out
    }
}

static CACHE: OnceLock<BlockCache> = OnceLock::new();

fn cache() -> &'static BlockCache {
    CACHE.get_or_init(BlockCache::new)
}

/// Give every block the cache holds back to the C heap: what the facade calls when a run ends
/// (`PyKernel::release_run`), so nothing a run's kernels freed outlives it.
pub(crate) fn drain_numpy_cache() {
    let Some(api) = NUMPY.get().and_then(Option::as_ref) else {
        return;
    };
    let Some(free) = api.default.free else {
        return;
    };
    for (base, capacity) in cache().drain() {
        // SAFETY: a block NumPy's default allocator made at `capacity` bytes, freed once, here.
        unsafe { free(api.default.ctx, base, capacity) };
    }
}

/// # Safety
/// `ctx` is the `KernelHandler` the capsule owns (NumPy passes the handler's own context).
unsafe fn handler_of<'a>(ctx: *mut c_void) -> &'a KernelHandler {
    // SAFETY: the caller's contract; the capsule keeps the box alive while any array can still
    // call through it (NEP 49 stores the handler on the array).
    unsafe { &*(ctx as *const KernelHandler) }
}

unsafe extern "C" fn np_malloc(ctx: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: NumPy calls with the handler's own context.
    let h = unsafe { handler_of(ctx) };
    let (Some(total), Some(malloc)) = (size.checked_add(HEADER), h.underneath.malloc) else {
        return core::ptr::null_mut();
    };
    if let Some((class, capacity)) = class_of(total) {
        let base = guard::allocate_owned(&h.owner, size as u64, || {
            cache().take(class).unwrap_or_else(|| {
                // SAFETY: NumPy's default allocator, with its own context.
                unsafe { malloc(h.underneath.ctx, capacity) }
            })
        });
        // SAFETY: a cached block or the default allocator's: null or `capacity >= total` bytes.
        return unsafe { handed_classed(base, size, capacity) };
    }
    let base = guard::allocate_owned(&h.owner, size as u64, || {
        // SAFETY: NumPy's default allocator, with its own context.
        unsafe { malloc(h.underneath.ctx, total) }
    });
    // SAFETY: the default allocator returns null or `total >= HEADER` bytes aligned as `malloc`.
    unsafe { handed(base, size) }
}

unsafe extern "C" fn np_calloc(ctx: *mut c_void, nelem: usize, elsize: usize) -> *mut c_void {
    // SAFETY: NumPy calls with the handler's own context.
    let h = unsafe { handler_of(ctx) };
    let size = nelem.checked_mul(elsize);
    let (Some(size), Some(calloc)) = (size, h.underneath.calloc) else {
        return core::ptr::null_mut();
    };
    let Some(total) = size.checked_add(HEADER) else {
        return core::ptr::null_mut();
    };
    if let Some((class, capacity)) = class_of(total) {
        let base = guard::allocate_owned(&h.owner, size as u64, || match cache().take(class) {
            Some(base) => {
                // SAFETY: a cached block of `capacity >= total` bytes nobody else holds; the
                // bytes NumPy will see are zeroed, as `calloc` promises.
                unsafe { (base as *mut u8).add(HEADER).write_bytes(0, size) };
                base
            }
            // SAFETY: NumPy's default allocator, with its own context; one element.
            None => unsafe { calloc(h.underneath.ctx, 1, capacity) },
        });
        // SAFETY: as in `np_malloc`.
        return unsafe { handed_classed(base, size, capacity) };
    }
    let base = guard::allocate_owned(&h.owner, size as u64, || {
        // SAFETY: NumPy's default allocator, with its own context; one element of `total`.
        unsafe { calloc(h.underneath.ctx, 1, total) }
    });
    // SAFETY: as in `np_malloc`.
    unsafe { handed(base, size) }
}

unsafe extern "C" fn np_realloc(ctx: *mut c_void, ptr: *mut c_void, new: usize) -> *mut c_void {
    if ptr.is_null() {
        // SAFETY: `realloc(NULL, n)` is `malloc(n)`; same context.
        return unsafe { np_malloc(ctx, new) };
    }
    // SAFETY: NumPy calls with the handler's own context and a block this handler made.
    let (h, (base, old, capacity)) = unsafe { (handler_of(ctx), block_of(ptr)) };
    let (Some(total), Some(realloc)) = (new.checked_add(HEADER), h.underneath.realloc) else {
        return core::ptr::null_mut();
    };
    if capacity > 0 {
        if total <= capacity && total >= CACHE_MIN {
            // Still its class's: resized where it is.
            let same = guard::resize_owned(&h.owner, old, new as u64, || base);
            // SAFETY: the block heads `capacity >= total` bytes; null leaves it as it was.
            return unsafe { handed_classed(same, new, capacity) };
        }
        // Another class, or none: a new block, the bytes copied, the old one freed.
        // SAFETY: the handler's own allocation and free, each with its own context.
        unsafe {
            let moved = np_malloc(ctx, new);
            if moved.is_null() {
                return moved;
            }
            core::ptr::copy_nonoverlapping(
                ptr as *const u8,
                moved as *mut u8,
                new.min(old as usize),
            );
            np_free(ctx, ptr, old as usize);
            return moved;
        }
    }
    let moved = guard::resize_owned(&h.owner, old, new as u64, || {
        // SAFETY: the block's start, which the default allocator made.
        unsafe { realloc(h.underneath.ctx, base, total) }
    });
    // SAFETY: as in `np_malloc`; null leaves the old block valid, as `realloc` requires.
    unsafe { handed(moved, new) }
}

unsafe extern "C" fn np_free(ctx: *mut c_void, ptr: *mut c_void, _size: usize) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: NumPy calls with the handler's own context and a block this handler made.
    let (h, (base, old, capacity)) = unsafe { (handler_of(ctx), block_of(ptr)) };
    guard::release_owned(&h.owner, old);
    if capacity > 0
        && let Some((class, _)) = class_of(capacity)
        && cache().put(class, base, cache_limit(&h.owner))
    {
        return;
    }
    let whole = if capacity > 0 {
        capacity
    } else {
        old as usize + HEADER
    };
    if let Some(free) = h.underneath.free {
        // SAFETY: the block's start and its whole size, to the allocator that made it.
        unsafe { free(h.underneath.ctx, base, whole) };
    }
}

unsafe extern "C" fn drop_handler(capsule: *mut ffi::PyObject) {
    // SAFETY: the capsule was made by `numpy_handler` over a leaked `Box<KernelHandler>` whose
    // first field is the handler the capsule names; this is its only destructor.
    unsafe {
        let ptr = ffi::PyCapsule_GetPointer(capsule, CAPSULE_NAME.as_ptr()) as *mut KernelHandler;
        if ptr.is_null() {
            ffi::PyErr_Clear();
            return;
        }
        drop(Box::from_raw(ptr));
    }
}

/// A kernel's handler capsule (f.11), when NumPy is loaded and has NEP 49.
pub(crate) fn numpy_handler(py: Python<'_>, owner: &Arc<KernelMemory>) -> Option<Py<PyAny>> {
    let api = numpy_api(py)?;
    let mut name = [0 as c_char; 127];
    for (to, from) in name.iter_mut().zip(HANDLER_NAME) {
        *to = *from as c_char;
    }
    let boxed = Box::new(KernelHandler {
        handler: DataMemHandler {
            name,
            version: 1,
            allocator: DataMemAllocator {
                ctx: core::ptr::null_mut(),
                malloc: Some(np_malloc),
                calloc: Some(np_calloc),
                realloc: Some(np_realloc),
                free: Some(np_free),
            },
        },
        owner: Arc::clone(owner),
        underneath: api.default,
    });
    let raw = Box::into_raw(boxed);
    // SAFETY: `raw` is the box just leaked; its handler's context is the box itself.
    unsafe { (*raw).handler.allocator.ctx = raw as *mut c_void };
    // SAFETY: a capsule over the box, named as NumPy requires, owning it through
    // `drop_handler`; on failure the box is reclaimed here.
    unsafe {
        let capsule = ffi::PyCapsule_New(
            raw as *mut c_void,
            CAPSULE_NAME.as_ptr(),
            Some(drop_handler),
        );
        if capsule.is_null() {
            ffi::PyErr_Clear();
            drop(Box::from_raw(raw));
            return None;
        }
        Some(Bound::from_owned_ptr(py, capsule).unbind())
    }
}

/// Make `capsule` this thread context's NumPy handler for the length of `f`, and put the
/// previous one back afterwards whatever `f` did (f.11).
pub(crate) fn with_numpy_handler<R>(
    py: Python<'_>,
    capsule: Option<&Py<PyAny>>,
    f: impl FnOnce() -> R,
) -> R {
    let (Some(capsule), Some(api)) = (capsule, NUMPY.get().and_then(Option::as_ref)) else {
        return f();
    };
    // SAFETY: `PyDataMem_SetHandler` takes a "mem_handler" capsule and returns a new
    // reference to the previous handler, or null with an exception set; attached.
    let previous = unsafe { (api.set_handler)(capsule.as_ptr()) };
    if previous.is_null() {
        let err = PyErr::fetch(py);
        tracing::warn!(target: "adapter.guard", error = %err, "the NumPy handler could not be set; this call's NumPy requests are not counted");
        return f();
    }
    let value = f();
    // SAFETY: as above, putting the previous handler back, then releasing both references.
    unsafe {
        let ours = (api.set_handler)(previous);
        if ours.is_null() {
            let err = PyErr::fetch(py);
            tracing::warn!(target: "adapter.guard", error = %err, "the NumPy handler could not be restored");
        } else {
            ffi::Py_DECREF(ours);
        }
        ffi::Py_DECREF(previous);
    }
    value
}

// ---------------------------------------------------------------------------------------------
// f.14 Arrow from its pool's statistics.

/// `(bytes_allocated, max_memory, total_bytes_allocated, num_allocations)` of pyarrow's default
/// pool now; `None` when pyarrow is not loaded or its pool cannot say.
pub(crate) fn arrow_pool(py: Python<'_>) -> Option<(u64, u64, u64, u64)> {
    let modules = py.import("sys").and_then(|s| s.getattr("modules")).ok()?;
    let pyarrow = modules.get_item("pyarrow").ok()?;
    let pool = pyarrow.call_method0("default_memory_pool").ok()?;
    let read = |name: &str| -> Option<u64> { pool.call_method0(name).ok()?.extract().ok() };
    Some((
        read("bytes_allocated")?,
        read("max_memory")?,
        read("total_bytes_allocated")?,
        read("num_allocations")?,
    ))
}

static ARROW_WARNED: Once = Once::new();

/// The call's Arrow counts from two reads of the pool; zero when either failed.
pub(crate) fn arrow_counts(
    before: Option<(u64, u64, u64, u64)>,
    after: Option<(u64, u64, u64, u64)>,
) -> AllocCounts {
    match (before, after) {
        (Some(b), Some(a)) => guard::arrow_counts(b, a),
        _ => {
            ARROW_WARNED.call_once(|| {
                tracing::warn!(target: "adapter.guard", "pyarrow's default pool gave no statistics; Arrow's figures are zero");
            });
            AllocCounts::default()
        }
    }
}

#[cfg(test)]
mod block_cache_tests {
    use super::*;

    #[test]
    fn a_class_is_the_next_quarter_step_and_holds_its_request() {
        assert_eq!(class_of(CACHE_MIN - 1), None);
        assert_eq!(class_of(CACHE_MIN).map(|(_, c)| c), Some(CACHE_MIN));
        for total in [
            CACHE_MIN + 1,
            300 << 10,
            1 << 20,
            (1 << 20) + 1,
            7_000_000,
            1 << 30,
        ] {
            let (class, capacity) = class_of(total).expect("a class");
            assert!(capacity >= total, "{total}");
            assert!(
                capacity <= total + total / STEPS + 1,
                "a quarter at most: {total} {capacity}"
            );
            assert_eq!(capacity_of(class), capacity);
            // A block of that capacity is of the same class.
            assert_eq!(class_of(capacity).map(|(c, _)| c), Some(class));
        }
    }

    #[test]
    fn the_cache_keeps_within_its_limit_and_gives_everything_back() {
        let cache = BlockCache::new();
        let (class, capacity) = class_of(1 << 20).expect("a class");
        let limit = (capacity * 2) as u64;
        let a = 0x1000 as *mut c_void;
        let b = 0x2000 as *mut c_void;
        let c = 0x3000 as *mut c_void;
        assert!(cache.put(class, a, limit));
        assert!(cache.put(class, b, limit));
        assert!(
            !cache.put(class, c, limit),
            "past the limit, the caller frees it"
        );
        assert_eq!(cache.take(class), Some(b));
        assert!(cache.put(class, c, limit));
        let mut drained: Vec<usize> = cache
            .drain()
            .into_iter()
            .map(|(p, cap)| {
                assert_eq!(cap, capacity);
                p as usize
            })
            .collect();
        drained.sort_unstable();
        assert_eq!(drained, vec![0x1000, 0x3000]);
        assert_eq!(cache.held.load(Ordering::Acquire), 0);
        assert_eq!(cache.take(class), None);
    }
}
