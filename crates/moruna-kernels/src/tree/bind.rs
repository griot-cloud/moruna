//! Checking a tree against an input schema, and compiling it (spec/expr.md, "Types").
//!
//! Binding resolves every column, every function and every implicit cast, and gives each node
//! its Arrow type. Identical subtrees (after binding) become one instruction, evaluated once per
//! chunk however many outputs use them. The instructions are in evaluation order, each writing
//! one register; a register is reused once the last instruction reading it has run, so the
//! working set is a few registers of one chunk each, whatever the tree's size.

use std::collections::HashMap;
use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, StringArray, new_null_array,
};
use moruna_kernel::arrow::compute::kernels::cast;
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use moruna_kernel::{MorunaError, Result};
use serde_json::{Map, Value};

use super::round::RoundMode;
use super::temporal::{self, Part, Zone};
use super::{Node, Tree, spell_data_type};

/// Every function of the vocabulary, by its Arrow compute name.
pub const FUNCTIONS: &[&str] = &[
    // Arithmetic.
    "add",
    "add_checked",
    "subtract",
    "subtract_checked",
    "multiply",
    "multiply_checked",
    "divide",
    "divide_checked",
    "power",
    "power_checked",
    "negate",
    "negate_checked",
    "abs",
    "abs_checked",
    "sign",
    // Unary maths.
    "sqrt",
    "sqrt_checked",
    "exp",
    "ln",
    "ln_checked",
    "log10",
    "log10_checked",
    "log2",
    "log2_checked",
    "log1p",
    "log1p_checked",
    "sin",
    "sin_checked",
    "cos",
    "cos_checked",
    "tan",
    "tan_checked",
    "asin",
    "asin_checked",
    "acos",
    "acos_checked",
    "atan",
    "atan2",
    // Rounding.
    "round",
    "floor",
    "ceil",
    "trunc",
    // Comparison.
    "equal",
    "not_equal",
    "less",
    "less_equal",
    "greater",
    "greater_equal",
    // Boolean logic.
    "and",
    "and_kleene",
    "or",
    "or_kleene",
    "xor",
    "and_not",
    "and_not_kleene",
    "invert",
    // Nulls and selection.
    "is_null",
    "is_valid",
    "is_nan",
    "is_finite",
    "is_inf",
    "if_else",
    "coalesce",
    "fill_null",
    "min_element_wise",
    "max_element_wise",
    // Temporal parts.
    "year",
    "quarter",
    "month",
    "day",
    "day_of_week",
    "day_of_year",
    "iso_year",
    "iso_week",
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
];

/// How values of a type are held while a chunk is evaluated.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Lane {
    /// Booleans, one byte each.
    Bool,
    /// Signed integers of any width, dates, timestamps and durations, as `i64`.
    I64,
    /// Unsigned integers of any width, as `u64`.
    U64,
    /// Singles.
    F32,
    /// Doubles.
    F64,
    /// UTF-8 strings, borrowed from the input or a literal.
    Str,
    /// Any other type: compared and tested for null through Arrow's kernels, never computed.
    Opaque,
}

/// The lane of a type.
pub(crate) fn lane(dt: &DataType) -> Lane {
    match dt {
        DataType::Boolean => Lane::Bool,
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::Date32
        | DataType::Timestamp(..)
        | DataType::Duration(_) => Lane::I64,
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => Lane::U64,
        DataType::Float32 => Lane::F32,
        DataType::Float64 => Lane::F64,
        DataType::Utf8 | DataType::LargeUtf8 => Lane::Str,
        _ => Lane::Opaque,
    }
}

/// An integer type's width and signedness.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IntTy {
    pub bits: u32,
    pub signed: bool,
}

impl IntTy {
    pub(crate) fn of(dt: &DataType) -> Option<IntTy> {
        let (bits, signed) = match dt {
            DataType::Int8 => (8, true),
            DataType::Int16 => (16, true),
            DataType::Int32 | DataType::Date32 => (32, true),
            DataType::Int64 | DataType::Timestamp(..) | DataType::Duration(_) => (64, true),
            DataType::UInt8 => (8, false),
            DataType::UInt16 => (16, false),
            DataType::UInt32 => (32, false),
            DataType::UInt64 => (64, false),
            _ => return None,
        };
        Some(IntTy { bits, signed })
    }

    pub(crate) fn min(self) -> i128 {
        if self.signed {
            -(1i128 << (self.bits - 1))
        } else {
            0
        }
    }

    pub(crate) fn max(self) -> i128 {
        if self.signed {
            (1i128 << (self.bits - 1)) - 1
        } else {
            (1i128 << self.bits) - 1
        }
    }

    pub(crate) fn fits(self, v: i128) -> bool {
        v >= self.min() && v <= self.max()
    }

    /// `v` wrapped into the type (two's complement), as an `i64` lane holds it.
    pub(crate) fn wrap_i(self, v: i64) -> i64 {
        if self.bits >= 64 {
            v
        } else {
            let s = 64 - self.bits;
            (v << s) >> s
        }
    }

    /// `v` wrapped into the type, as a `u64` lane holds it.
    pub(crate) fn wrap_u(self, v: u64) -> u64 {
        if self.bits >= 64 {
            v
        } else {
            v & ((1u64 << self.bits) - 1)
        }
    }
}

/// A binary arithmetic operation.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
}

/// A unary maths function.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Math {
    Sqrt,
    Exp,
    Ln,
    Log10,
    Log2,
    Log1p,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
}

/// A comparison.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A binary Boolean function.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Logic {
    And,
    AndKleene,
    Or,
    OrKleene,
    Xor,
    AndNot,
    AndNotKleene,
}

/// A literal's value in its lane.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Lit {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F32(f32),
    F64(f64),
    Str(String),
    /// An opaque type's one-element array.
    Opaque(ArrayRef),
}

