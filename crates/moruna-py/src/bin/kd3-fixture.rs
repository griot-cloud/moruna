//! KD3 real one-job fixture. Same binary/data/expectations on Linux and native.
//! `seed INPUT.arrow` writes the canonical input; `run CASE.json ADAPTER` runs it.
//! CASE.json carries actual `case`, `job`, optional `declarations` (KA6), and optional
//! actual `sandbox_python`/`sandbox_cgroup` config. No missing asset is skipped.
#![deny(unsafe_code)]
use arrow::{
    array::{Array, FixedSizeListArray, Int64Array, RecordBatch},
    datatypes::{DataType, Field, Schema},
    ipc::writer::FileWriter,
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
    readback: Option<Box<Facts>>,
}
struct Observe {
    inner: Box<dyn Sink>,
    facts: Arc<Mutex<Facts>>,
    readback: Option<Readback>,
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
        {
            let mut facts = self
                .facts
                .lock()
                .map_err(|_| invalid("observer poisoned"))?;
            facts.finished += 1;
            facts.committed = summary.rows;
            facts.files = summary.files.clone();
        }
        if let Some(reader) = self.readback.take() {
            let files = summary.files.clone();
            let read = std::thread::spawn(move || reader.read(files))
                .join()
                .map_err(|_| invalid("actual readback thread panicked"))??;
            self.facts
                .lock()
                .map_err(|_| invalid("observer poisoned"))?
                .readback = Some(Box::new(read));
        }
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
// Retains the ORIGINAL run's arena/reactor owners and shared claims. No second
// Runtime, Guest claim, account, or source-input query is introduced.
struct Readback {
    context: moruna_runtime::spec::BuildCtx,
    kind: String,
    target: String,
}
impl Readback {
    fn read(self, files: Vec<String>) -> moruna_kernel::Result<Facts> {
        use moruna_runtime::kinds::{Context, Entry, Registry, Standalone};
        if files.is_empty() {
            return Err(invalid("sink ACK named no files"));
        }
        let registry = Registry::standard();
        let mut context = Context::new(Arc::new(Standalone));
        let source_doc: moruna_job::KindDoc =
            serde_json::from_value(json!({"kind":self.kind,"url":files}))
                .map_err(|e| invalid(format!("actual receipt source: {e}")))?;
        let (_, adapter) = registry.source("source", &self.kind)?;
        let source = adapter
            .source(&Entry::new("source", None, &source_doc), &mut context)?
            .spec
            .build(&self.context)?;
        let sink_kind = if self.kind == "tensor" {
            "tensor"
        } else {
            "arrow_ipc"
        };
        let sink_doc: moruna_job::KindDoc =
            serde_json::from_value(json!({"kind":sink_kind,"url":self.target}))
                .map_err(|e| invalid(format!("readback sink: {e}")))?;
        let (_, adapter) = registry.sink("sink", sink_kind)?;
        let inner = adapter
            .sink(&Entry::new("sink", None, &sink_doc), &mut context)?
            .spec
            .build(&self.context)?;
        let facts = Arc::new(Mutex::new(Facts::default()));
        let mut sink = Observe {
            inner,
            facts: facts.clone(),
            readback: None,
        };
        sink.open(&source.schema())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| invalid(format!("readback IO executor: {e}")))?;
        for (seq, split) in source.plan()?.iter().enumerate() {
            let payload = runtime.block_on(source.read(
                split,
                None,
                self.context.alloc.as_ref(),
                moruna_kernel::Tier::Host,
            ))?;
            runtime.block_on(sink.write(seq as u64, payload))?;
        }
        sink.finish()?;
        drop(sink);
        Arc::try_unwrap(facts)
            .map_err(|_| invalid("readback facts still owned"))?
            .into_inner()
            .map_err(|_| invalid("readback facts poisoned"))
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
    let mut writer = FileWriter::try_new(
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
fn run_fixture(fixture: Value, adapter: &str) -> Result<Value> {
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
    let assigned = guest.as_ref().map(|guest| guest.run_id().to_hex());
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
    // Native imagefs grants writable scratch ONLY under /tmp. Each real run
    // receives fresh private paths; inputs/packages/fixtures remain read-only.
    let run_text = job
        .id
        .as_deref()
        .ok_or("fixture requires explicit run identity")?;
    let run = moruna_kernel::RunId::from_hex(run_text)
        .ok_or("fixture run identity is not canonical")?
        .to_hex();
    if run != run_text || assigned.as_deref().is_some_and(|assigned| assigned != run) {
        return Err("fixture scratch identity differs from actual assigned run".into());
    }
    let writable = Path::new("/tmp/kd3").join(&run);
    let staging = fixture["job"]["staging"]["dir"]
        .as_str()
        .ok_or("fixture staging absent")?;
    if Path::new(staging) != writable.join("staging") {
        return Err("fixture staging must match actual run scratch".into());
    }
    fs::create_dir_all(&writable)?;
    if let Some(input) = fixture["governed_input"].as_str() {
        let root = writable.join("governed");
        for task in &job.tasks {
            if matches!(task.kind.as_str(), "datafusion" | "peql")
                && task.settings.get("root").and_then(Value::as_str) != root.to_str()
            {
                return Err("governed fixture root differs from actual run scratch".into());
            }
        }
        let mut source = fs::File::open(input)?;
        let bytes = source.metadata()?.len();
        if bytes > 16 * 1024 * 1024 {
            return Err("actual governed fixture input exceeds 16MiB".into());
        }
        // peQL's Parquet binding is a dataset directory, not one file.
        let input_dir = root.join("input");
        fs::create_dir_all(&input_dir)?;
        let mut target = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(input_dir.join("part-00000.parquet"))?;
        if std::io::copy(&mut source, &mut target)? != bytes {
            return Err("actual governed input copy length differs".into());
        }
    }
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
                json!({"case":id,"adapter":adapter,"phase":phase,"mode":mode,"outcome":"refused","diagnostic":diagnostic,"run_id":assigned}),
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
    let reader = (case["role"] == "sink" && phase == "execute" && mode == "rows").then(|| {
        (
            if case["kind"] == "peql" {
                "parquet".to_owned()
            } else {
                case["kind"].as_str().unwrap_or("").to_owned()
            },
            format!(
                "{}/receipt-readback",
                fixture["job"]["staging"]["dir"]
                    .as_str()
                    .unwrap_or("/tmp/kd3-staging")
            ),
        )
    });
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
                readback: reader.map(|(kind, target)| Readback {
                    context: context.clone(),
                    kind,
                    target,
                }),
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
    let execute = json!({"case":id,"adapter":adapter,"phase":phase,"mode":mode,"outcome":"rows","rows":actual,"finish":facts.finished,"committed":facts.committed,"outside_after":outside_after,"files":facts.files,"producer":producer,"run_id":report.run_id.to_string(),"report":report});
    if let Some(read) = &facts.readback {
        let mut actual = read.batches.values().flatten().copied().collect::<Vec<_>>();
        actual.sort_unstable();
        if actual != expected || read.finished != 1 || read.committed != expected.len() as u64 {
            return Err("actual receipt source readback differs".into());
        }
        let readback = json!({"case":id,"adapter":adapter,"phase":"readback","mode":"rows","outcome":"rows","rows":actual,"finish":read.finished,"committed":read.committed,"outside_after":outside_after,"files":read.files,"source_urls":facts.files,"producer":execute,"run_id":report.run_id.to_string()});
        return Ok(json!({"run_id":report.run_id.to_string(),"receipts":[execute,readback]}));
    }
    Ok(execute)
}
#[cfg(feature = "peql")]
fn seed_contract(path: &Path) -> Result<()> {
    use moruna_datafusion::engine::Engine;
    let schema = Schema::new(vec![Field::new("x", DataType::Int64, false)]);
    let engine = Engine::in_memory(path.parent().ok_or("contract path has no parent")?);
    let mut entries = Vec::new();
    for (name, binding) in [("input", "input/"), ("output", "output/")] {
        let source = format!(
            "contract: kd3/{name}\nversion: 1\nowner: kd3\nbinding: {{parquet: {binding}}}\nexpose:\n  - {{name: x, type: int64}}\nrules:\n  - {{id: fixture, op: decide, expr: \"ctx.purpose == 'conformance'\"}}\n"
        );
        let registered = engine.register_contract(&source, &schema)?;
        entries.push(moruna_runtime::kinds::governed::entry_of(&registered)?);
    }
    let mut out = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    serde_json::to_writer_pretty(&mut out, &entries)?;
    out.write_all(b"\n")?;
    out.sync_all()?;
    Ok(())
}
fn emit(value: &Value, case: &str, adapter: &str) -> Result<()> {
    let Some(nonce) = std::env::var_os("MORUNA_KD3_PROOF_NONCE") else {
        let mut out = std::io::stdout().lock();
        serde_json::to_writer(&mut out, value)?;
        out.write_all(b"\n")?;
        return Ok(());
    };
    let nonce = nonce.to_str().ok_or("proof nonce is not UTF-8")?;
    let run = value["run_id"]
        .as_str()
        .ok_or("native result has no assigned run")?;
    let hex32 = |s: &str| {
        s.len() == 32
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if adapter != "native"
        || !hex32(nonce)
        || !hex32(run)
        || case.len() > 80
        || case.is_empty()
        || !case
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err("invalid explicit native proof envelope".into());
    }
    let wrapped = if value.get("receipts").is_some() {
        value.clone()
    } else {
        json!({"run_id":run,"receipts":[value]})
    };
    let bytes = serde_json::to_vec(&wrapped)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("native proof result exceeds 8MiB".into());
    }
    let mut out = std::io::stdout().lock();
    for (index, chunk) in bytes.chunks(96).enumerate() {
        write!(out, "KD3R {case} {run} {nonce} {} {index} ", bytes.len())?;
        for byte in chunk {
            write!(out, "{byte:02x}")?;
        }
        out.write_all(b"\n")?;
    }
    writeln!(
        out,
        "KD3E {case} {run} {nonce} {} {}",
        bytes.len(),
        bytes.len().div_ceil(96)
    )?;
    out.flush()?;
    Ok(())
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn read_case_grant(slot: &str) -> Result<Value> {
    use moruna_sys::{Memory, ObjectType, PAGE_SIZE, Rights, map};
    if slot != "kd3.case" || !moruna_micro::on_kernel() {
        return Err("run-grant requires actual native kd3.case startup Memory".into());
    }
    let handle = moruna_micro::start::take(slot).ok_or("missing actual kd3.case grant")?;
    let (kind, rights) = handle
        .info()
        .map_err(|e| invalid(format!("case grant info: {e:?}")))?;
    if kind != ObjectType::Memory
        || !rights.contains(Rights::READ | Rights::MAP)
        || rights.contains(Rights::WRITE)
    {
        return Err("case grant must be actual readonly Memory".into());
    }
    let memory = Memory(handle);
    let first = memory
        .map(0, PAGE_SIZE, map::READ)
        .map_err(|e| invalid(format!("case prefix mapping: {e:?}")))?;
    let mut prefix = [0u8; 8];
    first
        .read(0, &mut prefix)
        .map_err(|e| invalid(format!("case prefix read: {e:?}")))?;
    let length = u64::from_le_bytes(prefix);
    if length == 0 || length > 1024 * 1024 {
        return Err("case grant JSON exceeds bounded 1MiB".into());
    }
    let total = length + 8;
    let mapped = total.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let mapping = memory
        .map(0, mapped, map::READ)
        .map_err(|e| invalid(format!("case mapping: {e:?}")))?;
    let mut bytes = vec![0; mapped as usize];
    mapping
        .read(0, &mut bytes)
        .map_err(|e| invalid(format!("case read: {e:?}")))?;
    if bytes[total as usize..].iter().any(|byte| *byte != 0) {
        return Err("case grant padding is not zero".into());
    }
    let wrapper: Value = serde_json::from_slice(&bytes[8..total as usize])?;
    let object = wrapper
        .as_object()
        .ok_or("case grant wrapper is not an object")?;
    if object.len() != 3
        || !object.contains_key("nonce")
        || !object.contains_key("run_id")
        || !object.contains_key("fixture")
    {
        return Err("case grant wrapper fields differ".into());
    }
    let nonce = std::env::var("MORUNA_KD3_PROOF_NONCE")?;
    if wrapper["nonce"].as_str() != Some(nonce.as_str())
        || wrapper["run_id"] != wrapper["fixture"]["job"]["run_id"]
    {
        return Err("case grant nonce/run identity differs".into());
    }
    Ok(wrapper["fixture"].clone())
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn read_case_grant(_slot: &str) -> Result<Value> {
    Err("run-grant requires actual native x86_64 startup Memory".into())
}
fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    #[cfg(feature = "peql")]
    if args.len() == 2 && args[0] == "seed-contract" {
        return seed_contract(Path::new(&args[1]));
    }
    if args.len() == 2 && args[0] == "seed" {
        return seed(Path::new(&args[1]));
    }
    if args.len() != 3 || !matches!(args[0].as_str(), "run" | "run-grant") {
        return Err(
            "usage: moruna-kd3-fixture seed INPUT.arrow | run CASE.json linux|native".into(),
        );
    }
    let fixture: Value = if args[0] == "run" {
        serde_json::from_slice(&fs::read(&args[1])?)?
    } else {
        if args[2] != "native" {
            return Err("run-grant cannot label a host run native".into());
        }
        read_case_grant(&args[1])?
    };
    let case = fixture["case"].as_str().ok_or("case absent")?.to_owned();
    Python::initialize();
    let result = Python::attach(|py| -> Result<Value> {
        let module = pyo3::wrap_pymodule!(_core::module::_core)(py);
        py.import("sys")?
            .getattr("modules")?
            .set_item("moruna._core", module)?;
        py.detach(|| run_fixture(fixture, &args[2]))
    });
    let value = result?;
    emit(&value, &case, &args[2])
}
