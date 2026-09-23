//! Fixtures and helpers shared by the SO tests (section k).
//!
//! Section k has the files come from the bench generator. `bench` is not a dependency of any
//! crate of preamble 6.1 and may not become one, so the same shapes are written here with the
//! writers of the crates this one already depends on: the generator's controlled row count,
//! column mix, null ratio and row group size, with a ledger the tests assert against.
//! Reported as a documentation item.
//!
//! Every file is written under a directory unique to this process and this fixture
//! (preamble 6.7), never a fixed path.

#![allow(dead_code)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_kernel::arrow::array::{
    ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray, StringDictionaryBuilder,
};
use moruna_kernel::arrow::datatypes::{DataType, Field, Int32Type, Schema, SchemaRef};
use moruna_kernel::{AllocStats, Allocator, Buffer, Completion, ObjectMeta, ObjectMetadata, Tier};
use moruna_testkit::FakeAllocator;

/// A directory unique to this process and this call, removed by the operating system's
/// temporary sweep rather than by the test, so a failure leaves the file to look at.
pub fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "moruna-sources-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    dir
}

/// Drive a future to completion on this thread, with no runtime (the contracts' `Completion`
/// is a plain `Future` over `std`, contracts d.9).
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(value) => return value,
            std::task::Poll::Pending => std::thread::park(),
        }
    }
}

/// What a generated Parquet file holds, so a test can assert against a ledger rather than
/// against the reader it is testing (SO-T2).
pub struct Ledger {
    pub path: PathBuf,
    pub rows: u64,
    pub row_group_rows: u64,
    pub null_every: u64,
    pub groups: Vec<u64>,
}

impl Ledger {
    /// Value of column `i64_0` at row `r`; `None` where the null ratio puts a null.
    pub fn i64_at(&self, row: u64) -> Option<i64> {
        if self.null_every > 0 && row.is_multiple_of(self.null_every) {
            None
        } else {
            Some(row as i64)
        }
    }

    /// Value of column `f64_0` at row `r`.
    pub fn f64_at(&self, row: u64) -> f64 {
        row as f64 * 0.5
    }

    /// Value of column `str_0` at row `r`.
    pub fn str_at(&self, row: u64) -> String {
        format!("s{row:08}")
    }

    /// Nulls in `i64_0` for the rows of group `g`.
    pub fn nulls_in_group(&self, group: usize) -> u64 {
        let start: u64 = self.groups[..group].iter().sum();
        (start..start + self.groups[group])
            .filter(|r| self.i64_at(*r).is_none())
            .count() as u64
    }
}

/// The schema every generated Parquet file here has: one i64, one f64, one short string.
pub fn parquet_schema(nullable: bool) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("i64_0", DataType::Int64, nullable),
        Field::new("f64_0", DataType::Float64, false),
        Field::new("str_0", DataType::Utf8, false),
    ]))
}

/// Write a Parquet file of `rows` rows in row groups of `row_group_rows`, with a null in
/// `i64_0` every `null_every` rows (0 for none).
pub fn write_parquet(
    dir: &Path,
    name: &str,
    rows: u64,
    row_group_rows: u64,
    null_every: u64,
) -> Ledger {
    let path = dir.join(format!("{name}.parquet"));
    let schema = parquet_schema(null_every > 0);
    let properties = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(row_group_rows as usize))
        .set_statistics_enabled(parquet::file::properties::EnabledStatistics::Chunk)
        .build();
    let file = std::fs::File::create(&path).expect("the parquet file");
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(file, Arc::clone(&schema), Some(properties))
            .expect("the parquet writer");
    let mut groups = Vec::new();
    let mut written = 0u64;
    while written < rows {
        let take = row_group_rows.min(rows - written);
        let range = written..written + take;
        let ints: Int64Array = range
            .clone()
            .map(|r| {
                if null_every > 0 && r.is_multiple_of(null_every) {
                    None
                } else {
                    Some(r as i64)
                }
            })
            .collect();
        let floats: Float64Array = range.clone().map(|r| r as f64 * 0.5).collect();
        let strings: StringArray = range
            .clone()
            .map(|r| Some(format!("s{r:08}")))
            .collect::<Vec<_>>()
            .into();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(ints) as ArrayRef,
                Arc::new(floats) as ArrayRef,
                Arc::new(strings) as ArrayRef,
            ],
        )
        .expect("the batch");
        writer.write(&batch).expect("writing the batch");
        writer.flush().expect("closing the row group");
        groups.push(take);
        written += take;
    }
    writer.close().expect("closing the file");
    Ledger {
        path,
        rows,
        row_group_rows,
        null_every,
        groups,
    }
}

