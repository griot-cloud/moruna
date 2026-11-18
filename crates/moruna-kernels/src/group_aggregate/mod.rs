//! `moruna.group_aggregate@1`: count, sum, min, max and mean per group of keys (system design
//! 4B, 4C; specification `spec/group_aggregate.md`).
//!
//! One signature, two paths. When the job declares every key's range and the cells of that
//! domain are small enough ([`DENSE_MAX_BYTES`]), an instance keeps one cell per combination of
//! key values and finds a row's cell by arithmetic: the **dense path**. Otherwise, and for the
//! rest of its input after a value outside its declared range, it keeps a hash table from key to
//! group: the **hash path**. Both give the same rows in the same order.
//!
//! The kernel is stateful (4C: accumulate, merge, finish) and runs in three modes:
//! - `single`: one instance takes rows; `finish` gives the final result;
//! - `partial`: one instance per CPU of the job (6.3e) takes rows; each `finish` gives its
//!   partial result;
//! - `final`: one instance takes partial results and merges them; `finish` gives the final
//!   result.
//!
//! Every `apply` gives an empty batch: the rows come at `finish`.

pub(crate) mod accum;
#[doc(hidden)]
pub mod binned;
pub mod exact;
pub(crate) mod groups;

use std::num::NonZeroUsize;
use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, RecordBatch};
use moruna_kernel::arrow::datatypes::{DataType, Field, FieldRef, Schema, SchemaRef};
use moruna_kernel::arrow::ipc::reader::StreamReader;
use moruna_kernel::arrow::ipc::writer::StreamWriter;
use moruna_kernel::arrow::util::display::array_value_to_string;
use moruna_kernel::declare::{ColumnDecl, Declared, SchemaDecl};
use moruna_kernel::output::{self, Level};
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError, Payload,
    PayloadKind, PayloadSpec, Result, ResumePolicy, Seq, SourceSchema, StageId, TierPref,
};
use serde_json::{Map, Value};

use crate::catalogue::{Implementation, Params};

use accum::{Acc, Class, bytes_per_group, class_of, physical};
use groups::{DenseGroups, Groups, HashGroups, canonical_key, dense_type_ok, key_type_ok};

pub use groups::KeyRange;

/// The kernel's name in the `moruna` namespace.
pub const NAME: &str = "group_aggregate";
/// The version this implementation claims.
pub const VERSION: u32 = 1;
/// Its identity, `moruna.<name>@<version>` (4A).
pub const IDENTITY: &str = "moruna.group_aggregate@1";
/// The most bytes one instance's dense state may take; a declared domain whose cells take more
/// is aggregated on the hash path.
pub const DENSE_MAX_BYTES: u64 = 64 << 20;
/// The most instances of `partial` mode (the most processes one stage runs, 6.3e).
pub const MAX_INSTANCES: usize = 64;
/// The metadata key on a partial `sum` column naming the final type: `int64`, `uint64` or
/// `float64`.
pub const SUM_TYPE: &str = "moruna.group_aggregate.sum";

/// The catalogue's entry for this kernel's CPU reference.
pub const CPU_REFERENCE: Implementation = Implementation {
    kernel: "moruna.group_aggregate",
    implementation: crate::catalogue::CPU_REFERENCE,
    versions: &[VERSION],
    configure,
};

/// Configure `moruna.group_aggregate@version` from the catalogue's parameters: JSON only.
pub fn configure(version: u32, params: &Params) -> Result<Arc<dyn Kernel>> {
    if version != VERSION {
        return Err(refused(format!(
            "this build has version {VERSION}, not {version}"
        )));
    }
    if !params.arrays.is_empty() {
        return Err(refused("takes no Arrow parameters"));
    }
    Ok(Arc::new(GroupAggregate::new(&params.json)?))
}

/// A configuration refused, or an input of the wrong shape: a plan-time error.
pub(crate) fn refused(msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Plan(format!("{IDENTITY}: {msg}"))
}

/// A failure while running.
pub(crate) fn failed(msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Kernel {
        stage: StageId::MAX,
        seq: Seq::MAX,
        msg: format!("{IDENTITY}: {msg}"),
        features: None,
    }
}

/// An aggregate's operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggOp {
    /// Rows of the group, or a column's non-null values.
    Count,
    /// The sum of the non-null values.
    Sum,
    /// The least non-null value.
    Min,
    /// The greatest non-null value.
    Max,
    /// The sum over the count of the non-null values.
    Mean,
}

impl AggOp {
    fn parse(s: &str) -> Option<AggOp> {
        Some(match s {
            "count" => AggOp::Count,
            "sum" => AggOp::Sum,
            "min" => AggOp::Min,
            "max" => AggOp::Max,
            "mean" => AggOp::Mean,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            AggOp::Count => "count",
            AggOp::Sum => "sum",
            AggOp::Min => "min",
            AggOp::Max => "max",
            AggOp::Mean => "mean",
        }
    }
}

/// One aggregate: its operation, the column it reads (none for `count` of rows) and the name of
/// its output column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Aggregate {
    /// What it computes.
    pub op: AggOp,
    /// The column it reads.
    pub column: Option<String>,
    /// Its output column.
    pub output: String,
}

