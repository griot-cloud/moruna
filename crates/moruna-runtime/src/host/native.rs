//! Duplex host protocol over the authoritative native microkernel launcher.
//! Schema, limits and control messages are the existing host protocol's, not a
//! second VM-specific message dialect.
use super::launch_witness::{Admission, Key, Phase, Witness, checked_directory};
use super::protocol::{self, Inbound, Outbound};
use moruna_vmm::{
    Budget,
    error::{Result, VmmError},
    microkernel::RunningNode,
};
use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    os::unix::fs::MetadataExt,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
fn refused(message: impl Into<String>) -> VmmError {
    VmmError::config("native session", message)
}

/// A typed canonical host message. Binary events retain the canonical owned model.
#[derive(Debug)]
pub enum Message {
    /// One canonical typed JSON control/result message.
    Control(Outbound),
    /// One validated binary record decoded by the sole event codec.
    Event(moruna_event::EventMsg),
}
#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct Producer {
    start: String,
    attempt: u32,
    log: bool,
    task: Option<String>,
}
#[derive(Default)]
struct Correlation {
    run: Option<String>,
    producers: std::collections::BTreeMap<Producer, u64>,
}
impl Correlation {
    fn check(&mut self, event: &moruna_event::EventMsg) -> Result<()> {
        if let Some(run) = &event.run {
            if self.run.as_ref().is_some_and(|known| known != run) {
                return Err(refused("event changed canonical run identity"));
            }
            self.run = Some(run.clone());
        }
        let (log, task) = match &event.body {
            moruna_event::Event::Log { task, .. } => (true, task.clone()),
            _ => (false, None),
        };
        let key = Producer {
            start: event.start.clone(),
            attempt: event.attempt,
            log,
            task,
        };
        if self
            .producers
            .get(&key)
            .is_some_and(|seq| event.seq <= *seq)
            || event.seq == 0
        {
            return Err(refused("event sequence repeated or moved backwards"));
        }
        if !self.producers.contains_key(&key) && self.producers.len() == 65_536 {
            return Err(refused("too many event producers"));
        }
        self.producers.insert(key, event.seq);
        Ok(())
    }
}
/// Own partial transport state across timeouts. Event validation remains solely in
/// canonical frame::read_frame and moruna_event::decode; no alternate event codec.
#[derive(Default)]
struct Framing {
    partial: Vec<u8>,
}
impl Framing {
    fn read(&mut self, reader: &mut dyn BufRead) -> std::io::Result<Option<protocol::Message>> {
        use std::io::{Error, ErrorKind};
        loop {
            let framed = self.partial.first() == Some(&moruna_event::frame::FRAME);
            let expected = if framed && self.partial.len() >= 5 {
                let n =
                    u32::from_le_bytes(self.partial[1..5].try_into().expect("four bytes")) as usize;
                if !(moruna_record::FIXED - 4..=moruna_record::MAX_RECORD).contains(&n) {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "invalid canonical event record length",
                    ));
                }
                Some(5 + n)
            } else {
                None
            };
            if expected == Some(self.partial.len()) {
                let bytes = core::mem::take(&mut self.partial);
                return moruna_event::frame::read_frame(&mut std::io::Cursor::new(bytes))
                    .map(|record| Some(protocol::Message::Event(record)));
            }
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return if self.partial.is_empty() {
                    Ok(None)
                } else {
                    Err(Error::new(
                        ErrorKind::UnexpectedEof,
                        "partial canonical host message",
                    ))
                };
            }
            let framed =
                framed || self.partial.is_empty() && available[0] == moruna_event::frame::FRAME;
            let (take, complete) = if framed {
                let take = expected
                    .unwrap_or(5)
                    .saturating_sub(self.partial.len())
                    .min(available.len());
                (take, false)
            } else {
                let take = available
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(available.len(), |i| i + 1);
                (take, available[take - 1] == b'\n')
            };
            let bound = if framed {
                moruna_record::MAX_RECORD + 5
            } else {
                protocol::MAX_PORT_LINE
            };
            if self
                .partial
                .len()
                .checked_add(take)
                .is_none_or(|n| n > bound)
            {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "canonical host frame exceeds bound",
                ));
            }
            self.partial.extend_from_slice(&available[..take]);
            reader.consume(take);
            if complete {
                let bytes = core::mem::take(&mut self.partial);
                return String::from_utf8(bytes)
                    .map(|line| Some(protocol::Message::Line(line)))
                    .map_err(|e| Error::new(ErrorKind::InvalidData, e));
            }
        }
    }
}

