//! What the facade's tests need that is not a fake: a scratch directory unique to this
//! process (preamble 6.7), a Parquet writer, a Parquet reader and a real kernel.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow::array::{ArrayRef, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::{
    DType, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError,
    NoState, Payload, PayloadSpec, SourceSchema, TierPref,
};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Serialises the runs that build a real arena.
///
/// The arena reserves and pre-faults its region (02 f.1), so two runs in one process each
/// raise the other's baseline and neither is left a budget (12 g says `run` is not
/// re-entrant; the Rust facade does not enforce it, which is in the report).
static RUNS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold this for the length of a run that builds a real arena.
pub fn one_run_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    RUNS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A directory unique to this process and this test, removed when the test ends.
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Make one, named after the test, the process id and a counter (preamble 6.7: a fixed
    /// path made two runs of the gate delete each other's files).
    pub fn new(name: &str) -> Scratch {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("moruna-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the scratch directory");
        Scratch { path }
    }

    /// Where it is.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The schema every test file and every kernel here uses: two 64-bit integer columns.
pub fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("value", DataType::Int64, false),
    ]))
}

/// Write `rows` rows of `schema()` to `path` in `groups` row groups.
pub fn write_parquet(path: &Path, rows: u64, groups: u64) {
    let schema = schema();
    let per_group = rows.div_ceil(groups.max(1));
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(per_group as usize))
        .build();
    let file = std::fs::File::create(path).expect("the input file");
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props)).expect("a writer");
    let ids: Int64Array = (0..rows as i64).collect::<Vec<i64>>().into();
    let values: Int64Array = (0..rows as i64).map(|v| v * 2).collect::<Vec<i64>>().into();
    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::new(ids) as ArrayRef, Arc::new(values) as ArrayRef],
    )
    .expect("a batch");
    writer.write(&batch).expect("the batch is written");
    writer.close().expect("the footer is written");
}

/// Rows in every Parquet file under `dir`.
pub fn read_back_rows(dir: &Path) -> u64 {
    let mut total = 0u64;
    let entries = std::fs::read_dir(dir).expect("the output directory is readable");
    for entry in entries {
        let entry = entry.expect("a directory entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("parquet") {
            continue;
        }
        let file = std::fs::File::open(&path).expect("an output file");
        let reader = parquet::file::reader::SerializedFileReader::new(file).expect("a reader");
        use parquet::file::reader::FileReader;
        total += reader.metadata().file_metadata().num_rows() as u64;
    }
    total
}

/// A real kernel: it doubles the `value` column. Rust, stateless, amplification about one.
pub struct Doubler {
    fingerprint: Fingerprint,
}

impl Doubler {
    /// One doubler.
    pub fn new() -> Doubler {
        Doubler {
            fingerprint: Fingerprint::compute("moruna-runtime::tests::Doubler", b"v1"),
        }
    }
}

impl Kernel for Doubler {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: Some(1.0),
            ..Default::default()
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        let kernel_error = |msg: &str| MorunaError::Kernel {
            stage: 1,
            seq: 0,
            msg: msg.to_string(),
            features: None,
        };
        let Payload::Table(batch, _) = &input else {
            return Err(kernel_error("the doubler wants a table"));
        };
        let column = batch
            .column_by_name("value")
            .ok_or_else(|| kernel_error("the batch has no value column"))?;
        let values = column
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| kernel_error("the value column is not Int64"))?;
        let doubled: Int64Array = values
            .iter()
            .map(|v| v.map(|v| v.wrapping_mul(2)))
            .collect();
        input.with_column("value", Arc::new(doubled) as ArrayRef)
    }
}

/// Keeps `DType` referenced so a change to the contracts' dtype list breaks here too.
pub const _DTYPE_IN_USE: DType = DType::I64;

/// A real kernel that fails once, on the `fail_at`-th `apply`, and then stops failing when
/// the test clears the flag. Its fingerprint does not change, so a resumed run recognises it.
pub struct FailOnce {
    fingerprint: Fingerprint,
    applies: Arc<AtomicU64>,
    fail_at: Arc<AtomicU64>,
}

