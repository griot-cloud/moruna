//! Kinds their embedder serves (SDD 18 e.4, KD-I11): one adapter runs every one of them.
//!
//! The program running Moruna declares a kind in the format of every other declaration, with
//! `isolation: embedder` and `served`: the port each batch goes to, and the rules of what the kind
//! takes and gives (`Registry::declare`). Moruna holds none of the kind's code. A task of the kind
//! is a stage whose kernel hands each batch to the port, with the kind's name, the task's settings
//! and the input port the batch came in on ([`moruna_job::ServedCall`]), and takes what the port
//! answers, an Arrow IPC stream, as the task's output for that batch. The answer is untrusted: it
//! is read through the checked decode (contracts CT-I13), inflated within half of what the stage's
//! arena can still give (e.8), and compared with the declared output before anything downstream
//! sees it.
//!
//! The call is a port call like any other (MH 4.10): it waits within its deadline, gives up at the
//! run's cancel, tells the run it is waiting, and fails the stage naming the port when it reaches
//! no answer. A port's own refusal fails the stage with what the port said. A task keeps
//! [`CALLS_AT_ONCE`] instances, each one call in flight and the arena it lands answers in, and
//! nothing between batches, so a resumed run asks the port again for every batch that was not
//! committed (MH 4.7).

use std::sync::{Arc, OnceLock};

use moruna_job::{
    Declaration, Role, Served as ServedDoc, ServedCall, SpecError, canonical_json,
    settings_refusals,
};
use moruna_kernel::arrow::buffer::Buffer;
use moruna_kernel::arrow::datatypes::{Field, Schema};
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::declare::{Declared, Expected, SchemaDecl, TypeDecl};
use moruna_kernel::{
    Allocator, Answer, CallFailure, Fingerprint, InitCtx, Kernel, KernelKind, KernelState,
    MorunaError, Payload, PayloadKind, PayloadSpec, Result, SourceSchema, StagePorts, TierPref,
    ipc,
};
use serde_json::{Map, Value};

use super::ports::{self, StageCalls};
use super::{Context, Entry, RunnableKind, Stage};
use crate::job::refusal;

/// How many calls one task of a served kind may have waiting on its port at once: its instances,
/// each of which holds one call and the arena it lands the answer in.
pub const CALLS_AT_ONCE: usize = 4;

/// The adapter of one served kind: its declaration, and its schema rules as the kernel contract's
/// types.
pub struct Served {
    declaration: Declaration,
    served: ServedDoc,
    input: Option<SchemaDecl>,
    output: Option<SchemaDecl>,
}

impl Served {
    /// The adapter of `declaration`, a served kind's (KD-I11): refused, `Config { name: "kinds" }`,
    /// when it is not one the registry accepts as a kernel or names no `served`.
    pub fn new(declaration: Declaration) -> Result<Served> {
        let refuse = |msg: String| MorunaError::Config { name: "kinds", msg };
        declaration.check(Role::Kernel).map_err(refuse)?;
        let Some(served) = declaration.served.clone() else {
            return Err(refuse(format!(
                "`{}` is not served by a port: a kind whose code the program has is registered \
                 with its adapter (register_kernel)",
                declaration.name
            )));
        };
        let decl = |value: &Option<Value>, field: &str, output: bool| {
            value
                .as_ref()
                .map(|v| moruna_package::manifest::schema_decl(v, field, output))
                .transpose()
                .map_err(|e| refuse(format!("`{}`'s {e}", declaration.name)))
        };
        let input = decl(&served.input_schema, "served.input_schema", false)?;
        let output = decl(&served.output_schema, "served.output_schema", true)?;
        Ok(Served {
            declaration,
            served,
            input,
            output,
        })
    }
}

impl RunnableKind for Served {
    fn declaration(&self) -> Declaration {
        self.declaration.clone()
    }

