//! The allocator guard's hooks (05 f.10, f.11, f.14, e.5; E13): CPython's allocator domains
//! through PEP 445, NumPy's data allocator through NEP 49, and Arrow's default pool read around
//! a call. Every hook hands its decision to `crate::guard`, which needs no interpreter.

use core::ffi::{c_char, c_uint, c_void};
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

/// The block's start and the size its header records.
///
/// # Safety
/// `ptr` was returned by `handed`.
unsafe fn block_of(ptr: *mut c_void) -> (*mut c_void, u64) {
    // SAFETY: the caller's contract: `HEADER` bytes before `ptr` are the header `handed` wrote.
    unsafe {
        let base = (ptr as *mut u8).sub(HEADER) as *mut c_void;
        (base, (base as *const u64).read())
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
    let (h, (base, old)) = unsafe { (handler_of(ctx), block_of(ptr)) };
    let (Some(total), Some(realloc)) = (new.checked_add(HEADER), h.underneath.realloc) else {
        return core::ptr::null_mut();
    };
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
    let (h, (base, old)) = unsafe { (handler_of(ctx), block_of(ptr)) };
    guard::release_owned(&h.owner, old);
    if let Some(free) = h.underneath.free {
        // SAFETY: the block's start and its whole size, to the allocator that made it.
        unsafe { free(h.underneath.ctx, base, old as usize + HEADER) };
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