/// What an instruction computes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Op {
    Column(usize),
    Literal(Lit),
    Cast { safe: bool },
    Arith { op: Arith, checked: bool },
    Neg { checked: bool },
    Abs { checked: bool },
    Sign,
    Math { f: Math, checked: bool },
    Atan2,
    Round { ndigits: i64, mode: RoundMode },
    Floor,
    Ceil,
    Trunc,
    Cmp { op: Cmp, total: bool },
    CmpOpaque { op: Cmp },
    Logic(Logic),
    Invert,
    IsNull { nan_is_null: bool },
    IsValid,
    IsNan,
    IsFinite,
    IsInf,
    IfElse,
    CaseWhen { has_else: bool },
    Coalesce,
    MinMax { max: bool, skip_nulls: bool },
    Part(Part),
}

/// One instruction: an operation on earlier instructions' values, of one type.
#[derive(Clone, Debug)]
pub(crate) struct Instr {
    pub op: Op,
    pub args: Vec<usize>,
    pub ty: DataType,
    pub lane: Lane,
    /// The function, for errors.
    pub name: String,
    /// The register it writes.
    pub reg: usize,
    /// It depends on no column, so it is computed once.
    pub constant: bool,
    /// For a timestamp argument of a temporal part or a cast to a date: its zone.
    pub zone: Option<Zone>,
}

/// One output of the program.
#[derive(Clone, Debug)]
pub(crate) struct Out {
    pub instr: usize,
    /// The input column it passes through unchanged, zero copy.
    pub column: Option<usize>,
}

/// A tree checked against an input schema and compiled: what [`Program::eval`] runs.
#[derive(Clone, Debug)]
pub struct Program {
    pub(crate) instrs: Vec<Instr>,
    pub(crate) outputs: Vec<Out>,
    pub(crate) regs: usize,
    pub(crate) input: SchemaRef,
    schema: SchemaRef,
}

impl Program {
    /// The output schema: one field per output, in order. A column passed through keeps its
    /// field; every computed output is nullable.
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// The input schema the program was checked against.
    pub fn input(&self) -> &SchemaRef {
        &self.input
    }

    /// How many instructions one chunk runs, after identical subtrees are merged.
    pub fn instructions(&self) -> usize {
        self.instrs.len()
    }

    /// How many chunk-sized registers the program evaluates in.
    pub fn registers(&self) -> usize {
        self.regs
    }
}

fn refuse(at: &str, msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Plan(format!("moruna.std.expr: {at}: {msg}"))
}

fn is_int(dt: &DataType) -> bool {
    matches!(lane(dt), Lane::I64 | Lane::U64) && dt.is_integer()
}

fn is_float(dt: &DataType) -> bool {
    matches!(dt, DataType::Float32 | DataType::Float64)
}

fn is_numeric(dt: &DataType) -> bool {
    is_int(dt) || is_float(dt)
}

fn is_string(dt: &DataType) -> bool {
    matches!(dt, DataType::Utf8 | DataType::LargeUtf8)
}

fn finer(a: TimeUnit, b: TimeUnit) -> TimeUnit {
    if temporal::per_second(a) >= temporal::per_second(b) {
        a
    } else {
        b
    }
}

/// Arrow's common numeric type: the widest float if any is a float; else the widest integer,
/// signed when any is, wide enough for every unsigned one up to 64 bits.
fn common_numeric(types: &[&DataType]) -> Option<DataType> {
    if types.iter().any(|t| !is_numeric(t)) {
        return None;
    }
    if types.iter().any(|t| **t == DataType::Float64) {
        return Some(DataType::Float64);
    }
    if types.iter().any(|t| **t == DataType::Float32) {
        return Some(DataType::Float32);
    }
    let ints: Vec<IntTy> = types.iter().filter_map(|t| IntTy::of(t)).collect();
    let signed = ints.iter().filter(|i| i.signed).map(|i| i.bits).max();
    let unsigned = ints.iter().filter(|i| !i.signed).map(|i| i.bits).max();
    let int = |bits: u32, signed: bool| match (bits, signed) {
        (8, true) => DataType::Int8,
        (16, true) => DataType::Int16,
        (32, true) => DataType::Int32,
        (_, true) => DataType::Int64,
        (8, false) => DataType::UInt8,
        (16, false) => DataType::UInt16,
        (32, false) => DataType::UInt32,
        (_, false) => DataType::UInt64,
    };
    Some(match (signed, unsigned) {
        (Some(s), None) => int(s, true),
        (None, Some(u)) => int(u, false),
        (Some(s), Some(u)) => int(s.max(2 * u).min(64), true),
        (None, None) => return None,
    })
}

