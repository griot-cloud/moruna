//! The CPU references of `moruna.filter@1` and `moruna.take@1` (system design 4B), as their
//! specifications state them (`spec/filter.md`, `spec/take.md`).

use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, RecordBatch, RecordBatchOptions, UInt32Array,
    UInt64Array, new_null_array,
};
use moruna_kernel::arrow::buffer::{BooleanBuffer, ScalarBuffer};
use moruna_kernel::arrow::compute::{TakeOptions, filter_record_batch, take};
use moruna_kernel::arrow::datatypes::{
    DataType, Field, FieldRef, Int8Type, Int16Type, Int32Type, Int64Type, Schema, SchemaRef,
    UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use moruna_kernel::declare::{ColumnDecl, Declared, SchemaDecl};
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError, NoState,
    Payload, PayloadKind, PayloadSpec, Result, SourceSchema, TierPref,
};
use serde_json::Value;

use crate::catalogue::{KernelId, MORUNA, Params, fingerprint};

/// The identity of a version of a `moruna` kernel.
fn identity(name: &str, version: u32) -> KernelId {
    KernelId {
        namespace: MORUNA.to_string(),
        name: name.to_string(),
        version,
    }
}

/// A refusal at configure or at plan, naming the kernel.
fn plan(id: &KernelId, msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Plan(format!("{id}: {msg}"))
}

/// A refusal of a batch, naming the kernel.
fn refuse(id: &KernelId, msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Kernel {
        stage: moruna_kernel::StageId::MAX,
        seq: moruna_kernel::Seq::MAX,
        msg: format!("{id}: {msg}"),
        features: None,
    }
}

/// The parameters as an object with only the members `known`.
fn members(
    id: &KernelId,
    params: &Params,
    known: &[&str],
    arrays: &[&str],
) -> Result<serde_json::Map<String, Value>> {
    let object = match &params.json {
        Value::Null => serde_json::Map::new(),
        Value::Object(object) => object.clone(),
        other => return Err(plan(id, format!("parameters are an object, not {other}"))),
    };
    if let Some(key) = object.keys().find(|k| !known.contains(&k.as_str())) {
        return Err(plan(
            id,
            format!("unknown parameter `{key}` (it takes {})", known.join(", ")),
        ));
    }
    if let Some(key) = params.arrays.keys().find(|k| !arrays.contains(&k.as_str())) {
        return Err(plan(
            id,
            format!(
                "unknown Arrow parameter `{key}` (it takes {})",
                if arrays.is_empty() {
                    "none".to_string()
                } else {
                    arrays.join(", ")
                }
            ),
        ));
    }
    Ok(object)
}

fn table_only(id: &KernelId, input: &SourceSchema) -> Result<SchemaRef> {
    match input {
        SourceSchema::Table(schema) => Ok(Arc::clone(schema)),
        SourceSchema::Tensor { .. } => Err(plan(id, "takes a table, and the input is a tensor")),
    }
}

fn host_table() -> PayloadSpec {
    PayloadSpec {
        kind: PayloadKind::Table,
        tier: TierPref::Host,
    }
}

// ---------------------------------------------------------------------------------------------
// filter

/// What `filter` gives (the platform chooses by density, through this parameter).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FilterOutput {
    /// The compacted batch, without the predicate column.
    Batch,
    /// The positions of the selected rows, a `UInt32` column `selection`.
    Selection,
}

/// `moruna.filter@1`: the rows of a batch its Boolean predicate column selects.
#[derive(Clone, Debug)]
pub struct Filter {
    id: KernelId,
    predicate: String,
    output: FilterOutput,
    fingerprint: Fingerprint,
}

/// The name of the selection form's one column.
pub const SELECTION: &str = "selection";