/// A Parquet file of one dictionary-encoded string column (f.4).
pub fn write_dictionary_parquet(dir: &Path, name: &str, rows: u64) -> PathBuf {
    let path = dir.join(format!("{name}.parquet"));
    let schema = Arc::new(Schema::new(vec![Field::new(
        "dict_0",
        DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
        false,
    )]));
    let file = std::fs::File::create(&path).expect("the parquet file");
    let mut writer = parquet::arrow::ArrowWriter::try_new(file, Arc::clone(&schema), None)
        .expect("the parquet writer");
    let mut builder = StringDictionaryBuilder::<Int32Type>::new();
    for row in 0..rows {
        builder.append_value(format!("v{}", row % 4));
    }
    let batch = RecordBatch::try_new(schema, vec![Arc::new(builder.finish()) as ArrayRef])
        .expect("the batch");
    writer.write(&batch).expect("writing the batch");
    writer.close().expect("closing the file");
    path
}

/// A Parquet file with one very large string row among small ones (SO-T14).
pub fn write_one_huge_row(dir: &Path, name: &str, huge_bytes: usize) -> PathBuf {
    let path = dir.join(format!("{name}.parquet"));
    let schema = Arc::new(Schema::new(vec![Field::new(
        "text_0",
        DataType::Utf8,
        false,
    )]));
    let properties = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(4))
        .set_compression(parquet::basic::Compression::UNCOMPRESSED)
        .build();
    let file = std::fs::File::create(&path).expect("the parquet file");
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(file, Arc::clone(&schema), Some(properties))
            .expect("the parquet writer");
    let huge = "x".repeat(huge_bytes);
    let values: StringArray = vec![Some("a"), Some(huge.as_str()), Some("b"), Some("c")].into();
    let batch =
        RecordBatch::try_new(schema, vec![Arc::new(values) as ArrayRef]).expect("the batch");
    writer.write(&batch).expect("writing the batch");
    writer.close().expect("closing the file");
    path
}