/// The type every value of a selection (`if_else`, `coalesce`, an `if`, a `switch`, the
/// element-wise extremes) is cast to.
fn common_value(at: &str, types: &[&DataType]) -> Result<DataType> {
    if let Some(t) = common_numeric(types) {
        return Ok(t);
    }
    let first = types[0];
    let all = |f: &dyn Fn(&DataType) -> bool| types.iter().all(|t| f(t));
    if all(&|t| *t == DataType::Boolean) {
        return Ok(DataType::Boolean);
    }
    if all(&is_string) {
        return Ok(if all(&|t| *t == DataType::Utf8) {
            DataType::Utf8
        } else {
            DataType::LargeUtf8
        });
    }
    if let DataType::Timestamp(_, tz) = first
        && all(&|t| matches!(t, DataType::Timestamp(_, z) if z == tz))
    {
        let unit = types.iter().fold(TimeUnit::Second, |u, t| match t {
            DataType::Timestamp(v, _) => finer(u, *v),
            _ => u,
        });
        return Ok(DataType::Timestamp(unit, tz.clone()));
    }
    if all(&|t| matches!(t, DataType::Duration(_))) {
        let unit = types.iter().fold(TimeUnit::Second, |u, t| match t {
            DataType::Duration(v) => finer(u, *v),
            _ => u,
        });
        return Ok(DataType::Duration(unit));
    }
    if all(&|t| *t == DataType::Date32) {
        return Ok(DataType::Date32);
    }
    // One type throughout (an opaque one, which `selectable` then refuses by name).
    if all(&|t| t == first) {
        return Ok(first.clone());
    }
    Err(refuse(
        at,
        format!(
            "values of types {} have no common type to select between",
            types
                .iter()
                .map(|t| spell_data_type(t))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

struct Binder<'s> {
    schema: &'s Schema,
    instrs: Vec<Instr>,
    memo: HashMap<String, usize>,
}

fn opt_bool(at: &str, options: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    match options.get(key) {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(refuse(
            at,
            format!("option `{key}` is true or false, not {other}"),
        )),
    }
}

fn only_options(at: &str, name: &str, options: &Map<String, Value>, known: &[&str]) -> Result<()> {
    for key in options.keys() {
        if !known.contains(&key.as_str()) {
            return Err(refuse(
                at,
                if known.is_empty() {
                    format!("`{name}` takes no options, and is given `{key}`")
                } else {
                    format!(
                        "`{name}` has no option `{key}` (its options are {})",
                        known.join(", ")
                    )
                },
            ));
        }
    }
    Ok(())
}

impl Binder<'_> {
    fn ty(&self, id: usize) -> &DataType {
        &self.instrs[id].ty
    }

    fn push(&mut self, op: Op, args: Vec<usize>, ty: DataType, name: &str) -> usize {
        let zone = match (&op, args.first().map(|a| &self.instrs[*a].ty)) {
            (Op::Part(_) | Op::Cast { .. }, Some(DataType::Timestamp(_, tz))) => {
                // Checked when the type was bound; an unknown zone cannot reach here.
                Zone::of(tz.as_deref()).ok()
            }
            _ => None,
        };
        let key = format!("{op:?}|{args:?}|{ty:?}");
        if let Some(&id) = self.memo.get(&key) {
            return id;
        }
        let constant =
            !matches!(op, Op::Column(_)) && args.iter().all(|a| self.instrs[*a].constant);
        let id = self.instrs.len();
        self.instrs.push(Instr {
            lane: lane(&ty),
            op,
            args,
            ty,
            name: name.to_string(),
            reg: usize::MAX,
            constant,
            zone,
        });
        self.memo.insert(key, id);
        id
    }

    fn check_zone(&self, at: &str, ty: &DataType) -> Result<()> {
        if let DataType::Timestamp(_, tz) = ty {
            Zone::of(tz.as_deref()).map_err(|e| refuse(at, e))?;
        }
        Ok(())
    }

    fn node(&mut self, at: &str, node: &Node) -> Result<usize> {
        match node {
            Node::Column(name) => {
                let (idx, field) = self
                    .schema
                    .column_with_name(name)
                    .ok_or_else(|| refuse(at, format!("no column `{name}`")))?;
                self.check_zone(at, field.data_type())?;
                Ok(self.push(
                    Op::Column(idx),
                    Vec::new(),
                    field.data_type().clone(),
                    "column",
                ))
            }
            Node::Literal { value, ty } => {
                let (lit, ty) = literal(at, value, ty.as_ref())?;
                self.check_zone(at, &ty)?;
                Ok(self.push(Op::Literal(lit), Vec::new(), ty, "literal"))
            }
            Node::Scalar(array) => {
                let (lit, ty) = scalar(at, array)?;
                Ok(self.push(Op::Literal(lit), Vec::new(), ty, "literal"))
            }
            Node::Cast { arg, to, safe } => {
                let id = self.node(&format!("{at}.cast"), arg)?;
                self.check_zone(at, to)?;
                self.cast(at, id, to, *safe)
            }
            Node::If { clauses, otherwise } => {
                let mut conds = Vec::with_capacity(clauses.len());
                let mut values = Vec::with_capacity(clauses.len() + 1);
                for (i, (w, t)) in clauses.iter().enumerate() {
                    let c = self.node(&format!("{at}.if[{i}].when"), w)?;
                    if *self.ty(c) != DataType::Boolean {
                        return Err(refuse(
                            &format!("{at}.if[{i}].when"),
                            format!(
                                "a condition is a boolean, not {}",
                                spell_data_type(self.ty(c))
                            ),
                        ));
                    }
                    conds.push(c);
                    values.push(self.node(&format!("{at}.if[{i}].then"), t)?);
                }
                if let Some(e) = otherwise {
                    values.push(self.node(&format!("{at}.else"), e)?);
                }
                self.case_when(at, conds, values, otherwise.is_some())
            }
            Node::Switch {
                arg,
                cases,
                otherwise,
            } => {
                let a = self.node(&format!("{at}.switch"), arg)?;
                let mut conds = Vec::with_capacity(cases.len());
                let mut values = Vec::with_capacity(cases.len() + 1);
                for (i, (m, t)) in cases.iter().enumerate() {
                    let here = format!("{at}.cases[{i}]");
                    let lit = self.node(&format!("{here}.match"), m)?;
                    conds.push(self.compare(&here, "equal", Cmp::Eq, false, a, lit)?);
                    values.push(self.node(&format!("{here}.then"), t)?);
                }
                if let Some(e) = otherwise {
                    values.push(self.node(&format!("{at}.else"), e)?);
                }
                self.case_when(at, conds, values, otherwise.is_some())
            }
            Node::Call {
                name,
                args,
                options,
            } => {
                let ids = args
                    .iter()
                    .enumerate()
                    .map(|(i, a)| self.node(&format!("{at}.args[{i}]"), a))
                    .collect::<Result<Vec<_>>>()?;
                self.call(at, name, ids, options)
            }
        }
    }

    fn case_when(
        &mut self,
        at: &str,
        conds: Vec<usize>,
        values: Vec<usize>,
        has_else: bool,
    ) -> Result<usize> {
        let types: Vec<DataType> = values.iter().map(|v| self.ty(*v).clone()).collect();
        let common = common_value(at, &types.iter().collect::<Vec<_>>())?;
        self.selectable(at, &common)?;
        let values = values
            .into_iter()
            .map(|v| self.cast(at, v, &common, true))
            .collect::<Result<Vec<_>>>()?;
        let mut args = Vec::with_capacity(conds.len() * 2 + 1);
        for (i, c) in conds.iter().enumerate() {
            args.push(*c);
            args.push(values[i]);
        }
        if has_else {
            args.push(values[conds.len()]);
        }
        Ok(self.push(Op::CaseWhen { has_else }, args, common, "case_when"))
    }

    fn selectable(&self, at: &str, ty: &DataType) -> Result<()> {
        if lane(ty) == Lane::Opaque {
            return Err(refuse(
                at,
                format!(
                    "values of type {} can be compared and tested for null, not selected or computed",
                    spell_data_type(ty)
                ),
            ));
        }
        Ok(())
    }

    /// `id` as type `to`: unchanged when it already is; else a cast, refused where the table of
    /// casts has none.
    fn cast(&mut self, at: &str, id: usize, to: &DataType, safe: bool) -> Result<usize> {
        let from = self.ty(id).clone();
        if from == *to {
            return Ok(id);
        }
        if !castable(&from, to) {
            return Err(refuse(
                at,
                format!(
                    "no cast from {} to {}",
                    spell_data_type(&from),
                    spell_data_type(to)
                ),
            ));
        }
        Ok(self.push(Op::Cast { safe }, vec![id], to.clone(), "cast"))
    }

    fn no_kernel(&self, at: &str, name: &str, args: &[usize]) -> MorunaError {
        refuse(
            at,
            format!(
                "`{name}` has no kernel for ({})",
                args.iter()
                    .map(|a| spell_data_type(self.ty(*a)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    }

    fn arity(&self, at: &str, name: &str, args: &[usize], n: usize) -> Result<()> {
        if args.len() != n {
            return Err(refuse(
                at,
                format!(
                    "`{name}` takes {n} argument(s), and is given {}",
                    args.len()
                ),
            ));
        }
        Ok(())
    }

    fn compare(
        &mut self,
        at: &str,
        name: &str,
        op: Cmp,
        total: bool,
        a: usize,
        b: usize,
    ) -> Result<usize> {
        let (ta, tb) = (self.ty(a).clone(), self.ty(b).clone());
        let common = if let Some(t) = common_numeric(&[&ta, &tb]) {
            t
        } else if is_string(&ta) && is_string(&tb) {
            // Both lanes hold &str: no conversion is needed to compare bytes.
            return Ok(self.push(Op::Cmp { op, total }, vec![a, b], DataType::Boolean, name));
        } else if ta == DataType::Boolean && tb == DataType::Boolean {
            DataType::Boolean
        } else if let (DataType::Timestamp(ua, za), DataType::Timestamp(ub, zb)) = (&ta, &tb) {
            if za.is_some() != zb.is_some() {
                return Err(refuse(
                    at,
                    "a timestamp with a zone cannot be compared with one without",
                ));
            }
            // Instants compare whatever their zones; the type keeps the left zone.
            let unit = finer(*ua, *ub);
            let a = self.cast(at, a, &DataType::Timestamp(unit, za.clone()), true)?;
            let b = self.cast(at, b, &DataType::Timestamp(unit, za.clone()), true)?;
            return Ok(self.push(Op::Cmp { op, total }, vec![a, b], DataType::Boolean, name));
        } else if let (DataType::Duration(ua), DataType::Duration(ub)) = (&ta, &tb) {
            DataType::Duration(finer(*ua, *ub))
        } else if ta == DataType::Date32 && tb == DataType::Date32 {
            DataType::Date32
        } else if lane(&ta) == Lane::Opaque && ta == tb {
            if ta == DataType::Float16 && !total {
                return Err(refuse(
                    at,
                    "a float16 compares in total order only (`\"order\": \"total\"`)",
                ));
            }
            return Ok(self.push(Op::CmpOpaque { op }, vec![a, b], DataType::Boolean, name));
        } else {
            return Err(self.no_kernel(at, name, &[a, b]));
        };
        let a = self.cast(at, a, &common, true)?;
        let b = self.cast(at, b, &common, true)?;
        Ok(self.push(Op::Cmp { op, total }, vec![a, b], DataType::Boolean, name))
    }

    fn arith(
        &mut self,
        at: &str,
        name: &str,
        op: Arith,
        checked: bool,
        args: &[usize],
    ) -> Result<usize> {
        self.arity(at, name, args, 2)?;
        let (a, b) = (args[0], args[1]);
        let (ta, tb) = (self.ty(a).clone(), self.ty(b).clone());
        if let Some(t) = common_numeric(&[&ta, &tb]) {
            let a = self.cast(at, a, &t, true)?;
            let b = self.cast(at, b, &t, true)?;
            return Ok(self.push(Op::Arith { op, checked }, vec![a, b], t, name));
        }
        use DataType::{Date32, Duration, Timestamp};
        let (left, right, out) = match (op, &ta, &tb) {
            (Arith::Add, Timestamp(u, z), Duration(v)) => {
                let unit = finer(*u, *v);
                let ts = Timestamp(unit, z.clone());
                (ts.clone(), Duration(unit), ts)
            }
            // The same sum with the duration first: each operand is cast to its own kind.
            (Arith::Add, Duration(v), Timestamp(u, z)) => {
                let unit = finer(*u, *v);
                let ts = Timestamp(unit, z.clone());
                (Duration(unit), ts.clone(), ts)
            }
            (Arith::Add | Arith::Sub, Duration(u), Duration(v)) => {
                let d = Duration(finer(*u, *v));
                (d.clone(), d.clone(), d)
            }
            (Arith::Sub, Timestamp(u, z), Duration(v)) => {
                let unit = finer(*u, *v);
                let ts = Timestamp(unit, z.clone());
                (ts.clone(), Duration(unit), ts)
            }
            (Arith::Sub, Timestamp(u, za), Timestamp(v, zb)) => {
                if za.is_some() != zb.is_some() {
                    return Err(refuse(
                        at,
                        "subtracting a timestamp with a zone and one without is ambiguous",
                    ));
                }
                let unit = finer(*u, *v);
                (
                    Timestamp(unit, za.clone()),
                    Timestamp(unit, za.clone()),
                    Duration(unit),
                )
            }
            (Arith::Sub, Date32, Date32) => {
                let ts = Timestamp(TimeUnit::Second, None);
                (ts.clone(), ts, Duration(TimeUnit::Second))
            }
            (Arith::Mul, Duration(u), t) | (Arith::Mul, t, Duration(u)) if is_int(t) => {
                if matches!(ta, Duration(_)) {
                    (Duration(*u), DataType::Int64, Duration(*u))
                } else {
                    (DataType::Int64, Duration(*u), Duration(*u))
                }
            }
            _ => return Err(self.no_kernel(at, name, args)),
        };
        let a = self.cast(at, a, &left, true)?;
        let b = self.cast(at, b, &right, true)?;
        Ok(self.push(Op::Arith { op, checked }, vec![a, b], out, name))
    }

    /// An integer argument as a double, as Arrow's maths, `floor`, `ceil` and `trunc` take it.
    fn floating(&mut self, at: &str, name: &str, id: usize) -> Result<usize> {
        let t = self.ty(id).clone();
        if is_float(&t) {
            Ok(id)
        } else if is_int(&t) {
            self.cast(at, id, &DataType::Float64, true)
        } else {
            Err(self.no_kernel(at, name, &[id]))
        }
    }

    fn call(
        &mut self,
        at: &str,
        name: &str,
        args: Vec<usize>,
        options: &Map<String, Value>,
    ) -> Result<usize> {
        let math = match name.strip_suffix("_checked").unwrap_or(name) {
            "sqrt" => Some(Math::Sqrt),
            "exp" => Some(Math::Exp),
            "ln" => Some(Math::Ln),
            "log10" => Some(Math::Log10),
            "log2" => Some(Math::Log2),
            "log1p" => Some(Math::Log1p),
            "sin" => Some(Math::Sin),
            "cos" => Some(Math::Cos),
            "tan" => Some(Math::Tan),
            "asin" => Some(Math::Asin),
            "acos" => Some(Math::Acos),
            "atan" => Some(Math::Atan),
            _ => None,
        };
        let known = FUNCTIONS.contains(&name);
        if !known {
            return Err(refuse(at, format!("no function `{name}`")));
        }
        let checked = name.ends_with("_checked");
        let cmp_options = ["order"];
        let opts: &[&str] = match name {
            "round" => &["ndigits", "round_mode"],
            "is_null" => &["nan_is_null"],
            "min_element_wise" | "max_element_wise" => &["skip_nulls"],
            "day_of_week" => &["count_from_zero", "week_start"],
            "equal" | "not_equal" | "less" | "less_equal" | "greater" | "greater_equal" => {
                &cmp_options
            }
            _ => &[],
        };
        only_options(at, name, options, opts)?;
        if let Some(f) = math {
            self.arity(at, name, &args, 1)?;
            let x = self.floating(at, name, args[0])?;
            let ty = self.ty(x).clone();
            return Ok(self.push(Op::Math { f, checked }, vec![x], ty, name));
        }
        let base = name.strip_suffix("_checked").unwrap_or(name);
        match base {
            "add" | "subtract" | "multiply" | "divide" | "power" => {
                let op = match base {
                    "add" => Arith::Add,
                    "subtract" => Arith::Sub,
                    "multiply" => Arith::Mul,
                    "divide" => Arith::Div,
                    _ => Arith::Pow,
                };
                if matches!(op, Arith::Div | Arith::Pow)
                    && args.iter().any(|a| !is_numeric(self.ty(*a)))
                {
                    return Err(self.no_kernel(at, name, &args));
                }
                self.arith(at, name, op, checked, &args)
            }
            "negate" | "abs" => {
                self.arity(at, name, &args, 1)?;
                let t = self.ty(args[0]).clone();
                let ok = is_numeric(&t) || matches!(t, DataType::Duration(_));
                let unsigned_checked =
                    base == "negate" && checked && IntTy::of(&t).is_some_and(|i| !i.signed);
                if !ok || unsigned_checked {
                    return Err(self.no_kernel(at, name, &args));
                }
                let op = if base == "negate" {
                    Op::Neg { checked }
                } else {
                    Op::Abs { checked }
                };
                Ok(self.push(op, args, t, name))
            }
            "sign" => {
                self.arity(at, name, &args, 1)?;
                let t = self.ty(args[0]).clone();
                let out = if is_float(&t) {
                    t
                } else if is_int(&t) {
                    DataType::Int8
                } else {
                    return Err(self.no_kernel(at, name, &args));
                };
                Ok(self.push(Op::Sign, args, out, name))
            }
            "atan2" => {
                self.arity(at, name, &args, 2)?;
                let (y, x) = (
                    self.floating(at, name, args[0])?,
                    self.floating(at, name, args[1])?,
                );
                let t = common_numeric(&[self.ty(y), self.ty(x)])
                    .ok_or_else(|| self.no_kernel(at, name, &args))?;
                let y = self.cast(at, y, &t, true)?;
                let x = self.cast(at, x, &t, true)?;
                Ok(self.push(Op::Atan2, vec![y, x], t, name))
            }
            "round" => {
                self.arity(at, name, &args, 1)?;
                let ndigits = match options.get("ndigits") {
                    None => 0,
                    Some(v) => v.as_i64().ok_or_else(|| {
                        refuse(at, format!("option `ndigits` is an integer, not {v}"))
                    })?,
                };
                let mode = match options.get("round_mode") {
                    None => RoundMode::HalfToEven,
                    Some(Value::String(s)) => RoundMode::parse(s).ok_or_else(|| {
                        refuse(
                            at,
                            format!(
                                "no round mode `{s}` (the modes are {})",
                                super::round::MODES
                                    .iter()
                                    .map(|(n, _)| *n)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        )
                    })?,
                    Some(other) => {
                        return Err(refuse(
                            at,
                            format!("option `round_mode` is a string, not {other}"),
                        ));
                    }
                };
                let t = self.ty(args[0]).clone();
                if !is_numeric(&t) {
                    return Err(self.no_kernel(at, name, &args));
                }
                Ok(self.push(Op::Round { ndigits, mode }, args, t, name))
            }
            "floor" | "ceil" | "trunc" => {
                self.arity(at, name, &args, 1)?;
                let x = self.floating(at, name, args[0])?;
                let t = self.ty(x).clone();
                let op = match base {
                    "floor" => Op::Floor,
                    "ceil" => Op::Ceil,
                    _ => Op::Trunc,
                };
                Ok(self.push(op, vec![x], t, name))
            }
            "equal" | "not_equal" | "less" | "less_equal" | "greater" | "greater_equal" => {
                self.arity(at, name, &args, 2)?;
                let op = match base {
                    "equal" => Cmp::Eq,
                    "not_equal" => Cmp::Ne,
                    "less" => Cmp::Lt,
                    "less_equal" => Cmp::Le,
                    "greater" => Cmp::Gt,
                    _ => Cmp::Ge,
                };
                let total = match options.get("order") {
                    None => false,
                    Some(Value::String(s)) if s == "ieee" => false,
                    Some(Value::String(s)) if s == "total" => true,
                    Some(other) => {
                        return Err(refuse(
                            at,
                            format!("option `order` is \"ieee\" or \"total\", not {other}"),
                        ));
                    }
                };
                self.compare(at, name, op, total, args[0], args[1])
            }
            "and" | "and_kleene" | "or" | "or_kleene" | "xor" | "and_not" | "and_not_kleene" => {
                self.arity(at, name, &args, 2)?;
                if args.iter().any(|a| *self.ty(*a) != DataType::Boolean) {
                    return Err(self.no_kernel(at, name, &args));
                }
                let op = match base {
                    "and" => Logic::And,
                    "and_kleene" => Logic::AndKleene,
                    "or" => Logic::Or,
                    "or_kleene" => Logic::OrKleene,
                    "xor" => Logic::Xor,
                    "and_not" => Logic::AndNot,
                    _ => Logic::AndNotKleene,
                };
                Ok(self.push(Op::Logic(op), args, DataType::Boolean, name))
            }
            "invert" => {
                self.arity(at, name, &args, 1)?;
                if *self.ty(args[0]) != DataType::Boolean {
                    return Err(self.no_kernel(at, name, &args));
                }
                Ok(self.push(Op::Invert, args, DataType::Boolean, name))
            }
            "is_null" | "is_valid" => {
                self.arity(at, name, &args, 1)?;
                let op = if base == "is_null" {
                    Op::IsNull {
                        nan_is_null: opt_bool(at, options, "nan_is_null", false)?,
                    }
                } else {
                    Op::IsValid
                };
                Ok(self.push(op, args, DataType::Boolean, name))
            }
            "is_nan" | "is_finite" | "is_inf" => {
                self.arity(at, name, &args, 1)?;
                if !is_numeric(self.ty(args[0])) {
                    return Err(self.no_kernel(at, name, &args));
                }
                let op = match base {
                    "is_nan" => Op::IsNan,
                    "is_finite" => Op::IsFinite,
                    _ => Op::IsInf,
                };
                Ok(self.push(op, args, DataType::Boolean, name))
            }
            "if_else" => {
                self.arity(at, name, &args, 3)?;
                if *self.ty(args[0]) != DataType::Boolean {
                    return Err(self.no_kernel(at, name, &args));
                }
                let common = common_value(at, &[self.ty(args[1]), self.ty(args[2])])?;
                self.selectable(at, &common)?;
                let a = self.cast(at, args[1], &common, true)?;
                let b = self.cast(at, args[2], &common, true)?;
                Ok(self.push(Op::IfElse, vec![args[0], a, b], common, name))
            }
            "coalesce" | "fill_null" | "min_element_wise" | "max_element_wise" => {
                if base == "fill_null" {
                    self.arity(at, name, &args, 2)?;
                } else if args.is_empty() {
                    return Err(refuse(at, format!("`{name}` takes one or more arguments")));
                }
                let types: Vec<DataType> = args.iter().map(|a| self.ty(*a).clone()).collect();
                let common = common_value(at, &types.iter().collect::<Vec<_>>())?;
                self.selectable(at, &common)?;
                let extreme = matches!(base, "min_element_wise" | "max_element_wise");
                if extreme
                    && matches!(
                        common,
                        DataType::Boolean | DataType::Utf8 | DataType::LargeUtf8
                    )
                {
                    return Err(self.no_kernel(at, name, &args));
                }
                let ids = args
                    .iter()
                    .map(|a| self.cast(at, *a, &common, true))
                    .collect::<Result<Vec<_>>>()?;
                let op = if extreme {
                    Op::MinMax {
                        max: base == "max_element_wise",
                        skip_nulls: opt_bool(at, options, "skip_nulls", true)?,
                    }
                } else {
                    Op::Coalesce
                };
                Ok(self.push(op, ids, common, name))
            }
            part_name => {
                let part = Part::named(part_name)
                    .ok_or_else(|| refuse(at, format!("no function `{name}`")))?;
                self.arity(at, name, &args, 1)?;
                let part = match part {
                    Part::DayOfWeek { .. } => {
                        let week_start = match options.get("week_start") {
                            None => 1,
                            Some(v) => v
                                .as_u64()
                                .filter(|w| (1..=7).contains(w))
                                .ok_or_else(|| refuse(at, format!("option `week_start` is 1 (Monday) to 7 (Sunday), not {v}")))?
                                as u8,
                        };
                        Part::DayOfWeek {
                            count_from_zero: opt_bool(at, options, "count_from_zero", true)?,
                            week_start,
                        }
                    }
                    p => p,
                };
                match self.ty(args[0]) {
                    DataType::Timestamp(..) => {}
                    DataType::Date32 if !part.of_time() => {}
                    _ => return Err(self.no_kernel(at, name, &args)),
                }
                Ok(self.push(Op::Part(part), args, DataType::Int64, name))
            }
        }
    }
}

/// Whether the reference casts `from` to `to` (spec/expr.md, "Casts").
pub(crate) fn castable(from: &DataType, to: &DataType) -> bool {
    use DataType::*;
    let numeric_or_bool = |t: &DataType| is_numeric(t) || *t == Boolean;
    match (from, to) {
        (a, b) if a == b => true,
        (a, b) if numeric_or_bool(a) && numeric_or_bool(b) => true,
        (Int64, Timestamp(..) | Duration(_)) | (Timestamp(..) | Duration(_), Int64) => true,
        (Int32, Date32) | (Date32, Int32) => true,
        (Timestamp(..), Timestamp(..)) | (Duration(_), Duration(_)) => true,
        (Timestamp(..), Date32) => true,
        (Date32, Timestamp(_, None)) => true,
        (Utf8 | LargeUtf8, Utf8 | LargeUtf8) => true,
        _ => false,
    }
}

/// A JSON literal as a value of its type: `type` when given, else its JSON kind's (a boolean,
/// an int64, a double, a string).
fn literal(at: &str, value: &Value, ty: Option<&DataType>) -> Result<(Lit, DataType)> {
    let bad = |msg: String| refuse(at, msg);
    let ty = match ty {
        Some(t) => t.clone(),
        None => match value {
            Value::Bool(_) => DataType::Boolean,
            Value::String(_) => DataType::Utf8,
            Value::Number(n) if n.is_f64() => DataType::Float64,
            Value::Number(_) => DataType::Int64,
            _ => return Err(bad("a null literal needs a `type`".into())),
        },
    };
    if value.is_null() {
        if lane(&ty) == Lane::Opaque {
            return Ok((Lit::Opaque(new_null_array(&ty, 1)), ty));
        }
        return Ok((Lit::Null, ty));
    }
    let mismatch = || bad(format!("{value} is not a {}", spell_data_type(&ty)));
    let float = |v: &Value| -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => match s.as_str() {
                "NaN" => Some(f64::NAN),
                "Infinity" => Some(f64::INFINITY),
                "-Infinity" => Some(f64::NEG_INFINITY),
                _ => None,
            },
            _ => None,
        }
    };
    // An integer literal's exact value, whether JSON wrote it as 5 or 5.0.
    let integer = |v: &Value| -> Option<i128> {
        match v {
            Value::Number(n) => n
                .as_i64()
                .map(i128::from)
                .or_else(|| n.as_u64().map(i128::from))
                .or_else(|| {
                    n.as_f64()
                        .filter(|f| f.fract() == 0.0 && f.abs() < 1.8e19)
                        .map(|f| f as i128)
                }),
            _ => None,
        }
    };
    let lit = match lane(&ty) {
        Lane::Bool => Lit::Bool(value.as_bool().ok_or_else(mismatch)?),
        Lane::Str => Lit::Str(value.as_str().ok_or_else(mismatch)?.to_string()),
        Lane::F64 => Lit::F64(float(value).ok_or_else(mismatch)?),
        // The nearest single to the double JSON's number reads as.
        Lane::F32 => Lit::F32(float(value).ok_or_else(mismatch)? as f32),
        Lane::I64 => {
            let v = integer(value).ok_or_else(mismatch)?;
            let it = IntTy::of(&ty).ok_or_else(mismatch)?;
            if !it.fits(v) {
                return Err(mismatch());
            }
            Lit::I64(v as i64)
        }
        Lane::U64 => {
            let v = integer(value).ok_or_else(mismatch)?;
            let it = IntTy::of(&ty).ok_or_else(mismatch)?;
            if !it.fits(v) {
                return Err(mismatch());
            }
            Lit::U64(v as u64)
        }
        Lane::Opaque => {
            // Read through Arrow's cast from the JSON kind, refusing what does not convert.
            let source: ArrayRef = match value {
                Value::String(s) => Arc::new(StringArray::from(vec![s.as_str()])),
                Value::Bool(b) => Arc::new(BooleanArray::from(vec![*b])),
                Value::Number(n) => match n.as_i64() {
                    Some(i) => Arc::new(Int64Array::from(vec![i])),
                    None => Arc::new(Float64Array::from(vec![n.as_f64().unwrap_or(f64::NAN)])),
                },
                _ => return Err(mismatch()),
            };
            let options = cast::CastOptions {
                safe: false,
                ..Default::default()
            };
            let array = cast::cast_with_options(&source, &ty, &options)
                .map_err(|e| bad(format!("{value} is not a {}: {e}", spell_data_type(&ty))))?;
            Lit::Opaque(array)
        }
    };
    Ok((lit, ty))
}

/// A one-element array as a literal of its type.
fn scalar(at: &str, array: &ArrayRef) -> Result<(Lit, DataType)> {
    use moruna_kernel::arrow::array::AsArray;
    use moruna_kernel::arrow::datatypes as t;
    let ty = array.data_type().clone();
    if array.len() != 1 {
        return Err(refuse(at, "a scalar is one value"));
    }
    if array.is_null(0) {
        if lane(&ty) == Lane::Opaque {
            return Ok((Lit::Opaque(Arc::clone(array)), ty));
        }
        return Ok((Lit::Null, ty));
    }
    let a = array.as_ref();
    let lit = match &ty {
        DataType::Boolean => Lit::Bool(a.as_boolean().value(0)),
        DataType::Int8 => Lit::I64(a.as_primitive::<t::Int8Type>().value(0).into()),
        DataType::Int16 => Lit::I64(a.as_primitive::<t::Int16Type>().value(0).into()),
        DataType::Int32 => Lit::I64(a.as_primitive::<t::Int32Type>().value(0).into()),
        DataType::Int64 => Lit::I64(a.as_primitive::<t::Int64Type>().value(0)),
        DataType::Date32 => Lit::I64(a.as_primitive::<t::Date32Type>().value(0).into()),
        DataType::Timestamp(..) | DataType::Duration(_) => {
            let as_int = cast::cast(array, &DataType::Int64)
                .map_err(|e| refuse(at, format!("a scalar: {e}")))?;
            Lit::I64(as_int.as_primitive::<t::Int64Type>().value(0))
        }
        DataType::UInt8 => Lit::U64(a.as_primitive::<t::UInt8Type>().value(0).into()),
        DataType::UInt16 => Lit::U64(a.as_primitive::<t::UInt16Type>().value(0).into()),
        DataType::UInt32 => Lit::U64(a.as_primitive::<t::UInt32Type>().value(0).into()),
        DataType::UInt64 => Lit::U64(a.as_primitive::<t::UInt64Type>().value(0)),
        DataType::Float32 => Lit::F32(a.as_primitive::<t::Float32Type>().value(0)),
        DataType::Float64 => Lit::F64(a.as_primitive::<t::Float64Type>().value(0)),
        DataType::Utf8 => Lit::Str(a.as_string::<i32>().value(0).to_string()),
        DataType::LargeUtf8 => Lit::Str(a.as_string::<i64>().value(0).to_string()),
        _ => Lit::Opaque(Arc::clone(array)),
    };
    Ok((lit, ty))
}

/// Bind `tree` to `input` and compile it.
pub(crate) fn bind(tree: &Tree, input: &Schema) -> Result<Program> {
    let mut b = Binder {
        schema: input,
        instrs: Vec::new(),
        memo: HashMap::new(),
    };
    let mut outputs = Vec::with_capacity(tree.outputs.len());
    let mut fields = Vec::with_capacity(tree.outputs.len());
    for (i, o) in tree.outputs.iter().enumerate() {
        let at = format!("outputs[{i}] `{}`", o.name);
        let id = b.node(&at, &o.expr)?;
        let instr = &b.instrs[id];
        let column = match instr.op {
            Op::Column(c) => Some(c),
            _ => None,
        };
        if column.is_none() && instr.lane == Lane::Opaque && !matches!(instr.op, Op::Literal(_)) {
            return Err(refuse(
                &at,
                "an opaque value is output only as a column passed through",
            ));
        }
        fields.push(match column {
            Some(c) => input.field(c).clone().with_name(o.name.clone()),
            None => Field::new(o.name.clone(), instr.ty.clone(), true),
        });
        outputs.push(Out { instr: id, column });
    }
    let mut instrs = b.instrs;
    let regs = allocate(&mut instrs, &outputs);
    Ok(Program {
        instrs,
        outputs,
        regs,
        input: Arc::new(input.clone()),
        schema: Arc::new(Schema::new(fields)),
    })
}

/// Give every instruction a register: a fresh one, or one whose value no later instruction
/// reads. Outputs and constants keep theirs for the whole chunk (constants for every chunk).
fn allocate(instrs: &mut [Instr], outputs: &[Out]) -> usize {
    let n = instrs.len();
    let mut last = vec![0usize; n];
    for (i, ins) in instrs.iter().enumerate() {
        for &a in &ins.args {
            last[a] = last[a].max(i);
        }
    }
    let mut pinned = vec![false; n];
    for o in outputs {
        pinned[o.instr] = true;
    }
    for (i, ins) in instrs.iter().enumerate() {
        if ins.constant {
            pinned[i] = true;
        }
    }
    let mut free: Vec<(Lane, usize)> = Vec::new();
    let mut count = 0usize;
    for i in 0..n {
        let lane_i = instrs[i].lane;
        // A constant is computed in the first chunk only, so its register is its own: one that
        // another instruction wrote in later chunks would lose it.
        let reuse = if instrs[i].constant {
            None
        } else {
            free.iter().position(|(l, _)| *l == lane_i)
        };
        let reg = match reuse {
            Some(p) => free.swap_remove(p).1,
            None => {
                count += 1;
                count - 1
            }
        };
        instrs[i].reg = reg;
        let args = instrs[i].args.clone();
        for a in args {
            if !pinned[a] && last[a] == i && !free.iter().any(|(_, r)| *r == instrs[a].reg) {
                free.push((instrs[a].lane, instrs[a].reg));
            }
        }
        // An instruction nothing reads (a dead clause's value) frees its register at once.
        if !pinned[i] && !instrs.iter().skip(i + 1).any(|x| x.args.contains(&i)) {
            free.push((lane_i, reg));
        }
    }
    count
}