/// Which part of the work an instance does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Rows in, the final result out, one instance.
    Single,
    /// Rows in, a partial result out, one instance per CPU.
    Partial,
    /// Partial results in, the final result out, one instance.
    Final,
}

/// The path a configured kernel starts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// A declared domain, small enough.
    Dense,
    /// Any keys.
    Hash,
}

/// How `sum` of floats is computed. `@1` is [`FloatSum::Exact`]; the others exist to measure
/// it against (4G.4) and no job's arguments reach them.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatSum {
    /// Exact, rounded once: the specification's.
    Exact,
    /// Binned (ReproBLAS's fold 3): reproducible, not exact.
    Binned,
    /// In arrival order: neither.
    Plain,
}

/// `moruna.group_aggregate@1`, configured.
#[derive(Clone, Debug)]
pub struct GroupAggregate {
    float_sum: FloatSum,
    keys: Vec<String>,
    aggregates: Vec<Aggregate>,
    domain: Option<Vec<KeyRange>>,
    mode: Mode,
    instances: NonZeroUsize,
    path: Path,
    args: Value,
    fingerprint: Fingerprint,
}

const ARGS: [&str; 5] = ["keys", "aggregates", "domain", "mode", "instances"];

fn integer(v: &Value, what: &str) -> Result<i64> {
    v.as_i64()
        .ok_or_else(|| refused(format!("{what} must be an integer, not {v}")))
}

