//! One job, from the document to the exit code (MH 4.3).
//!
//! The run happens on the calling thread. Beside it, one thread sends a heartbeat every
//! [`HEARTBEAT`] and one reads the peer. Every message goes through [`Wire`], which drops the
//! peer the first time a write fails and never lets that failure reach the run: a peer that is
//! gone is a note in the report, not a reason to stop (H2). The report file is written before
//! the `report` and `exit` messages are sent, so a host that hears `exit` can read the file.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moruna_discovery::DiscoveryInput;
use moruna_kernel::{CancelToken, RunId};
use moruna_placement::PlacementEngine;
use moruna_trace::RunReport;

use super::protocol::{self, Heartbeat, Inbound, LimitsMsg, Outbound};
use super::transport::{self, Address, Conn};
use super::{VERSION, diagnostic, exit, exit_code};
use crate::job::{BuildOptions, JobSpec, KernelLoader, SpecError, build};
use crate::observe::RunObserver;
use crate::run::Runtime;
use crate::spec::Components;

/// The heartbeat cadence (MH 4.3: at least every 2 s; half that, so one late tick is not a gap).
pub const HEARTBEAT: Duration = Duration::from_millis(1000);

/// How a job is run.
pub struct HostOptions<'a> {
    /// Refuse any field the environment would fill (H8).
    pub strict: bool,
    /// The environment, as a lookup.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Cancelling it cancels the run, as a `cancel` message does.
    pub cancel: CancelToken,
    /// The heartbeat cadence; [`HEARTBEAT`] outside tests.
    pub heartbeat: Duration,
}

/// How a job ended.
#[derive(Debug)]
pub struct Outcome {
    /// The exit code (MH 4.2).
    pub code: i32,
    /// Why, when the code is not 0.
    pub diagnostic: Option<String>,
    /// The run report, when the run produced one.
    pub report: Option<RunReport>,
    /// Where the report file went, when it could be written.
    pub report_file: Option<PathBuf>,
    /// What happened between Moruna and its peer.
    pub notes: Vec<String>,
}

/// The outbound half of the connection, shared by the run and the heartbeat.
pub struct Wire {
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    notes: Mutex<Vec<String>>,
    started: Instant,
}

impl Wire {
    /// A wire over a writer, or over nothing (no peer).
    pub fn new(writer: Option<Box<dyn Write + Send>>, started: Instant) -> Arc<Wire> {
        Arc::new(Wire {
            writer: Mutex::new(writer),
            notes: Mutex::new(Vec::new()),
            started,
        })
    }

    /// Send one message. The first failed write drops the peer, with a note; later sends do
    /// nothing. The run never sees the failure (H2).
    pub fn send(&self, message: &Outbound) {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let Some(w) = writer.as_mut() else {
            return;
        };
        let line = message.line();
        if let Err(error) = w.write_all(line.as_bytes()).and_then(|()| w.flush()) {
            *writer = None;
            drop(writer);
            self.note(format!(
                "the peer stopped receiving at {} ms ({error}); the run continued and the report \
                 is on disk",
                self.elapsed_ms()
            ));
        }
    }

    /// Record something about the peer for the report.
    pub fn note(&self, note: String) {
        self.notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(note);
    }

    fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// `moruna run <spec.json>`: read the document, then [`run_job`], connecting to
/// `report.socket` when the document names one.
pub fn run_file(path: &Path, loader: &dyn KernelLoader, opts: &HostOptions<'_>) -> Outcome {
    let started = Instant::now();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            let refusal = SpecError::new(
                "spec",
                format!("`{}` could not be read: {e}", path.display()),
            );
            return refused_without_document(None, &refusal, &Wire::new(None, started));
        }
    };
    let job = match JobSpec::from_json(&text) {
        Ok(job) => job,
        Err(refusal) => {
            let raw = serde_json::from_str::<serde_json::Value>(&text).ok();
            return refused_without_document(raw.as_ref(), &refusal, &Wire::new(None, started));
        }
    };
    let (wire, reader, closer) = match &job.report.socket {
        None => (Wire::new(None, started), None, None),
        Some(text) => match Address::parse(text)
            .map_err(std::io::Error::other)
            .and_then(|a| transport::connect(&a))
        {
            Ok(conn) => {
                let (reader, writer, closer) = conn.into_parts();
                let wire = Wire::new(Some(writer), started);
                let reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(reader));
                (wire, Some(reader), Some(closer))
            }
            Err(error) => {
                let wire = Wire::new(None, started);
                wire.note(format!(
                    "report.socket {text} could not be reached ({error}); the run reports to its \
                     file only"
                ));
                (wire, None, None)
            }
        },
    };
    wire.send(&Outbound::Hello {
        moruna_version: VERSION.to_string(),
        spec_digest: Some(job.digest()),
        limits: limits_for(&job),
    });
    let outcome = run_job(job, loader, opts, &wire, reader, Mode::Run);
    if let Some(close) = closer {
        close();
    }
    outcome
}