    /// The stage of a task of the kind: its settings checked by the declared schema (the kind has
    /// no settings type of Moruna's to read them with, KD-I11), its port's route made (refused at
    /// the entry when the port is not there to call), and a kernel that calls it for every batch.
    fn stage(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<Stage> {
        let kind = &self.declaration.name;
        let settings = entry.doc().settings.clone();
        if let Some(first) =
            settings_refusals(entry.field(), kind, &self.declaration.settings, &settings)
                .into_iter()
                .next()
        {
            return Err(refusal(SpecError::new(first.field, first.message)));
        }
        let place = self
            .served
            .port
            .place()
            .map_err(|f| refusal(SpecError::new(entry.at("kind"), f.reason)))?;
        let port = place.name().to_string();
        let route = ports::route_with(&self.served.port, cx, &|_, reason| {
            refusal(SpecError::new(
                entry.at("kind"),
                format!(
                    "`{kind}` is served by the {} port `{port}`, which cannot be called: {reason}",
                    place.as_str()
                ),
            ))
        })?;
        let identity = Value::Object(Map::from_iter([
            (
                "declaration".to_string(),
                serde_json::to_value(&self.declaration)
                    .map_err(|e| MorunaError::Plan(e.to_string()))?,
            ),
            ("settings".to_string(), Value::Object(settings.clone())),
        ]));
        let kernel = Arc::new(ServedKernel {
            kind: kind.clone(),
            port: port.clone(),
            settings,
            fingerprint: Fingerprint::compute(
                "moruna-runtime/served",
                canonical_json(&identity).as_bytes(),
            ),
            declared: Declared {
                input: self.input.clone(),
                output: self.output.clone(),
            },
            ports: OnceLock::new(),
        });
        let mut stage = Stage::of(Arc::clone(&kernel) as Arc<dyn Kernel>);
        let bound = Arc::clone(&kernel);
        stage.ports = Some(StageCalls {
            routes: [(port, route)].into_iter().collect(),
            bind: Box::new(move |ports| {
                let _ = bound.ports.set(ports);
            }),
        });
        Ok(stage)
    }
}

/// The kernel of a served kind's task: every batch a call to its port.
pub struct ServedKernel {
    kind: String,
    port: String,
    settings: Map<String, Value>,
    fingerprint: Fingerprint,
    declared: Declared,
    ports: OnceLock<Arc<StagePorts>>,
}

/// An instance: the arena it inflates and lands the port's answers in.
struct Instance {
    alloc: Arc<dyn Allocator>,
}

impl KernelState for Instance {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl ServedKernel {
    /// A failure of the task, naming the kind and the port, as the stage's.
    fn fail(&self, what: impl core::fmt::Display) -> MorunaError {
        MorunaError::Kernel {
            stage: self.stage(),
            seq: moruna_adapters::UNKNOWN_SEQ,
            msg: format!("`{}` (port `{}`): {what}", self.kind, self.port),
            features: None,
        }
    }

    fn stage(&self) -> moruna_kernel::StageId {
        self.ports
            .get()
            .map_or(moruna_adapters::UNKNOWN_STAGE, |p| p.stage())
    }

    /// The input schema `schema` fits the declared input, or the plan is refused naming the
    /// kind, the input and every column that does not fit.
    fn takes(&self, port: usize, schema: &Schema) -> Result<()> {
        let Some(input) = &self.declared.input else {
            return Ok(());
        };
        let misfits = input.resolve(schema)?.compare(schema);
        if misfits.is_empty() {
            return Ok(());
        }
        Err(MorunaError::Plan(format!(
            "`{}` takes what its input_schema declares, and its input {port} does not fit: {}",
            self.kind,
            misfits
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )))
    }

    /// The request for one batch on input `input`.
    fn request(&self, batch: &RecordBatch, input: u16) -> Result<Vec<u8>> {
        let mut stream = Vec::with_capacity(batch.get_array_memory_size() + 1024);
        let mut writer = StreamWriter::try_new(&mut stream, &batch.schema())
            .map_err(|e| self.fail(format!("the batch could not be written for the port: {e}")))?;
        writer
            .write(batch)
            .and_then(|()| writer.finish())
            .map_err(|e| self.fail(format!("the batch could not be written for the port: {e}")))?;
        drop(writer);
        Ok(ServedCall::new(&self.kind, self.settings.clone(), input).encode(&stream))
    }

