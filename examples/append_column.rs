//! The first program a user writes: a Parquet file in, a kernel that appends a column, a
//! Parquet file out, inside a budget. It is here because every defect this example would have
//! caught reached a built wheel while the test suite was green.
//!
//! Run it with `cargo run --example append_column`; the quality gate runs it on every commit.
//! It writes its own input into a temporary directory and removes it at the end.

// `MorunaError` is a large enum on purpose (it carries a diagnostic), and the source and sink
// builders return it; the facade's own tests carry the same allowance.
#![allow(clippy::result_large_err)]

use std::path::Path;
use std::sync::Arc;

use moruna_kernel::{
    MorunaError, CancelToken, Fingerprint, InitCtx, Kernel, KernelKind, KernelState, NoState,
    Payload, PayloadKind, PayloadSpec, SourceSchema, TierPref,
};
use moruna_runtime::{RunSpec, Runtime, SinkSpec, SourceSpec};
use arrow::array::{ArrayRef, BooleanArray, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;

/// Rows the example writes and reads back.
const ROWS: i64 = 200_000;
/// The budget a user's first program is most likely to give: half a gigabyte. `MORUNA_BUDGET`
/// overrides it, which is how the same program is measured at a second budget without a second
/// copy of it: the example then leaves `spec.budget` unset and discovery reads the variable
/// (DS-I1).
const BUDGET: u64 = 512 << 20;

/// The kernel: it appends a boolean column, which is what a model or a tokeniser does. Its
/// `output_schema` reports its input unchanged, which is all an opaque kernel can honestly say
/// before it has seen a batch (05 d.1), so the sink learns the real schema from the first
/// payload (08 f.1).
struct Appender;

impl Kernel for Appender {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna::examples::append_column", b"v1")
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> moruna_kernel::Result<Payload> {
        let Payload::Table(batch, _) = &input else {
            return Err(MorunaError::Kernel {
                stage: 1,
                seq: 0,
                msg: "this kernel wants a table".to_string(),
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
        let loud: BooleanArray = values.iter().map(|v| v.map(|v| v % 3 == 0)).collect();
        input.with_column("loud", Arc::new(loud) as ArrayRef)
    }
}

fn write_input(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("value", DataType::Int64, false),
    ]));
    let ids: Int64Array = (0..ROWS).collect::<Vec<i64>>().into();
    let values: Int64Array = (0..ROWS).map(|v| v * 2).collect::<Vec<i64>>().into();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(ids) as ArrayRef, Arc::new(values) as ArrayRef],
    )
    .expect("a batch");
    let file = std::fs::File::create(path).expect("the input file");
    let mut writer = ArrowWriter::try_new(file, schema, None).expect("a writer");
    writer.write(&batch).expect("the batch is written");
    writer.close().expect("the footer is written");
}

fn rows_written(dir: &Path) -> i64 {
    use parquet::file::reader::FileReader;
    let mut total = 0i64;
    for entry in std::fs::read_dir(dir).expect("the output directory") {
        let path = entry.expect("a directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("parquet") {
            continue;
        }
        let file = std::fs::File::open(&path).expect("an output file");
        let reader = parquet::file::reader::SerializedFileReader::new(file).expect("a reader");
        total += reader.metadata().file_metadata().num_rows();
    }
    total
}

fn main() {
    let dir = std::env::temp_dir().join(format!("moruna-example-{}", std::process::id()));
    let input = dir.join("in.parquet");
    let out_dir = dir.join("out");
    std::fs::create_dir_all(&out_dir).expect("the working directory");
    write_input(&input);

    let source_path = input.clone();
    let sink_url = format!("file://{}", out_dir.display());
    let mut spec = RunSpec::new(
        SourceSpec::Build(Box::new(move |ctx| {
            moruna_sources::ParquetSource::new(
                moruna_sources::ParquetSourceConfig {
                    urls: vec![format!("file://{}", source_path.display())],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )
            .map(|source| Arc::new(source) as Arc<dyn moruna_kernel::Source>)
        })),
        vec![Arc::new(Appender) as Arc<dyn Kernel>],
        SinkSpec::Build(Box::new(move |ctx| {
            moruna_sinks::ParquetSink::new(
                moruna_sinks::ParquetSinkConfig {
                    url: sink_url.clone(),
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn moruna_kernel::Sink>)
        })),
    );
    // The whole configuration a first program needs (PY-I3): a budget. Everything else is the
    // preamble's default, including `sink.file_bytes` at 1 GiB, which is larger than this
    // budget and must not stop the run (08 f.1).
    spec.budget = match std::env::var_os("MORUNA_BUDGET") {
        Some(_) => None,
        None => Some(BUDGET),
    };
    spec.staging_dir = Some(dir.join("staging"));
    spec.profiles_dir = Some(dir.join("profiles"));
    std::fs::create_dir_all(dir.join("staging")).expect("the staging directory");

    let report = match Runtime::run(spec, CancelToken::new()) {
        Ok(report) => report,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("the example did not run: {error}");
        }
    };
    let written = rows_written(&out_dir);
    println!("{report}");
    println!("rows in {ROWS}, rows out {written}");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(written, ROWS, "every row is written");
}
