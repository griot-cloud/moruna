//! Moruna component 17, the `.wasm` half: a function package's component in wasmtime, inside
//! Moruna's process and without Moruna's access to the machine (FP 4).
//!
//! Design: `architecture/sdd/17-sandbox.md`. One engine per process (f.1), a component verified
//! against the `moruna:kernel` world with no imports at all (f.2, SB-I1), compiled or read from
//! its `.cwasm` (f.3), and one instance per stage whose every memory growth passes the resource
//! limiter, which is where Moruna refuses (SB-I2), and whose every call has fuel (SB-I3). Batches
//! cross as Arrow IPC streams (16 e.5) and the output is landed into the arena once (05 f.3). A
//! component reaches outside its memory through two imports only: `read-asset` and the stage's
//! ports through `call` (MH 4.10).

#![deny(missing_docs)]
// `cwasm.rs` deserialises a precompiled component, which wasmtime marks unsafe; nothing else is.
#![deny(unsafe_op_in_unsafe_fn)]
// Every fallible call returns the contract's `MorunaError`, whose size is the contract's.
#![allow(clippy::result_large_err)]

mod calls;
pub mod cwasm;
mod ipc;
pub(crate) mod limiter;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use moruna_adapters::guard::MemoryGate;
use moruna_adapters::{UNKNOWN_SEQ, UNKNOWN_STAGE};
use moruna_kernel::declare::Declared;
use moruna_kernel::{
    Allocator, Fingerprint, InitCtx, Kernel, KernelAlloc, KernelHints, KernelKind, KernelState,
    MorunaError, Payload, PayloadKind, PayloadSpec, Result, ResumePolicy, SourceSchema, StagePorts,
    TierPref, set_kernel_alloc,
};
use moruna_package::{Format, Inputs, Manifest, Package};
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, Func, Instance, Linker, Type};
use wasmtime::{Config, Engine, OptLevel, Store, Trap};

pub use calls::CALL;
pub use cwasm::aot;
use limiter::Limits;

/// Fuel every call has before its input is counted (17 i, SB-I3).
pub const FUEL_FLOOR: u64 = 20_000_000_000;
/// Fuel a call has per byte of its input (17 i, SB-I3).
pub const FUEL_PER_INPUT_BYTE: u64 = 100_000;
/// A component's table elements (17 i).
pub const TABLE_ELEMENTS_MAX: usize = 1_000_000;
/// A component's core instances (17 i).
pub const INSTANCES_MAX: usize = 64;
/// A component's memories (17 i).
pub const MEMORIES_MAX: usize = 16;
/// The most a wasm32 linear memory can be: the limit when no gate is bound.
pub const WASM32_MAX_BYTES: u64 = 1 << 32;

/// The exports of the world (16 e.5).
const EXPORTS: [&str; 5] = ["declare", "process", "open", "close", "configure"];

/// The world's import of an asset (16 e.5).
pub const READ_ASSET: &str = "read-asset";

/// The world's imports: an asset, and a port (MH 4.10).
const IMPORTS: [&str; 2] = [READ_ASSET, CALL];

static ENGINE: OnceLock<std::result::Result<Engine, String>> = OnceLock::new();

/// The one engine of this process (f.1).
pub fn engine() -> Result<&'static Engine> {
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config
                .wasm_component_model(true)
                .consume_fuel(true)
                .cranelift_opt_level(OptLevel::Speed)
                .wasm_memory64(false);
            Engine::new(&config).map_err(|e| format!("{e}"))
        })
        .as_ref()
        .map_err(|e| MorunaError::Config {
            name: "wasm",
            msg: format!("the wasm engine cannot be built: {e}"),
        })
}

fn refused(field: &str, reason: impl Into<String>) -> MorunaError {
    MorunaError::Config {
        name: "package",
        msg: format!("package: {field}: {}", reason.into()),
    }
}

/// A component compiled for this engine, and what a run should say about how.
pub struct Compiled {
    /// The component.
    pub component: Component,
    /// The compile note, when the `.cwasm` was not used (f.3).
    pub notes: Vec<String>,
}