    /// The port's answer read as the task's output for `input`: an Arrow IPC stream of one batch
    /// or more, every message checked before it is read, a compressed batch inflated within half
    /// of what the arena can still give, its schema what the kind declares it gives.
    fn reply(&self, bytes: Vec<u8>, input: &Schema, alloc: &Arc<dyn Allocator>) -> Result<Payload> {
        let inflate = ipc::Inflate::share(alloc.as_ref());
        let batches = ipc::read_stream(&Buffer::from_vec(bytes), &inflate).map_err(|e| {
            self.fail(format!(
                "the reply is not an Arrow IPC stream Moruna reads: {e}"
            ))
        })?;
        let Some(first) = batches.first() else {
            return Err(self.fail(
                "the reply holds no record batch; a reply answers a batch with one or more (of \
                 no rows, for nothing)",
            ));
        };
        let schema = first.schema();
        if let Some(output) = &self.declared.output {
            let expected = output.resolve(input).map_err(|e| self.fail(e))?;
            let misfits = expected.compare(&schema);
            if !misfits.is_empty() {
                return Err(self.fail(format!(
                    "the reply is not what `{}` declares it gives: {}",
                    self.kind,
                    misfits
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                )));
            }
        }
        let batch = match batches.len() {
            1 => batches
                .into_iter()
                .next()
                .ok_or_else(|| self.fail("no batch"))?,
            _ => moruna_kernel::arrow::compute::concat_batches(&schema, &batches).map_err(|e| {
                self.fail(format!(
                    "the reply's record batches are not of one schema: {e}"
                ))
            })?,
        };
        let (payload, _) = moruna_adapters::land::land_table(batch, alloc)?;
        Ok(payload)
    }
}

/// The schema an expectation resolves to, when it says every column, its type and its place.
fn exact_schema(expected: &Expected) -> Option<Schema> {
    if !expected.closed {
        return None;
    }
    let mut fields = Vec::with_capacity(expected.columns.len());
    for column in &expected.columns {
        let TypeDecl::Exact(ty) = &column.ty else {
            return None;
        };
        fields.push(Field::new(column.name.clone(), ty.clone(), column.nullable));
    }
    Some(Schema::new(fields))
}

impl Kernel for ServedKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// Instances, so each call has the arena its answer is inflated and landed in; nothing in
    /// them depends on the batches seen, so a resume makes them again (`ResumePolicy::Reinit`).
    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::new(CALLS_AT_ONCE)
                .unwrap_or(core::num::NonZeroUsize::MIN),
        }
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

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.output_schema_of(std::slice::from_ref(input))
    }

    /// Every input checked against the declared input; the output is the declared output resolved
    /// against input 0 when that says every column, and input 0's schema otherwise, as for every
    /// kernel whose output is only known once it runs (05 d.1).
    fn output_schema_of(&self, inputs: &[SourceSchema]) -> Result<SourceSchema> {
        let mut first = None;
        for (port, input) in inputs.iter().enumerate() {
            self.accepts().check(input)?;
            let SourceSchema::Table(schema) = input else {
                return Err(MorunaError::Plan(format!(
                    "`{}` takes tables, and its input {port} is a tensor",
                    self.kind
                )));
            };
            self.takes(port, schema)?;
            first.get_or_insert_with(|| Arc::clone(schema));
        }
        let Some(input) = first else {
            return Err(MorunaError::Plan(format!(
                "`{}` is given no input",
                self.kind
            )));
        };
        if let Some(output) = &self.declared.output
            && let Some(schema) = exact_schema(&output.resolve(&input)?)
        {
            return Ok(SourceSchema::Table(Arc::new(schema)));
        }
        Ok(SourceSchema::Table(input))
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(Instance {
            alloc: Arc::clone(&ctx.alloc),
        }))
    }

    /// A task of a served kind takes as many inputs as its task names: each batch says which.
    fn max_inputs(&self) -> u16 {
        u16::MAX
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        self.apply_port(state, 0, input)
    }

    fn apply_port(
        &self,
        state: &mut dyn KernelState,
        port: u16,
        input: Payload,
    ) -> Result<Payload> {
        let alloc = state
            .as_any_mut()
            .downcast_ref::<Instance>()
            .map(|i| Arc::clone(&i.alloc))
            .ok_or_else(|| self.fail("a state this kernel did not make"))?;
        let Payload::Table(batch, _) = input else {
            return Err(self.fail("a served kind takes tables"));
        };
        let schema = batch.schema();
        let request = self.request(&batch, port)?;
        drop(batch);
        let Some(ports) = self.ports.get() else {
            return Err(CallFailure::Unbound {
                name: self.port.clone(),
            }
            .into_error(moruna_adapters::UNKNOWN_STAGE));
        };
        match ports.call(&self.port, request) {
            Ok(Answer::Ok(bytes)) => self.reply(bytes, &schema, &alloc),
            Ok(Answer::Error(said)) => {
                Err(self.fail(format!("the port refused the batch: {said}")))
            }
            Ok(Answer::Lost(reason)) => Err(self.fail(reason)),
            Err(CallFailure::Cancelled) => Err(MorunaError::Cancelled),
            Err(failure) => match failure.into_error(ports.stage()) {
                MorunaError::Kernel { msg, .. } => Err(MorunaError::Kernel {
                    stage: ports.stage(),
                    seq: moruna_adapters::UNKNOWN_SEQ,
                    msg: format!("`{}`: {msg}", self.kind),
                    features: None,
                }),
                other => Err(other),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_job::{Fact, Flow, Isolation, Needs, PortDoc};
    use moruna_kernel::arrow::datatypes::DataType;
    use serde_json::json;

    fn declaration(input: Option<Value>, output: Option<Value>) -> Declaration {
        Declaration {
            name: "judge".into(),
            role: Role::Kernel,
            summary: "a kind its embedder serves".into(),
            settings: json!({
                "$schema": moruna_job::SCHEMA_DIALECT,
                "type": "object",
                "additionalProperties": false,
                "properties": {"tone": {"type": "string"}},
                "examples": [{}]
            }),
            inputs: Flow::Batches,
            outputs: Flow::Batches,
            needs: Needs::a_port(),
            deterministic: Fact::No,
            preserves_columns: Fact::No,
            isolation: Isolation::Embedder,
            served: Some(ServedDoc {
                port: PortDoc::guest("judge"),
                input_schema: input,
                output_schema: output,
            }),
        }
    }

    fn kernel(input: Option<Value>, output: Option<Value>, tone: &str) -> ServedKernel {
        let served = Served::new(declaration(input, output)).expect("an adapter");
        let settings: Map<String, Value> = json!({"tone": tone}).as_object().cloned().unwrap();
        ServedKernel {
            kind: "judge".into(),
            port: "judge".into(),
            fingerprint: Fingerprint::compute(
                "moruna-runtime/served",
                canonical_json(&Value::Object(settings.clone())).as_bytes(),
            ),
            settings,
            declared: Declared {
                input: served.input,
                output: served.output,
            },
            ports: OnceLock::new(),
        }
    }

    fn table(fields: &[(&str, DataType)]) -> SourceSchema {
        SourceSchema::Table(Arc::new(Schema::new(
            fields
                .iter()
                .map(|(n, t)| Field::new(*n, t.clone(), true))
                .collect::<Vec<_>>(),
        )))
    }

    #[test]
    fn only_a_served_declaration_makes_an_adapter() {
        let mut plain = declaration(None, None);
        plain.served = None;
        plain.isolation = Isolation::Builtin;
        plain.needs = Needs::nothing();
        let refused = Served::new(plain).err().expect("refused").to_string();
        assert!(
            refused.contains("`judge` is not served by a port"),
            "{refused}"
        );
        let mut odd = declaration(None, None);
        odd.role = Role::Sink;
        assert!(Served::new(odd).is_err());
        let served = Served::new(declaration(None, None)).expect("an adapter");
        assert_eq!(served.declaration(), declaration(None, None));
    }

    #[test]
    fn the_output_schema_is_known_where_the_rules_say_every_column() {
        let input = table(&[("id", DataType::Int64), ("text", DataType::Utf8)]);
        let schema_of = |k: &ServedKernel| match k.output_schema(&input).expect("planned") {
            SourceSchema::Table(s) => s
                .fields()
                .iter()
                .map(|f| (f.name().clone(), f.data_type().clone()))
                .collect::<Vec<_>>(),
            SourceSchema::Tensor { .. } => panic!("a table"),
        };
        let exact = kernel(None, Some(json!([["verdict", "bool", false]])), "x");
        assert_eq!(schema_of(&exact), [("verdict".into(), DataType::Boolean)]);
        let relative = kernel(
            None,
            Some(json!({"adds": {"verdict": "bool"}, "drops": ["text"]})),
            "x",
        );
        assert_eq!(
            schema_of(&relative),
            [
                ("id".into(), DataType::Int64),
                ("verdict".into(), DataType::Boolean)
            ]
        );
        // Open or `any` rules say less than every column: the input is what the run goes by.
        for opaque in [
            kernel(None, Some(json!({"verdict": "bool"})), "x"),
            kernel(None, Some(json!([["verdict", "any"]])), "x"),
            kernel(None, None, "x"),
        ] {
            assert_eq!(
                schema_of(&opaque),
                [
                    ("id".into(), DataType::Int64),
                    ("text".into(), DataType::Utf8)
                ]
            );
        }
        // Each input is checked against the declared input.
        let wants = kernel(Some(json!({"text": "string"})), None, "x");
        assert!(
            wants
                .output_schema_of(&[input.clone(), input.clone()])
                .is_ok()
        );
        let other = table(&[("id", DataType::Int64)]);
        let refused = wants
            .output_schema_of(&[input.clone(), other])
            .unwrap_err()
            .to_string();
        assert!(
            refused
                .contains("its input 1 does not fit: column `text`: declared string, not produced"),
            "{refused}"
        );
        assert!(wants.output_schema_of(&[]).is_err());
        let tensor = SourceSchema::Tensor {
            dtype: moruna_kernel::DType::F32,
            shape: vec![-1, 4],
        };
        assert!(wants.output_schema(&tensor).is_err());
        // A relative rule that drops a column the input does not have is a plan error.
        let drops = kernel(None, Some(json!({"drops": ["gone"]})), "x");
        assert!(drops.output_schema(&input).is_err());
    }

    #[test]
    fn a_kernel_is_its_kind_and_settings() {
        let a = kernel(None, None, "polite");
        assert_eq!(a.fingerprint(), kernel(None, None, "polite").fingerprint());
        assert_ne!(a.fingerprint(), kernel(None, None, "blunt").fingerprint());
        assert!(
            matches!(a.kind(), KernelKind::Stateful { max_instances } if max_instances.get() == CALLS_AT_ONCE)
        );
        assert_eq!(a.accepts().kind, PayloadKind::Table);
        assert_eq!(a.hints().resume, moruna_kernel::ResumePolicy::Reinit);
    }

    #[test]
    fn a_kernel_outside_a_run_calls_nothing() {
        let k = kernel(None, None, "x");
        let alloc: Arc<dyn Allocator> = Arc::new(moruna_testkit::FakeAllocator::new());
        let mut state = k
            .init(&InitCtx {
                instance: 0,
                device: None,
                alloc,
            })
            .expect("an instance");
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)])),
            vec![Arc::new(moruna_kernel::arrow::array::Int64Array::from(
                vec![1i64],
            ))],
        )
        .unwrap();
        let refused = k
            .apply(
                state.as_mut(),
                Payload::Table(batch, moruna_kernel::Tier::Host),
            )
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("the kernel called a port outside a run"),
            "{refused}"
        );
        let mut other = moruna_kernel::NoState;
        assert!(
            k.apply(
                &mut other,
                Payload::Table(
                    RecordBatch::new_empty(Arc::new(Schema::empty())),
                    moruna_kernel::Tier::Host
                )
            )
            .unwrap_err()
            .to_string()
            .contains("a state this kernel did not make")
        );
    }
}