/// A cloneable peer-to-engine writer, usable while another thread reads events.
/// Port replies, cancellation and checkpoints use canonical Inbound messages.
#[derive(Clone)]
pub struct Control {
    stream: Arc<Mutex<UnixStream>>,
    deadline: Instant,
    failed: Arc<AtomicBool>,
}
impl Control {
    /// Send one bounded canonical control message. A second Spec is rejected by
    /// the actual runtime; all port reply versions/ids remain its responsibility.
    pub fn send(&self, message: &Inbound) -> Result<()> {
        let mut bytes = serde_json::to_vec(message).map_err(|e| refused(e.to_string()))?;
        bytes.push(b'\n');
        let limit = match message {
            Inbound::Spec { .. } => protocol::MAX_SPEC_LINE,
            Inbound::PortReply { .. } => protocol::MAX_PORT_LINE,
            _ => protocol::MAX_CONTROL_LINE,
        };
        if bytes.len() > limit {
            return Err(refused("control message exceeds canonical protocol bound"));
        }
        let mut stream = loop {
            if Instant::now() >= self.deadline {
                return Err(refused("control deadline expired"));
            }
            match self.stream.try_lock() {
                Ok(stream) => break stream,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(refused("control stream lock poisoned"));
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(1))
                }
            }
        };
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(refused("control deadline expired"));
        }
        stream
            .set_write_timeout(Some(remaining.min(Duration::from_secs(15))))
            .map_err(|e| VmmError::io("timeout", "native control", &e))?;
        stream.write_all(&bytes).map_err(|e| {
            self.failed.store(true, Ordering::Release);
            VmmError::io("write", "native control", &e)
        })
    }
}
/// The launch identity a native run's disk grants are checked against (0.6.6): the public
/// digest of the exact MJS bytes the Spec carries, and the witness key's run and attempt. A
/// version 1 Spec sent as JSON has no exact document bytes, so it carries no job digest and a
/// granted disk is refused for it.
fn run_authority(
    key: &super::launch_witness::Key,
    spec: &Inbound,
) -> moruna_vmm::microkernel::RunAuthority {
    use base64::Engine as _;
    let job_digest = match spec {
        Inbound::Spec {
            spec: serde_json::Value::String(encoded),
            ..
        } => base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map(|bytes| crate::report_doc::document_digest(&bytes))
            .unwrap_or_default(),
        _ => String::new(),
    };
    moruna_vmm::microkernel::RunAuthority {
        job_digest,
        run_id: key.run_id.clone(),
        attempt: key.attempt,
    }
}

/// The exact installed launcher tools; no downloading or fallback backend.
pub struct Tools<'a> {
    /// Explicit QEMU KVM executable.
    pub qemu: &'a Path,
    /// Existing moruna-vhost-vsock helper executable.
    pub vsock: &'a Path,
    /// Where the run's jail is accounted: a delegated cgroup, or none.
    pub cgroup: &'a moruna_vmm::jail::Cgroup,
}
type NodeLauncher = fn(
    &Path,
    Budget,
    &Path,
    &Path,
    &Path,
    &moruna_vmm::jail::Cgroup,
    &moruna_vmm::microkernel::RunAuthority,
) -> Result<RunningNode>;
struct NodeBackend<'a> {
    tools: Tools<'a>,
    launch: NodeLauncher,
}
/// Original-call dispatch certainty; later disk probes never return NotDispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartDisposition {
    /// Original call refused before its durable dispatch claim; no older-run assertion.
    NotDispatched,
    /// Claim/spawn window may have executed; reconcile, never infer failure.
    MayHaveDispatched,
}
/// Probe result backed by this actual live owner, never recovered PID ownership.
#[derive(Debug)]
pub enum Reconcile {
    /// Actual owned child handles remain live; readiness is the recorded real Hello.
    Owned(super::launch_witness::Record),
    /// Owner evidence is insufficient; never release inputs or redispatch from this.
    Unknown(super::launch_witness::Record),
}
/// Final cancellation result; uncertainty retains the run's obligations.
#[derive(Debug)]
pub enum CancelOutcome {
    /// Job reported exit and both owned child processes were reaped.
    Exited(super::launch_witness::Record),
    /// Explicit owned cancellation and both reaps completed durably.
    Cancelled(super::launch_witness::Record),
    /// Cleanup/durable publication did not establish finality.
    Unknown {
        /// Last retained evidence, not a final result.
        record: super::launch_witness::Record,
        /// Actual cleanup/publication error.
        error: VmmError,
    },
}
/// Original-call failure retaining explicit dispatch uncertainty.
#[derive(Debug)]
pub struct StartError {
    /// Whether this original call can prove it never reached dispatch intent.
    pub disposition: StartDisposition,
    /// Actual validation, launch or transport error.
    pub source: VmmError,
}
impl From<StartError> for VmmError {
    fn from(error: StartError) -> Self {
        VmmError::Control(format!("{:?}: {}", error.disposition, error.source))
    }
}
fn checked_identity(spec: &mut Inbound, run: moruna_kernel::RunId) -> Result<(String, String)> {
    use base64::Engine as _;
    let Inbound::Spec { spec, plan } = spec else {
        return Err(refused("canonical Spec required"));
    };
    let bytes = match spec {
        serde_json::Value::String(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map_err(|e| refused(e.to_string()))?,
        value => serde_json::to_vec(value).map_err(|e| refused(e.to_string()))?,
    };
    let job = super::session::read_document(&bytes).map_err(|(_, e)| refused(e.to_string()))?;
    let assigned = run.to_hex();
    if job.id.as_deref() != Some(assigned.as_str()) {
        return Err(refused(
            "explicit document run must equal canonical admission",
        ));
    }
    let checked_plan = match plan.as_ref() {
        Some(encoded) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.as_bytes())
                .map_err(|e| refused(e.to_string()))?;
            super::session::read_plan(&bytes).map_err(|e| refused(e.to_string()))?
        }
        None => crate::job::plan::default_plan(&job).map_err(|e| refused(e.to_string()))?,
    };
    moruna_plan::check_plan(&checked_plan, &job)
        .map_err(|e| refused(format!("plan refused: {e:?}")))?;
    // Pin and deliver the actual default plan too: guest resource discovery
    // cannot silently execute a different plan under this witness digest.
    if plan.is_none() {
        *plan =
            Some(base64::engine::general_purpose::STANDARD.encode(checked_plan.to_flatbuffer()));
    }
    Ok((moruna_plan::job_digest(&job), checked_plan.content_id()))
}