/// Write an `MRB1` tensor (contracts e.4) of `f32` values `base + i`.
pub fn write_amb1(dir: &Path, name: &str, shape: Vec<i64>, base: f32) -> (PathBuf, Vec<f32>) {
    let path = dir.join(format!("{name}.mrb1"));
    let header = moruna_kernel::mrb1::Header {
        dtype: moruna_kernel::DType::F32,
        shape: shape.clone(),
        data_offset: moruna_kernel::mrb1::Header::data_offset_for(shape.len(), 4096)
            .expect("a data offset"),
    };
    let count = header.element_count() as usize;
    let values: Vec<f32> = (0..count).map(|i| base + i as f32).collect();
    let mut bytes = vec![0u8; header.payload_end() as usize];
    header.write(&mut bytes).expect("the header");
    let start = header.data_offset as usize;
    for (i, v) in values.iter().enumerate() {
        bytes[start + 4 * i..start + 4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
    std::fs::write(&path, &bytes).expect("the mrb1 file");
    (path, values)
}

/// Write a safetensors file of one `f32` tensor, with a header length chosen so the data
/// section does not begin on a 64 byte boundary (the typical case, e.4).
pub fn write_safetensors(
    dir: &Path,
    name: &str,
    shape: Vec<i64>,
    base: f32,
) -> (PathBuf, Vec<f32>) {
    let path = dir.join(format!("{name}.safetensors"));
    let count: i64 = shape.iter().product();
    let values: Vec<f32> = (0..count).map(|i| base + i as f32).collect();
    let dims = shape
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut header = format!(
        "{{\"weight\":{{\"dtype\":\"F32\",\"shape\":[{dims}],\"data_offsets\":[0,{}]}}}}",
        values.len() * 4
    );
    // The safetensors writer pads the header with spaces to a multiple of eight, so the data
    // section starts eight byte aligned and never on a 64 byte boundary (e.4).
    while !(8 + header.len()).is_multiple_of(8) {
        header.push(' ');
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(header.len() as u64).to_le_bytes());
    bytes.extend_from_slice(header.as_bytes());
    for v in &values {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(&path, &bytes).expect("the safetensors file");
    (path, values)
}

/// Write a `.npy` file, optionally in Fortran order (e.3).
pub fn write_npy(dir: &Path, name: &str, shape: &[i64], fortran: bool) -> PathBuf {
    let path = dir.join(format!("{name}.npy"));
    let count: i64 = shape.iter().product();
    let dims = shape
        .iter()
        .map(|d| format!("{d},"))
        .collect::<Vec<_>>()
        .concat();
    let order = if fortran { "True" } else { "False" };
    let mut header = format!("{{'descr': '<f4', 'fortran_order': {order}, 'shape': ({dims}), }}");
    while !(10 + header.len() + 1).is_multiple_of(64) {
        header.push(' ');
    }
    header.push('\n');
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x93NUMPY\x01\x00");
    bytes.extend_from_slice(&(header.len() as u16).to_le_bytes());
    bytes.extend_from_slice(header.as_bytes());
    for i in 0..count {
        bytes.extend_from_slice(&(i as f32).to_le_bytes());
    }
    std::fs::write(&path, &bytes).expect("the npy file");
    path
}

/// Seed a `FakeReactor` with a file's real bytes, so a payload read goes through the fake
/// while the plan reads the file itself (section k).
pub fn seed(reactor: moruna_testkit::FakeReactor, path: &Path) -> moruna_testkit::FakeReactor {
    let bytes = std::fs::read(path).expect("the fixture file");
    reactor.with_file(&path.display().to_string(), bytes)
}

/// Seed a `FakeReactor` with fewer bytes than the file holds, so the short-read check of e.4
/// fires (SO-T9).
pub fn seed_truncated(
    reactor: moruna_testkit::FakeReactor,
    path: &Path,
    keep: usize,
) -> moruna_testkit::FakeReactor {
    let mut bytes = std::fs::read(path).expect("the fixture file");
    bytes.truncate(keep);
    reactor.with_file(&path.display().to_string(), bytes)
}

/// The error of a `Result` whose `Ok` type is not `Debug` (a source is not).
pub fn err<T>(r: moruna_kernel::Result<T>) -> moruna_kernel::MorunaError {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(e) => e,
    }
}

/// An `ObjectMetadata` that panics: a local path or a `file://` URL must never reach it
/// (SO-T16).
pub struct NoObjectStore;

impl ObjectMetadata for NoObjectStore {
    fn head_object(&self, url: &str) -> Completion<ObjectMeta> {
        panic!("head_object({url}) on a local path");
    }

    fn list_prefix(&self, url: &str) -> Completion<Vec<ObjectMeta>> {
        panic!("list_prefix({url}) on a local path");
    }
}

/// A `FakeAllocator` that also counts `Allocator::note_payload_copy`, which the fake of
/// contracts d.15 has no observable for and whose `AllocStats::payload_copies_total` is always
/// zero. Every allocation is the fake's; this only adds the counter SO-T5 needs. Reported as a
/// testkit gap.
#[derive(Clone)]
pub struct CountingAllocator {
    inner: FakeAllocator,
    copies: Arc<AtomicU64>,
    copied_bytes: Arc<AtomicU64>,
}

impl CountingAllocator {
    pub fn new(inner: FakeAllocator) -> CountingAllocator {
        CountingAllocator {
            inner,
            copies: Arc::new(AtomicU64::new(0)),
            copied_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// How many times a payload copy was announced.
    pub fn payload_copies(&self) -> u64 {
        self.copies.load(Ordering::SeqCst)
    }

    /// The bytes those copies announced.
    pub fn payload_copy_bytes(&self) -> u64 {
        self.copied_bytes.load(Ordering::SeqCst)
    }

    /// The fake underneath, for its own observables.
    pub fn fake(&self) -> &FakeAllocator {
        &self.inner
    }
}

impl Allocator for CountingAllocator {
    fn alloc(&self, bytes: usize, tier: Tier) -> moruna_kernel::Result<Buffer> {
        Allocator::alloc(&self.inner, bytes, tier)
    }

    fn page_bytes(&self) -> usize {
        Allocator::page_bytes(&self.inner)
    }

    fn stats(&self) -> AllocStats {
        let mut stats = Allocator::stats(&self.inner);
        stats.payload_copies_total = self.copies.load(Ordering::SeqCst);
        stats
    }

    fn contains(&self, ptr: *const u8) -> bool {
        Allocator::contains(&self.inner, ptr)
    }

    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        Allocator::tier_of(&self.inner, ptr)
    }

    fn note_payload_copy(&self, bytes: u64) {
        self.copies.fetch_add(1, Ordering::SeqCst);
        self.copied_bytes.fetch_add(bytes, Ordering::SeqCst);
    }

    fn is_pinned(&self) -> bool {
        Allocator::is_pinned(&self.inner)
    }
}
