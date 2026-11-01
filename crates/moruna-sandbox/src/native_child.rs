//! Actual native PEX child state machine. Shared package/factory loading with the
//! Linux sandbox; canonical logical controls over the native checked record plane.
use crate::{
    native_wire::{self, Command, Response, VERSION},
    protocol::{AllocDoc, FromChild, ToChild},
};
use moruna_adapters::{PyKernel, guard::MemoryGate};
use moruna_kernel::arrow::{buffer::Buffer, ipc::writer::StreamWriter, record_batch::RecordBatch};
use moruna_kernel::{
    Allocator, InitCtx, Kernel, KernelState, MorunaError, NoState, Payload, Sampler,
};
use moruna_package::{Inputs, Package};
use pyo3::prelude::*;
use std::{path::Path, sync::Arc};
fn failed(why: impl Into<String>) -> MorunaError {
    MorunaError::Plan(format!("native PEX child: {}", why.into()))
}
struct Loaded {
    state: Box<dyn KernelState>,
    kernel: PyKernel,
    ctx: InitCtx,
    _package: Package,
    _tree: moruna_package::Tree,
}
/// One real child process; owns its extracted tree/callable/state until actual exit.
pub struct NativeRunner {
    alloc: Arc<dyn Allocator>,
    sampler: Arc<dyn Sampler>,
    limit: u64,
    loaded: Option<Loaded>,
    last: u64,
    stopped: bool,
}
impl NativeRunner {
    /// Bind only the actual child's charged allocator and own-account sampler.
    pub fn new(alloc: Arc<dyn Allocator>, sampler: Arc<dyn Sampler>, limit: u64) -> Self {
        Self {
            alloc,
            sampler,
            limit,
            loaded: None,
            last: 0,
            stopped: false,
        }
    }
    fn load(
        &mut self,
        body: Vec<u8>,
        fingerprint: &str,
        args: Option<serde_json::Value>,
        assets: Option<String>,
    ) -> moruna_kernel::Result<()> {
        if self.loaded.is_some() {
            return Err(failed("package already loaded"));
        }
        let package = Package::from_bytes(
            Path::new("native-snapshot.pex"),
            moruna_package::Format::Pex,
            body,
        )
        .map_err(MorunaError::from)?;
        if package.fingerprint_string() != fingerprint {
            return Err(failed(
                "snapshot differs from validated package fingerprint",
            ));
        }
        let inputs = Inputs {
            args,
            assets: assets.map(Into::into),
        };
        inputs.check(&package.manifest).map_err(MorunaError::from)?;
        // The image filesystem is readonly; unlike a host sandbox, this adapter cannot
        // conjure an arbitrary host directory mount. Never ignore a requested asset root.
        if inputs
            .assets
            .as_deref()
            .is_some_and(|path| path != Path::new(crate::confine::ASSETS))
        {
            return Err(failed(
                "native assets require an actual readonly /assets directory in the granted image",
            ));
        }
        let tree = moruna_package::private_tree(&package).map_err(MorunaError::from)?;
        let (kernel, state, ctx) = Python::attach(|py| -> moruna_kernel::Result<_> {
            let disabled = py
                .import("sysconfig")
                .and_then(|module| module.call_method1("get_config_var", ("Py_GIL_DISABLED",)))
                .and_then(|value| value.extract::<Option<u32>>())
                .map_err(|error| failed(error.to_string()))?;
            if disabled == Some(1) {
                return Err(failed(
                    "a PEX requires the actual standard-build interpreter, not cp3NNt",
                ));
            }
            let sandbox = py
                .import("moruna._sandbox")
                .map_err(|e| failed(e.to_string()))?;
            let libraries = sandbox
                .call_method0("_preload")
                .and_then(|value| value.extract::<std::collections::BTreeMap<String, String>>())
                .map_err(|e| failed(e.to_string()))?;
            let python = py.import("platform").map_err(|e| failed(e.to_string()))?;
            let version = python
                .call_method0("python_version")
                .and_then(|value| value.extract::<String>())
                .map_err(|e| failed(e.to_string()))?;
            let machine = python
                .call_method0("machine")
                .and_then(|value| value.extract::<String>())
                .map_err(|e| failed(e.to_string()))?;
            let glibc = sandbox
                .call_method0("_glibc")
                .and_then(|value| value.extract::<Option<Vec<u32>>>())
                .map_err(|e| failed(e.to_string()))?;
            let glibc = match glibc {
                Some(values) if values.len() == 2 => Some((values[0], values[1])),
                None => None,
                _ => return Err(failed("invalid actual glibc metadata")),
            };
            let hello = crate::template::Hello {
                pid: std::process::id(),
                python: version,
                libraries,
                glibc,
                machine,
            };
            let target = moruna_package::host::choose(
                &package.manifest.platforms,
                &hello.target().map_err(failed)?,
            )
            .map_err(MorunaError::from)?
            .to_owned();
            let deps = tree.deps(&target);
            crate::python::conflicts(&deps, &hello.libraries).map_err(failed)?;
            let kernel = crate::python::load_at(
                py,
                &tree.root,
                &deps,
                &package.manifest.entry,
                &package.manifest,
                inputs.args.as_ref(),
            )
            .map_err(failed)?;
            kernel.bind_gate(MemoryGate::new(self.sampler.clone(), self.limit)?);
            kernel.bind_allocator(self.alloc.clone());
            let ctx = InitCtx {
                instance: 0,
                device: None,
                alloc: self.alloc.clone(),
            };
            let state = if package.manifest.hints().stateful {
                kernel.new_state(&ctx)?
            } else {
                Box::new(NoState) as Box<dyn KernelState>
            };
            Ok((kernel, state, ctx))
        })?;
        self.loaded = Some(Loaded {
            _package: package,
            _tree: tree,
            kernel,
            state,
            ctx,
        });
        Ok(())
    }
    fn execute(
        &mut self,
        command: ToChild,
        body: Vec<u8>,
    ) -> moruna_kernel::Result<(FromChild, Vec<u8>)> {
        let loaded = self
            .loaded
            .as_mut()
            .ok_or_else(|| failed("control before package load"))?;
        match command {
            ToChild::Call {
                lease: _,
                offset,
                len,
                out_offset,
                out_len,
            } => {
                if offset != 0 || out_offset != 0 || len != body.len() as u64 {
                    return Err(failed("call range differs from actual body"));
                }
                let batches = moruna_kernel::ipc::read_stream(
                    &Buffer::from(body),
                    &moruna_kernel::ipc::Inflate::share(self.alloc.as_ref()),
                )
                .map_err(|diagnostic| failed(format!("invalid call Arrow IPC: {diagnostic}")))?;
                if batches.len() != 1 {
                    return Err(failed("one native call must carry exactly one real batch"));
                }
                let batch = batches
                    .into_iter()
                    .next()
                    .ok_or_else(|| failed("batch absent"))?;
                let output = loaded
                    .kernel
                    .apply(loaded.state.as_mut(), Payload::table(batch)?)?;
                let Payload::Table(batch, _) = output else {
                    return Err(failed("package returned tensor to table protocol"));
                };
                let mut encoded = Vec::new();
                {
                    let mut writer = StreamWriter::try_new(&mut encoded, &batch.schema())
                        .map_err(|e| failed(e.to_string()))?;
                    writer.write(&batch).map_err(|e| failed(e.to_string()))?;
                    writer.finish().map_err(|e| failed(e.to_string()))?;
                }
                if encoded.len() as u64 > out_len {
                    return Err(failed(format!(
                        "actual output {} exceeds admitted body {out_len}",
                        encoded.len()
                    )));
                }
                let alloc = AllocDoc::from(&moruna_kernel::take_kernel_alloc());
                Ok((
                    FromChild::Done {
                        offset: 0,
                        len: encoded.len() as u64,
                        alloc,
                        released: Vec::new(),
                    },
                    encoded,
                ))
            }
            ToChild::Open { offset, len } => {
                if offset != 0 || len != body.len() as u64 {
                    return Err(failed("restore range differs from actual state"));
                }
                loaded.state = loaded.kernel.restore(&loaded.ctx, &body)?;
                Ok((FromChild::Opened, Vec::new()))
            }
            ToChild::Checkpoint { offset, len } => {
                if offset != 0 || !body.is_empty() {
                    return Err(failed("checkpoint request has unexpected bytes/range"));
                }
                let state = loaded
                    .state
                    .checkpoint()?
                    .ok_or_else(|| failed("package declared checkpoint without actual state"))?;
                if state.len() as u64 > len {
                    return Err(failed("actual checkpoint exceeds admitted body"));
                }
                Ok((
                    FromChild::State {
                        offset: 0,
                        len: state.len() as u64,
                        released: Vec::new(),
                    },
                    state,
                ))
            }
            ToChild::Release => {
                Python::attach(|py| {
                    py.import("gc")
                        .and_then(|gc| gc.call_method0("collect"))
                        .map(|_| ())
                })
                .map_err(|e| failed(e.to_string()))?;
                Ok((
                    FromChild::Released {
                        released: Vec::new(),
                    },
                    Vec::new(),
                ))
            }
            ToChild::Stop => {
                loaded.kernel.finish(loaded.state.as_mut())?;
                self.stopped = true;
                Ok((
                    FromChild::Released {
                        released: Vec::new(),
                    },
                    Vec::new(),
                ))
            }
        }
    }
    /// Whether the real Stop command completed before the plane closed.
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Execute one actual canonical request; errors remain typed child errors, never
    /// empty successful output or partial memory measurements.
    pub fn handle(&mut self, batch: RecordBatch) -> moruna_kernel::Result<RecordBatch> {
        let (request, body) = native_wire::read_request(&batch).map_err(failed)?;
        if self.stopped
            || request.seq
                != self
                    .last
                    .checked_add(1)
                    .ok_or_else(|| failed("sequence exhausted"))?
        {
            return Err(failed("out-of-order command or command after stop"));
        }
        self.last = request.seq;
        // Clear previous-call counts; only the actual current adapter call may supply them.
        let _ = moruna_kernel::take_kernel_alloc();
        let output = Arc::new(crate::native_output::Capture::default());
        let sink: Arc<dyn moruna_kernel::OutputSink> = output.clone();
        let _output_context = moruna_kernel::output::enter(&sink, 0);
        let mut observed_counts = None;
        let result = (|| {
            let reply = match request.command {
                Command::Load {
                    fingerprint,
                    args,
                    assets,
                } => {
                    self.load(body, &fingerprint, args, assets)?;
                    (FromChild::Ready, Vec::new())
                }
                Command::Execute { command } => self.execute(command, body)?,
            };
            if let FromChild::Done { alloc, .. } | FromChild::Error { alloc, .. } = &reply.0 {
                observed_counts = Some(*alloc);
            }
            self.sampler.sample()?;
            let state_bytes = self
                .loaded
                .as_ref()
                .map(|loaded| loaded.state.footprint())
                .transpose()?
                .flatten();
            Ok::<_, MorunaError>((reply, state_bytes))
        })();
        let (reply, body, state_bytes) = match result {
            Ok(((reply, body), state_bytes)) => (reply, body, state_bytes),
            Err(error) => (
                FromChild::Error {
                    message: error.to_string(),
                    refusal: match &error {
                        MorunaError::Refused {
                            requested,
                            in_use,
                            ceiling,
                            ..
                        } => Some(crate::protocol::RefusalDoc {
                            requested: *requested,
                            in_use: *in_use,
                            ceiling: *ceiling,
                        }),
                        _ => None,
                    },
                    alloc: observed_counts
                        .unwrap_or_else(|| AllocDoc::from(&moruna_kernel::take_kernel_alloc())),
                    released: Vec::new(),
                },
                Vec::new(),
                None,
            ),
        };
        native_wire::response(
            &Response {
                v: VERSION,
                seq: request.seq,
                reply,
                state_bytes,
                logs: output.finish().map_err(failed)?,
            },
            &body,
        )
        .map_err(failed)
    }
}
