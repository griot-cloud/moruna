//! Shared plumbing for the SI tests: a cooperative executor, a scratch directory unique to this
//! process, and arena-backed payloads.
//!
//! Every payload a test builds is over `FakeAllocator` memory, because a sink writes bodies from
//! the payload's own buffers through `BufferView::of_arrow` and `of_tensor`, which only an arena
//! buffer satisfies (contracts d.3, d.15).

#![allow(dead_code)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};

use moruna_kernel::arrow::array::{ArrayData, ArrayRef, make_array};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{DType, ManagedTensor, Payload, SourceSchema, Tier};
use moruna_testkit::FakeAllocator;

/// A waker that does nothing: these executors poll in turn rather than wait to be woken.
fn noop_waker() -> &'static Waker {
    Waker::noop()
}

/// Drive one future to completion, parking briefly whenever nothing is ready.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut cx = Context::from_waker(noop_waker());
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// Drive many futures at once on one thread, round robin, returning their outputs in order.
///
/// The sink futures are cooperative: a reorder buffer waiting for a lower sequence number
/// returns `Pending`, and a reactor completion resolves from the fake's own thread, so polling
/// them in turn makes the same interleaving a thread per morsel would, deterministically.
pub fn block_on_all<T>(futures: Vec<Pin<Box<dyn Future<Output = T> + Send + '_>>>) -> Vec<T> {
    let mut cx = Context::from_waker(noop_waker());
    let mut pending: Vec<Option<Pin<Box<dyn Future<Output = T> + Send + '_>>>> =
        futures.into_iter().map(Some).collect();
    let mut out: Vec<Option<T>> = (0..pending.len()).map(|_| None).collect();
    let mut left = pending.len();
    while left > 0 {
        let mut progressed = false;
        for i in 0..pending.len() {
            let Some(future) = pending[i].as_mut() else {
                continue;
            };
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                out[i] = Some(value);
                pending[i] = None;
                left -= 1;
                progressed = true;
            }
        }
        if !progressed && left > 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    out.into_iter()
        .map(|v| match v {
            Some(v) => v,
            None => panic!("a future was dropped without a value"),
        })
        .collect()
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory this process alone writes to, removed when the test ends. Several gates run on
/// one machine at the same time, so a fixed path would make two runs delete each other's files.
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub fn new(tag: &str) -> Scratch {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("moruna-sinks-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch directory");
        Scratch { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn url(&self) -> String {
        format!("file://{}", self.path.display())
    }

    /// The names in the directory, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(&self.path)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().to_string())
            .collect();
        out.sort();
        out
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The two-column Int64 schema the table tests use.
pub fn table_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("a", DataType::Int64, false),
        Field::new("b", DataType::Int64, false),
    ]))
}

pub fn table_source_schema() -> SourceSchema {
    SourceSchema::Table(table_schema())
}

/// One Int64 column of `rows` values starting at `base`, over arena memory.
fn arena_column(alloc: &FakeAllocator, rows: usize, base: i64) -> ArrayRef {
    let mut bytes = Vec::with_capacity(rows * 8);
    for i in 0..rows {
        bytes.extend_from_slice(&(base + i as i64).to_le_bytes());
    }
    let buffer = alloc.arrow_buffer(&bytes, alloc.host_tier());
    let data = ArrayData::builder(DataType::Int64)
        .len(rows)
        .add_buffer(buffer)
        .build()
        .expect("array data");
    make_array(data)
}

/// A batch of `rows` rows whose buffers the allocator owns.
pub fn arena_batch(alloc: &FakeAllocator, rows: usize, base: i64) -> RecordBatch {
    RecordBatch::try_new(
        table_schema(),
        vec![
            arena_column(alloc, rows, base),
            arena_column(alloc, rows, base + 1_000_000),
        ],
    )
    .expect("record batch")
}

/// A table payload of `rows` rows whose buffers the allocator owns.
pub fn arena_payload(alloc: &FakeAllocator, rows: usize, base: i64) -> Payload {
    Payload::table(arena_batch(alloc, rows, base)).expect("payload")
}

/// The tensor schema the tensor tests use: a variable leading dimension by `width` f32 columns.
pub fn tensor_source_schema(width: i64) -> SourceSchema {
    SourceSchema::Tensor {
        dtype: DType::F32,
        shape: vec![-1, width],
    }
}