/// The package's component: its `.cwasm` when that matches, else compiled from the package's
/// bytes (f.3).
pub fn compile(package: &Package) -> Result<Compiled> {
    if package.format != Format::Wasm {
        return Err(refused("format", "only a .wasm package is a component"));
    }
    let engine = engine()?;
    let reason = match cwasm::load(engine, package) {
        Ok(component) => {
            return Ok(Compiled {
                component,
                notes: Vec::new(),
            });
        }
        Err(reason) => reason,
    };
    let component = Component::new(engine, package.bytes())
        .map_err(|e| refused("wasm", format!("not a valid component: {e}")))?;
    Ok(Compiled {
        component,
        notes: vec![format!("{}: compiled ({reason})", package.path.display())],
    })
}

fn type_name(ty: &Type) -> String {
    match ty {
        Type::U8 => "u8".into(),
        Type::String => "string".into(),
        Type::List(list) => format!("list<{}>", type_name(&list.ty())),
        Type::Result(r) => format!(
            "result<{}, {}>",
            r.ok().map_or("_".into(), |t| type_name(&t)),
            r.err().map_or("_".into(), |t| type_name(&t))
        ),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// The signature of an exported function as text: `(<params>) -> <result>`.
fn signature(f: &wasmtime::component::types::ComponentFunc) -> String {
    let params: Vec<String> = f.params().map(|(_, t)| type_name(&t)).collect();
    let results: Vec<String> = f.results().map(|t| type_name(&t)).collect();
    format!("({}) -> {}", params.join(", "), results.join(", "))
}

/// The world's signatures (16 e.5).
fn expected(name: &str) -> &'static str {
    match name {
        "declare" => "() -> string",
        "process" => "(list<u8>) -> result<list<u8>, string>",
        "open" => "(list<u8>) -> result<_, string>",
        "configure" => "(string) -> result<_, string>",
        "read-asset" => "(string) -> result<list<u8>, string>",
        "call" => "(string, list<u8>) -> result<list<u8>, string>",
        _ => "() -> list<u8>",
    }
}

/// f.2: no import but `read-asset` and `call`; exactly `declare` and `process`, `open` and `close` when
/// `stateful`, `configure` when built by `args`; each with the world's type (16 PK-I4).
pub fn verify(component: &Component, stateful: bool, args: bool) -> Result<()> {
    let engine = engine()?;
    let ty = component.component_type();
    for (name, item) in ty.imports(engine) {
        let refused_import = || {
            refused(
                "wasm",
                format!(
                    "the component imports `{name}`; a moruna:kernel component imports \
                     `{READ_ASSET}` and `{CALL}` and nothing else"
                ),
            )
        };
        let ComponentItem::ComponentFunc(f) = item.ty else {
            return Err(refused_import());
        };
        if !IMPORTS.contains(&name) {
            return Err(refused_import());
        }
        let found = signature(&f);
        if found != expected(name) {
            return Err(refused(
                "wasm",
                format!(
                    "`{name}` is imported as `{found}` and the world's is `{}`",
                    expected(name)
                ),
            ));
        }
    }
    let mut seen = Vec::new();
    for (name, item) in ty.exports(engine) {
        if !EXPORTS.contains(&name) {
            return Err(refused(
                "wasm",
                format!(
                    "the component exports `{name}`, which is not in the moruna:kernel world \
                     (declare, process, open, close, configure)"
                ),
            ));
        }
        let ComponentItem::ComponentFunc(f) = item.ty else {
            return Err(refused(
                "wasm",
                format!("`{name}` is exported, and is not a function"),
            ));
        };
        let found = signature(&f);
        if found != expected(name) {
            return Err(refused(
                "wasm",
                format!(
                    "`{name}` is `{found}` and the world's is `{}`",
                    expected(name)
                ),
            ));
        }
        seen.push(name.to_string());
    }
    for required in ["declare", "process"] {
        if !seen.iter().any(|s| s == required) {
            return Err(refused(
                "wasm",
                format!("the component does not export `{required}`"),
            ));
        }
    }
    let configure = seen.iter().any(|s| s == "configure");
    if configure != args {
        return Err(refused(
            "wasm",
            if args {
                "the manifest says args and the component does not export `configure`"
            } else {
                "the component exports `configure` and the manifest does not say args"
            },
        ));
    }
    let open = seen.iter().any(|s| s == "open");
    let close = seen.iter().any(|s| s == "close");
    match (stateful, open, close) {
        (true, true, true) | (false, false, false) => Ok(()),
        (_, true, false) => Err(refused(
            "wasm",
            "the component exports `open` without `close`",
        )),
        (_, false, true) => Err(refused(
            "wasm",
            "the component exports `close` without `open`",
        )),
        (true, false, false) => Err(refused(
            "wasm",
            "the manifest says stateful and the component exports neither `open` nor `close`",
        )),
        (false, true, true) => Err(refused(
            "wasm",
            "the component exports `open` and `close` and the manifest does not say stateful",
        )),
    }
}

/// A store for one instance of `component`, with the limiter installed and the world's two
/// imports defined: `read-asset` and `call`.
fn instantiate(component: &Component, limits: Limits) -> Result<(Store<Limits>, Instance)> {
    let engine = engine()?;
    let mut store = Store::new(engine, limits);
    store.limiter(|limits| limits);
    store
        .set_fuel(FUEL_FLOOR)
        .map_err(|e| kernel_error(format!("wasm: setting fuel: {e}")))?;
    let mut linker: Linker<Limits> = Linker::new(engine);
    linker
        .root()
        .func_wrap(
            READ_ASSET,
            |store: wasmtime::StoreContextMut<'_, Limits>, (name,): (String,)| {
                Ok((store.data().read_asset(&name),))
            },
        )
        .map_err(|e| kernel_error(format!("wasm: defining `{READ_ASSET}`: {e}")))?;
    linker
        .root()
        .func_wrap(
            CALL,
            |store: wasmtime::StoreContextMut<'_, Limits>, (port, request): (String, Vec<u8>)| {
                calls::call(store, port, request)
            },
        )
        .map_err(|e| kernel_error(format!("wasm: defining `{CALL}`: {e}")))?;
    let instance = linker
        .instantiate(&mut store, component)
        .map_err(|e| kernel_error(format!("wasm: instantiating the component: {e}")))?;
    Ok((store, instance))
}

