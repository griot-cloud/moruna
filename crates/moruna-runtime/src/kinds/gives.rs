//! A task's declared `gives`, enforced at the runtime boundary (0.6.6, "Output, identity and
//! recovery"): every batch a task produces, its first, every later one and its final
//! (completion) output, is compared with the [`ArrowSchema`] it declares before it is published
//! downstream. A refusal names the task, which batch, and the mismatched field. A task whose
//! definition and kind declare no `gives` keeps its documented, undeclared behaviour: nothing is
//! checked and nothing is wrapped.
//!
//! The build wraps a declaring task's stage kernel in [`GuardedKernel`] (its `apply`,
//! `apply_port`, `apply_aligned` and `finish` outputs) and a declaring source in
//! [`GuardedSource`] (its `read` and `read_next` outputs).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_job::{ArrowField, ArrowSchema};
use moruna_kernel::arrow::datatypes::Schema;
use moruna_kernel::{
    Allocator, BoxFuture, Declared, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind,
    KernelState, LogicalRows, LogicalTaskRows, MorunaError, Payload, PayloadSpec, Result, RowRange,
    Source, SourceRead, SourceSchema, Split, Tier,
};

/// Which of a task's outputs is being checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Produced {
    /// A batch the task produced while running; the guard numbers them.
    Batch,
    /// The task's completion output (a function's `finish`).
    Final,
}

/// The check of one task's produced batches against what it declares it gives. Shared by every
/// instance of the task's stage, so batches are numbered across instances.
#[derive(Debug)]
pub struct GivesGuard {
    task: String,
    declared: ArrowSchema,
    batches: AtomicU64,
}

impl GivesGuard {
    /// The guard for task `task`, which declares it gives `declared`.
    pub fn new(task: impl Into<String>, declared: ArrowSchema) -> GivesGuard {
        GivesGuard {
            task: task.into(),
            declared,
            batches: AtomicU64::new(0),
        }
    }

    /// The guard for task `task` when it declares what it gives; `None` leaves it unchecked.
    pub fn of(task: impl Into<String>, declared: Option<&ArrowSchema>) -> Option<Arc<GivesGuard>> {
        declared.map(|d| Arc::new(GivesGuard::new(task, d.clone())))
    }

    /// How many batches have been checked and passed.
    pub fn checked(&self) -> u64 {
        self.batches.load(Ordering::Relaxed)
    }

    fn which(&self, produced: Produced) -> String {
        match (produced, self.checked()) {
            (Produced::Final, _) => "its final output".into(),
            (Produced::Batch, 0) => "its first batch".into(),
            (Produced::Batch, n) => format!("its batch {}", n + 1),
        }
    }