/// A `rows` by `width` f32 tensor over arena memory.
pub fn arena_tensor(alloc: &FakeAllocator, rows: i64, width: i64, base: f32) -> Payload {
    let count = (rows * width) as usize;
    let mut buffer = alloc.buffer(count * 4, alloc.host_tier());
    for i in 0..count {
        let value = base + i as f32;
        buffer[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    let tensor = ManagedTensor::from_buffer(buffer, 0, DType::F32, vec![rows, width])
        .expect("tensor over an arena buffer");
    Payload::tensor(tensor).expect("payload")
}

/// A `rows` by `width` tensor of `dtype` over arena memory, filled with a counting pattern.
pub fn arena_tensor_of(alloc: &FakeAllocator, rows: i64, width: i64, dtype: DType) -> Payload {
    let count = (rows * width) as usize;
    let mut buffer = alloc.buffer(count * dtype.item_size(), alloc.host_tier());
    for (i, byte) in buffer.iter_mut().enumerate() {
        *byte = (i % 251) as u8;
    }
    let tensor = ManagedTensor::from_buffer(buffer, 0, dtype, vec![rows, width])
        .expect("tensor over an arena buffer");
    Payload::tensor(tensor).expect("payload")
}

/// A batch with a nullable column and a fixed size list column, whose buffers the encoder has
/// to treat differently from a plain primitive column.
pub fn arena_wide_batch(alloc: &FakeAllocator, rows: usize) -> RecordBatch {
    use moruna_kernel::arrow::array::Int64Array;
    let nullable: ArrayRef = Arc::new(Int64Array::from_iter(
        (0..rows).map(|i| if i % 3 == 0 { None } else { Some(i as i64) }),
    ));
    let values = arena_column(alloc, rows * 2, 0);
    let field = Arc::new(Field::new("item", DataType::Int64, false));
    let list = ArrayData::builder(DataType::FixedSizeList(field.clone(), 2))
        .len(rows)
        .add_child_data(values.to_data())
        .build()
        .expect("list data");
    let schema = Arc::new(Schema::new(vec![
        Field::new("n", DataType::Int64, true),
        Field::new("l", DataType::FixedSizeList(field, 2), false),
    ]));
    RecordBatch::try_new(schema, vec![nullable, make_array(list)]).expect("record batch")
}

/// The schema `arena_wide_batch` produces.
pub fn wide_source_schema(alloc: &FakeAllocator) -> SourceSchema {
    SourceSchema::Table(arena_wide_batch(alloc, 4).schema())
}

/// The bytes a sink wrote for `name`, whether the reactor still holds them under the temporary
/// name or under the final one.
pub fn written(reactor: &moruna_testkit::FakeReactor, dir: &Path, name: &str) -> Vec<u8> {
    let final_path = dir.join(name).display().to_string();
    let tmp_path = dir.join(format!("{name}.tmp")).display().to_string();
    match reactor.file(&final_path) {
        Some(bytes) => bytes,
        None => reactor
            .file(&tmp_path)
            .unwrap_or_else(|| panic!("the reactor holds no file for {name}")),
    }
}

/// Copy every in-memory file the reactor holds onto the real filesystem, under both its
/// temporary and its committed name, so a `resume` that lists the directory sees what the run
/// produced (contracts d.15: `FakeReactor` keeps its files in memory).
pub fn materialise(reactor: &moruna_testkit::FakeReactor, dir: &Path) {
    for op in reactor.ops() {
        let path = PathBuf::from(&op.path_or_url);
        if path.parent() != Some(dir) {
            continue;
        }
        let Some(bytes) = reactor.file(&op.path_or_url) else {
            continue;
        };
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let committed = name.strip_suffix(".tmp").unwrap_or(&name).to_string();
        // Both names: the file in progress is still `.tmp` on a real filesystem, and the ones
        // the sink committed were renamed to their final name.
        let _ = std::fs::write(dir.join(&name), &bytes);
        let _ = std::fs::write(dir.join(&committed), &bytes);
    }
}

/// The host tier of an unpinned allocator, which every test uses.
pub fn host() -> Tier {
    Tier::Host
}

/// The bytes of an object the sink wrote, read back through the reactor.
///
/// `FakeReactor` keeps objects in a map of its own and exposes no reader for it (contracts
/// d.15), so the way to see what a sink put there is the reactor's own `read_object`.
pub fn object_bytes(
    reactor: &moruna_testkit::FakeReactor,
    alloc: &FakeAllocator,
    url: &str,
    len: usize,
) -> Vec<u8> {
    use moruna_kernel::Reactor;
    let buffer = alloc.buffer(len.max(1), alloc.host_tier());
    let filled = reactor
        .read_object(url, 0, buffer)
        .wait()
        .expect("read_object");
    filled[..len].to_vec()
}

/// A `FakeAllocator` that also counts `note_payload_copy`, which the fake leaves at zero
/// (contracts d.15 fixes `AllocStats::payload_copies_total` at 0 and gives the fake no way to
/// raise it). Test code, not an extension of the testkit: it delegates every method.
pub struct CountingAlloc {
    inner: FakeAllocator,
    payload_copies: AtomicU64,
    payload_copy_bytes: AtomicU64,
}

impl CountingAlloc {
    pub fn new(inner: FakeAllocator) -> Arc<CountingAlloc> {
        Arc::new(CountingAlloc {
            inner,
            payload_copies: AtomicU64::new(0),
            payload_copy_bytes: AtomicU64::new(0),
        })
    }

    pub fn fake(&self) -> &FakeAllocator {
        &self.inner
    }

    pub fn payload_copies(&self) -> u64 {
        self.payload_copies.load(Ordering::SeqCst)
    }

    pub fn payload_copy_bytes(&self) -> u64 {
        self.payload_copy_bytes.load(Ordering::SeqCst)
    }
}

impl moruna_kernel::Allocator for CountingAlloc {
    fn alloc(&self, bytes: usize, tier: Tier) -> moruna_kernel::Result<moruna_kernel::Buffer> {
        self.inner.alloc(bytes, tier)
    }

    fn page_bytes(&self) -> usize {
        moruna_kernel::Allocator::page_bytes(&self.inner)
    }

    fn stats(&self) -> moruna_kernel::AllocStats {
        moruna_kernel::AllocStats {
            payload_copies_total: self.payload_copies(),
            ..self.inner.stats()
        }
    }

    fn contains(&self, ptr: *const u8) -> bool {
        self.inner.contains(ptr)
    }

    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        self.inner.tier_of(ptr)
    }

    fn is_pinned(&self) -> bool {
        self.inner.is_pinned()
    }

    fn note_payload_copy(&self, bytes: u64) {
        self.payload_copies.fetch_add(1, Ordering::SeqCst);
        self.payload_copy_bytes.fetch_add(bytes, Ordering::SeqCst);
    }
}