/// A validated, owned Spec and its exact canonical launch key, prepared without
/// claiming or dispatching a run. Persist [`Self::key`] before starting: even a
/// start error with no `NativeRun` can then be reconciled conservatively.
///
/// Fields are private and there is no deserializer or mutable Spec accessor.
/// The discovered default plan is materialized once and travels with this value.
pub struct PreparedLaunch {
    spec: Inbound,
    key: Key,
    directory_path: PathBuf,
    directory: File,
}
impl PreparedLaunch {
    /// Engine job/plan identities, distinct from hashes of caller custody files.
    pub fn key(&self) -> &Key {
        &self.key
    }
    fn validate_admission(&self, admission: &Admission<'_>) -> Result<()> {
        if self.directory_path != admission.directory || !self.key.matches_admission(admission) {
            return Err(refused("prepared launch differs from admission"));
        }
        let current = checked_directory(admission.directory)?;
        let expected = self
            .directory
            .metadata()
            .map_err(|e| refused(e.to_string()))?;
        let current = current.metadata().map_err(|e| refused(e.to_string()))?;
        if (expected.dev(), expected.ino()) != (current.dev(), current.ino()) {
            return Err(refused("prepared durable directory was replaced"));
        }
        Ok(())
    }
}
/// Read/check the canonical job and plan exactly once, without creating a claim.
/// Requires an explicit document run id matching admission. Owns the input, so
/// changes to caller values/files afterward cannot change the dispatched Spec.
/// A missing plan is discovered and pinned here, never recomputed at start.
pub fn prepare(admission: &Admission<'_>, mut spec: Inbound) -> Result<PreparedLaunch> {
    let directory = checked_directory(admission.directory)?;
    let (job, plan) = checked_identity(&mut spec, admission.run_id)?;
    let bytes = serde_json::to_vec(&spec).map_err(|e| refused(e.to_string()))?;
    if bytes.len() >= protocol::MAX_SPEC_LINE {
        return Err(refused("prepared Spec exceeds canonical protocol bound"));
    }
    Ok(PreparedLaunch {
        spec,
        key: Key::checked(admission, job, plan),
        directory_path: admission.directory.into(),
        directory,
    })
}