/// `moruna serve --listen <address>`: wait for one peer, say `hello`, wait for its `spec`, run
/// it as `moruna run` would, and exit (MH 4.2). One job per process.
pub fn serve(address: &Address, loader: &dyn KernelLoader, opts: &HostOptions<'_>) -> Outcome {
    let listener = match transport::listen(address) {
        Ok(listener) => listener,
        Err(error) => {
            let refusal = SpecError::new("listen", format!("{address}: {error}"));
            return refused_without_document(None, &refusal, &Wire::new(None, Instant::now()));
        }
    };
    let conn = match listener.accept() {
        Ok(conn) => conn,
        Err(error) => {
            let refusal = SpecError::new("listen", format!("{address}: accept failed: {error}"));
            return refused_without_document(None, &refusal, &Wire::new(None, Instant::now()));
        }
    };
    drop(listener);
    serve_conn(conn, loader, opts)
}

/// [`serve`] once the peer has connected.
pub fn serve_conn(conn: Conn, loader: &dyn KernelLoader, opts: &HostOptions<'_>) -> Outcome {
    let started = Instant::now();
    let (reader, writer, closer) = conn.into_parts();
    let wire = Wire::new(Some(writer), started);
    wire.send(&Outbound::Hello {
        moruna_version: VERSION.to_string(),
        spec_digest: None,
        limits: Runtime::inspect(&DiscoveryInput::default())
            .ok()
            .map(|d| LimitsMsg::of(&d.limits)),
    });
    let mut reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(reader));
    let outcome = loop {
        let line = match protocol::read_line(reader.as_mut(), protocol::MAX_SPEC_LINE) {
            Ok(Some(line)) => line,
            Ok(None) => {
                let refusal =
                    SpecError::new("spec", "the peer closed the connection before sending one");
                break refused_without_document(None, &refusal, &wire);
            }
            Err(error) => {
                let refusal = SpecError::new(
                    "spec",
                    format!("the peer's message could not be read: {error}"),
                );
                break refused_without_document(None, &refusal, &wire);
            }
        };
        match Inbound::parse(&line) {
            Ok(Inbound::Spec { spec }) => {
                let raw = spec.clone();
                match JobSpec::from_value(spec) {
                    Ok(job) => {
                        if job.report.socket.is_some() {
                            wire.note(
                                "report.socket is ignored under moruna serve: the peer that sent \
                                 the document is the peer"
                                    .to_string(),
                            );
                        }
                        break run_job(job, loader, opts, &wire, Some(reader), Mode::Serve);
                    }
                    Err(refusal) => break refused_without_document(Some(&raw), &refusal, &wire),
                }
            }
            Ok(Inbound::Cancel) => {
                break finish(
                    &wire,
                    None,
                    None,
                    exit::CANCELLED,
                    Some("cancelled before a spec arrived".to_string()),
                    None,
                );
            }
            Ok(Inbound::Checkpoint) => {
                wire.note(
                    "checkpoint before a spec arrived: there was nothing to write".to_string(),
                );
            }
            Err(reason) => wire.note(format!("an unreadable message was ignored: {reason}")),
        }
    };
    closer();
    outcome
}

/// Where the document came from, which decides what a `spec` on the socket means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `moruna run`: the document came from a file.
    Run,
    /// `moruna serve`: the document came from the peer.
    Serve,
}

