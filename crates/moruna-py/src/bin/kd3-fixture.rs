//! KD3 real one-job fixture. Same binary/data/expectations on Linux and native.
//! `seed INPUT.arrow` writes the canonical input; `run CASE.json ADAPTER` runs it.
//! CASE.json carries actual `case`, `job`, optional `declarations` (KA6), and optional
//! actual `sandbox_python`/`sandbox_cgroup` config. No missing asset is skipped.
#![deny(unsafe_code)]
use arrow::{
    array::{Array, FixedSizeListArray, Int64Array, RecordBatch},
    datatypes::{DataType, Field, Schema},
    ipc::writer::StreamWriter,
};
use moruna_kernel::{
    BoxFuture, CancelToken, MorunaError, Payload, PayloadSpec, Seq, Sink, SinkSummary, SourceSchema,
};
use moruna_runtime::{
    Runtime, SinkSpec,
    job::{self, BuildOptions},
};
use pyo3::prelude::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const MATRIX: &str = include_str!("../../tests/fixtures/kd3-matrix.json");
fn invalid(why: impl Into<String>) -> MorunaError {
    MorunaError::Plan(format!("KD3 fixture: {}", why.into()))
}
#[derive(Default)]
struct Facts {
    batches: BTreeMap<Seq, Vec<i64>>,
    finished: u64,
    committed: u64,
    files: Vec<String>,
}
struct Observe {
    inner: Box<dyn Sink>,
    facts: Arc<Mutex<Facts>>,
}
impl Sink for Observe {
    fn open(&mut self, schema: &SourceSchema) -> moruna_kernel::Result<()> {
        self.inner.open(schema)
    }
    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }
    fn requires_order(&self) -> bool {
        self.inner.requires_order()
    }
    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, moruna_kernel::Result<()>> {
        Box::pin(async move {
            let array = match &payload {
                Payload::Table(batch, _) => {
                    if batch.num_columns() != 1 || batch.schema().field(0).name() != "x" {
                        return Err(invalid("expected one x column"));
                    }
                    batch.column(0).clone()
                }
                Payload::Tensor(_, _) => payload.as_column("x")?,
            };
            // A real rank-two single-column tensor has a canonical list view;
            // inspect its actual values, retaining the real owner, never synthesize rows.
            let array = if let Some(list) = array.as_any().downcast_ref::<FixedSizeListArray>() {
                if list.value_length() != 1 || list.null_count() != 0 {
                    return Err(invalid(
                        "expected scalar rows, not multi-column/null tensor",
                    ));
                }
                list.values().clone()
            } else {
                array
            };
            let values = array
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(|| invalid("real payload is not int64"))?;
            if values.null_count() != 0 {
                return Err(invalid("unexpected null values"));
            }
            let values = values.values().to_vec();
            self.inner.write(seq, payload).await?;
            let mut facts = self
                .facts
                .lock()
                .map_err(|_| invalid("observer poisoned"))?;
            if facts.batches.insert(seq, values).is_some() {
                return Err(invalid("duplicate acknowledged sink sequence"));
            }
            Ok(())
        })
    }
    fn finish(&mut self) -> moruna_kernel::Result<SinkSummary> {
        let summary = self.inner.finish()?;
        let mut facts = self
            .facts
            .lock()
            .map_err(|_| invalid("observer poisoned"))?;
        facts.finished += 1;
        facts.committed = summary.rows;
        facts.files = summary.files.clone();
        Ok(summary)
    }
    fn committed_seq(&self) -> Option<Seq> {
        self.inner.committed_seq()
    }
    fn skip(&self, seq: Seq) {
        self.inner.skip(seq)
    }
    fn checkpoint(&self) -> moruna_kernel::Result<Option<Vec<u8>>> {
        self.inner.checkpoint()
    }
    fn resume(
        &mut self,
        schema: &SourceSchema,
        state: &[u8],
        seq: Option<Seq>,
    ) -> moruna_kernel::Result<()> {
        self.inner.resume(schema, state, seq)
    }
}
// A REAL executable handler supplied by this test embedder through the public
// GuestPort contract. It runs on the canonical owned port thread, decodes the sole
// ServedCall codec, and returns actual IPC input bytes for the served adapter to
// verify/land. No simulated completion, model result or storage authority is minted.
struct FixtureEmbedding;
struct Echo;
impl moruna_runtime::kinds::Embedding for FixtureEmbedding {
    fn guest_port(&self, name: &str) -> Option<Arc<dyn moruna_kernel::GuestPort>> {
        (name == "fixture.echo").then(|| Arc::new(Echo) as Arc<dyn moruna_kernel::GuestPort>)
    }
}
impl moruna_kernel::GuestPort for Echo {
    fn call(&self, request: Vec<u8>, call: &moruna_kernel::GuestCall) -> moruna_kernel::Answer {
        if call.cancel.is_cancelled() || std::time::Instant::now() >= call.deadline {
            return moruna_kernel::Answer::Error("actual echo call cancelled/deadline".into());
        }
        match moruna_job::ServedCall::decode(&request) {
            Ok((header, stream))
                if header.kind == "fixture.echo"
                    && header.input == 0
                    && header.settings.is_empty() =>
            {
                moruna_kernel::Answer::Ok(stream.to_vec())
            }
            Ok(_) => moruna_kernel::Answer::Error("unexpected actual served envelope".into()),
            Err(error) => moruna_kernel::Answer::Error(error.to_string()),
        }
    }
}
fn expected() -> Result<Vec<i64>> {
    Ok(serde_json::from_str::<Value>(MATRIX)?["rows"]
        .as_array()
        .ok_or("canonical rows absent")?
        .iter()
        .map(|v| v.as_i64().ok_or("canonical row not int64"))
        .collect::<std::result::Result<Vec<_>, _>>()?)
}
fn seed(path: &Path) -> Result<()> {
    let rows = expected()?;
    let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)]));
    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(rows))])?;
    let mut writer = StreamWriter::try_new(
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?,
        &schema,
    )?;
    writer.write(&batch)?;
    writer.finish()?;
    Ok(())
}
fn run(path: &Path, adapter: &str) -> Result<Value> {
    let fixture: Value = serde_json::from_slice(&fs::read(path)?)?;
    let matrix: Value = serde_json::from_str(MATRIX)?;
    let id = fixture["case"].as_str().ok_or("missing case")?;
    let case = matrix["cases"]
        .as_array()
        .ok_or("matrix absent")?
        .iter()
        .find(|case| case["id"] == id)
        .ok_or("case outside required matrix")?;
    if case["runner"] != "document" {
        return Err("use real Python library fixture for caller-owned adapters".into());
    }
    let guest = moruna_runtime::bootstrap::Guest::for_current_process()?;
    if !cfg!(target_os = "linux") {
        return Err("KD3 adapters require actual Linux target".into());
    }
    match (adapter, guest.is_some()) {
        ("native", true) | ("linux", false) => {}
        _ => return Err("adapter label contradicts actual startup environment".into()),
    }
    let phase = fixture["phase"].as_str().unwrap_or("execute");
    let mode = fixture["mode"].as_str().unwrap_or("rows");
    if !["execute", "readback"].contains(&phase) || !["rows", "unknown-setting"].contains(&mode) {
        return Err("invalid fixture phase/mode".into());
    }
    let mut document = fixture["job"].clone();
    if mode == "unknown-setting" {
        let role = case["role"].as_str().ok_or("case role")?;
        let target = match role {
            "source" => &mut document["source"],
            "sink" => &mut document["sink"],
            "kernel" => document["kernels"]
                .as_array_mut()
                .ok_or("kernels absent")?
                .iter_mut()
                .find(|entry| entry["kind"] == case["kind"])
                .ok_or("named kernel absent")?,
            _ => return Err("invalid case role".into()),
        };
        target["moruna_conformance_unknown"] = json!(true);
    }
    if id == "kernel.package.pex.assets" && mode == "rows" {
        let package = document["kernels"]
            .as_array()
            .and_then(|entries| entries.iter().find(|entry| entry["kind"] == "package"))
            .ok_or("assets case has no actual package")?;
        let assets = package["assets"]
            .as_str()
            .ok_or("assets case requires real Inputs.assets source")?;
        if !Path::new(assets).is_absolute()
            || assets == "/assets"
            || package["args"] != json!({"fixture":"kd3-assets-v1"})
        {
            return Err(
                "assets case requires arbitrary absolute source and exact factory args".into(),
            );
        }
    }
    let spec = moruna_job::JobSpec::from_value(document)?;
    let job = moruna_job::Job::from_v1(&spec);
    let role = if phase == "readback" {
        if case["role"] != "sink" || mode != "rows" {
            return Err("readback applies to successful sink cases".into());
        }
        "source"
    } else {
        case["role"].as_str().ok_or("case role")?
    };
    let kind = if phase == "readback" && case["kind"] == "peql" {
        "parquet"
    } else {
        case["kind"].as_str().ok_or("case kind")?
    };
    if !job
        .tasks
        .iter()
        .any(|t| t.kind == kind && format!("{:?}", t.role).eq_ignore_ascii_case(role))
    {
        return Err("job does not exercise named kind/role".into());
    }
    // Readback must address the exact objects the actual preceding sink ACK named.
    // A second run over the original input is not artifact verification.
    let producer = if phase == "readback" {
        let path = fixture["producer_result"]
            .as_str()
            .ok_or("readback requires actual producer_result JSON")?;
        let producer: Value = serde_json::from_slice(&fs::read(path)?)?;
        if producer["case"] != id
            || producer["adapter"] != adapter
            || producer["phase"] != "execute"
            || producer["mode"] != "rows"
            || producer["outcome"] != "rows"
        {
            return Err("readback producer identity/phase/outcome differs".into());
        }
        let files: Vec<String> = serde_json::from_value(producer["files"].clone())?;
        if files.is_empty() {
            return Err("producer named no actual committed files".into());
        }
        let sources = job
            .tasks
            .iter()
            .filter(|task| task.role == moruna_job::Role::Source)
            .collect::<Vec<_>>();
        if sources.len() != 1 {
            return Err("readback requires one exact artifact source".into());
        }
        let url = sources[0]
            .settings
            .get("url")
            .ok_or("readback source has no explicit file URLs")?;
        let mut urls: Vec<String> = if let Some(single) = url.as_str() {
            vec![single.to_owned()]
        } else {
            serde_json::from_value(url.clone())?
        };
        let mut expected_files = files.clone();
        urls.sort();
        expected_files.sort();
        if urls != expected_files {
            return Err("readback URLs differ from actual committed sink files".into());
        }
        Some(producer)
    } else {
        None
    };
    let mut kinds = _core::kinds::command()?.with_embedding(Arc::new(FixtureEmbedding));
    kinds.declare(moruna_job::Declaration {
        name:"fixture.echo".into(),role:moruna_job::Role::Kernel,summary:"KD3 actual embedder identity handler".into(),
        settings:json!({"$schema":moruna_job::SCHEMA_DIALECT,"type":"object","additionalProperties":false,"properties":{},"examples":[{}]}),
        inputs:moruna_job::Flow::Batches,outputs:moruna_job::Flow::Batches,needs:moruna_job::Needs::a_port(),
        deterministic:moruna_job::Fact::Yes,preserves_columns:moruna_job::Fact::Yes,isolation:moruna_job::Isolation::Embedder,
        served:Some(moruna_job::Served {port:moruna_job::PortDoc::guest("fixture.echo"),input_schema:None,output_schema:None}),
    })?;
    if let Some(declarations) = fixture["declarations"].as_array() {
        for declaration in declarations {
            kinds.declare(serde_json::from_value(declaration.clone())?)?;
        }
    }
    let plan = job::plan::default_plan(&job)?;
    let built = job::build(
        &job,
        &plan,
        &kinds,
        BuildOptions {
            strict: true,
            env: &|_| None,
            notes: vec![],
            packages: moruna_runtime::packaged::PackageHost {
                sandbox_python: fixture["sandbox_python"].as_str().map(Into::into),
                sandbox_cgroup: fixture["sandbox_cgroup"].as_str().map(Into::into),
            },
            host_ports: None,
        },
    );
    let mut built = match built {
        Ok(built) => {
            if case["expect"] == "strict-import-refusal" || mode == "unknown-setting" {
                return Err("strict imported code unexpectedly built".into());
            }
            built
        }
        Err(error) => {
            if case["expect"] != "strict-import-refusal" && mode != "unknown-setting" {
                return Err(error.into());
            }
            // Preserve the actual canonical refusal; compare its exact diagnostic between adapters.
            let diagnostic = error.to_string();
            if !(if mode == "unknown-setting" {
                diagnostic.contains("moruna_conformance_unknown")
            } else {
                diagnostic.contains("import")
            }) {
                return Err(error.into());
            }
            return Ok(
                json!({"case":id,"adapter":adapter,"phase":phase,"mode":mode,"outcome":"refused","diagnostic":diagnostic}),
            );
        }
    };
    if id == "kernel.package.wasm"
        && !built
            .spec
            .packaged
            .iter()
            .any(|package| package.kind() == "wasm")
        || id.starts_with("kernel.package.pex")
            && !built
                .spec
                .packaged
                .iter()
                .any(|package| package.kind() == "pex")
    {
        return Err("actual opened package format does not match case".into());
    }
    // Retain only actual outside-account observers across the real run. This is a
    // native owner/retirement assertion, not a model counter or guessed quiescence.
    let outside = built
        .spec
        .packaged
        .iter()
        .filter_map(|package| package.outside())
        .collect::<Vec<_>>();
    let components = match guest {
        Some(guest) => guest.components(&job, &built.parts)?,
        None => Default::default(),
    };
    let facts = Arc::new(Mutex::new(Facts::default()));
    let observe = facts.clone();
    if built.spec.sinks.len() != 1 {
        return Err("KD3 fixture requires one real sink".into());
    }
    let original = built.spec.sinks.pop().ok_or("sink absent")?;
    built
        .spec
        .sinks
        .push(SinkSpec::Build(Box::new(move |context| {
            Ok(Box::new(Observe {
                inner: original.build(context)?,
                facts: observe,
            }))
        })));
    let cancel = CancelToken::new();
    let timeout = cancel.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if done_rx.recv_timeout(Duration::from_secs(60)).is_err() {
            timeout.cancel();
        }
    });
    let result = Runtime::run_with(built.spec, cancel, components);
    let _ = done_tx.send(());
    watchdog.join().map_err(|_| "watchdog panicked")?;
    let report = result?;
    if report.exit != moruna_runtime::ExitReason::Completed {
        return Err(format!("actual run ended {:?}", report.exit).into());
    }
    let outside_after = outside
        .iter()
        .map(|memory| memory.in_use())
        .collect::<moruna_kernel::Result<Vec<_>>>()?;
    if outside_after.iter().any(|bytes| *bytes != 0) {
        return Err("actual task account reservation remains after completed run".into());
    }
    let facts = facts.lock().map_err(|_| "observer poisoned")?;
    let mut actual = facts
        .batches
        .values()
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    actual.sort_unstable();
    let mut expected = expected()?;
    expected.sort_unstable();
    if actual != expected || facts.finished != 1 || facts.committed != expected.len() as u64 {
        return Err("actual rows/finish/committed summary mismatch".into());
    }
    Ok(
        json!({"case":id,"adapter":adapter,"phase":phase,"mode":mode,"outcome":"rows","rows":actual,"finish":facts.finished,"committed":facts.committed,"outside_after":outside_after,"files":facts.files,"producer":producer,"report":report}),
    )
}
fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() == 2 && args[0] == "seed" {
        return seed(Path::new(&args[1]));
    }
    if args.len() != 3 || args[0] != "run" {
        return Err(
            "usage: moruna-kd3-fixture seed INPUT.arrow | run CASE.json linux|native".into(),
        );
    }
    Python::initialize();
    let result = Python::attach(|py| -> Result<Value> {
        let module = pyo3::wrap_pymodule!(_core::module::_core)(py);
        py.import("sys")?
            .getattr("modules")?
            .set_item("moruna._core", module)?;
        py.detach(|| run(Path::new(&args[1]), &args[2]))
    });
    let value = result?;
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    out.write_all(b"\n")?;
    Ok(())
}
