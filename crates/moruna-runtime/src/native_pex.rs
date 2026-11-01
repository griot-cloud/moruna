//! A genuine native PEX process on the existing KD2 Account/record-plane path.
//! Package bytes, logical controls and replies use the sole sandbox codecs.
use moruna_kernel::arrow::{buffer::Buffer, ipc::writer::StreamWriter};
use moruna_kernel::{
    Allocator, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError,
    OutsideMemory, Payload, PayloadKind, PayloadSpec, Result, ResumePolicy, RunPorts, SourceSchema,
    Tier, TierPref,
};
use moruna_package::{Inputs, Package};
use moruna_sandbox::{
    native_wire::{self, Command, Request, VERSION},
    protocol::{FRAME_MAX_BYTES, FromChild, ToChild},
};
use std::sync::Arc;
fn fail(why: impl Into<String>) -> MorunaError {
    MorunaError::Kernel {
        stage: moruna_adapters::UNKNOWN_STAGE,
        seq: moruna_adapters::UNKNOWN_SEQ,
        msg: format!("native PEX: {}", why.into()),
        features: None,
    }
}
/// One package, whose per-instance process owns actual native state.
pub struct NativePex {
    package: Arc<Package>,
    inputs: Inputs,
    task: Arc<moruna_micro::task::ImageTask>,
    hints: KernelHints,
    declared: moruna_kernel::Declared,
}
impl NativePex {
    /// Use ONLY an explicitly named interpreter in the actual attached image.
    /// No cgroup/namespace defaults or host filesystem fallback are selected.
    pub fn new(
        package: Arc<Package>,
        host: &crate::packaged::PackageHost,
        inputs: Inputs,
    ) -> Result<Self> {
        inputs.check(&package.manifest).map_err(MorunaError::from)?;
        if host.sandbox_cgroup.is_some() {
            return Err(fail("a Linux cgroup is not native task authority"));
        }
        let python = host.sandbox_python.as_ref().ok_or_else(|| {
            fail("explicit --sandbox-python in the actual attached image is required")
        })?;
        if !python.is_absolute() || !python.is_file() {
            return Err(fail(
                "explicit interpreter is not an actual absolute image file",
            ));
        }
        let command = vec![
            python.to_string_lossy().into_owned(),
            "-I".into(),
            "-m".into(),
            "moruna._native_pex".into(),
        ];
        let mut task = moruna_micro::task::ImageTask::new(
            &package.manifest.name,
            &package.fingerprint_string(),
            python.to_string_lossy().into_owned(),
            command,
            Vec::new(),
            Vec::new(),
        )
        .map_err(fail)?
        .managed_windows();
        if let Some(assets) = inputs.assets.as_deref() {
            task = task.with_assets(assets).map_err(fail)?;
        }
        let h = package.manifest.hints();
        let hints = KernelHints {
            expected_amplification: h.expected_amplification,
            uses_device_memory: false,
            releases_gil: None,
            preferred_rows: h.preferred_rows,
            resume: match h.resume.as_deref() {
                Some("checkpoint") => ResumePolicy::Checkpoint,
                Some("forbid") => ResumePolicy::Forbid,
                _ => ResumePolicy::Reinit,
            },
            state_bytes: h.state_bytes,
        };
        let declared = package.manifest.declared().map_err(MorunaError::from)?;
        Ok(Self {
            package,
            inputs,
            task: Arc::new(task),
            hints,
            declared,
        })
    }
    /// The same canonical package used for registration/fingerprinting/live identity.
    pub fn package(&self) -> &Arc<Package> {
        &self.package
    }
    /// Actual native child-account reservation.
    pub fn outside(&self) -> Arc<dyn OutsideMemory> {
        self.task.outside()
    }
    /// Actual run ceiling; the task still carves/enforces its own real child Account.
    pub fn bind_run(&self, ceiling: u64) {
        self.task.bind_run(ceiling)
    }
    /// Actual cancellation/wait ownership, independent of whether the package lists ports.
    pub fn bind_run_ports(&self, ports: &RunPorts) {
        self.task.bind_run_ports(ports)
    }
    /// Real adapter facts, including actual capability-backed readonly asset delegation.
    pub fn notes(&self) -> Vec<String> {
        vec!["PEX executes in a real native child Account through checked record controls; no Linux sandbox/cgroup fallback".into()]
    }
    fn state(&self, ctx: &InitCtx, restore: Option<&[u8]>) -> Result<Box<dyn KernelState>> {
        let available = ctx
            .alloc
            .available(Tier::Host)
            .ok_or_else(|| fail("native PEX requires explicit charged arena admission"))?;
        let reply = available / 4;
        if reply <= 2 * FRAME_MAX_BYTES as u64 {
            return Err(fail(
                "no charged room for bounded native PEX reply framing/body",
            ));
        }
        let mut state = NativeState {
            task: self.task.clone(),
            inner: self.task.init(ctx)?,
            alloc: ctx.alloc.clone(),
            seq: 0,
            reply: reply.min(128 << 20),
            footprint: None,
            checkpoint: self.package.manifest.hints().stateful
                && self.hints.resume == ResumePolicy::Checkpoint,
        };
        let result = state.request(
            Command::Load {
                fingerprint: self.package.fingerprint_string(),
                args: self.inputs.args.clone(),
                // This is the actual actor-local mount backed by the transferred
                // RO directory capability, never a pathname-only alias.
                assets: self.inputs.assets.as_ref().map(|_| "/assets".to_owned()),
            },
            self.package.bytes(),
        )?;
        if !matches!(result.0, FromChild::Ready) || !result.1.is_empty() {
            return Err(fail("load did not acknowledge actual package"));
        }
        if let Some(bytes) = restore {
            let result = state.control(
                ToChild::Open {
                    offset: 0,
                    len: bytes.len() as u64,
                },
                bytes,
            )?;
            if !matches!(result.0, FromChild::Opened) || !result.1.is_empty() {
                return Err(fail("restore did not acknowledge actual state"));
            }
        }
        Ok(Box::new(state))
    }
}
struct NativeState {
    task: Arc<moruna_micro::task::ImageTask>,
    inner: Box<dyn KernelState>,
    alloc: Arc<dyn Allocator>,
    seq: u64,
    reply: u64,
    footprint: Option<u64>,
    checkpoint: bool,
}
impl NativeState {
    fn body_limit(&self) -> u64 {
        self.reply - 2 * FRAME_MAX_BYTES as u64
    }
    fn control(&mut self, command: ToChild, body: &[u8]) -> Result<(FromChild, Vec<u8>)> {
        self.request(Command::Execute { command }, body)
    }
    fn request(&mut self, command: Command, body: &[u8]) -> Result<(FromChild, Vec<u8>)> {
        self.seq = self
            .seq
            .checked_add(1)
            .ok_or_else(|| fail("request sequence exhausted"))?;
        let request = native_wire::request(
            &Request {
                v: VERSION,
                seq: self.seq,
                command,
            },
            body,
        )
        .map_err(fail)?;
        let reply =
            self.task
                .exchange(self.inner.as_mut(), Payload::table(request)?, self.reply)?;
        let Payload::Table(reply, _) = reply else {
            return Err(fail("reply is not a record"));
        };
        let (response, body) = native_wire::read_response(&reply).map_err(fail)?;
        if response.seq != self.seq {
            return Err(fail("reply does not belong to original request"));
        }
        if let Some((sink, stage)) = moruna_kernel::output::current() {
            for log in &response.logs {
                match log.stream {
                    1 => sink.bytes(Some(stage), moruna_kernel::Stream::Stdout, &log.bytes),
                    2 => sink.bytes(Some(stage), moruna_kernel::Stream::Stderr, &log.bytes),
                    3 => {
                        use moruna_kernel::Level;
                        let level = log.level.map(|n| match n {
                            0 => Level::Trace,
                            1 => Level::Debug,
                            2 => Level::Info,
                            3 => Level::Warning,
                            4 => Level::Error,
                            _ => Level::Critical,
                        });
                        let text =
                            std::str::from_utf8(&log.bytes).map_err(|e| fail(e.to_string()))?;
                        sink.record(Some(stage), level, text);
                    }
                    _ => return Err(fail("unvalidated native output stream")),
                }
            }
        }
        self.footprint = response.state_bytes;
        match response.reply {
            FromChild::Error {
                message,
                refusal,
                alloc,
                released,
            } => {
                moruna_kernel::set_kernel_alloc((&alloc).into());
                if !released.is_empty() {
                    return Err(fail(
                        "native reply claimed nonexistent shared-mapping leases",
                    ));
                }
                if let Some(refusal) = refusal {
                    return Err(MorunaError::Refused {
                        stage: moruna_adapters::UNKNOWN_STAGE,
                        seq: moruna_adapters::UNKNOWN_SEQ,
                        kernel: "native PEX".into(),
                        requested: refusal.requested,
                        in_use: refusal.in_use,
                        ceiling: refusal.ceiling,
                        features: None,
                    });
                }
                Err(fail(message))
            }
            reply => Ok((reply, body)),
        }
    }
}
impl KernelState for NativeState {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn checkpoint(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.checkpoint {
            return Ok(None);
        }
        let (reply, body) = self.control(
            ToChild::Checkpoint {
                offset: 0,
                len: self.body_limit(),
            },
            &[],
        )?;
        match reply {
            FromChild::State {
                offset: 0,
                len,
                released,
            } if len == body.len() as u64 && released.is_empty() => Ok(Some(body)),
            _ => Err(fail("checkpoint reply differs from actual state body")),
        }
    }
    fn footprint(&self) -> Result<Option<u64>> {
        Ok(self.footprint)
    }
}
impl Kernel for NativePex {
    fn fingerprint(&self) -> Fingerprint {
        self.package.kernel_fingerprint()
    }
    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::MIN,
        }
    }
    fn hints(&self) -> KernelHints {
        self.hints.clone()
    }
    fn declared(&self) -> moruna_kernel::Declared {
        self.declared.clone()
    }
    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.accepts().check(input)?;
        Ok(input.clone())
    }
    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        self.state(ctx, None)
    }
    fn restore(&self, ctx: &InitCtx, bytes: &[u8]) -> Result<Box<dyn KernelState>> {
        if self.hints.resume != ResumePolicy::Checkpoint {
            return Err(MorunaError::Resume(
                "package does not declare checkpoint".into(),
            ));
        }
        self.state(ctx, Some(bytes))
    }
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let state = state
            .as_any_mut()
            .downcast_mut::<NativeState>()
            .ok_or_else(|| fail("foreign kernel state"))?;
        if !Arc::ptr_eq(&state.task, &self.task) {
            return Err(fail("state belongs to another actual native package task"));
        }
        let Payload::Table(batch, _) = input else {
            return Err(fail("PEX takes table records"));
        };
        let mut bytes = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut bytes, &batch.schema())
                .map_err(|e| fail(e.to_string()))?;
            writer.write(&batch).map_err(|e| fail(e.to_string()))?;
            writer.finish().map_err(|e| fail(e.to_string()))?;
        }
        let (reply, body) = state.control(
            ToChild::Call {
                lease: state
                    .seq
                    .checked_add(1)
                    .ok_or_else(|| fail("request sequence exhausted"))?,
                offset: 0,
                len: bytes.len() as u64,
                out_offset: 0,
                out_len: state.body_limit(),
            },
            &bytes,
        )?;
        match reply {
            FromChild::Done {
                offset: 0,
                len,
                alloc,
                released,
            } if len == body.len() as u64 && released.is_empty() => {
                moruna_kernel::set_kernel_alloc((&alloc).into());
                let mut batches = moruna_kernel::ipc::read_stream(
                    &Buffer::from(body),
                    &moruna_kernel::ipc::Inflate::share(state.alloc.as_ref()),
                )
                .map_err(|diagnostic| fail(format!("invalid output Arrow IPC: {diagnostic}")))?;
                if batches.len() != 1 {
                    return Err(fail("PEX returned other than one actual batch"));
                }
                let batch = batches.pop().ok_or_else(|| fail("output absent"))?;
                Ok(moruna_adapters::land::land_table(batch, &state.alloc)?.0)
            }
            _ => Err(fail("call reply differs from actual output body")),
        }
    }
    fn finish(&self, state: &mut dyn KernelState) -> Result<()> {
        let state = state
            .as_any_mut()
            .downcast_mut::<NativeState>()
            .ok_or_else(|| fail("foreign kernel state"))?;
        if !Arc::ptr_eq(&state.task, &self.task) {
            return Err(fail("state belongs to another actual native package task"));
        }
        let (reply, body) = state.control(ToChild::Stop, &[])?;
        if !matches!(reply,FromChild::Released{ref released} if released.is_empty())
            || !body.is_empty()
        {
            return Err(fail("stop was not acknowledged"));
        }
        self.task.finish(state.inner.as_mut())
    }
}
/// Real native child's entrypoint, called only by the Python-enabled SDK module.
#[cfg(feature = "python")]
pub fn child() -> Result<()> {
    let (sampler, limit) = moruna_micro::pex::own_sampler()?;
    let bytes = (limit / 8).min(64 << 20);
    if bytes < 1 << 20 {
        return Err(fail("own Account cannot admit child control arena"));
    }
    let alloc = moruna_arena::Arena::new(moruna_arena::ArenaConfig {
        host_bytes: bytes,
        ..Default::default()
    })?;
    let mut runner = moruna_sandbox::native_child::NativeRunner::new(alloc.clone(), sampler, limit);
    moruna_micro::pex::serve_records(alloc.as_ref(), |batch| runner.handle(batch))?;
    if !runner.stopped() {
        return Err(fail(
            "native plane ended without actual Stop/finish acknowledgement",
        ));
    }
    Ok(())
}
