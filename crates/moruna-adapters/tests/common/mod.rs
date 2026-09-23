//! Shared fixtures for the AD tests.
//!
//! Every test that needs an allocator uses `FakeAllocator` (contracts d.15) with `with_limit`
//! and `pinned` only, bound through `bind_allocator`; a batch "in the arena" is one built over
//! `FakeAllocator` buffers, which is what `arena_i64_batch` makes.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_adapters::python::{PyKernel, PyKernelSpec};
use moruna_kernel::arrow::array::ArrayData;
use moruna_kernel::arrow::array::{ArrayRef, make_array};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{AllocStats, Allocator, Buffer, Payload, Result, Tier};
use moruna_testkit::FakeAllocator;
use pyo3::prelude::*;
use pyo3::types::PyModule;

/// Import a module of Python source and return one of its attributes.
///
/// Each call uses its own module name so that two fixtures in one test binary cannot collide in
/// `sys.modules`.
pub fn py_object(source: &str, attribute: &str, module_name: &str) -> Py<PyAny> {
    Python::attach(|py| {
        let code = std::ffi::CString::new(source).expect("kernel source has no interior nul");
        let file = std::ffi::CString::new(format!("{module_name}.py")).expect("file name");
        let name = std::ffi::CString::new(module_name).expect("module name");
        let module = PyModule::from_code(py, &code, &file, &name).expect("the fixture imports");
        module
            .getattr(attribute)
            .expect("the fixture defines the attribute")
            .unbind()
    })
}

/// A directory of Python source files, unique to this process, so that `inspect.getsource` can
/// read a fixture's text the way it reads a real kernel's.
///
/// A module built with `PyModule::from_code` has no loader, so `inspect.getsource` cannot find
/// its text and e.4's source path is never exercised. Writing the fixture to a file and importing
/// it is what a user's kernel looks like. The directory carries the process id and a counter,
/// never a fixed name, because several gates run on one machine at once (preamble 6.7).
pub struct SourceFixture {
    dir: std::path::PathBuf,
}

impl Default for SourceFixture {
    fn default() -> Self {
        SourceFixture::new()
    }
}

impl SourceFixture {
    pub fn new() -> SourceFixture {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "moruna-adapters-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("a scratch directory of this process's own");
        SourceFixture { dir }
    }

    /// Write `source` as `<module_name>.py` in a fresh sub directory, import it, and return one
    /// of its attributes. Each load gets its own directory, so the same module name can be
    /// loaded twice from different text.
    pub fn load(&self, source: &str, attribute: &str, module_name: &str) -> Py<PyAny> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = self
            .dir
            .join(NEXT.fetch_add(1, Ordering::SeqCst).to_string());
        std::fs::create_dir_all(&dir).expect("a load directory");
        std::fs::write(dir.join(format!("{module_name}.py")), source)
            .expect("the fixture is written");
        Python::attach(|py| {
            let sys = py.import("sys").expect("sys");
            sys.getattr("path")
                .expect("sys.path")
                .call_method1("insert", (0, dir.to_string_lossy().as_ref()))
                .expect("the load directory goes first on sys.path");
            let modules = sys.getattr("modules").expect("sys.modules");
            if modules.contains(module_name).unwrap_or(false) {
                modules
                    .del_item(module_name)
                    .expect("the previous module goes");
            }
            py.import("importlib")
                .expect("importlib")
                .call_method0("invalidate_caches")
                .expect("the finder forgets the old directory");
            py.import("linecache")
                .expect("linecache")
                .call_method0("clearcache")
                .expect("the source cache forgets the old file");
            let module = py.import(module_name).expect("the fixture imports");
            module
                .getattr(attribute)
                .expect("the fixture defines the attribute")
                .unbind()
        })
    }
}

impl Drop for SourceFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A stateless `PyKernel` over one Python function, with the allocator already bound.
pub fn stateless_kernel(
    source: &str,
    attribute: &str,
    module_name: &str,
    alloc: Arc<dyn Allocator>,
) -> PyKernel {
    let kernel = PyKernel::new(PyKernelSpec::new(py_object(source, attribute, module_name)))
        .expect("the kernel is well formed");
    kernel.bind_allocator(alloc);
    kernel
}