/// `declare()` on a fresh instance with no imports: the manifest the component says it has.
pub fn declare(component: &Component) -> Result<String> {
    let (mut store, instance) = instantiate(component, Limits::unbound())?;
    let f = instance
        .get_typed_func::<(), (String,)>(&mut store, "declare")
        .map_err(|e| refused("wasm", format!("`declare`: {e}")))?;
    let (text,) = f
        .call(&mut store, ())
        .map_err(|e| refused("wasm", format!("`declare` trapped: {e}")))?;
    Ok(text)
}

/// 16 f.5 and PK-I4: `declare()` agrees with `manifest` on every field but `moruna`.
pub fn agrees(component: &Component, manifest: &Manifest) -> Result<()> {
    let text = declare(component)?;
    let said = Manifest::parse(text.as_bytes())
        .map_err(|e| refused("declare", format!("the component's declare(): {e}")))?;
    let fields: [(&str, bool); 6] = [
        ("name", said.name == manifest.name),
        ("entry", said.entry == manifest.entry),
        ("declaration", said.declaration == manifest.declaration),
        ("abi", said.abi == manifest.abi),
        ("format", said.format == manifest.format),
        ("platforms", said.platforms == manifest.platforms),
    ];
    if let Some((field, _)) = fields.iter().find(|(_, same)| !same) {
        return Err(refused(
            field,
            "the component's declare() and the manifest disagree",
        ));
    }
    Ok(())
}