    /// Check one produced batch's schema; on a mismatch nothing is counted and the batch must
    /// not be published.
    pub fn check(&self, produced: Produced, schema: &Schema) -> Result<()> {
        let misfits = misfits(&self.declared, schema);
        if !misfits.is_empty() {
            return Err(MorunaError::Plan(format!(
                "`{}` declares it gives {}, and {} does not fit: {}",
                self.task,
                self.declared,
                self.which(produced),
                misfits.join("; ")
            )));
        }
        self.batches.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Check a produced payload: a table by its schema; a tensor is refused, since what is
    /// declared is an Arrow schema.
    pub fn check_payload(&self, produced: Produced, payload: Payload) -> Result<Payload> {
        match &payload {
            Payload::Table(batch, _) => self.check(produced, &batch.schema())?,
            Payload::Tensor(..) => {
                return Err(MorunaError::Plan(format!(
                    "`{}` declares it gives {}, and {} is a tensor",
                    self.task,
                    self.declared,
                    self.which(produced)
                )));
            }
        }
        Ok(payload)
    }
}

/// `field` as a definition spells it: its type in the shared type grammar (MH 4.9), which is
/// pyarrow's `str(type)`, as Python writes a definition's `takes` and `gives`.
fn written(field: &moruna_kernel::arrow::datatypes::Field) -> ArrowField {
    ArrowField {
        name: field.name().clone(),
        data_type: moruna_kernel::declare::type_name(field.data_type()),
        nullable: field.is_nullable(),
    }
}

/// A declared type in its canonical spelling: an alias the grammar reads (`utf8`) is spelled as
/// the grammar writes it (`string`); a spelling it does not read is compared as written.
fn canonical(spelling: &str) -> String {
    moruna_types::parse(spelling)
        .map(|ty| ty.spelling())
        .unwrap_or_else(|_| spelling.to_string())
}

/// Every way `schema` differs from `declared`, each naming its field; empty when it fits. A
/// column that holds no nulls fits a nullable declaration.
pub fn misfits(declared: &ArrowSchema, schema: &Schema) -> Vec<String> {
    let mut out = Vec::new();
    let actual: Vec<ArrowField> = schema.fields().iter().map(|f| written(f)).collect();
    for (at, want) in declared.fields.iter().enumerate() {
        match actual.iter().position(|f| f.name == want.name) {
            None => out.push(format!("field `{}` is missing", want.name)),
            Some(found) => {
                let got = &actual[found];
                let same_type = got.data_type == canonical(&want.data_type);
                if !same_type {
                    out.push(format!(
                        "field `{}` is {}, declared {}",
                        want.name, got.data_type, want.data_type
                    ));
                }
                if got.nullable && !want.nullable {
                    out.push(format!(
                        "field `{}` may hold nulls, declared not nullable",
                        want.name
                    ));
                }
                if found != at && same_type {
                    out.push(format!(
                        "field `{}` is at position {found}, declared {at}",
                        want.name
                    ));
                }
            }
        }
    }
    for got in &actual {
        if !declared.fields.iter().any(|w| w.name == got.name) {
            out.push(format!("field `{}` is not declared", got.name));
        }
    }
    out
}

/// A stage kernel whose every output is checked by its task's [`GivesGuard`]; everything else
/// is the wrapped kernel's.
pub struct GuardedKernel {
    inner: Arc<dyn Kernel>,
    guard: Arc<GivesGuard>,
}

impl GuardedKernel {
    /// `inner`, its outputs checked by `guard`.
    pub fn wrap(inner: Arc<dyn Kernel>, guard: Arc<GivesGuard>) -> Arc<dyn Kernel> {
        Arc::new(GuardedKernel { inner, guard })
    }
}

impl Kernel for GuardedKernel {
    fn with_logical_rows(
        &self,
        tasks: &[LogicalTaskRows],
        rows: Arc<dyn LogicalRows>,
    ) -> Result<Arc<dyn Kernel>> {
        let bound = self.inner.with_logical_rows(tasks, rows)?;
        Ok(GuardedKernel::wrap(bound, self.guard.clone()))
    }
    fn fingerprint(&self) -> Fingerprint {
        self.inner.fingerprint()
    }
    fn kind(&self) -> KernelKind {
        self.inner.kind()
    }
    fn hints(&self) -> KernelHints {
        self.inner.hints()
    }
    fn state_uses_arena(&self) -> bool {
        self.inner.state_uses_arena()
    }
    fn declared(&self) -> Declared {
        self.inner.declared()
    }
    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.inner.output_schema(input)
    }
    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        self.inner.init(ctx)
    }
    fn restore(&self, ctx: &InitCtx, state: &[u8]) -> Result<Box<dyn KernelState>> {
        self.inner.restore(ctx, state)
    }
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let out = self.inner.apply(state, input)?;
        self.guard.check_payload(Produced::Batch, out)
    }
    fn finish(&self, state: &mut dyn KernelState) -> Result<Option<Payload>> {
        match self.inner.finish(state)? {
            Some(out) => self.guard.check_payload(Produced::Final, out).map(Some),
            None => Ok(None),
        }
    }
    fn max_inputs(&self) -> u16 {
        self.inner.max_inputs()
    }
    fn apply_port(
        &self,
        state: &mut dyn KernelState,
        port: u16,
        input: Payload,
    ) -> Result<Payload> {
        let out = self.inner.apply_port(state, port, input)?;
        self.guard.check_payload(Produced::Batch, out)
    }
    fn aligns_inputs(&self) -> bool {
        self.inner.aligns_inputs()
    }
    fn apply_aligned(&self, state: &mut dyn KernelState, inputs: Vec<Payload>) -> Result<Payload> {
        let out = self.inner.apply_aligned(state, inputs)?;
        self.guard.check_payload(Produced::Batch, out)
    }
    fn output_schema_of(&self, inputs: &[SourceSchema]) -> Result<SourceSchema> {
        self.inner.output_schema_of(inputs)
    }
    fn waiting_state(&self, instance: usize) -> Option<Vec<u8>> {
        self.inner.waiting_state(instance)
    }
}

/// A source whose every batch is checked by its task's [`GivesGuard`].
pub struct GuardedSource {
    inner: Arc<dyn Source>,
    guard: Arc<GivesGuard>,
}