/// Run one document to its exit code (MH 4.3). `wire` is the peer, or nobody; `reader` is the
/// peer's inbound half, read on its own thread for `cancel` and `checkpoint`.
pub fn run_job(
    job: JobSpec,
    loader: &dyn KernelLoader,
    opts: &HostOptions<'_>,
    wire: &Arc<Wire>,
    reader: Option<Box<dyn BufRead + Send>>,
    mode: Mode,
) -> Outcome {
    let fallback_id = job
        .run_id
        .as_deref()
        .and_then(RunId::from_hex)
        .or_else(|| crate::run::mint_run_id().ok())
        .unwrap_or(RunId([0; 16]));
    let digest = job.digest();
    let built = match build(
        &job,
        loader,
        BuildOptions {
            strict: opts.strict,
            env: opts.env,
            notes: Vec::new(),
        },
    ) {
        Ok(built) => built,
        Err(error) => {
            let file = report_path(&job, &fallback_id.to_hex());
            return finish(
                wire,
                Some(&file),
                Some((&fallback_id.to_hex(), &digest)),
                exit_code(&error),
                Some(diagnostic(&error)),
                None,
            );
        }
    };
    // The facade takes the run id from the manifest on a resume (12 f.7); the report file is
    // named for the same id.
    let run_id = match &built.spec.resume {
        Some(path) => PlacementEngine::read_manifest_header(path)
            .map(|h| h.run_id)
            .unwrap_or(fallback_id),
        None => built.run_id.unwrap_or(fallback_id),
    };
    let file = report_path(&job, &run_id.to_hex());

    let observer = RunObserver::new();
    let stop = Arc::new(AtomicBool::new(false));
    let beat = {
        let (wire, observer, stop, every) =
            (wire.clone(), observer.clone(), stop.clone(), opts.heartbeat);
        std::thread::Builder::new()
            .name("moruna-heartbeat".to_string())
            .spawn(move || heartbeat(&wire, &observer, &stop, every))
            .ok()
    };
    if let Some(reader) = reader {
        let (peer, observer, cancel) = (wire.clone(), observer.clone(), opts.cancel.clone());
        let spawned = std::thread::Builder::new()
            .name("moruna-peer".to_string())
            .spawn(move || listen_to_peer(reader, &peer, &observer, &cancel, mode));
        if spawned.is_err() {
            wire.note("the peer's messages cannot be read: no thread for them".to_string());
        }
    }

    let result = Runtime::run_with(
        built.spec,
        opts.cancel.clone(),
        Components {
            run_id: Some(run_id),
            observer: Some(observer),
            ..Components::default()
        },
    );
    stop.store(true, Ordering::SeqCst);
    if let Some(beat) = beat {
        let _ = beat.join();
    }
    let (code, diagnostic, report) = match result {
        Ok(report) => (exit::COMPLETED, None, Some(report)),
        Err(error) => {
            let failure = error.into_parts();
            (
                exit_code(&failure.error),
                Some(diagnostic(&failure.error)),
                failure.report,
            )
        }
    };
    finish(
        wire,
        Some(&file),
        Some((&run_id.to_hex(), &digest)),
        code,
        diagnostic,
        report,
    )
}

/// The report file, then `report` and `exit` (MH 4.1, f.5).
fn finish(
    wire: &Arc<Wire>,
    file: Option<&Path>,
    identity: Option<(&str, &str)>,
    code: i32,
    diagnostic: Option<String>,
    mut report: Option<RunReport>,
) -> Outcome {
    let notes = wire.take_notes();
    if let Some(report) = report.as_mut() {
        report.notes.extend(notes.iter().cloned());
    }
    let mut written = None;
    if let Some(path) = file {
        let (run_id, digest) = identity.unwrap_or(("", ""));
        let envelope = envelope(
            run_id,
            digest,
            code,
            diagnostic.as_deref(),
            report.as_ref(),
            &notes,
        );
        match write_file(path, &envelope) {
            Ok(()) => written = Some(path.to_path_buf()),
            Err(error) => wire.note(format!(
                "the report file {} could not be written: {error}",
                path.display()
            )),
        }
    }
    if let Some(report) = &report {
        wire.send(&Outbound::Report {
            report: serde_json::to_value(report).unwrap_or(serde_json::Value::Null),
        });
    }
    wire.send(&Outbound::Exit {
        code,
        diagnostic: diagnostic.clone(),
    });
    let mut notes = notes;
    notes.extend(wire.take_notes());
    Outcome {
        code,
        diagnostic,
        report,
        report_file: written,
        notes,
    }
}

/// A refusal before a document could be read, or of the document itself. The report file goes
/// where the document says, when enough of it could be read to say (MH 4.1).
fn refused_without_document(
    raw: Option<&serde_json::Value>,
    refusal: &SpecError,
    wire: &Arc<Wire>,
) -> Outcome {
    let run_id = raw
        .and_then(|v| v.get("run_id"))
        .and_then(|v| v.as_str())
        .and_then(RunId::from_hex)
        .or_else(|| crate::run::mint_run_id().ok())
        .unwrap_or(RunId([0; 16]))
        .to_hex();
    // A document too broken to build says where its report goes only if it names a file or a
    // staging directory; one that names neither leaves nothing in the working directory.
    let file = raw.and_then(|v| {
        let file = v
            .get("report")
            .and_then(|r| r.get("file"))
            .and_then(|f| f.as_str());
        let staging = v
            .get("staging")
            .and_then(|s| s.get("dir"))
            .and_then(|d| d.as_str());
        (file.is_some() || staging.is_some()).then(|| path_for(file, staging, &run_id))
    });
    finish(
        wire,
        file.as_deref(),
        Some((&run_id, "")),
        exit::SPEC_REFUSED,
        Some(refusal.to_string()),
        None,
    )
}