impl GroupAggregate {
    /// Configure from the job's arguments:
    ///
    /// ```json
    /// {"keys": ["zone", "minute_of_day"],
    ///  "aggregates": [{"op": "count", "as": "car_minutes"},
    ///                 {"op": "mean", "column": "fare", "as": "mean_fare"}],
    ///  "domain": {"zone": [0, 299], "minute_of_day": [0, 1439]},
    ///  "mode": "partial", "instances": 4}
    /// ```
    ///
    /// Anything else, a name used twice, or a missing argument, is refused naming it.
    pub fn new(args: &Value) -> Result<GroupAggregate> {
        let map: &Map<String, Value> = args
            .as_object()
            .ok_or_else(|| refused(format!("arguments must be an object, not {args}")))?;
        if let Some(unknown) = map.keys().find(|k| !ARGS.contains(&k.as_str())) {
            return Err(refused(format!(
                "unknown argument `{unknown}` (expected one of {})",
                ARGS.join(", ")
            )));
        }
        let keys: Vec<String> = match map.get("keys") {
            Some(Value::Array(items)) if !items.is_empty() => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| refused("`keys` must list column names"))
                })
                .collect::<Result<_>>()?,
            Some(Value::Array(_)) => {
                return Err(refused(
                    "`keys` names no column (a global aggregate is not this kernel)",
                ));
            }
            Some(other) => return Err(refused(format!("`keys` must be a list, not {other}"))),
            None => return Err(refused("`keys` is required")),
        };
        let mut aggregates = Vec::new();
        match map.get("aggregates") {
            Some(Value::Array(items)) => {
                for item in items {
                    aggregates.push(Self::aggregate(item)?);
                }
            }
            None => {}
            Some(other) => {
                return Err(refused(format!("`aggregates` must be a list, not {other}")));
            }
        }
        let mode = match map.get("mode") {
            None => Mode::Single,
            Some(Value::String(s)) => match s.as_str() {
                "single" => Mode::Single,
                "partial" => Mode::Partial,
                "final" => Mode::Final,
                _ => {
                    return Err(refused(format!(
                        "`mode` is single, partial or final, not {s}"
                    )));
                }
            },
            Some(other) => return Err(refused(format!("`mode` must be a string, not {other}"))),
        };
        let instances = match map.get("instances") {
            None => 1,
            Some(v) => match v.as_u64() {
                Some(n) if (1..=MAX_INSTANCES as u64).contains(&n) => n as usize,
                _ => {
                    return Err(refused(format!(
                        "`instances` is a whole number from 1 to {MAX_INSTANCES}, not {v}"
                    )));
                }
            },
        };
        if instances > 1 && mode != Mode::Partial {
            return Err(refused(
                "only `partial` mode runs more than one instance; `single` and `final` run one",
            ));
        }
        let domain = match map.get("domain") {
            None => None,
            Some(Value::Object(ranges)) => {
                if let Some(extra) = ranges.keys().find(|k| !keys.contains(k)) {
                    return Err(refused(format!(
                        "`domain` names `{extra}`, which is not a key"
                    )));
                }
                let mut out = Vec::with_capacity(keys.len());
                for key in &keys {
                    let range = ranges.get(key).ok_or_else(|| {
                        refused(format!(
                            "`domain` must declare every key's range, and `{key}` has none"
                        ))
                    })?;
                    let pair = range.as_array().filter(|a| a.len() == 2).ok_or_else(|| {
                        refused(format!("`domain.{key}` must be [lo, hi], not {range}"))
                    })?;
                    let lo = integer(&pair[0], &format!("`domain.{key}`'s lo"))?;
                    let hi = integer(&pair[1], &format!("`domain.{key}`'s hi"))?;
                    if lo > hi {
                        return Err(refused(format!("`domain.{key}` is empty: [{lo}, {hi}]")));
                    }
                    out.push(KeyRange { lo, hi });
                }
                Some(out)
            }
            Some(other) => {
                return Err(refused(format!(
                    "`domain` must map each key to [lo, hi], not {other}"
                )));
            }
        };
        let mut names: Vec<&str> = keys.iter().map(String::as_str).collect();
        let means: Vec<String> = aggregates
            .iter()
            .filter(|a| a.op == AggOp::Mean && mode != Mode::Single)
            .flat_map(|a| [format!("{}.sum", a.output), format!("{}.count", a.output)])
            .collect();
        names.extend(aggregates.iter().map(|a| a.output.as_str()));
        names.extend(means.iter().map(String::as_str));
        for (i, name) in names.iter().enumerate() {
            if names[..i].contains(name) {
                return Err(refused(format!("the column `{name}` is named twice")));
            }
        }
        let mut kernel = GroupAggregate {
            float_sum: FloatSum::Exact,
            keys,
            aggregates,
            domain,
            mode,
            instances: NonZeroUsize::new(instances).unwrap_or(NonZeroUsize::MIN),
            path: Path::Hash,
            args: args.clone(),
            fingerprint: crate::std_fingerprint(IDENTITY, args),
        };
        if kernel
            .dense_bytes()
            .is_some_and(|bytes| bytes <= DENSE_MAX_BYTES)
        {
            kernel.path = Path::Dense;
        }
        Ok(kernel)
    }

    fn aggregate(item: &Value) -> Result<Aggregate> {
        let obj = item
            .as_object()
            .ok_or_else(|| refused(format!("an aggregate must be an object, not {item}")))?;
        if let Some(unknown) = obj
            .keys()
            .find(|k| !["op", "column", "as"].contains(&k.as_str()))
        {
            return Err(refused(format!(
                "an aggregate has op, column and as, not `{unknown}`"
            )));
        }
        let text = |key: &str| -> Result<Option<String>> {
            match obj.get(key) {
                None => Ok(None),
                Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
                Some(other) => Err(refused(format!(
                    "an aggregate's `{key}` must be a name, not {other}"
                ))),
            }
        };
        let op_name = text("op")?.ok_or_else(|| refused("an aggregate needs `op`"))?;
        let op = AggOp::parse(&op_name).ok_or_else(|| {
            refused(format!(
                "`{op_name}` is not an aggregate (count, sum, min, max, mean)"
            ))
        })?;
        let column = text("column")?;
        if column.is_none() && op != AggOp::Count {
            return Err(refused(format!("`{op_name}` needs a `column`")));
        }
        let output = text("as")?.ok_or_else(|| {
            refused(format!(
                "the aggregate `{op_name}` needs `as`, its output's name"
            ))
        })?;
        Ok(Aggregate { op, column, output })
    }

    /// The same kernel with `sum` of floats computed another way, in `single` mode: for
    /// measurement only (4G.4), not part of `@1`.
    #[doc(hidden)]
    pub fn measure_float_sum(&self, how: FloatSum) -> GroupAggregate {
        GroupAggregate {
            float_sum: how,
            ..self.clone()
        }
    }

    /// The keys, in order.
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// The aggregates, in order.
    pub fn aggregates(&self) -> &[Aggregate] {
        &self.aggregates
    }

    /// The mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The path an instance starts on.
    pub fn path(&self) -> Path {
        self.path
    }

    /// The canonical arguments it was configured with.
    pub fn args(&self) -> &Value {
        &self.args
    }

    /// The same kernel in another mode (one instance unless `partial`): what a platform that
    /// splits a `single` stage into per-CPU `partial` instances and one `final` merge builds.
    pub fn in_mode(&self, mode: Mode, instances: usize) -> Result<GroupAggregate> {
        let mut args = self.args.clone();
        if let Value::Object(map) = &mut args {
            map.insert(
                "mode".into(),
                Value::String(
                    match mode {
                        Mode::Single => "single",
                        Mode::Partial => "partial",
                        Mode::Final => "final",
                    }
                    .into(),
                ),
            );
            if instances > 1 {
                map.insert("instances".into(), Value::from(instances));
            } else {
                map.remove("instances");
            }
        }
        GroupAggregate::new(&args)
    }

    /// The bytes of one instance's dense state, when a domain is declared: its cells times the
    /// most each takes. `None` without a domain, or past `u64`.
    pub fn dense_bytes(&self) -> Option<u64> {
        let domain = self.domain.as_ref()?;
        let mut cells: u128 = 1;
        for r in domain {
            cells = cells.checked_mul(r.slots())?;
        }
        let per_cell: u64 = 8 + self
            .aggregates
            .iter()
            .map(|a| bytes_per_group(a.op))
            .sum::<u64>();
        u64::try_from(cells.checked_mul(per_cell as u128)?).ok()
    }

    fn column<'a>(&self, schema: &'a Schema, name: &str) -> Result<(usize, &'a FieldRef)> {
        schema
            .column_with_name(name)
            .map(|(i, _)| (i, &schema.fields()[i]))
            .ok_or_else(|| {
                refused(format!(
                    "the input has no column `{name}` (it has {})",
                    schema
                        .fields()
                        .iter()
                        .map(|f| f.name().as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// What one instance does with batches of `input`, or why it cannot.
    fn plan(&self, input: &SchemaRef) -> Result<Plan> {
        let mut key_cols = Vec::new();
        let mut key_fields = Vec::new();
        for key in &self.keys {
            let (i, field) = self.column(input, key)?;
            if !key_type_ok(field.data_type()) {
                return Err(refused(format!(
                    "the key `{key}` is {}, and keys are fixed-width, string or binary types",
                    field.data_type()
                )));
            }
            if self.domain.is_some() && !dense_type_ok(field.data_type()) {
                return Err(refused(format!(
                    "the key `{key}` is {}, and a declared domain is of integer keys",
                    field.data_type()
                )));
            }
            key_cols.push(i);
            key_fields.push(Arc::clone(field));
        }
        let mut aggs = Vec::new();
        for a in &self.aggregates {
            aggs.push(match self.mode {
                Mode::Final => self.plan_merge(a, input)?,
                _ => self.plan_rows(a, input)?,
            });
        }
        let final_schema = Arc::new(Schema::new(
            key_fields
                .iter()
                .cloned()
                .chain(aggs.iter().map(|p| {
                    Arc::new(Field::new(
                        &p.name,
                        p.final_ty.clone(),
                        p.op != AggOp::Count,
                    ))
                }))
                .collect::<Vec<FieldRef>>(),
        ));
        let partial_schema = Arc::new(Schema::new(
            key_fields
                .iter()
                .cloned()
                .chain(aggs.iter().flat_map(|p| p.partial_fields()))
                .collect::<Vec<FieldRef>>(),
        ));
        let output = match self.mode {
            Mode::Partial => Arc::clone(&partial_schema),
            _ => Arc::clone(&final_schema),
        };
        Ok(Plan {
            input: Arc::clone(input),
            key_cols,
            key_types: key_fields.iter().map(|f| f.data_type().clone()).collect(),
            aggs,
            output,
            partial: partial_schema,
        })
    }

    fn plan_rows(&self, a: &Aggregate, input: &Schema) -> Result<AggPlan> {
        let Some(column) = &a.column else {
            return Ok(AggPlan {
                op: a.op,
                class: Class::Signed,
                cols: Vec::new(),
                final_ty: DataType::Int64,
                name: a.output.clone(),
            });
        };
        let (i, field) = self.column(input, column)?;
        let dt = field.data_type();
        let class = class_of(a.op, dt).ok_or_else(|| {
            refused(format!(
                "`{}` does not take `{column}`, which is {dt}",
                a.op.name()
            ))
        })?;
        Ok(AggPlan {
            op: a.op,
            class,
            cols: vec![i],
            final_ty: final_type(a.op, class, dt),
            name: a.output.clone(),
        })
    }

    /// An aggregate's partial columns in a partial result, by its output's name.
    fn plan_merge(&self, a: &Aggregate, input: &Schema) -> Result<AggPlan> {
        let wrong = |name: &str, f: &Field, want: &str| {
            refused(format!(
                "`{name}` is {}, and a partial `{}` is {want}",
                f.data_type(),
                a.op.name()
            ))
        };
        let state_class = |name: &str, f: &Field| -> Result<Class> {
            match f.data_type() {
                DataType::Decimal128(38, 0) => {
                    match f.metadata().get(SUM_TYPE).map(String::as_str) {
                        Some("uint64") => Ok(Class::Unsigned),
                        _ => Ok(Class::Signed),
                    }
                }
                DataType::Binary => Ok(Class::Float),
                _ => Err(wrong(name, f, "Decimal128(38, 0) or Binary")),
            }
        };
        let (cols, class, final_ty) = match a.op {
            AggOp::Count => {
                let (i, f) = self.column(input, &a.output)?;
                if f.data_type() != &DataType::Int64 {
                    return Err(wrong(&a.output, f, "Int64"));
                }
                (vec![i], Class::Signed, DataType::Int64)
            }
            AggOp::Sum => {
                let (i, f) = self.column(input, &a.output)?;
                let class = state_class(&a.output, f)?;
                (
                    vec![i],
                    class,
                    final_type(AggOp::Sum, class, &DataType::Int64),
                )
            }
            AggOp::Min | AggOp::Max => {
                let (i, f) = self.column(input, &a.output)?;
                let class = class_of(a.op, f.data_type())
                    .ok_or_else(|| wrong(&a.output, f, "a number or a time"))?;
                (vec![i], class, f.data_type().clone())
            }
            AggOp::Mean => {
                let sum = format!("{}.sum", a.output);
                let count = format!("{}.count", a.output);
                let (i, f) = self.column(input, &sum)?;
                let class = state_class(&sum, f)?;
                let (j, g) = self.column(input, &count)?;
                if g.data_type() != &DataType::Int64 {
                    return Err(wrong(&count, g, "Int64"));
                }
                (vec![i, j], class, DataType::Float64)
            }
        };
        Ok(AggPlan {
            op: a.op,
            class,
            cols,
            final_ty,
            name: a.output.clone(),
        })
    }

    fn state<'s>(&self, st: &'s mut dyn KernelState) -> Result<&'s mut GroupState> {
        st.as_any_mut()
            .downcast_mut::<GroupState>()
            .ok_or_else(|| failed("was given a state it did not make"))
    }

    /// Start an instance's state on its first batch's schema.
    fn start(&self, st: &mut GroupState, input: &SchemaRef) -> Result<()> {
        let plan = self.plan(input)?;
        st.groups = Some(match (&self.domain, self.path) {
            (Some(domain), Path::Dense) => Groups::Dense(DenseGroups::new(domain)),
            _ => Groups::Hash(HashGroups::new(&plan.key_types)?),
        });
        st.accs = plan
            .aggs
            .iter()
            .map(|p| match (p.op, p.class, self.float_sum) {
                (AggOp::Sum, Class::Float, FloatSum::Binned) => Acc::SumBinned {
                    sum: Vec::new(),
                    seen: Vec::new(),
                },
                (AggOp::Sum, Class::Float, FloatSum::Plain) => Acc::SumPlain {
                    sum: Vec::new(),
                    seen: Vec::new(),
                },
                _ => Acc::new(p.op, p.class, p.cols.is_empty() && self.mode != Mode::Final),
            })
            .collect();
        st.plan = Some(Arc::new(plan));
        Ok(())
    }

    /// Find each row's group, leaving the dense path when a value is outside its range.
    fn assign(&self, st: &mut GroupState, plan: &Plan, keys: &[ArrayRef]) -> Result<()> {
        loop {
            let groups = st.groups.as_mut().ok_or_else(|| failed("has no groups"))?;
            match groups.assign(keys, &mut st.ids)? {
                Ok(()) => break,
                Err(d) => {
                    let msg = format!(
                        "{IDENTITY}: dense path declined: value {} outside [{}, {}] (key `{}`); \
                         this instance continues on the hash path",
                        d.value, d.range.lo, d.range.hi, self.keys[d.key]
                    );
                    output::log(Level::Warning, &msg);
                    let (hash, keep) = groups.to_hash(&st.rows, &plan.key_types)?;
                    st.rows = keep.iter().map(|&c| st.rows[c as usize]).collect();
                    for acc in &mut st.accs {
                        acc.compact(&keep);
                    }
                    st.groups = Some(hash);
                    st.declined = Some(msg);
                }
            }
        }
        let len = st.groups.as_ref().map_or(0, Groups::len);
        if st.rows.len() < len {
            st.rows.resize(len, 0);
            for acc in &mut st.accs {
                acc.grow(len);
            }
        }
        Ok(())
    }

    /// Take one batch of rows (or, in `final` mode, of partial results).
    fn take_batch(&self, st: &mut GroupState, batch: &RecordBatch) -> Result<()> {
        if st.plan.is_none() {
            self.start(st, &batch.schema())?;
        }
        let plan = Arc::clone(st.plan.as_ref().ok_or_else(|| failed("has no plan"))?);
        plan.check(batch)?;
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let keys: Vec<ArrayRef> = plan
            .key_cols
            .iter()
            .map(|&i| canonical_key(batch.column(i)))
            .collect();
        self.assign(st, &plan, &keys)?;
        for &g in &st.ids {
            st.rows[g as usize] += 1;
        }
        for (acc, p) in st.accs.iter_mut().zip(&plan.aggs) {
            match (self.mode, p.op) {
                (Mode::Final, _) => {
                    let state = p.state_columns(batch, &p.cols)?;
                    acc.merge(&st.ids, &state)?;
                }
                (_, AggOp::Count) => acc.update(&st.ids, p.cols.first().map(|&c| batch.column(c))),
                _ => {
                    let values = physical(batch.column(p.cols[0]), p.class)?;
                    acc.update(&st.ids, Some(&values));
                }
            }
        }
        Ok(())
    }

    /// Merge a partial result into a `single` or `partial` instance (a restore).
    fn take_partial(&self, st: &mut GroupState, partial: &RecordBatch) -> Result<()> {
        let plan = Arc::clone(st.plan.as_ref().ok_or_else(|| failed("has no plan"))?);
        if partial.schema().fields() != plan.partial.fields() {
            return Err(MorunaError::Resume(format!(
                "{IDENTITY}: the checkpoint's partial result does not match the input's plan"
            )));
        }
        if partial.num_rows() == 0 {
            return Ok(());
        }
        let k = plan.key_cols.len();
        let keys: Vec<ArrayRef> = partial.columns()[..k].to_vec();
        self.assign(st, &plan, &keys)?;
        let mut at = k;
        let mut rows_from: Option<ArrayRef> = None;
        for (acc, p) in st.accs.iter_mut().zip(&plan.aggs) {
            let n = if p.op == AggOp::Mean { 2 } else { 1 };
            let cols: Vec<usize> = (at..at + n).collect();
            at += n;
            let state = p.state_columns(partial, &cols)?;
            if matches!(acc, Acc::Rows) {
                rows_from = Some(Arc::clone(&state[0]));
            }
            acc.merge(&st.ids, &state)?;
        }
        match rows_from {
            Some(counts) => {
                let counts = counts
                    .as_any()
                    .downcast_ref::<moruna_kernel::arrow::array::Int64Array>()
                    .ok_or_else(|| failed("a partial count is not Int64"))?;
                for (row, &g) in st.ids.iter().enumerate() {
                    st.rows[g as usize] += counts.value(row);
                }
            }
            None => {
                for &g in &st.ids {
                    st.rows[g as usize] += 1;
                }
            }
        }
        Ok(())
    }

    /// The groups seen so far, as the result `schema` names: final or partial.
    fn emit(&self, st: &GroupState, partial: bool) -> Result<Option<RecordBatch>> {
        let (Some(plan), Some(groups)) = (&st.plan, &st.groups) else {
            return Ok(None);
        };
        let (order, keys) = groups.emit(&st.rows, &plan.key_types)?;
        if order.is_empty() {
            return Ok(None);
        }
        let mut columns = keys.clone();
        for (acc, p) in st.accs.iter().zip(&plan.aggs) {
            if partial {
                columns.extend(acc.emit_partial(&order, &st.rows, &p.final_ty)?);
            } else {
                match acc.emit_final(&order, &st.rows, &p.final_ty)? {
                    Ok(a) => columns.push(a),
                    Err(o) => {
                        let key: Vec<String> = keys
                            .iter()
                            .map(|k| array_value_to_string(k, o.row).unwrap_or_default())
                            .collect();
                        return Err(failed(format!(
                            "the sum `{}` of the group ({}) is {}, past {}",
                            p.name,
                            key.join(", "),
                            o.total,
                            p.final_ty
                        )));
                    }
                }
            }
        }
        let schema = if partial {
            Arc::clone(&plan.partial)
        } else {
            Arc::clone(&plan.output)
        };
        RecordBatch::try_new(schema, columns)
            .map(Some)
            .map_err(groups::arrow_err)
    }
}

/// The final type of `op` over values of `dt` in `class`.
fn final_type(op: AggOp, class: Class, dt: &DataType) -> DataType {
    match (op, class) {
        (AggOp::Count, _) => DataType::Int64,
        (AggOp::Sum, Class::Signed) => DataType::Int64,
        (AggOp::Sum, Class::Unsigned) => DataType::UInt64,
        (AggOp::Sum, Class::Float) | (AggOp::Mean, _) => DataType::Float64,
        (AggOp::Min | AggOp::Max, _) => dt.clone(),
    }
}

/// What one instance does with batches of one schema.
struct Plan {
    input: SchemaRef,
    key_cols: Vec<usize>,
    key_types: Vec<DataType>,
    aggs: Vec<AggPlan>,
    output: SchemaRef,
    partial: SchemaRef,
}

impl Plan {
    /// A later batch must carry the first one's columns, by type.
    fn check(&self, batch: &RecordBatch) -> Result<()> {
        let schema = batch.schema();
        let used = self
            .key_cols
            .iter()
            .chain(self.aggs.iter().flat_map(|p| p.cols.iter()));
        for &i in used {
            let want = self.input.field(i);
            let same = schema
                .fields()
                .get(i)
                .is_some_and(|f| f.name() == want.name() && f.data_type() == want.data_type());
            if !same {
                return Err(failed(format!(
                    "a batch's columns differ from the first batch's: `{}` was {} at {i}",
                    want.name(),
                    want.data_type()
                )));
            }
        }
        Ok(())
    }
}

/// One aggregate of a plan.
struct AggPlan {
    op: AggOp,
    class: Class,
    /// The value column (rows), the partial columns (`final`), or none (`count` of rows).
    cols: Vec<usize>,
    final_ty: DataType,
    name: String,
}

impl AggPlan {
    fn partial_fields(&self) -> Vec<FieldRef> {
        let sum_ty = |class| match class {
            Class::Float => DataType::Binary,
            _ => DataType::Decimal128(38, 0),
        };
        match self.op {
            AggOp::Count => vec![Arc::new(Field::new(&self.name, DataType::Int64, false))],
            AggOp::Sum => {
                let named = match self.class {
                    Class::Signed => "int64",
                    Class::Unsigned => "uint64",
                    Class::Float => "float64",
                };
                vec![Arc::new(
                    Field::new(&self.name, sum_ty(self.class), true).with_metadata(
                        [(SUM_TYPE.to_string(), named.to_string())]
                            .into_iter()
                            .collect(),
                    ),
                )]
            }
            AggOp::Min | AggOp::Max => {
                vec![Arc::new(Field::new(
                    &self.name,
                    self.final_ty.clone(),
                    true,
                ))]
            }
            AggOp::Mean => vec![
                Arc::new(Field::new(
                    format!("{}.sum", self.name),
                    sum_ty(self.class),
                    false,
                )),
                Arc::new(Field::new(
                    format!("{}.count", self.name),
                    DataType::Int64,
                    false,
                )),
            ],
        }
    }

    /// This aggregate's partial columns `cols` of `batch`, minima and maxima in their class.
    fn state_columns(&self, batch: &RecordBatch, cols: &[usize]) -> Result<Vec<ArrayRef>> {
        cols.iter()
            .map(|&c| {
                let col = batch.column(c);
                match self.op {
                    AggOp::Min | AggOp::Max => physical(col, self.class),
                    _ => Ok(Arc::clone(col)),
                }
            })
            .collect()
    }
}

/// One instance's state.
pub struct GroupState {
    plan: Option<Arc<Plan>>,
    groups: Option<Groups>,
    rows: Vec<i64>,
    accs: Vec<Acc>,
    ids: Vec<u32>,
    declined: Option<String>,
    checkpoint_of: Option<GroupAggregate>,
}

impl GroupState {
    fn new(kernel: &GroupAggregate) -> GroupState {
        GroupState {
            plan: None,
            groups: None,
            rows: Vec::new(),
            accs: Vec::new(),
            ids: Vec::new(),
            declined: None,
            checkpoint_of: Some(kernel.clone()),
        }
    }

    /// Whether this instance is on the dense path (`None` before its first batch).
    pub fn dense(&self) -> Option<bool> {
        self.groups.as_ref().map(Groups::is_dense)
    }

    /// Why this instance left the dense path, when it did.
    pub fn declined(&self) -> Option<&str> {
        self.declined.as_deref()
    }

    /// Groups (on the dense path, cells) this instance holds.
    pub fn groups(&self) -> usize {
        self.groups.as_ref().map_or(0, Groups::len)
    }
}

impl KernelState for GroupState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// The instance's partial result, after the schema of the input it was planned for: two
    /// Arrow IPC streams, each after its length as a little-endian u64. Empty before the first
    /// batch.
    fn checkpoint(&mut self) -> Result<Option<Vec<u8>>> {
        let (Some(plan), Some(kernel)) = (&self.plan, &self.checkpoint_of) else {
            return Ok(Some(Vec::new()));
        };
        // In every mode the state is what it has merged so far, in the partial form.
        let partial = kernel.emit(self, true)?;
        let mut out = Vec::new();
        for (schema, batch) in [
            (Arc::clone(&plan.input), None),
            (Arc::clone(&plan.partial), partial),
        ] {
            let mut bytes = Vec::new();
            {
                let mut w =
                    StreamWriter::try_new(&mut bytes, &schema).map_err(groups::arrow_err)?;
                if let Some(batch) = &batch {
                    w.write(batch).map_err(groups::arrow_err)?;
                }
                w.finish().map_err(groups::arrow_err)?;
            }
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(&bytes);
        }
        Ok(Some(out))
    }

    fn footprint(&self) -> Result<Option<u64>> {
        let rows = (self.rows.capacity() * 8) as u64;
        let ids = (self.ids.capacity() * 4) as u64;
        let groups = self.groups.as_ref().map_or(0, Groups::bytes);
        let accs: u64 = self.accs.iter().map(Acc::bytes).sum();
        Ok(Some(rows + ids + groups + accs))
    }
}

impl GroupAggregate {
    fn restore_state(&self, bytes: &[u8]) -> Result<GroupState> {
        let mut st = GroupState::new(self);
        if bytes.is_empty() {
            return Ok(st);
        }
        let bad = || MorunaError::Resume(format!("{IDENTITY}: the checkpoint is malformed"));
        let mut streams = Vec::new();
        let mut at = 0usize;
        while at < bytes.len() {
            let len = bytes
                .get(at..at + 8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(bad)? as usize;
            let stream = bytes.get(at + 8..at + 8 + len).ok_or_else(bad)?;
            streams.push(stream);
            at += 8 + len;
        }
        let [input, partial] = streams[..] else {
            return Err(bad());
        };
        let input = StreamReader::try_new(input, None)
            .map_err(|_| bad())?
            .schema();
        let reader = StreamReader::try_new(partial, None).map_err(|_| bad())?;
        let batches = reader
            .collect::<core::result::Result<Vec<_>, _>>()
            .map_err(|_| bad())?;
        self.start(&mut st, &input)?;
        for batch in &batches {
            self.take_partial(&mut st, batch)?;
        }
        Ok(st)
    }
}

impl Kernel for GroupAggregate {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: self.instances,
        }
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: Some(1.0),
            uses_device_memory: false,
            releases_gil: Some(true),
            preferred_rows: None,
            resume: ResumePolicy::Checkpoint,
            state_bytes: match self.path {
                Path::Dense => self.dense_bytes(),
                Path::Hash => None,
            },
        }
    }

    fn declared(&self) -> Declared {
        let mut input: Vec<ColumnDecl> = self.keys.iter().map(ColumnDecl::any).collect();
        let mut output: Vec<ColumnDecl> = input.clone();
        for a in &self.aggregates {
            match self.mode {
                Mode::Final if a.op == AggOp::Mean => {
                    input.push(ColumnDecl::any(format!("{}.sum", a.output)));
                    input.push(ColumnDecl::new(
                        format!("{}.count", a.output),
                        DataType::Int64,
                    ));
                }
                Mode::Final => input.push(ColumnDecl::any(&a.output)),
                _ => {
                    if let Some(c) = &a.column
                        && !input.iter().any(|d| &d.name == c)
                    {
                        input.push(ColumnDecl::any(c));
                    }
                }
            }
            let partial_mean = self.mode == Mode::Partial && a.op == AggOp::Mean;
            match a.op {
                _ if partial_mean => {
                    output.push(ColumnDecl::any(format!("{}.sum", a.output)));
                    output.push(ColumnDecl {
                        nullable: false,
                        ..ColumnDecl::new(format!("{}.count", a.output), DataType::Int64)
                    });
                }
                AggOp::Count => output.push(ColumnDecl {
                    nullable: false,
                    ..ColumnDecl::new(&a.output, DataType::Int64)
                }),
                AggOp::Mean => output.push(ColumnDecl::new(&a.output, DataType::Float64)),
                _ => output.push(ColumnDecl::any(&a.output)),
            }
        }
        Declared {
            input: Some(SchemaDecl::Subset(input)),
            output: Some(SchemaDecl::Exact(output)),
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        match input {
            SourceSchema::Table(schema) => Ok(SourceSchema::Table(self.plan(schema)?.output)),
            SourceSchema::Tensor { .. } => Err(refused("takes a table, and the input is a tensor")),
        }
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(GroupState::new(self)))
    }

    fn restore(&self, _ctx: &InitCtx, state: &[u8]) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(self.restore_state(state)?))
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Payload::Table(batch, _) = &input else {
            return Err(refused("takes a table, and was given a tensor"));
        };
        let st = self.state(state)?;
        self.take_batch(st, batch)?;
        let plan = st.plan.as_ref().ok_or_else(|| failed("has no plan"))?;
        let empty = RecordBatch::new_empty(Arc::clone(&plan.output));
        drop(input);
        Payload::table(empty)
    }

    fn finish(&self, state: &mut dyn KernelState) -> Result<Option<Payload>> {
        let st = self.state(state)?;
        match self.emit(st, self.mode == Mode::Partial)? {
            Some(batch) => Payload::table(batch).map(Some),
            None => Ok(None),
        }
    }
}