/// Configure `moruna.filter@<version>` from `{"predicate": "<column>", "output": "batch" |
/// "selection"}`.
pub fn configure_filter(version: u32, params: &Params) -> Result<Arc<dyn Kernel>> {
    let id = identity("filter", version);
    if version != 1 {
        return Err(plan(&id, "is not published"));
    }
    let object = members(&id, params, &["predicate", "output"], &[])?;
    let predicate = match object.get("predicate") {
        Some(Value::String(p)) if !p.is_empty() => p.clone(),
        _ => return Err(plan(&id, "`predicate` is required, the name of a column")),
    };
    let output = match object.get("output") {
        None => FilterOutput::Batch,
        Some(Value::String(s)) if s == "batch" => FilterOutput::Batch,
        Some(Value::String(s)) if s == "selection" => FilterOutput::Selection,
        Some(other) => {
            return Err(plan(
                &id,
                format!("`output` is \"batch\" or \"selection\", not {other}"),
            ));
        }
    };
    Ok(Arc::new(Filter {
        fingerprint: fingerprint(&id, params)?,
        id,
        predicate,
        output,
    }))
}

impl Filter {
    /// The predicate column's position in `schema`, which must be Boolean.
    fn predicate_of(&self, schema: &Schema) -> Result<usize> {
        let at = schema.index_of(&self.predicate).map_err(|_| {
            plan(
                &self.id,
                format!("the input has no predicate column `{}`", self.predicate),
            )
        })?;
        match schema.field(at).data_type() {
            DataType::Boolean => Ok(at),
            other => Err(plan(
                &self.id,
                format!(
                    "the predicate column `{}` is {other}; it must be Boolean",
                    self.predicate
                ),
            )),
        }
    }

    fn table_output(&self, input: &Schema) -> Result<SchemaRef> {
        let at = self.predicate_of(input)?;
        Ok(match self.output {
            FilterOutput::Batch => {
                let fields: Vec<FieldRef> = input
                    .fields()
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != at)
                    .map(|(_, f)| Arc::clone(f))
                    .collect();
                Arc::new(Schema::new_with_metadata(fields, input.metadata().clone()))
            }
            FilterOutput::Selection => Arc::new(Schema::new(vec![Field::new(
                SELECTION,
                DataType::UInt32,
                false,
            )])),
        })
    }

    /// The rows the predicate selects: true and not null.
    fn selected(mask: &BooleanArray) -> BooleanBuffer {
        match mask.nulls() {
            Some(nulls) => mask.values() & nulls.inner(),
            None => mask.values().clone(),
        }
    }

    /// The kernel's work on one batch.
    pub fn filter(&self, batch: &RecordBatch) -> Result<RecordBatch> {
        let schema = batch.schema();
        let at = self.predicate_of(&schema)?;
        let out_schema = self.table_output(&schema)?;
        let mask = batch.column(at).as_boolean();
        let selected = Self::selected(mask);
        match self.output {
            FilterOutput::Batch => {
                let rest: Vec<usize> = (0..batch.num_columns()).filter(|i| *i != at).collect();
                let rest = batch.project(&rest).map_err(|e| refuse(&self.id, e))?;
                let mask = BooleanArray::new(selected, None);
                let kept = filter_record_batch(&rest, &mask).map_err(|e| refuse(&self.id, e))?;
                let rows = kept.num_rows();
                RecordBatch::try_new_with_options(
                    out_schema,
                    kept.columns().to_vec(),
                    &RecordBatchOptions::new().with_row_count(Some(rows)),
                )
                .map_err(|e| refuse(&self.id, e))
            }
            FilterOutput::Selection => {
                if batch.num_rows() > 1usize << 32 {
                    return Err(refuse(
                        &self.id,
                        format!(
                            "a batch of {} rows has positions past UInt32; the selection form \
                             takes at most 4294967296 rows",
                            batch.num_rows()
                        ),
                    ));
                }
                let positions: ScalarBuffer<u32> =
                    selected.set_indices().map(|i| i as u32).collect();
                RecordBatch::try_new(
                    out_schema,
                    vec![Arc::new(UInt32Array::new(positions, None))],
                )
                .map_err(|e| refuse(&self.id, e))
            }
        }
    }
}