/// The heartbeat thread: one message per `every` until the run ends.
fn heartbeat(wire: &Wire, observer: &RunObserver, stop: &AtomicBool, every: Duration) {
    let step = Duration::from_millis(10).min(every);
    loop {
        let deadline = Instant::now() + every;
        while Instant::now() < deadline {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(step);
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        wire.send(&Outbound::Heartbeat(Heartbeat::of(
            &observer.progress(),
            wire.elapsed_ms(),
        )));
    }
}

/// The peer's thread: `cancel` and `checkpoint` act, anything else is a note (MH 4.3). It ends
/// when the peer closes or the connection is shut down at exit.
fn listen_to_peer(
    mut reader: Box<dyn BufRead + Send>,
    wire: &Wire,
    observer: &RunObserver,
    cancel: &CancelToken,
    mode: Mode,
) {
    loop {
        let line = match protocol::read_line(reader.as_mut(), protocol::MAX_CONTROL_LINE) {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(error) => {
                wire.note(format!("the peer's messages stopped: {error}"));
                return;
            }
        };
        match Inbound::parse(&line) {
            Ok(Inbound::Cancel) => {
                wire.note(format!("cancelled by the peer at {} ms", wire.elapsed_ms()));
                cancel.cancel();
            }
            Ok(Inbound::Checkpoint) => {
                if !observer.request_checkpoint() {
                    wire.note(
                        "checkpoint asked for while no manifest could be written (the run is not \
                         checkpointing, or has not started)"
                            .to_string(),
                    );
                }
            }
            Ok(Inbound::Spec { .. }) => wire.note(match mode {
                Mode::Serve => "a second spec was refused: one job per process".to_string(),
                Mode::Run => {
                    "a spec on the socket was refused: this run's document came from a file"
                        .to_string()
                }
            }),
            Err(reason) => wire.note(format!("an unreadable message was ignored: {reason}")),
        }
    }
}

/// The limits for `hello`, from the fields of the document discovery would be given.
fn limits_for(job: &JobSpec) -> Option<LimitsMsg> {
    Runtime::inspect(&DiscoveryInput {
        explicit_budget: job.budget.memory_bytes,
        explicit_cpu: job.budget.cpu,
        explicit_staging_dir: job.staging.dir.as_deref().map(PathBuf::from),
        explicit_spill_limit: job.staging.limit_bytes,
        profile_override: None,
    })
    .ok()
    .map(|d| LimitsMsg::of(&d.limits))
}

/// Where a document's report file goes (MH 4.1).
pub fn report_path(job: &JobSpec, run_id: &str) -> PathBuf {
    path_for(
        job.report.file.as_deref(),
        job.staging.dir.as_deref(),
        run_id,
    )
}

fn path_for(file: Option<&str>, staging_dir: Option<&str>, run_id: &str) -> PathBuf {
    match file {
        Some(template) => PathBuf::from(template.replace("<run_id>", run_id)),
        None => {
            let name = format!("moruna-{run_id}.report.json");
            match staging_dir {
                Some(dir) => Path::new(dir).join(name),
                None => PathBuf::from(name),
            }
        }
    }
}

/// The report file's content (MH 4.1).
fn envelope(
    run_id: &str,
    digest: &str,
    code: i32,
    diagnostic: Option<&str>,
    report: Option<&RunReport>,
    notes: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "moruna_report": 1,
        "run_id": run_id,
        "spec_digest": if digest.is_empty() { serde_json::Value::Null } else { digest.into() },
        "exit": { "code": code, "diagnostic": diagnostic },
        "host_notes": notes,
        "report": match report {
            Some(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        },
    })
}

/// Write through a temporary file and a rename, so a reader never sees half a report.
fn write_file(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, path)
}

/// Read a report file back, for a host (and a test) that has only the disk (MH 6).
pub fn read_report_file(path: &Path) -> std::io::Result<serde_json::Value> {
    let mut text = String::new();
    std::fs::File::open(path)?.read_to_string(&mut text)?;
    serde_json::from_str(&text).map_err(std::io::Error::other)
}