/// An arena owned Arrow buffer holding `values` as little endian `i64`s.
pub fn arena_i64_buffer(
    alloc: &FakeAllocator,
    values: &[i64],
) -> moruna_kernel::arrow::buffer::Buffer {
    let mut buffer: Buffer = alloc.buffer(values.len() * 8, alloc.host_tier());
    for (index, value) in values.iter().enumerate() {
        buffer[index * 8..(index + 1) * 8].copy_from_slice(&value.to_le_bytes());
    }
    buffer
        .into_arrow_buffer()
        .expect("a host buffer becomes an Arrow buffer")
}

/// A one column `i64` batch whose bytes the arena owns.
pub fn arena_i64_batch(alloc: &FakeAllocator, values: &[i64]) -> RecordBatch {
    let buffer = arena_i64_buffer(alloc, values);
    let data = ArrayData::builder(DataType::Int64)
        .len(values.len())
        .add_buffer(buffer)
        .build()
        .expect("a primitive array over one buffer");
    let column: ArrayRef = make_array(data);
    let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false)]));
    RecordBatch::try_new(schema, vec![column]).expect("one column, one length")
}

/// The `i64` values of column 0 of a table payload.
pub fn i64_column(payload: &Payload) -> Vec<i64> {
    let Payload::Table(batch, _) = payload else {
        panic!("expected a table payload");
    };
    let array = batch.column(0);
    let data = array.to_data();
    let bytes = data.buffers()[0].as_slice();
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .take(batch.num_rows())
        .map(|chunk| i64::from_le_bytes(*chunk))
        .collect()
}

/// A `FakeAllocator` that also records the boundary copies the adapter reports.
///
/// `FakeAllocator::stats()` reports `boundary_copies_total` as a constant zero and does not
/// implement `Allocator::note_boundary_copy`, so AD-T2 cannot read the counter d.3 defines
/// through the fake alone. This decorator is not a second fake: every allocator method delegates
/// to the `FakeAllocator` underneath, and the only behaviour it adds is the counting the fake is
/// missing. Reported as a testkit gap (E10); delete it when the fake counts.
pub struct CountingAllocator {
    inner: FakeAllocator,
    boundary_copies: AtomicU64,
    boundary_bytes: AtomicU64,
}

impl CountingAllocator {
    pub fn new(inner: FakeAllocator) -> Arc<CountingAllocator> {
        Arc::new(CountingAllocator {
            inner,
            boundary_copies: AtomicU64::new(0),
            boundary_bytes: AtomicU64::new(0),
        })
    }

    pub fn fake(&self) -> &FakeAllocator {
        &self.inner
    }

    /// How many times `note_boundary_copy` was called.
    pub fn boundary_copies(&self) -> u64 {
        self.boundary_copies.load(Ordering::SeqCst)
    }

    /// The bytes those calls reported.
    pub fn boundary_bytes(&self) -> u64 {
        self.boundary_bytes.load(Ordering::SeqCst)
    }
}

impl Allocator for CountingAllocator {
    fn alloc(&self, bytes: usize, tier: Tier) -> Result<Buffer> {
        self.inner.alloc(bytes, tier)
    }
    fn page_bytes(&self) -> usize {
        Allocator::page_bytes(&self.inner)
    }
    fn stats(&self) -> AllocStats {
        let mut stats = self.inner.stats();
        stats.boundary_copies_total = self.boundary_copies.load(Ordering::SeqCst);
        stats
    }
    fn contains(&self, ptr: *const u8) -> bool {
        self.inner.contains(ptr)
    }
    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        self.inner.tier_of(ptr)
    }
    fn note_boundary_copy(&self, bytes: u64) {
        self.boundary_copies.fetch_add(1, Ordering::SeqCst);
        self.boundary_bytes.fetch_add(bytes, Ordering::SeqCst);
    }
    fn is_pinned(&self) -> bool {
        self.inner.is_pinned()
    }
}