impl Kernel for Filter {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: Some(1.0),
            releases_gil: Some(true),
            ..KernelHints::default()
        }
    }

    fn declared(&self) -> Declared {
        Declared {
            input: Some(SchemaDecl::Subset(vec![ColumnDecl::new(
                self.predicate.clone(),
                DataType::Boolean,
            )])),
            output: Some(match self.output {
                FilterOutput::Batch => SchemaDecl::Relative {
                    adds: Vec::new(),
                    drops: vec![self.predicate.clone()],
                    changes: Vec::new(),
                },
                FilterOutput::Selection => SchemaDecl::Exact(vec![ColumnDecl {
                    nullable: false,
                    ..ColumnDecl::new(SELECTION, DataType::UInt32)
                }]),
            }),
        }
    }

    fn accepts(&self) -> PayloadSpec {
        host_table()
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        let schema = table_only(&self.id, input)?;
        Ok(SourceSchema::Table(self.table_output(&schema)?))
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Payload::Table(batch, _) = &input else {
            return Err(plan(&self.id, "takes a table, and was given a tensor"));
        };
        let out = self.filter(batch)?;
        drop(input);
        Payload::table(out)
    }
}

// ---------------------------------------------------------------------------------------------
// take

/// `moruna.take@1`: values gathered by position. The values are fixed at configure (the Arrow
/// parameter `values`), or come with each index batch on a second input joined by morsel.
#[derive(Clone, Debug)]
pub struct Take {
    id: KernelId,
    values: Option<(FieldRef, ArrayRef)>,
    fingerprint: Fingerprint,
}

/// Configure `moruna.take@<version>`: no JSON parameters, and optionally the Arrow parameter
/// `values`, a batch of one column.
pub fn configure_take(version: u32, params: &Params) -> Result<Arc<dyn Kernel>> {
    let id = identity("take", version);
    if version != 1 {
        return Err(plan(&id, "is not published"));
    }
    members(&id, params, &[], &["values"])?;
    let values = match params.arrays.get("values") {
        None => None,
        Some(batch) if batch.num_columns() == 1 => Some((
            Arc::clone(&batch.schema().fields()[0]),
            Arc::clone(batch.column(0)),
        )),
        Some(batch) => {
            return Err(plan(
                &id,
                format!(
                    "`values` is a batch of one column, and has {}",
                    batch.num_columns()
                ),
            ));
        }
    };
    Ok(Arc::new(Take {
        fingerprint: fingerprint(&id, params)?,
        id,
        values,
    }))
}

/// An index entry of any integer type, as `i128`, which holds them all; `None` for a null.
fn index_values(array: &dyn Array) -> Vec<Option<i128>> {
    macro_rules! of {
        ($t:ty) => {
            array
                .as_primitive::<$t>()
                .iter()
                .map(|v| v.map(i128::from))
                .collect()
        };
    }
    match array.data_type() {
        DataType::Int8 => of!(Int8Type),
        DataType::Int16 => of!(Int16Type),
        DataType::Int32 => of!(Int32Type),
        DataType::Int64 => of!(Int64Type),
        DataType::UInt8 => of!(UInt8Type),
        DataType::UInt16 => of!(UInt16Type),
        DataType::UInt32 => of!(UInt32Type),
        DataType::UInt64 => of!(UInt64Type),
        _ => Vec::new(),
    }
}

/// Whether `ty` is an index type: any integer.
fn is_index(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
    )
}

impl Take {
    /// The one field of an input of one column, `what` naming it.
    fn one_column(&self, schema: &Schema, what: &str) -> Result<FieldRef> {
        match schema.fields().as_ref() {
            [field] => Ok(Arc::clone(field)),
            fields => Err(plan(
                &self.id,
                format!(
                    "the {what} is a batch of one column, and has {}",
                    fields.len()
                ),
            )),
        }
    }

