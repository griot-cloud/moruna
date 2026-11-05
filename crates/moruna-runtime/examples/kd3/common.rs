//! Sole KD3 case authorship and strict result reducer, shared by socket and SDK peers.
use arrow::{
    array::{Array, Int64Array},
    ipc::reader::StreamReader,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use moruna_runtime::host::protocol::{self, Inbound, Outbound};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
};
#[path = "sink_ack.rs"]
mod sink_ack;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const ROWS: usize = 100_000;
const NOBODY: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub enum Message {
    Control(Outbound),
    Event(moruna_event::EventMsg),
}
pub fn image_digest(path: &std::path::Path) -> Result<String> {
    let value = std::fs::read_to_string(path)?;
    let digest = value.trim();
    if digest.len() != 71
        || !digest.starts_with("sha256:")
        || !digest[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("attached image digest invalid".into());
    }
    Ok(digest.to_owned())
}
fn image(digest: &str) -> Value {
    json!({"kind":"image","image":{"build_key":null,"digest":digest,"location":"/images/kd2"}})
}
fn doc(kernels: Vec<Value>) -> Value {
    json!({"moruna_spec":1,"source":{"kind":"arrow_ipc","url":"file:///kd2/input.arrow"},"kernels":kernels,"sink":{"kind":"arrow_ipc","url":"file:///tmp/kd2-out/"},"budget":{"memory_bytes":384u64<<20},"staging":{"dir":"/tmp/kd2-staging"},"report":{"file":"/tmp/kd2-report.json"},"ordered":true})
}
#[derive(Default, serde::Serialize)]
pub struct Seen {
    #[serde(skip)]
    sink_ack: sink_ack::SinkAck,
    pub code: Option<i32>,
    pub diagnostic: Option<String>,
    pub events: Vec<Value>,
    pub stream: BTreeMap<u64, Vec<u8>>,
    pub stats: Option<String>,
    pub report: Option<Value>,
    pub cancelled: bool,
    pub cancels: Vec<String>,
    pub calls: usize,
}
pub fn converse(
    assigned: Option<String>,
    cancel: bool,
    mut receive: impl FnMut() -> Result<Message>,
    mut send: impl FnMut(&Inbound) -> Result<()>,
) -> Result<Seen> {
    let mut seen = Seen::default();
    let mut sequences = BTreeMap::new();
    let mut run = None;
    loop {
        let message = match receive()? {
            Message::Event(event) => {
                if event.seq == 0 {
                    return Err("zero event sequence".into());
                }
                if let Some(id) = &event.run {
                    if assigned.as_ref().is_some_and(|assigned| assigned != id) {
                        return Err("actual event run_id differs from approved assigned job".into());
                    }
                    if run.as_ref().is_some_and(|known| known != id) {
                        return Err("event run identity changed".into());
                    }
                    run = Some(id.clone());
                }
                // Logs have their own producer sequence; use the exact canonical envelope.
                let value = event.to_value();
                let key = (
                    event.start.clone(),
                    event.attempt,
                    value.get("kind").and_then(Value::as_str) == Some("log"),
                    if matches!(event.body, moruna_event::Event::Log { .. }) {
                        value.get("task").and_then(Value::as_str).map(str::to_owned)
                    } else {
                        None
                    },
                );
                if sequences
                    .insert(key, event.seq)
                    .is_some_and(|prior| prior >= event.seq)
                {
                    return Err("event sequence did not advance".into());
                }
                seen.sink_ack
                    .observe(&event, assigned.as_deref(), ROWS as u64);
                seen.events.push(value);
                continue;
            }
            Message::Control(control) => control,
        };
        match message {
            Outbound::PortCall {
                v,
                call,
                port,
                request,
                ..
            } => {
                if v != protocol::PORTS_VERSION || port != "tap" {
                    return Err("unexpected port/version".into());
                }
                seen.calls += 1;
                if cancel {
                    if !seen.cancelled {
                        send(&Inbound::Cancel)?;
                        seen.cancelled = true;
                    }
                    continue;
                }
                let request = STANDARD.decode(request)?;
                if request.len() < 16 {
                    return Err("short tap request".into());
                }
                let word = u64::from_le_bytes(request[8..16].try_into()?);
                match &request[..4] {
                    b"KD2T" => {
                        if seen.stream.insert(word, request[16..].to_vec()).is_some() {
                            return Err("duplicate tap range".into());
                        }
                    }
                    b"KD2S" => seen.stats = Some(String::from_utf8(request[16..].to_vec())?),
                    b"KD2P" => {}
                    _ => return Err("unknown tap request".into()),
                }
                send(&Inbound::PortReply {
                    v,
                    call,
                    ok: Some(STANDARD.encode(&request[..16])),
                    error: None,
                })?;
            }
            Outbound::PortCancel { v, reason, .. } => {
                if v != protocol::PORTS_VERSION {
                    return Err("port_cancel version".into());
                }
                seen.cancels.push(reason);
            }
            Outbound::Report { report, .. } => {
                if assigned.as_ref().is_some_and(|assigned| {
                    report.get("run_id").and_then(Value::as_str) != Some(assigned.as_str())
                }) {
                    return Err("actual report run_id differs from approved assigned job".into());
                }
                seen.report = Some(report);
            }
            Outbound::Exit { code, diagnostic } => {
                seen.code = Some(code);
                seen.diagnostic = diagnostic;
                return Ok(seen);
            }
            Outbound::Hello { .. } => return Err("duplicate hello".into()),
            Outbound::Heartbeat(_) | Outbound::LimitsChanged { .. } => {}
        }
    }
}
pub fn rows(seen: &Seen) -> Result<String> {
    if seen.code != Some(0) {
        return Err(format!("exit {:?}: {:?}", seen.code, seen.diagnostic).into());
    }
    let mut bytes = Vec::new();
    for (offset, chunk) in &seen.stream {
        if *offset != bytes.len() as u64 {
            return Err("tap bytes have gap/overlap".into());
        }
        bytes.extend_from_slice(chunk);
    }
    let reader = StreamReader::try_new(Cursor::new(bytes), None)?;
    let schema = reader.schema();
    if schema
        .fields()
        .iter()
        .map(|f| f.name().as_str())
        .collect::<Vec<_>>()
        != ["x", "y"]
    {
        return Err("tap schema not x,y".into());
    }
    let mut xs = BTreeSet::new();
    let mut count = 0;
    for batch in reader {
        let batch = batch?;
        let x = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or("x not int64")?;
        let y = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or("y not int64")?;
        for i in 0..batch.num_rows() {
            let a = x.value(i);
            if x.is_null(i)
                || y.is_null(i)
                || a < 0
                || a >= ROWS as i64
                || !xs.insert(a)
                || y.value(i) != 3 * a + (a * 7919) % 10007
            {
                return Err(format!("incorrect/duplicate/null row {a}").into());
            }
            count += 1;
        }
    }
    if count != ROWS || seen.stats.is_none() {
        return Err("missing rows or tap figures".into());
    }
    let report = seen.report.as_ref().ok_or("missing report")?;
    let tasks = report["tasks"].as_array().ok_or("missing report tasks")?;
    let functions: Vec<_> = tasks
        .iter()
        .filter(|task| task["role"] == "function")
        .collect();
    let keys: BTreeSet<_> = functions
        .iter()
        .filter_map(|task| task["key"].as_str())
        .collect();
    if functions.len() != 2
        || keys.len() != 2
        || functions.iter().any(|task| {
            task["rows_in"].as_u64() != Some(ROWS as u64)
                || task["rows_out"].as_u64() != Some(ROWS as u64)
                || task["failed_morsels"].as_u64() != Some(0)
                || task["skipped_morsels"].as_u64() != Some(0)
        })
    {
        return Err("function tasks did not each acknowledge all rows without errors".into());
    }
    seen.sink_ack.verify(report["run_id"].as_str())?;
    Ok(format!(
        "{count} exact rows and sink acknowledgement; {} port calls; {}",
        seen.calls,
        seen.stats.as_deref().unwrap_or("")
    ))
}
pub fn failure(seen: &Seen, words: &[&str]) -> Result<String> {
    if seen.code != Some(4) {
        return Err(format!("exit {:?}, not4: {:?}", seen.code, seen.diagnostic).into());
    }
    let failed = seen
        .events
        .iter()
        .find(|v| {
            v["kind"] == "task_failed"
                && v["code"] == "kernel_error"
                && v["task"] == "kernel-1"
                && v["scope"] == "task"
        })
        .ok_or("missing actual kernel-1 task_failed kernel_error")?;
    if failed["message"].as_str().is_none_or(str::is_empty) {
        return Err("missing failure summary".into());
    }
    let report = seen.report.as_ref().ok_or("missing failure report")?;
    let stages = report["stages"]
        .as_array()
        .ok_or("missing failure report stages")?;
    if !stages.iter().any(|stage| {
        stage["stage"].as_u64() == Some(1)
            && stage["errors"].as_u64().is_some_and(|errors| errors > 0)
    }) {
        return Err("failure report did not acknowledge kernel-1 error".into());
    }
    // TaskFailed deliberately quotes no data; the canonical report and Exit
    // carry the full failure. Require both to agree before checking its cause.
    let diagnostic = report["exit"]["Terminated"]["diagnostic"]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or("missing terminated failure diagnostic")?;
    if seen.diagnostic.as_deref() != Some(diagnostic) {
        return Err("report and Exit failure diagnostics differ".into());
    }
    if words.iter().any(|word| !diagnostic.contains(word)) {
        return Err(format!("wrong failure: {diagnostic}").into());
    }
    Ok(diagnostic.lines().next().unwrap_or("").into())
}
pub fn cases(digest: &str) -> [(&'static str, Value); 5] {
    let python = image(digest);
    let mut tap = image(digest);
    tap["command"] = json!(["/kc1/bin/tap"]);
    tap["ports"] = json!({"tap":{"host":"tap"}});
    let mut fail = image(digest);
    fail["command"] = json!(["python3", "/kd2/transform.py", "--fail-after", "1"]);
    let mut failure_job = doc(vec![fail]);
    // This case checks the Python task's intentional failure after one answered
    // batch, not budget refusal. At 384 MiB the controller can correctly stop
    // its modeled minimum footprint first (147 MB against 135 MB headroom in
    // CI). Fund the interpreter here without changing enforcement or the
    // surrounding 1 GiB process limit; all other cases retain their budgets.
    failure_job["budget"]["memory_bytes"] = json!(512u64 << 20);
    let mut bad = image(digest);
    bad["command"] = json!(["/kc1/bin/badrecord"]);
    [
        ("rows", doc(vec![python.clone(), tap.clone()])),
        ("a task that fails", failure_job),
        ("a refused record", doc(vec![bad])),
        ("cancel", doc(vec![python, tap.clone()])),
        ("an image nobody named", doc(vec![image(NOBODY), tap])),
    ]
}
pub const PATHS: &[&str] = &[
    "/source/url",
    "/sink/url",
    "/staging/dir",
    "/report/file",
    "/kernels/0/layout",
    "/kernels/1/layout",
];
pub fn checked_job(template: &Value, actual: &Value) -> Result<()> {
    let id = actual["run_id"]
        .as_str()
        .ok_or("dedicated job requires assigned id")?;
    if moruna_kernel::RunId::from_hex(id).is_none_or(|run| run.to_hex() != id) {
        return Err("job id is not canonical assigned run identity".into());
    }
    let mut allowed = template.clone();
    allowed["run_id"] = json!(id);
    for pointer in PATHS {
        if let Some(value) = actual.pointer(pointer) {
            if !value.is_string()
                || value
                    .as_str()
                    .is_none_or(|s| s.is_empty() || s.len() > 4096)
            {
                return Err("invalid explicit fixture path".into());
            }
            if let Some(destination) = allowed.pointer_mut(pointer) {
                *destination = value.clone();
            }
        }
    }
    if allowed != *actual {
        return Err(
            "job differs from selected canonical case beyond assigned id/explicit fixture paths"
                .into(),
        );
    }
    Ok(())
}

pub fn verdict(n: usize, seen: &Seen, digest: &str) -> Result<String> {
    match n {
        0 => rows(seen),
        1 => failure(
            seen,
            &[
                "exited with status 1",
                "kd2 transform fails on purpose after 1 batches",
            ],
        ),
        2 => failure(seen, &["refused by the engine's check"]),
        3 => {
            if seen.code != Some(130)
                || !seen.cancelled
                || !seen
                    .events
                    .iter()
                    .any(|v| v["kind"] == "cancel_acknowledged")
            {
                return Err("cancel lacked actual130/acknowledgement".into());
            }
            Ok(format!(
                "exit130 cancel_acknowledged; port_cancel {:?}",
                seen.cancels
            ))
        }
        _ => {
            let why = seen.diagnostic.as_deref().unwrap_or("");
            if seen.code != Some(2)
                || !why.contains(NOBODY)
                || !why.contains(digest)
                || seen.events.iter().any(|v| v["kind"] == "task_started")
            {
                return Err(
                    "unknown image was not refused before start with both identities".into(),
                );
            }
            Ok(why.into())
        }
    }
}