/// `moruna package --wasm` (16 f.5): the component at `module` verified against the world and
/// against `manifest`, with the manifest embedded, as the bytes of `<name>.wasm`.
pub fn package(module: &[u8], manifest: &Manifest) -> Result<Vec<u8>> {
    let engine = engine()?;
    let component = Component::new(engine, module)
        .map_err(|e| refused("wasm", format!("not a valid component: {e}")))?;
    verify(&component, manifest.hints().stateful, manifest.hints().args)?;
    agrees(&component, manifest)?;
    moruna_package::embed_manifest(module, manifest).map_err(MorunaError::from)
}

fn kernel_error(msg: String) -> MorunaError {
    MorunaError::Kernel {
        stage: UNKNOWN_STAGE,
        seq: UNKNOWN_SEQ,
        msg,
        features: None,
    }
}

/// A `.wasm` package as a kernel (17 d.1, f.4).
pub struct WasmKernel {
    package: Arc<Package>,
    component: Component,
    stateful: bool,
    hints: KernelHints,
    declared: Declared,
    notes: Vec<String>,
    gate: OnceLock<Arc<MemoryGate>>,
    inputs: Inputs,
    /// The stage's ports (MH 4.10), bound by the build of a run.
    ports: OnceLock<Arc<StagePorts>>,
    /// The entry states of the instances waiting on a port now (MH 4.10).
    waiting: Arc<Mutex<BTreeMap<usize, Vec<u8>>>>,
}

impl std::fmt::Debug for WasmKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmKernel")
            .field("package", &self.package)
            .field("stateful", &self.stateful)
            .finish()
    }
}