    /// The output schema for an index field and a values field.
    fn output_for(&self, index: &Schema, values: &Field) -> Result<SchemaRef> {
        let index = self.one_column(index, "index")?;
        if !is_index(index.data_type()) {
            return Err(plan(
                &self.id,
                format!(
                    "the index `{}` is {}; it must be an integer",
                    index.name(),
                    index.data_type()
                ),
            ));
        }
        Ok(Arc::new(Schema::new(vec![Field::new(
            values.name(),
            values.data_type().clone(),
            values.is_nullable() || index.is_nullable(),
        )])))
    }

    /// The values gathered at every position of `index`.
    pub fn gather(&self, index: &dyn Array, values: &ArrayRef) -> Result<ArrayRef> {
        let entries = index_values(index);
        let len = values.len() as i128;
        let mut at = Vec::with_capacity(entries.len());
        for (row, entry) in entries.iter().enumerate() {
            match entry {
                None => at.push(0u64),
                Some(v) if (0..len).contains(v) => at.push(*v as u64),
                Some(v) => {
                    return Err(refuse(
                        &self.id,
                        format!("the index {v} at row {row} is out of range for {len} values"),
                    ));
                }
            }
        }
        if values.is_empty() {
            // Every entry is null, or one would have been refused above.
            return Ok(new_null_array(values.data_type(), entries.len()));
        }
        let indices = UInt64Array::new(ScalarBuffer::from(at), index.nulls().cloned());
        take(
            values.as_ref(),
            &indices,
            Some(TakeOptions { check_bounds: true }),
        )
        .map_err(|e| refuse(&self.id, e))
    }

    fn batch(&self, index: &RecordBatch, field: &Field, values: &ArrayRef) -> Result<Payload> {
        let schema = self.output_for(&index.schema(), field)?;
        let out = self.gather(index.column(0).as_ref(), values)?;
        Payload::table(RecordBatch::try_new(schema, vec![out]).map_err(|e| refuse(&self.id, e))?)
    }
}