impl FailOnce {
    /// One kernel that fails on apply number `fail_at` (counting from one).
    pub fn new(fail_at: u64) -> FailOnce {
        FailOnce {
            fingerprint: Fingerprint::compute("moruna-runtime::tests::FailOnce", b"v1"),
            applies: Arc::new(AtomicU64::new(0)),
            fail_at: Arc::new(AtomicU64::new(fail_at)),
        }
    }

    /// The same kernel, sharing the counters, with failure turned off.
    pub fn without_failing(&self) -> FailOnce {
        self.fail_at.store(u64::MAX, Ordering::SeqCst);
        FailOnce {
            fingerprint: self.fingerprint,
            applies: self.applies.clone(),
            fail_at: self.fail_at.clone(),
        }
    }
}

impl Kernel for FailOnce {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        let at = self.applies.fetch_add(1, Ordering::SeqCst) + 1;
        if at == self.fail_at.load(Ordering::SeqCst) {
            return Err(MorunaError::Kernel {
                stage: 1,
                seq: at,
                msg: "the test asked this apply to fail".to_string(),
                features: None,
            });
        }
        Ok(input)
    }
}

/// A real kernel that appends a column, which is the canonical Moruna job: a model or a
/// tokeniser appends its output. Its `output_schema` reports its input unchanged, exactly as
/// an opaque kernel's does (05 d.1), so the sink is opened with a schema that is not the one
/// it will be written with (08 f.1).
pub struct Appender {
    fingerprint: Fingerprint,
    column: &'static str,
}

impl Appender {
    /// One appender, adding a boolean column of the given name.
    pub fn new(column: &'static str) -> Appender {
        Appender {
            fingerprint: Fingerprint::compute("moruna-runtime::tests::Appender", column.as_bytes()),
            column,
        }
    }
}

impl Kernel for Appender {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    /// What an opaque kernel can honestly say before it has seen a batch: nothing new.
    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        let Payload::Table(batch, _) = &input else {
            return Err(MorunaError::Kernel {
                stage: 1,
                seq: 0,
                msg: "the appender wants a table".to_string(),
                features: None,
            });
        };
        let values = batch
            .column_by_name("value")
            .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
            .ok_or_else(|| MorunaError::Kernel {
                stage: 1,
                seq: 0,
                msg: "the batch has no Int64 value column".to_string(),
                features: None,
            })?;
        let loud: arrow::array::BooleanArray =
            values.iter().map(|v| v.map(|v| v % 3 == 0)).collect();
        input.with_column(self.column, Arc::new(loud) as ArrayRef)
    }
}

/// Whether every Parquet file under `dir` carries a column of this name.
pub fn output_has_column(dir: &Path, name: &str) -> bool {
    use parquet::file::reader::FileReader;
    let mut seen = false;
    let entries = std::fs::read_dir(dir).expect("the output directory is readable");
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("parquet") {
            continue;
        }
        let file = std::fs::File::open(&path).expect("an output file");
        let reader = parquet::file::reader::SerializedFileReader::new(file).expect("a reader");
        let schema = reader.metadata().file_metadata().schema_descr();
        if !(0..schema.num_columns()).any(|i| schema.column(i).name() == name) {
            return false;
        }
        seen = true;
    }
    seen
}

/// Resident bytes of this process, or `None` where the platform's figure is not available.
/// Used by the test that proves an arena's mapping goes back at the end of a run (AR-I3).
pub fn resident_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().next()?.parse().ok()?;
        // SAFETY: test-only; `sysconf` takes no pointers.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        Some(pages * page)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss="])
            .arg(std::process::id().to_string())
            .output()
            .ok()?;
        let kib: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(kib * 1024)
    }
}

/// A real kernel that passes its input through after sleeping, so a run lasts long enough for
/// a host to hear it (MH HO-T6 to HO-T8).
pub struct Sleeper {
    fingerprint: Fingerprint,
    ms: u64,
}

impl Sleeper {
    /// One kernel that sleeps `ms` in every `apply`.
    pub fn new(ms: u64) -> Sleeper {
        Sleeper {
            fingerprint: Fingerprint::compute("moruna-runtime::tests::Sleeper", b"v1"),
            ms,
        }
    }
}

impl Kernel for Sleeper {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: Some(1.0),
            ..Default::default()
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        std::thread::sleep(std::time::Duration::from_millis(self.ms));
        Ok(input)
    }
}