impl WasmKernel {
    /// The package's component, compiled or read from its `.cwasm`, verified against the world
    /// and against its embedded manifest (f.2, f.3).
    pub fn new(package: Arc<Package>, inputs: Inputs) -> Result<WasmKernel> {
        let manifest = &package.manifest;
        inputs.check(manifest).map_err(MorunaError::from)?;
        let Compiled { component, notes } = compile(&package)?;
        let stateful = manifest.hints().stateful;
        verify(&component, stateful, manifest.hints().args)?;
        agrees(&component, manifest)?;
        let declared = manifest.declared().map_err(MorunaError::from)?;
        let h = manifest.hints();
        let hints = KernelHints {
            expected_amplification: h.expected_amplification,
            uses_device_memory: false,
            releases_gil: None,
            preferred_rows: h.preferred_rows,
            resume: if stateful {
                ResumePolicy::Checkpoint
            } else {
                ResumePolicy::Reinit
            },
            state_bytes: h.state_bytes,
        };
        Ok(WasmKernel {
            package,
            component,
            stateful,
            hints,
            declared,
            notes,
            gate: OnceLock::new(),
            inputs,
            ports: OnceLock::new(),
            waiting: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// What a run should say about this kernel (a compile note, f.3).
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// The package this kernel runs.
    pub fn package(&self) -> &Arc<Package> {
        &self.package
    }

    /// The run's gate (05 f.9): growth it refuses is refused, when the manifest leaves
    /// `memory_guard` on; a second call is ignored.
    pub fn bind_gate(&self, gate: Arc<MemoryGate>) {
        let _ = self.gate.set(gate);
    }

    /// The stage's ports (MH 4.10): the names its entry lists, where each goes, and the stage
    /// it calls from; a second binding is ignored. A kernel with none bound calls no port.
    pub fn bind_ports(&self, ports: Arc<StagePorts>) {
        let _ = self.ports.set(ports);
    }

    /// The stage's ports, once bound.
    pub fn ports(&self) -> Option<&Arc<StagePorts>> {
        self.ports.get()
    }

    fn limits(&self, instance: usize) -> Limits {
        let calls = self.ports.get().map(|ports| calls::Calls {
            ports: Arc::clone(ports),
            instance,
            entry: None,
            waiting: Arc::clone(&self.waiting),
        });
        Limits::new(
            self.gate.get().cloned(),
            self.package.manifest.hints().memory_guard,
        )
        .with_assets(self.inputs.assets.clone())
        .with_calls(calls)
    }

    fn new_state(
        &self,
        alloc: Arc<dyn Allocator>,
        state: Option<&[u8]>,
        instance: usize,
    ) -> Result<WasmState> {
        let (mut store, instance_handle) = instantiate(&self.component, self.limits(instance))?;
        let process = func(&instance_handle, &mut store, "process")?;
        // A stateful stage that lists ports keeps its state as each `process` begins, so a
        // checkpoint taken while it waits has one (MH 4.10).
        let keeps_entry = self.stateful && self.ports.get().is_some_and(|p| !p.is_empty());
        let mut st = WasmState {
            store,
            instance: instance_handle,
            process,
            alloc,
            name: self.package.manifest.name.clone(),
            stateful: self.stateful,
            keeps_entry,
        };
        if self.package.manifest.hints().args {
            st.configure(&self.inputs.args_json())?;
        }
        if self.stateful {
            st.open(state.unwrap_or(&[]))?;
        }
        Ok(st)
    }
}

fn func(instance: &Instance, store: &mut Store<Limits>, name: &str) -> Result<Func> {
    instance
        .get_func(&mut *store, name)
        .ok_or_else(|| kernel_error(format!("wasm: the instance has no `{name}`")))
}

/// One instance and its store (e.1): the stage's state.
pub struct WasmState {
    store: Store<Limits>,
    instance: Instance,
    process: Func,
    alloc: Arc<dyn Allocator>,
    name: String,
    stateful: bool,
    keeps_entry: bool,
}

impl WasmState {
    /// The call's fuel (SB-I3).
    fn fuel(bytes_in: u64) -> u64 {
        FUEL_FLOOR.saturating_add(FUEL_PER_INPUT_BYTE.saturating_mul(bytes_in))
    }

    /// What a trap or a call error is (f.4): a refusal when the limiter refused during the call,
    /// fuel, or the trap as wasmtime said it.
    fn failure(&mut self, error: wasmtime::Error) -> MorunaError {
        if let Some(trap) = error.downcast_ref::<calls::CallTrap>() {
            return trap.0.clone().into_error(UNKNOWN_STAGE);
        }
        if let Some(refusal) = self.store.data().refusal() {
            return MorunaError::Refused {
                stage: UNKNOWN_STAGE,
                seq: UNKNOWN_SEQ,
                kernel: self.name.clone(),
                requested: refusal.requested,
                in_use: refusal.in_use,
                ceiling: refusal.ceiling,
                features: None,
            };
        }
        if matches!(error.downcast_ref::<Trap>(), Some(Trap::OutOfFuel)) {
            return kernel_error(
                "wasm: fuel exhausted: the call ran past its compute budget (17 SB-I3)".into(),
            );
        }
        kernel_error(format!("wasm: {error:?}"))
    }

    fn begin(&mut self, bytes_in: u64) -> Result<()> {
        self.store.data_mut().begin_call();
        self.store
            .set_fuel(Self::fuel(bytes_in))
            .map_err(|e| kernel_error(format!("wasm: setting fuel: {e}")))
    }

    fn end(&mut self) -> KernelAlloc {
        let limits = self.store.data_mut();
        let linear = limits.end_call();
        KernelAlloc {
            measured: true,
            refusal_on: limits.refusal_on(),
            linear,
            ..KernelAlloc::NONE
        }
    }

    fn process(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if self.keeps_entry {
            let state = self.close()?;
            self.open(&state)?;
            if let Some(calls) = self.store.data_mut().calls.as_mut() {
                calls.entry = Some(state);
            }
        }
        let out = self.process_once(input);
        if let Some(calls) = self.store.data_mut().calls.as_mut() {
            calls.entry = None;
        }
        out
    }

    fn process_once(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        self.begin(input.len() as u64)?;
        let typed = self
            .process
            .typed::<(&[u8],), (std::result::Result<Vec<u8>, String>,)>(&self.store)
            .map_err(|e| kernel_error(format!("wasm: `process`: {e}")));
        let called = typed.and_then(|f| {
            f.call(&mut self.store, (input,))
                .map_err(|e| self.failure(e))
        });
        set_kernel_alloc(self.end());
        match called? {
            (Ok(out),) => Ok(out),
            (Err(said),) => Err(kernel_error(said)),
        }
    }

    fn open(&mut self, state: &[u8]) -> Result<()> {
        self.begin(state.len() as u64)?;
        let f = func(&self.instance, &mut self.store, "open")?
            .typed::<(&[u8],), (std::result::Result<(), String>,)>(&self.store)
            .map_err(|e| kernel_error(format!("wasm: `open`: {e}")))?;
        let called = f
            .call(&mut self.store, (state,))
            .map_err(|e| self.failure(e));
        self.end();
        match called? {
            (Ok(()),) => Ok(()),
            (Err(said),) => Err(kernel_error(format!("open: {said}"))),
        }
    }

    /// 17 f.11: the arguments, once, before anything else.
    fn configure(&mut self, args: &str) -> Result<()> {
        self.begin(args.len() as u64)?;
        let f = func(&self.instance, &mut self.store, "configure")?
            .typed::<(&str,), (std::result::Result<(), String>,)>(&self.store)
            .map_err(|e| kernel_error(format!("wasm: `configure`: {e}")))?;
        let called = f
            .call(&mut self.store, (args,))
            .map_err(|e| self.failure(e));
        self.end();
        match called? {
            (Ok(()),) => Ok(()),
            (Err(said),) => Err(kernel_error(format!("configure: {said}"))),
        }
    }

    fn close(&mut self) -> Result<Vec<u8>> {
        self.begin(0)?;
        let f = func(&self.instance, &mut self.store, "close")?
            .typed::<(), (Vec<u8>,)>(&self.store)
            .map_err(|e| kernel_error(format!("wasm: `close`: {e}")))?;
        let called = f.call(&mut self.store, ()).map_err(|e| self.failure(e));
        self.end();
        Ok(called?.0)
    }
}

impl KernelState for WasmState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// SB-I12: `close`, then `open` with what it returned, so the instance goes on. A stateless
    /// instance has nothing to save.
    fn checkpoint(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.stateful {
            return Ok(None);
        }
        let state = self.close()?;
        self.open(&state)?;
        Ok(Some(state))
    }

    fn footprint(&self) -> Option<u64> {
        Some(self.store.data().size())
    }
}

impl Kernel for WasmKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.package.kernel_fingerprint()
    }