/// One owned node VM/run and its full-duplex guest connection. Drop tears down
/// the actual VM/helper, without inferring success from process exit or EOF.
pub struct NativeRun {
    witness: Witness,
    guest_exit: Option<i32>,
    transport_uncertain: bool,
    node: RunningNode,
    reader: BufReader<UnixStream>,
    control: Control,
    framing: Framing,
    correlation: Correlation,
    hello: bool,
    exited: bool,
}
impl NativeRun {
    /// Boot checked artifacts and connect to their declared real runtime port.
    /// `runtime_dir` must not exist; callers own their deadline and lifecycle.
    pub fn start(
        admission: Admission<'_>,
        descriptor: &Path,
        budget: Budget,
        runtime_dir: &Path,
        tools: Tools<'_>,
        spec: Inbound,
        deadline: Instant,
    ) -> core::result::Result<Self, StartError> {
        if Instant::now() >= deadline {
            return Err(StartError {
                disposition: StartDisposition::NotDispatched,
                source: refused("launch deadline expired before dispatch intent"),
            });
        }
        let prepared = prepare(&admission, spec).map_err(|source| StartError {
            disposition: StartDisposition::NotDispatched,
            source,
        })?;
        Self::start_prepared(
            admission,
            descriptor,
            budget,
            runtime_dir,
            tools,
            prepared,
            deadline,
        )
    }
    /// Consume exactly the prepared Spec/key. Admission (including the real
    /// durable directory) must still match; mismatch refuses before any claim.
    /// After the durable claim, every error is `MayHaveDispatched`; use the key
    /// persisted before this call to probe, never infer permission to respawn.
    pub fn start_prepared(
        admission: Admission<'_>,
        descriptor: &Path,
        budget: Budget,
        runtime_dir: &Path,
        tools: Tools<'_>,
        prepared: PreparedLaunch,
        deadline: Instant,
    ) -> core::result::Result<Self, StartError> {
        Self::start_prepared_owned(
            admission,
            descriptor,
            budget,
            runtime_dir,
            NodeBackend {
                tools,
                launch: moruna_vmm::microkernel::launch_for_run,
            },
            prepared,
            deadline,
        )
    }
    /// Explicit TEST-ONLY TCG proof through the same prepared identity, durable
    /// claim, native protocol, events and owned cleanup as production. Requires
    /// the nondefault `native-tcg-proof` feature; never used as a KVM fallback.
    /// No throughput, isolation or KVM-performance claim follows from this path.
    #[cfg(feature = "native-tcg-proof")]
    pub fn start_prepared_tcg_proof(
        admission: Admission<'_>,
        descriptor: &Path,
        budget: Budget,
        runtime_dir: &Path,
        tools: Tools<'_>,
        prepared: PreparedLaunch,
        deadline: Instant,
    ) -> core::result::Result<Self, StartError> {
        Self::start_prepared_owned(
            admission,
            descriptor,
            budget,
            runtime_dir,
            NodeBackend {
                tools,
                launch: moruna_vmm::microkernel::launch_tcg_proof_for_run,
            },
            prepared,
            deadline,
        )
    }
    fn start_prepared_owned(
        admission: Admission<'_>,
        descriptor: &Path,
        budget: Budget,
        runtime_dir: &Path,
        backend: NodeBackend<'_>,
        prepared: PreparedLaunch,
        deadline: Instant,
    ) -> core::result::Result<Self, StartError> {
        prepared
            .validate_admission(&admission)
            .map_err(|source| StartError {
                disposition: StartDisposition::NotDispatched,
                source,
            })?;
        if Instant::now() >= deadline {
            return Err(StartError {
                disposition: StartDisposition::NotDispatched,
                source: refused("launch deadline expired before dispatch intent"),
            });
        }
        let PreparedLaunch {
            spec,
            key,
            directory,
            ..
        } = prepared;
        // From the exclusive durable claim onward, errors and crashes are
        // uncertain. An existing claim can never authorize a second launch.
        let witness =
            Witness::reserve_prepared(&admission, key, directory).map_err(|source| StartError {
                disposition: StartDisposition::MayHaveDispatched,
                source,
            })?;
        Self::start_owned(
            witness,
            descriptor,
            budget,
            runtime_dir,
            backend,
            spec,
            deadline,
        )
        .map_err(|source| StartError {
            disposition: StartDisposition::MayHaveDispatched,
            source,
        })
    }
    fn start_owned(
        mut witness: Witness,
        descriptor: &Path,
        budget: Budget,
        runtime_dir: &Path,
        backend: NodeBackend<'_>,
        spec: Inbound,
        deadline: Instant,
    ) -> Result<Self> {
        if !matches!(spec, Inbound::Spec { .. }) {
            return Err(refused("native run must start with canonical Spec"));
        }
        if Instant::now() >= deadline {
            return Err(refused("launch deadline expired after dispatch intent"));
        }
        let authority = run_authority(witness.key(), &spec);
        let node = (backend.launch)(
            descriptor,
            budget,
            runtime_dir,
            backend.tools.qemu,
            backend.tools.vsock,
            backend.tools.cgroup,
            &authority,
        )?;
        witness.advance(Phase::Launched)?;
        let reader = node.connect(
            node.agent_port(),
            deadline.min(Instant::now() + moruna_vmm::session::CONNECT_TIMEOUT),
        )?;
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| VmmError::io("timeout", "native read", &e))?;
        let writer = reader
            .get_ref()
            .try_clone()
            .map_err(|e| VmmError::io("clone", "native control", &e))?;
        writer
            .set_write_timeout(Some(Duration::from_secs(15)))
            .map_err(|e| VmmError::io("timeout", "native control", &e))?;
        let control = Control {
            stream: Arc::new(Mutex::new(writer)),
            deadline,
            failed: Arc::new(AtomicBool::new(false)),
        };
        control.send(&spec)?;
        let assigned_run = witness.key().run_id.clone();
        Ok(Self {
            witness,
            guest_exit: None,
            transport_uncertain: false,
            node,
            reader,
            control,
            framing: Framing::default(),
            correlation: Correlation {
                run: Some(assigned_run),
                ..Correlation::default()
            },
            hello: false,
            exited: false,
        })
    }
    /// Stop/reap the actual owned VM and backend with a reviewable error result.
    pub fn shutdown(self) -> Result<()> {
        match self.cancel() {
            CancelOutcome::Exited(_) | CancelOutcome::Cancelled(_) => Ok(()),
            CancelOutcome::Unknown { error, .. } => Err(error),
        }
    }
    /// Finality requires this actual owner and both real child reaps. Timeout,
    /// failed cleanup or failed durable publication is explicitly Unknown.
    pub fn cancel(mut self) -> CancelOutcome {
        let terminal = match self.guest_exit {
            Some(code) => Phase::Exited(code),
            None => Phase::Cancelled,
        };
        let result = self
            .node
            .shutdown()
            .and_then(|()| self.witness.advance(terminal));
        match result {
            Err(error) => CancelOutcome::Unknown {
                record: self.witness.record(),
                error,
            },
            Ok(()) if self.guest_exit.is_some() => CancelOutcome::Exited(self.witness.record()),
            Ok(()) => CancelOutcome::Cancelled(self.witness.record()),
        }
    }
    /// Disk evidence is useful after owner loss, but never permission to respawn.
    pub fn launch_key(&self) -> &super::launch_witness::Key {
        self.witness.key()
    }
    /// Only this live owner can establish current process evidence. A disappeared
    /// process or poisoned ownership is uncertainty, not fabricated job failure.
    pub fn reconcile(&self) -> Reconcile {
        if self.transport_uncertain
            || !self.witness.healthy()
            || self.control.failed.load(Ordering::Acquire)
            || self.control.stream.is_poisoned()
        {
            return Reconcile::Unknown(self.witness.record());
        }
        match self.node.try_wait() {
            Ok(None) => Reconcile::Owned(self.witness.record()),
            _ => Reconcile::Unknown(self.witness.record()),
        }
    }

    /// Peer replies/control can be sent concurrently with next(), including while
    /// the engine waits for a port response. This is the embedding seam.
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    /// Read one actual typed runtime message at the canonical maximum port/report
    /// frame bound. Partial frames survive socket timeouts; no unbounded read_line.
    pub fn next(&mut self, deadline: Instant) -> Result<Message> {
        let result = self.next_message(deadline);
        if result.is_err() && !self.exited {
            // Observed transport/deadline loss cannot be erased by finding the
            // emulator PID still alive. Reconciliation must retain uncertainty.
            self.transport_uncertain = true;
        }
        result
    }
    fn next_message(&mut self, deadline: Instant) -> Result<Message> {
        let deadline = deadline.min(self.control.deadline);
        if self.exited {
            return Err(refused("job already reported exit"));
        }
        loop {
            if self.control.failed.load(Ordering::Acquire) || self.control.stream.is_poisoned() {
                return Err(refused("control transport failed or poisoned"));
            }
            if Instant::now() >= deadline {
                return Err(refused("job deadline expired"));
            }
            let raw = match self.framing.read(&mut self.reader) {
                Ok(Some(message)) => message,
                Ok(None) => return Err(refused("guest closed without an explicit job exit")),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if self.node.try_wait()?.is_some() {
                        return Err(refused("node exited before job completion"));
                    }
                    continue;
                }
                Err(e) => return Err(VmmError::io("read", "native runtime message", &e)),
            };
            let message = match raw {
                protocol::Message::Event(record) => {
                    if !self.hello {
                        return Err(refused("event before protocol hello"));
                    }
                    let event = moruna_event::decode(&record)
                        .map_err(|e| refused(format!("invalid canonical event: {e}")))?;
                    self.correlation.check(&event)?;
                    return Ok(Message::Event(event));
                }
                protocol::Message::Line(line) => serde_json::from_str::<Outbound>(&line)
                    .map_err(|e| refused(format!("invalid canonical guest control: {e}")))?,
            };
            if !self.hello {
                if !matches!(message, Outbound::Hello { .. }) {
                    return Err(refused("guest omitted protocol hello"));
                }
                if let Outbound::Hello {
                    spec_digest: Some(digest),
                    ..
                } = &message
                    && digest != &self.witness.key().spec_digest
                {
                    return Err(refused("guest Hello spec digest differs from admission"));
                }
                self.witness.advance(Phase::Hello)?;
                self.hello = true;
            } else if matches!(message, Outbound::Hello { .. }) {
                return Err(refused("guest repeated protocol hello"));
            }
            if let Outbound::Exit { code, .. } = &message {
                if !(0..=255).contains(code) {
                    return Err(refused("invalid job exit code"));
                }
                self.witness.advance(Phase::GuestExit(*code))?;
                self.guest_exit = Some(*code);
                self.exited = true;
            }
            return Ok(Message::Control(message));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct LaunchFiles(PathBuf);
    impl LaunchFiles {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "moruna-prepared-launch-{}-{}",
                std::process::id(),
                crate::run::mint_run_id().unwrap().to_hex()
            ));
            std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
        fn admission(&self) -> Admission<'_> {
            Admission {
                run_id: moruna_kernel::RunId([1; 16]),
                node_boot: moruna_kernel::RunId([2; 16]),
                attempt: 3,
                directory: &self.0,
            }
        }
        fn spec(&self) -> Inbound {
            let path = self.0.join("job.json");
            let document = serde_json::json!({
                "moruna_spec": 1,
                "run_id": self.admission().run_id.to_hex(),
                "source": {"kind":"parquet", "url":"file:///unused-input.parquet"},
                "kernels": [],
                "sink": {"kind":"parquet", "url":"file:///unused-output"},
                "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
                "staging": {"dir": self.0.to_string_lossy(), "limit_bytes": 1u64 << 30}
            });
            std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
            Inbound::Spec {
                spec: serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap(),
                plan: None,
            }
        }
        fn fail_start(&self, admission: Admission<'_>, prepared: PreparedLaunch) -> StartError {
            // Actual launch validation must fail before any spawn: the descriptor
            // does not exist. No substitute launcher or mocked success is used.
            match NativeRun::start_prepared(
                admission,
                &self.0.join("absent-node.json"),
                Budget {
                    memory_bytes: 1 << 30,
                    memory_max_bytes: 1 << 30,
                    cpus: 2,
                    cpus_max: 2,
                },
                &self.0.join("never-started"),
                Tools {
                    qemu: &self.0.join("absent-qemu"),
                    vsock: &self.0.join("absent-vsock"),
                    cgroup: &moruna_vmm::jail::Cgroup::Undelegated,
                },
                prepared,
                Instant::now() + Duration::from_secs(5),
            ) {
                Ok(_) => panic!("nonexistent descriptor must never launch"),
                Err(error) => error,
            }
        }
        fn claimed(&self) -> bool {
            self.0
                .join(format!("{}.launch", self.admission().run_id.to_hex()))
                .exists()
        }
    }
    impl Drop for LaunchFiles {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    /// 0.6.6: a native launch's disk authority is the public digest of the Spec's exact MJS
    /// bytes, with the witness key's run and attempt; a JSON version 1 Spec carries no digest.
    #[test]
    fn run_authority_binds_the_public_digest_and_the_launch_identity() {
        use base64::Engine as _;
        let key = super::super::launch_witness::Key {
            run_id: "01".repeat(16),
            spec_digest: "sha256:fingerprint".into(),
            host_plan_digest: "plan".into(),
            node_boot: "02".repeat(16),
            attempt: 3,
        };
        let bytes = b"canonical mjs bytes";
        let v2 = Inbound::Spec {
            spec: serde_json::Value::String(
                base64::engine::general_purpose::STANDARD.encode(bytes),
            ),
            plan: None,
        };
        let authority = run_authority(&key, &v2);
        assert_eq!(
            authority.job_digest,
            crate::report_doc::document_digest(bytes)
        );
        assert_eq!(authority.run_id, key.run_id);
        assert_eq!(authority.attempt, 3);
        let v1 = Inbound::Spec {
            spec: serde_json::json!({"moruna_spec": 1}),
            plan: None,
        };
        assert_eq!(run_authority(&key, &v1).job_digest, "");
        let garbled = Inbound::Spec {
            spec: serde_json::Value::String("not base64!".into()),
            plan: None,
        };
        assert_eq!(run_authority(&key, &garbled).job_digest, "");
    }

    #[test]
    fn prepared_key_survives_failed_dispatch_and_never_authorizes_respawn() {
        use super::super::launch_witness::{Probe, probe};
        let files = LaunchFiles::new();
        let prepared = prepare(&files.admission(), files.spec()).unwrap();
        assert!(!files.claimed());
        let key = prepared.key().clone();
        let persisted = files.0.join("persisted-key.json");
        let mut file = std::fs::File::create(&persisted).unwrap();
        file.write_all(&serde_json::to_vec(&key).unwrap()).unwrap();
        file.sync_all().unwrap();
        let saved: Key = serde_json::from_slice(&std::fs::read(persisted).unwrap()).unwrap();
        assert_eq!(key, saved);
        let failure = files.fail_start(files.admission(), prepared);
        assert_eq!(failure.disposition, StartDisposition::MayHaveDispatched);
        assert!(
            matches!(probe(&files.0, &saved), Probe::Unknown(Some(record)) if record.key == saved && record.phase == Phase::PotentialDispatch)
        );
        let before = std::fs::read(files.0.join(format!("{}.launch", saved.run_id))).unwrap();
        let again = prepare(&files.admission(), files.spec()).unwrap();
        assert_eq!(again.key(), &saved);
        assert_eq!(
            files.fail_start(files.admission(), again).disposition,
            StartDisposition::MayHaveDispatched
        );
        assert_eq!(
            std::fs::read(files.0.join(format!("{}.launch", saved.run_id))).unwrap(),
            before
        );
        assert!(!files.0.join("never-started").exists());
    }
    #[test]
    fn preparation_pins_canonical_v1_identity_and_default_plan_not_caller_files() {
        use base64::Engine as _;
        let files = LaunchFiles::new();
        let mut input = files.spec();
        let prepared = prepare(&files.admission(), input.clone()).unwrap();
        let Inbound::Spec {
            spec,
            plan: Some(plan),
        } = &prepared.spec
        else {
            panic!("default plan must be pinned")
        };
        let job = super::super::session::read_document(&serde_json::to_vec(spec).unwrap()).unwrap();
        assert_eq!(
            job.id.as_deref(),
            Some(files.admission().run_id.to_hex().as_str())
        );
        assert_eq!(prepared.key().spec_digest, moruna_plan::job_digest(&job));
        let plan = super::super::session::read_plan(
            &base64::engine::general_purpose::STANDARD
                .decode(plan)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(prepared.key().host_plan_digest, plan.content_id());
        moruna_plan::check_plan(&plan, &job).unwrap();
        // Mutating caller-owned copies or the file cannot modify the opaque prepared value.
        std::fs::write(files.0.join("job.json"), b"not a job").unwrap();
        if let Inbound::Spec { spec, .. } = &mut input {
            spec["run_id"] = serde_json::json!("ff".repeat(16));
        }
        assert!(prepare(&files.admission(), input).is_err());
        let key = prepared.key().clone();
        assert_eq!(
            files.fail_start(files.admission(), prepared).disposition,
            StartDisposition::MayHaveDispatched
        );
        assert!(
            matches!(super::super::launch_witness::probe(&files.0, &key), super::super::launch_witness::Probe::Unknown(Some(record)) if record.key == key)
        );
    }
    #[test]
    fn changed_admission_refuses_before_claim_or_dispatch() {
        let files = LaunchFiles::new();
        let other = LaunchFiles::new();
        for field in 0..4 {
            let prepared = prepare(&files.admission(), files.spec()).unwrap();
            let mut changed = files.admission();
            match field {
                0 => changed.run_id = moruna_kernel::RunId([3; 16]),
                1 => changed.node_boot = moruna_kernel::RunId([3; 16]),
                2 => changed.attempt += 1,
                _ => changed.directory = &other.0,
            }
            assert_eq!(
                files.fail_start(changed, prepared).disposition,
                StartDisposition::NotDispatched
            );
            assert!(!files.claimed());
            assert!(!other.claimed());
            assert!(!files.0.join("never-started").exists());
        }
    }
    #[test]
    fn missing_run_and_crafted_controls_are_not_prepared() {
        let files = LaunchFiles::new();
        let mut missing = files.spec();
        if let Inbound::Spec { spec, .. } = &mut missing {
            spec.as_object_mut().unwrap().remove("run_id");
        }
        assert!(prepare(&files.admission(), missing).is_err());
        assert!(prepare(&files.admission(), Inbound::Cancel).is_err());
        let mut invalid = files.spec();
        if let Inbound::Spec { plan, .. } = &mut invalid {
            *plan = Some("not-base64".into());
        }
        assert!(prepare(&files.admission(), invalid).is_err());
        assert!(!files.claimed());
    }
    #[test]
    fn replaced_directory_is_not_the_prepared_admission() {
        let files = LaunchFiles::new();
        let prepared = prepare(&files.admission(), files.spec()).unwrap();
        let old = files.0.with_extension("old");
        std::fs::rename(&files.0, &old).unwrap();
        std::fs::create_dir(&files.0).unwrap();
        assert_eq!(
            files.fail_start(files.admission(), prepared).disposition,
            StartDisposition::NotDispatched
        );
        assert!(!files.claimed());
        std::fs::remove_dir_all(old).unwrap();
    }
    struct Fragmented {
        bytes: Vec<u8>,
        at: usize,
        pause: bool,
    }
    impl std::io::Read for Fragmented {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let available = self.fill_buf()?;
            let n = available.len().min(out.len());
            out[..n].copy_from_slice(&available[..n]);
            self.consume(n);
            Ok(n)
        }
    }
    impl BufRead for Fragmented {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            if self.pause {
                self.pause = false;
                return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
            }
            Ok(&self.bytes[self.at..(self.at + 1).min(self.bytes.len())])
        }
        fn consume(&mut self, n: usize) {
            self.at += n;
            self.pause = true;
        }
    }
    fn next_fragment(framing: &mut Framing, reader: &mut Fragmented) -> protocol::Message {
        loop {
            match framing.read(reader) {
                Ok(Some(message)) => return message,
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
                other => panic!("unexpected fragment result {other:?}"),
            }
        }
    }
    #[test]
    fn fragmented_binary_events_and_controls_survive_every_byte_timeout() {
        let mut encoder = moruna_event::Encoder::new();
        let record = encoder
            .encode(
                &moruna_event::Stamp {
                    run: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    plan: None,
                    attempt: 1,
                    start: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                },
                1,
                0,
                0,
                &moruna_event::Event::AttemptStarted {
                    resumed: false,
                    plan: None,
                },
            )
            .unwrap()
            .to_vec();
        let mut bytes = Outbound::Hello {
            moruna_version: env!("CARGO_PKG_VERSION").into(),
            spec_digest: None,
            limits: None,
        }
        .line()
        .into_bytes();
        moruna_event::frame::write_frame(&mut bytes, &mut Vec::new(), &record).unwrap();
        bytes.extend_from_slice(b"{\"type\":\"exit\",\"code\":0,\"diagnostic\":null}\n");
        let mut reader = Fragmented {
            bytes,
            at: 0,
            pause: false,
        };
        let mut framing = Framing::default();
        let protocol::Message::Line(hello) = next_fragment(&mut framing, &mut reader) else {
            panic!("hello lost")
        };
        assert!(matches!(
            serde_json::from_str::<Outbound>(&hello).unwrap(),
            Outbound::Hello { .. }
        ));
        let protocol::Message::Event(actual) = next_fragment(&mut framing, &mut reader) else {
            panic!("event lost")
        };
        assert_eq!(actual, record);
        let event = moruna_event::decode(&actual).unwrap();
        let mut correlation = Correlation::default();
        correlation.check(&event).unwrap();
        assert!(correlation.check(&event).is_err());
        let protocol::Message::Line(exit) = next_fragment(&mut framing, &mut reader) else {
            panic!("control lost")
        };
        assert!(matches!(
            serde_json::from_str::<Outbound>(&exit).unwrap(),
            Outbound::Exit { code: 0, .. }
        ));
    }
    #[test]
    fn truncated_and_oversized_frames_refuse_without_losing_prefix() {
        for bytes in [
            vec![moruna_event::frame::FRAME],
            vec![moruna_event::frame::FRAME, 1, 0],
            vec![moruna_event::frame::FRAME, 255, 255, 255, 255],
        ] {
            let mut framing = Framing::default();
            assert!(framing.read(&mut std::io::Cursor::new(bytes)).is_err());
        }
        let mut framing = Framing::default();
        assert!(
            framing
                .read(&mut std::io::Cursor::new(b"{partial".to_vec()))
                .is_err()
        );
    }

    #[test]
    fn duplex_control_uses_canonical_cancel_checkpoint_and_port_reply() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let control = Control {
            stream: Arc::new(Mutex::new(sender)),
            deadline: Instant::now() + Duration::from_secs(5),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let mut receiver = BufReader::new(receiver);
        for message in [
            Inbound::Cancel,
            Inbound::Checkpoint,
            Inbound::PortReply {
                v: protocol::PORTS_VERSION,
                call: 7,
                ok: Some("YWJj".into()),
                error: None,
            },
        ] {
            control.send(&message).unwrap();
            let mut line = String::new();
            receiver.read_line(&mut line).unwrap();
            assert_eq!(Inbound::parse(&line).unwrap(), message);
        }
    }
    #[test]
    fn canonical_report_and_port_events_are_not_limited_to_one_mib() {
        let report = Outbound::Report {
            report: serde_json::json!({"proof": "x".repeat(2 * 1024 * 1024)}),
            plan: None,
        };
        let encoded = report.line();
        assert!(encoded.len() < protocol::MAX_PORT_LINE);
        let decoded: Outbound = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, report);
        let call = Outbound::PortCall {
            v: protocol::PORTS_VERSION,
            call: 1,
            stage: 1,
            port: "input".into(),
            deadline_ms: 500,
            request: "x".repeat(2 * 1024 * 1024),
        };
        assert_eq!(
            serde_json::from_str::<Outbound>(&call.line()).unwrap(),
            call
        );
    }
}