/// Run `kernel` over `batches` in one instance and give what `finish` gives: the reference's
/// answer, for tests, the conformance runner and measurements.
pub fn run(kernel: &GroupAggregate, batches: &[RecordBatch]) -> Result<Option<RecordBatch>> {
    let mut st = GroupState::new(kernel);
    for batch in batches {
        kernel.take_batch(&mut st, batch)?;
    }
    kernel.emit(&st, kernel.mode == Mode::Partial)
}

/// Run `kernel` (in `partial` mode) as `instances` instances, batch `i` to instance
/// `i % instances`, then merge their partial results in one `final` instance: what the
/// platform does with one instance per CPU (6.3e). Gives the final result, and each instance's
/// state for inspection.
pub fn run_split(
    kernel: &GroupAggregate,
    batches: &[RecordBatch],
    instances: usize,
) -> Result<(Option<RecordBatch>, Vec<GroupState>)> {
    let partial = kernel.in_mode(Mode::Partial, instances)?;
    let merge = kernel.in_mode(Mode::Final, 1)?;
    let mut states: Vec<GroupState> = (0..instances.max(1))
        .map(|_| GroupState::new(&partial))
        .collect();
    for (i, batch) in batches.iter().enumerate() {
        let n = states.len();
        partial.take_batch(&mut states[i % n], batch)?;
    }
    let mut last = GroupState::new(&merge);
    for st in &states {
        if let Some(p) = partial.emit(st, true)? {
            merge.take_batch(&mut last, &p)?;
        }
    }
    Ok((merge.emit(&last, false)?, states))
}

/// Run one instance over `batches` and return its state, for inspection.
pub fn run_state(kernel: &GroupAggregate, batches: &[RecordBatch]) -> Result<GroupState> {
    let mut st = GroupState::new(kernel);
    for batch in batches {
        kernel.take_batch(&mut st, batch)?;
    }
    Ok(st)
}

/// The batch a state's instance gives at `finish`.
pub fn finish_state(kernel: &GroupAggregate, st: &GroupState) -> Result<Option<RecordBatch>> {
    kernel.emit(st, kernel.mode == Mode::Partial)
}

#[cfg(test)]
mod tests;