impl Kernel for Take {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: Some(1.0),
            releases_gil: Some(true),
            state_bytes: self
                .values
                .as_ref()
                .map(|(_, v)| v.get_array_memory_size() as u64),
            ..KernelHints::default()
        }
    }

    fn accepts(&self) -> PayloadSpec {
        host_table()
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.output_schema_of(core::slice::from_ref(input))
    }

    /// One input (the index) when the values are fixed; two otherwise, the index then the values.
    fn max_inputs(&self) -> u16 {
        if self.values.is_some() { 1 } else { 2 }
    }

    fn aligns_inputs(&self) -> bool {
        self.values.is_none()
    }

    fn output_schema_of(&self, inputs: &[SourceSchema]) -> Result<SourceSchema> {
        let schema = match (&self.values, inputs) {
            (Some((field, _)), [index]) => {
                let index = table_only(&self.id, index)?;
                self.output_for(index.as_ref(), field)?
            }
            (None, [index, values]) => {
                let values = table_only(&self.id, values)?;
                let field = self.one_column(&values, "values")?;
                let index = table_only(&self.id, index)?;
                self.output_for(index.as_ref(), &field)?
            }
            (Some(_), many) => {
                return Err(plan(
                    &self.id,
                    format!(
                        "its values are fixed at configure, so it takes one input, the index, \
                         and is given {}",
                        many.len()
                    ),
                ));
            }
            (None, many) => {
                return Err(plan(
                    &self.id,
                    format!(
                        "with no values fixed at configure it takes two inputs, the index then \
                         the values, and is given {}",
                        many.len()
                    ),
                ));
            }
        };
        Ok(SourceSchema::Table(schema))
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Some((field, values)) = &self.values else {
            return Err(plan(
                &self.id,
                "with no values fixed at configure it takes the index and the values together",
            ));
        };
        let Payload::Table(index, _) = &input else {
            return Err(plan(&self.id, "takes a table, and was given a tensor"));
        };
        self.batch(index, field, values)
    }

    fn apply_aligned(&self, _state: &mut dyn KernelState, inputs: Vec<Payload>) -> Result<Payload> {
        let (None, [Payload::Table(index, _), Payload::Table(values, _)]) =
            (&self.values, inputs.as_slice())
        else {
            return Err(plan(
                &self.id,
                "joined by morsel, it takes two tables, the index then the values",
            ));
        };
        let field = self.one_column(&values.schema(), "values")?;
        self.batch(index, &field, values.column(0))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use moruna_kernel::DType;
    use moruna_kernel::arrow::array::{Float64Array, Int32Array, Int64Array, StringArray};
    use moruna_kernel::arrow::datatypes::Int64Type;
    use serde_json::json;

    use super::*;

    fn batch(columns: Vec<(&str, ArrayRef)>) -> RecordBatch {
        RecordBatch::try_from_iter(columns.into_iter().map(|(n, a)| (n.to_string(), a)))
            .expect("batch")
    }

    fn table(b: RecordBatch) -> Payload {
        Payload::table(b).expect("payload")
    }

    fn tensor() -> Payload {
        let column = table(batch(vec![(
            "x",
            Arc::new(Float64Array::from(vec![1.0])) as ArrayRef,
        )]));
        Payload::tensor(column.as_tensor(Some("x")).expect("tensor")).expect("payload")
    }

    fn a_tensor_schema() -> SourceSchema {
        SourceSchema::Tensor {
            dtype: DType::F64,
            shape: vec![-1],
        }
    }

    fn ctx() -> InitCtx {
        InitCtx {
            instance: 0,
            device: None,
            alloc: Arc::new(moruna_testkit::FakeAllocator::new()),
        }
    }

    fn values(v: Vec<i64>) -> Params {
        Params {
            json: Value::Null,
            arrays: BTreeMap::from([(
                "values".to_string(),
                batch(vec![("v", Arc::new(Int64Array::from(v)) as ArrayRef)]),
            )]),
        }
    }

    #[test]
    fn filter_is_configured_strictly() {
        assert!(configure_filter(2, &Params::json(json!({"predicate": "p"}))).is_err());
        assert!(configure_filter(1, &Params::json(json!([1]))).is_err());
        assert!(configure_filter(1, &Params::json(json!({"predicate": ""}))).is_err());
        let mut arrays = values(vec![1]);
        arrays.json = json!({"predicate": "p"});
        let err = configure_filter(1, &arrays)
            .err()
            .expect("an Arrow parameter");
        assert!(err.to_string().contains("(it takes none)"), "{err}");
        let output = Params::json(json!({"predicate": "p", "output": 1}));
        assert!(configure_filter(1, &output).is_err());
    }

    #[test]
    fn filter_declares_its_columns_and_refuses_a_tensor() {
        let batch_form = configure_filter(1, &Params::json(json!({"predicate": "p"}))).expect("f");
        let selection = configure_filter(
            1,
            &Params::json(json!({"predicate": "p", "output": "selection"})),
        )
        .expect("s");
        assert!(matches!(batch_form.kind(), KernelKind::Stateless));
        assert_eq!(batch_form.hints().expected_amplification, Some(1.0));
        assert!(matches!(
            batch_form.declared().output,
            Some(SchemaDecl::Relative { ref drops, .. }) if drops == &["p".to_string()]
        ));
        assert!(matches!(
            selection.declared().output,
            Some(SchemaDecl::Exact(ref cols)) if cols[0].name == SELECTION && !cols[0].nullable
        ));
        assert!(matches!(
            batch_form.declared().input,
            Some(SchemaDecl::Subset(_))
        ));
        assert_eq!(batch_form.accepts().kind, PayloadKind::Table);
        let mut state = batch_form.init(&ctx()).expect("init");
        assert!(batch_form.apply(state.as_mut(), tensor()).is_err());
        assert!(batch_form.output_schema(&a_tensor_schema()).is_err());
    }

    #[test]
    fn take_is_configured_strictly() {
        assert!(configure_take(2, &Params::default()).is_err());
        assert!(configure_take(1, &Params::json(json!({"x": 1}))).is_err());
        let mut other = values(vec![1]);
        let again = other.arrays["values"].clone();
        other.arrays.insert("weights".into(), again);
        assert!(configure_take(1, &other).is_err());
        let fixed = configure_take(1, &values(vec![1, 2, 3])).expect("fixed");
        assert_eq!(fixed.max_inputs(), 1);
        assert!(!fixed.aligns_inputs());
        assert!(fixed.hints().state_bytes.is_some_and(|b| b > 0));
        assert!(matches!(fixed.kind(), KernelKind::Stateless));
        assert_eq!(fixed.accepts().kind, PayloadKind::Table);
        let free = configure_take(1, &Params::default()).expect("free");
        assert_eq!(free.max_inputs(), 2);
        assert!(free.aligns_inputs());
        assert_eq!(free.hints().state_bytes, None);
    }

    #[test]
    fn take_refuses_what_its_form_does_not_take() {
        let index = batch(vec![(
            "i",
            Arc::new(Int32Array::from(vec![0, 1])) as ArrayRef,
        )]);
        let strings = batch(vec![(
            "s",
            Arc::new(StringArray::from(vec!["a", "b"])) as ArrayRef,
        )]);
        let fixed = configure_take(1, &values(vec![1, 2, 3])).expect("fixed");
        let free = configure_take(1, &Params::default()).expect("free");
        let mut state = fixed.init(&ctx()).expect("init");
        // Values fixed: one input, by apply; never two, never joined, never a tensor.
        assert!(fixed.apply(state.as_mut(), tensor()).is_err());
        let joined = vec![table(index.clone()), table(strings.clone())];
        assert!(fixed.apply_aligned(state.as_mut(), joined).is_err());
        let two = [
            SourceSchema::Table(index.schema()),
            SourceSchema::Table(strings.schema()),
        ];
        assert!(fixed.output_schema_of(&two).is_err());
        assert!(fixed.output_schema(&a_tensor_schema()).is_err());
        // Values with each batch: two inputs joined; never one, never by apply.
        let mut state = free.init(&ctx()).expect("init");
        assert!(free.apply(state.as_mut(), table(index.clone())).is_err());
        assert!(
            free.output_schema(&SourceSchema::Table(index.schema()))
                .is_err()
        );
        let SourceSchema::Table(out) = free.output_schema_of(&two).expect("two") else {
            unreachable!()
        };
        assert_eq!(out.field(0).name(), "s");
        let joined = vec![table(index.clone()), table(strings.clone())];
        let Payload::Table(got, _) = free.apply_aligned(state.as_mut(), joined).expect("joined")
        else {
            unreachable!()
        };
        assert_eq!(got.num_rows(), 2);
        let alone = vec![table(index.clone())];
        assert!(free.apply_aligned(state.as_mut(), alone).is_err());
        assert!(
            free.apply_aligned(state.as_mut(), vec![table(index), tensor()])
                .is_err()
        );
        let wide = [
            SourceSchema::Table(Arc::new(Schema::empty())),
            SourceSchema::Table(
                batch(vec![
                    ("a", Arc::new(Int32Array::from(vec![0])) as ArrayRef),
                    ("b", Arc::new(Int32Array::from(vec![0])) as ArrayRef),
                ])
                .schema(),
            ),
        ];
        assert!(free.output_schema_of(&wide).is_err());
        assert!(free.output_schema_of(&[]).is_err());
    }

    #[test]
    fn take_gathers_any_integer_index() {
        let take = Take {
            id: identity("take", 1),
            values: None,
            fingerprint: Fingerprint([0; 32]),
        };
        let values: ArrayRef = Arc::new(Int64Array::from(vec![10, 20, 30]));
        let index = Int32Array::from(vec![Some(2), None, Some(0)]);
        let out = take.gather(&index, &values).expect("gather");
        assert_eq!(
            out.as_primitive::<Int64Type>().iter().collect::<Vec<_>>(),
            vec![Some(30), None, Some(10)]
        );
        let err = take
            .gather(&Int32Array::from(vec![3]), &values)
            .expect_err("out of range");
        assert!(
            err.to_string().contains("out of range for 3 values"),
            "{err}"
        );
        assert!(index_values(&StringArray::from(vec!["a"])).is_empty());
    }
}
