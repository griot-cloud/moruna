//! What the facade's tests need that is not a fake: a scratch directory unique to this
//! process (preamble 6.7), a Parquet writer, a Parquet reader and a real kernel.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use amoru_kernel::{
    AmoruError, DType, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, NoState,
    Payload, PayloadSpec, SourceSchema, TierPref,
};
use arrow::array::{ArrayRef, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
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
        let path = std::env::temp_dir().join(format!("amoru-{name}-{}-{n}", std::process::id()));
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
            fingerprint: Fingerprint::compute("amoru-runtime::tests::Doubler", b"v1"),
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
            kind: amoru_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> amoru_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> amoru_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> amoru_kernel::Result<Payload> {
        let kernel_error = |msg: &str| AmoruError::Kernel {
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
            fingerprint: Fingerprint::compute("amoru-runtime::tests::FailOnce", b"v1"),
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
            kind: amoru_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> amoru_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> amoru_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> amoru_kernel::Result<Payload> {
        let at = self.applies.fetch_add(1, Ordering::SeqCst) + 1;
        if at == self.fail_at.load(Ordering::SeqCst) {
            return Err(AmoruError::Kernel {
                stage: 1,
                seq: at,
                msg: "the test asked this apply to fail".to_string(),
                features: None,
            });
        }
        Ok(input)
    }
}