impl GuardedSource {
    /// `inner`, its batches checked by `guard`.
    pub fn wrap(inner: Arc<dyn Source>, guard: Arc<GivesGuard>) -> Arc<dyn Source> {
        Arc::new(GuardedSource { inner, guard })
    }
}

impl Source for GuardedSource {
    fn schema(&self) -> SourceSchema {
        self.inner.schema()
    }
    fn plan(&self) -> Result<Vec<Split>> {
        self.inner.plan()
    }
    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Payload>> {
        Box::pin(async move {
            let out = self.inner.read(split, rows, alloc, tier).await?;
            self.guard.check_payload(Produced::Batch, out)
        })
    }
    fn streaming(&self) -> bool {
        self.inner.streaming()
    }
    fn read_next<'a>(
        &'a self,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, Result<Option<Payload>>> {
        Box::pin(async move {
            match self.inner.read_next(alloc, tier).await? {
                Some(out) => self.guard.check_payload(Produced::Batch, out).map(Some),
                None => Ok(None),
            }
        })
    }
    fn repeatable(&self) -> bool {
        self.inner.repeatable()
    }
    fn read_report(&self) -> Option<SourceRead> {
        self.inner.read_report()
    }
}

/// `spec`, its source checked by `guard` once built.
pub fn guard_source(
    spec: crate::spec::SourceSpec,
    guard: Arc<GivesGuard>,
) -> crate::spec::SourceSpec {
    use crate::spec::SourceSpec;
    match spec {
        SourceSpec::Built(source) => SourceSpec::Built(GuardedSource::wrap(source, guard)),
        SourceSpec::Build(factory) => SourceSpec::Build(Box::new(move |ctx| {
            factory(ctx).map(|source| GuardedSource::wrap(source, guard))
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::arrow::array::{ArrayRef, Int32Array, Int64Array, StringArray};
    use moruna_kernel::arrow::datatypes::{DataType, Field};
    use moruna_kernel::arrow::record_batch::RecordBatch;

    fn declared() -> ArrowSchema {
        ArrowSchema {
            fields: vec![
                ArrowField {
                    name: "id".into(),
                    data_type: "int64".into(),
                    nullable: false,
                },
                ArrowField {
                    name: "text".into(),
                    data_type: "utf8".into(),
                    nullable: true,
                },
            ],
        }
    }

    fn good() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("text", DataType::Utf8, true),
        ])
    }

    fn guard() -> Arc<GivesGuard> {
        GivesGuard::of("score", Some(&declared())).expect("declared")
    }

    #[test]
    fn undeclared_is_not_guarded() {
        assert!(GivesGuard::of("t", None).is_none());
    }

    #[test]
    fn matching_batches_pass_and_are_counted() {
        let g = guard();
        g.check(Produced::Batch, &good()).unwrap();
        g.check(Produced::Batch, &good()).unwrap();
        g.check(Produced::Final, &good()).unwrap();
        assert_eq!(g.checked(), 3);
        // A column that holds no nulls fits a nullable declaration.
        let narrower = Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("text", DataType::Utf8, false),
        ]);
        g.check(Produced::Batch, &narrower).unwrap();
    }

    #[test]
    fn wrong_first_batch_is_refused_naming_task_and_field() {
        let g = guard();
        let bad = Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("text", DataType::Utf8, true),
        ]);
        let e = g.check(Produced::Batch, &bad).unwrap_err().to_string();
        assert!(e.contains("`score`"), "{e}");
        assert!(e.contains("its first batch"), "{e}");
        assert!(e.contains("field `id` is int32, declared int64"), "{e}");
        assert_eq!(g.checked(), 0);
    }

    #[test]
    fn wrong_later_batch_is_refused_with_its_ordinal() {
        let g = guard();
        g.check(Produced::Batch, &good()).unwrap();
        let bad = Schema::new(vec![Field::new("id", DataType::Int64, false)]);
        let e = g.check(Produced::Batch, &bad).unwrap_err().to_string();
        assert!(e.contains("its batch 2"), "{e}");
        assert!(e.contains("field `text` is missing"), "{e}");
        assert_eq!(g.checked(), 1);
    }

    #[test]
    fn wrong_final_output_is_refused() {
        let g = guard();
        let bad = Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("text", DataType::Utf8, true),
            Field::new("extra", DataType::Boolean, true),
        ]);
        let e = g.check(Produced::Final, &bad).unwrap_err().to_string();
        assert!(e.contains("its final output"), "{e}");
        assert!(e.contains("field `id` may hold nulls"), "{e}");
        assert!(e.contains("field `extra` is not declared"), "{e}");
    }

    #[test]
    fn reordered_fields_are_refused() {
        let swapped = Schema::new(vec![
            Field::new("text", DataType::Utf8, true),
            Field::new("id", DataType::Int64, false),
        ]);
        assert_eq!(
            misfits(&declared(), &swapped),
            vec![
                "field `id` is at position 1, declared 0".to_string(),
                "field `text` is at position 0, declared 1".to_string(),
            ]
        );
        assert!(misfits(&declared(), &good()).is_empty());
    }

    // ---- the wrappers, over a real kernel and a real source ----

    pub(super) fn batch(id: ArrayRef) -> Payload {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", id.data_type().clone(), false),
            Field::new("text", DataType::Utf8, true),
        ]));
        let text: ArrayRef = Arc::new(StringArray::from(vec![Some("a"); id.len()]));
        Payload::Table(
            RecordBatch::try_new(schema, vec![id, text]).unwrap(),
            Tier::Host,
        )
    }
    fn good_batch() -> Payload {
        batch(Arc::new(Int64Array::from(vec![1, 2])))
    }
    fn bad_batch() -> Payload {
        batch(Arc::new(Int32Array::from(vec![1, 2])))
    }

    /// A kernel that gives the batches it is told to, in order, then a final output.
    struct Scripted {
        outputs: std::sync::Mutex<Vec<Payload>>,
        last: std::sync::Mutex<Option<Payload>>,
    }
    impl Scripted {
        fn scripted(outputs: Vec<Payload>, last: Option<Payload>) -> Arc<dyn Kernel> {
            Arc::new(Scripted {
                outputs: std::sync::Mutex::new(outputs.into_iter().rev().collect()),
                last: std::sync::Mutex::new(last),
            })
        }
        fn next(&self) -> Result<Payload> {
            self.outputs
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| MorunaError::Plan("no more".into()))
        }
    }
    impl Kernel for Scripted {
        fn fingerprint(&self) -> Fingerprint {
            Fingerprint::compute("scripted", b"")
        }
        fn kind(&self) -> KernelKind {
            KernelKind::Stateless
        }
        fn accepts(&self) -> PayloadSpec {
            PayloadSpec {
                kind: moruna_kernel::PayloadKind::Table,
                tier: moruna_kernel::TierPref::Host,
            }
        }
        fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
            Ok(input.clone())
        }
        fn init(&self, _: &InitCtx) -> Result<Box<dyn KernelState>> {
            Ok(Box::new(moruna_kernel::NoState))
        }
        fn apply(&self, _: &mut dyn KernelState, _: Payload) -> Result<Payload> {
            self.next()
        }
        fn finish(&self, _: &mut dyn KernelState) -> Result<Option<Payload>> {
            Ok(self.last.lock().unwrap().take())
        }
        fn max_inputs(&self) -> u16 {
            2
        }
        fn apply_port(&self, _: &mut dyn KernelState, _: u16, _: Payload) -> Result<Payload> {
            self.next()
        }
        fn aligns_inputs(&self) -> bool {
            true
        }
        fn apply_aligned(&self, _: &mut dyn KernelState, _: Vec<Payload>) -> Result<Payload> {
            self.next()
        }
    }

    #[test]
    fn a_guarded_kernel_checks_every_output_and_forwards_the_rest() {
        let inner = Scripted::scripted(
            vec![good_batch(), good_batch(), good_batch(), bad_batch()],
            Some(bad_batch()),
        );
        let g = guard();
        let k = GuardedKernel::wrap(inner.clone(), g.clone());
        let mut state = moruna_kernel::NoState;
        k.apply(&mut state, good_batch()).unwrap();
        k.apply_port(&mut state, 1, good_batch()).unwrap();
        k.apply_aligned(&mut state, vec![good_batch()]).unwrap();
        let later = k.apply(&mut state, good_batch()).unwrap_err().to_string();
        assert!(later.contains("its batch 4"), "{later}");
        let last = k.finish(&mut state).unwrap_err().to_string();
        assert!(last.contains("its final output"), "{last}");
        assert!(k.finish(&mut state).unwrap().is_none());
        assert_eq!(g.checked(), 3);
        // Everything else is the wrapped kernel's.
        assert_eq!(k.fingerprint(), inner.fingerprint());
        assert_eq!(k.max_inputs(), 2);
        assert!(k.aligns_inputs());
        assert!(k.state_uses_arena());
        assert_eq!(k.waiting_state(0), None);
        assert!(k.with_logical_rows(&[], Arc::new(NoRows)).is_err());
        let _ = (k.kind(), k.hints(), k.declared(), k.accepts());
        let schema = table_schema();
        assert!(k.output_schema(&schema).is_ok());
        assert!(k.output_schema_of(std::slice::from_ref(&schema)).is_ok());
        let ctx = InitCtx {
            instance: 0,
            device: None,
            alloc: Arc::new(moruna_testkit::FakeAllocator::new()),
        };
        assert!(k.init(&ctx).is_ok());
        assert!(k.restore(&ctx, b"").is_err());
    }

    #[test]
    fn a_tensor_is_refused_where_a_table_is_declared() {
        let g = guard();
        let alloc = moruna_testkit::FakeAllocator::new();
        let buffer = alloc.alloc(8, Tier::Host).unwrap();
        let tensor = moruna_kernel::ManagedTensor::from_buffer(
            buffer,
            0,
            moruna_kernel::DType::I64,
            vec![1],
        )
        .unwrap();
        let e = g
            .check_payload(Produced::Batch, Payload::Tensor(tensor, Tier::Host))
            .unwrap_err()
            .to_string();
        assert!(e.contains("is a tensor"), "{e}");
    }

    struct NoRows;
    impl LogicalRows for NoRows {
        fn append(&self, _: &str, _: &str, _: &RecordBatch) -> Result<()> {
            Ok(())
        }
    }

    /// A source that reads the batches it is told to.
    struct Listed(std::sync::Mutex<Vec<Payload>>);
    impl Source for Listed {
        fn schema(&self) -> SourceSchema {
            table_schema()
        }
        fn plan(&self) -> Result<Vec<Split>> {
            Ok(Vec::new())
        }
        fn read<'a>(
            &'a self,
            _: &'a Split,
            _: Option<RowRange>,
            _: &'a dyn Allocator,
            _: Tier,
        ) -> BoxFuture<'a, Result<Payload>> {
            Box::pin(async move {
                self.0
                    .lock()
                    .unwrap()
                    .pop()
                    .ok_or_else(|| MorunaError::Plan("no more".into()))
            })
        }
        fn streaming(&self) -> bool {
            true
        }
        fn read_next<'a>(
            &'a self,
            _: &'a dyn Allocator,
            _: Tier,
        ) -> BoxFuture<'a, Result<Option<Payload>>> {
            Box::pin(async move { Ok(self.0.lock().unwrap().pop()) })
        }
    }

    #[test]
    fn a_guarded_source_checks_every_batch_it_reads() {
        let alloc = moruna_testkit::FakeAllocator::new();
        let split = Split {
            id: 0,
            rows: 2,
            uncompressed_bytes: 0,
            estimated: true,
            column_bytes: Vec::new(),
            null_counts: Vec::new(),
            sub_splittable: false,
        };
        // Popped from the end: good, good, bad.
        let inner: Arc<dyn Source> = Arc::new(Listed(std::sync::Mutex::new(vec![
            bad_batch(),
            good_batch(),
            good_batch(),
        ])));
        let g = guard();
        let spec = guard_source(crate::spec::SourceSpec::Built(inner), g.clone());
        let crate::spec::SourceSpec::Built(s) = spec else {
            panic!("a built source stays built");
        };
        futures_lite_block_on(s.read(&split, None, &alloc, Tier::Host)).unwrap();
        assert!(
            futures_lite_block_on(s.read_next(&alloc, Tier::Host))
                .unwrap()
                .is_some()
        );
        let e = futures_lite_block_on(s.read_next(&alloc, Tier::Host))
            .unwrap_err()
            .to_string();
        assert!(e.contains("its batch 3"), "{e}");
        assert!(
            futures_lite_block_on(s.read_next(&alloc, Tier::Host))
                .unwrap()
                .is_none()
        );
        assert!(s.streaming() && s.repeatable() && s.read_report().is_none());
        assert!(s.plan().unwrap().is_empty());
        let _ = s.schema();
    }

    fn table_schema() -> SourceSchema {
        SourceSchema::Table(Arc::new(good()))
    }

    fn futures_lite_block_on<T>(f: BoxFuture<'_, T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }
}