    /// One instance per stage (e.1).
    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::MIN,
        }
    }

    fn hints(&self) -> KernelHints {
        self.hints.clone()
    }

    fn declared(&self) -> Declared {
        self.declared.clone()
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    /// The input's schema, as for a Python kernel (05 d.1): the component is opaque until it
    /// runs, and the first morsel's output is what the run goes by.
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.accepts().check(input)?;
        Ok(input.clone())
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(self.new_state(
            ctx.alloc.clone(),
            None,
            ctx.instance,
        )?))
    }

    fn restore(&self, ctx: &InitCtx, state: &[u8]) -> Result<Box<dyn KernelState>> {
        if !self.stateful {
            return Err(MorunaError::Resume(format!(
                "{}: a stateless package has no state to restore",
                self.package.manifest.name
            )));
        }
        Ok(Box::new(self.new_state(
            ctx.alloc.clone(),
            Some(state),
            ctx.instance,
        )?))
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let st = state
            .as_any_mut()
            .downcast_mut::<WasmState>()
            .ok_or_else(|| kernel_error("wasm: a state this kernel did not make".into()))?;
        let Payload::Table(batch, _) = input else {
            return Err(MorunaError::Plan(
                "a wasm kernel takes a table; its batches cross as Arrow IPC".into(),
            ));
        };
        let bytes = ipc::write(&batch)?;
        drop(batch);
        let out = st.process(&bytes)?;
        let batch = ipc::read_one(out)?;
        let (payload, _) = moruna_adapters::land::land_table(batch, &st.alloc)?;
        Ok(payload)
    }

    /// MH 4.10: the instance's state as its `process` began, while that `process` waits on a
    /// port.
    fn waiting_state(&self, instance: usize) -> Option<Vec<u8>> {
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&instance)
            .cloned()
    }
}

#[cfg(test)]
mod tests;
