//! Moruna's standard kernels: the common transformations, constructed by arguments rather than
//! written (MH 4.9).
//!
//! `cast`, `rename`, `select`, `drop`, `filter(expr)`, `fill_null`, `dedupe(keys)`,
//! `hash(cols, algo)`, `mask(cols, mode)`, `explode`, `concat_str` and `date_trunc`, each a
//! [`StdKernel`] built from its name and a JSON object of arguments. Each declares its input and
//! output schemas as a function of its arguments, so every one of them is checkable; each ships
//! a first profile as its hints; none enters Python, so each releases the GIL by construction;
//! and two adjacent ones are fused into one stage where their combination is one operation
//! (`select` after `cast`, and any chain of `cast`, `rename`, `select` and `drop`, is one
//! projection; `filter` after `fill_null` is one pass). A chain that is all standard kernels
//! never enters the interpreter, which is where a host optimises.
//!
//! The fingerprint is `sha256(name, canonical args, crate version)` (MH 4.9), so a host can
//! pin a standard kernel in a job document without shipping any code.

#![deny(missing_docs)]
#![deny(unsafe_code)]
// Every fallible call returns the contract's `MorunaError`, whose size is the contract's.
#![allow(clippy::result_large_err)]

pub mod args;
pub mod expr;
pub mod ops;

use std::collections::HashSet;
use std::sync::Arc;

use moruna_kernel::arrow::datatypes::{DataType, Schema, SchemaRef};
use moruna_kernel::declare::{ColumnDecl, Declared, SchemaDecl};
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError, NoState,
    Payload, PayloadKind, PayloadSpec, Result, ResumePolicy, SourceSchema, TierPref,
};
use serde_json::Value;
use sha2::Digest;

use crate::args::{bad, canonical};
use crate::expr::{Expr, Literal};
use crate::ops::{Algo, MaskMode, Step, TruncUnit};

/// This crate's version, which is part of every standard kernel's fingerprint.
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Every standard kernel's name, in the order of MH 4.9.
pub const NAMES: [&str; 12] = [
    "cast",
    "rename",
    "select",
    "drop",
    "filter",
    "fill_null",
    "dedupe",
    "hash",
    "mask",
    "explode",
    "concat_str",
    "date_trunc",
];

/// What a standard kernel does.
#[derive(Clone, Debug)]
enum Op {
    /// `cast`, `rename`, `select`, `drop`, and any fused chain of them.
    Project(Vec<Step>),
    /// `filter`.
    Filter(Expr),
    /// `fill_null`.
    Fill(Vec<(String, Literal)>),
    /// `fill_null` then `filter`, fused.
    FillFilter(Vec<(String, Literal)>, Expr),
    /// `dedupe`: stateful, one instance.
    Dedupe(Vec<String>),
    /// `hash`.
    Hash {
        columns: Vec<String>,
        algo: Algo,
        output: String,
    },
    /// `mask`.
    Mask {
        columns: Vec<String>,
        mode: MaskMode,
    },
    /// `explode`.
    Explode(String),
    /// `concat_str`.
    Concat {
        columns: Vec<String>,
        separator: String,
        output: String,
    },
    /// `date_trunc`.
    Trunc {
        column: String,
        unit: TruncUnit,
        output: Option<String>,
    },
}

/// A standard kernel: its name, its canonical arguments, what it does and what it declares.
#[derive(Clone, Debug)]
pub struct StdKernel {
    name: String,
    args: Value,
    op: Op,
    declared: Declared,
    fingerprint: Fingerprint,
    amplification: f64,
}

/// The fingerprint of MH 4.9: SHA-256 over `"moruna-std\0" || name || "\0" || canonical args ||
/// "\0" || crate version`.
pub fn std_fingerprint(name: &str, args: &Value) -> Fingerprint {
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"moruna-std\0");
    hasher.update(name.as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical(args).as_bytes());
    hasher.update(b"\0");
    hasher.update(CRATE_VERSION.as_bytes());
    Fingerprint(hasher.finalize().into())
}

fn subset(columns: Vec<ColumnDecl>) -> Option<SchemaDecl> {
    Some(SchemaDecl::Subset(columns))
}

fn unchanged() -> Option<SchemaDecl> {
    Some(SchemaDecl::Relative {
        adds: Vec::new(),
        drops: Vec::new(),
        changes: Vec::new(),
    })
}

fn anys(names: &[String]) -> Vec<ColumnDecl> {
    names.iter().map(|n| ColumnDecl::any(n.clone())).collect()
}

impl StdKernel {
    /// Build `name` from `args` (MH 4.9). An unknown name, an unknown argument, a missing one or
    /// one of the wrong shape is a `Plan` error naming the kernel and the argument.
    pub fn new(name: &str, args: &Value) -> Result<StdKernel> {
        let (op, declared, amplification) = match name {
            "cast" => {
                args::only(name, args, &["columns", "strict"])?;
                let mut columns = Vec::new();
                for (column, ty) in args::mapping(name, args, "columns")? {
                    columns.push((column, args::exact_type(name, ty)?));
                }
                let strict = args::optional_bool(name, args, "strict")?.unwrap_or(false);
                let names: Vec<String> = columns.iter().map(|(c, _)| c.clone()).collect();
                let changes = columns
                    .iter()
                    .map(|(c, t)| ColumnDecl::new(c.clone(), t.clone()))
                    .collect();
                (
                    Op::Project(vec![Step::Cast { columns, strict }]),
                    Declared {
                        input: subset(anys(&names)),
                        output: Some(SchemaDecl::Relative {
                            adds: Vec::new(),
                            drops: Vec::new(),
                            changes,
                        }),
                    },
                    1.0,
                )
            }
            "rename" => {
                args::only(name, args, &["columns"])?;
                let mut pairs = Vec::new();
                for (old, new) in args::mapping(name, args, "columns")? {
                    let new = new
                        .as_str()
                        .ok_or_else(|| bad(name, "`columns` maps old names to new names"))?;
                    pairs.push((old, new.to_string()));
                }
                let olds: Vec<String> = pairs.iter().map(|(o, _)| o.clone()).collect();
                let adds = pairs
                    .iter()
                    .map(|(_, n)| ColumnDecl::any(n.clone()))
                    .collect();
                (
                    Op::Project(vec![Step::Rename(pairs)]),
                    Declared {
                        input: subset(anys(&olds)),
                        output: Some(SchemaDecl::Relative {
                            adds,
                            drops: olds,
                            changes: Vec::new(),
                        }),
                    },
                    0.0,
                )
            }
            "select" => {
                args::only(name, args, &["columns"])?;
                let columns = args::names(name, args, "columns")?;
                (
                    Op::Project(vec![Step::Select(columns.clone())]),
                    Declared {
                        input: subset(anys(&columns)),
                        output: Some(SchemaDecl::Exact(anys(&columns))),
                    },
                    0.0,
                )
            }
            "drop" => {
                args::only(name, args, &["columns"])?;
                let columns = args::names(name, args, "columns")?;
                (
                    Op::Project(vec![Step::Drop(columns.clone())]),
                    Declared {
                        input: subset(anys(&columns)),
                        output: Some(SchemaDecl::Relative {
                            adds: Vec::new(),
                            drops: columns,
                            changes: Vec::new(),
                        }),
                    },
                    0.0,
                )
            }
            "filter" => {
                args::only(name, args, &["expr"])?;
                let expr = Expr::parse(&args::string(name, args, "expr")?)?;
                (
                    Op::Filter(expr.clone()),
                    Declared {
                        input: subset(expr.columns()),
                        output: unchanged(),
                    },
                    1.0,
                )
            }
            "fill_null" => {
                args::only(name, args, &["values"])?;
                let mut values = Vec::new();
                let mut columns = Vec::new();
                for (column, value) in args::mapping(name, args, "values")? {
                    let literal = Literal::from_json(value).ok_or_else(|| {
                        bad(name, format!("the fill value for `{column}` must be a number, a string or a boolean"))
                    })?;
                    columns.push(ColumnDecl::new(column.clone(), literal.data_type()));
                    values.push((column, literal));
                }
                (
                    Op::Fill(values),
                    Declared {
                        input: subset(columns),
                        output: unchanged(),
                    },
                    1.0,
                )
            }
            "dedupe" => {
                args::only(name, args, &["keys"])?;
                let keys = args::names(name, args, "keys")?;
                (
                    Op::Dedupe(keys.clone()),
                    Declared {
                        input: subset(anys(&keys)),
                        output: unchanged(),
                    },
                    1.5,
                )
            }
            "hash" => {
                args::only(name, args, &["columns", "algo", "output"])?;
                let columns = args::names(name, args, "columns")?;
                let algo = Algo::parse(
                    name,
                    &args::optional_string(name, args, "algo")?.unwrap_or_else(|| "sha256".into()),
                )?;
                let output =
                    args::optional_string(name, args, "output")?.unwrap_or_else(|| "hash".into());
                (
                    Op::Hash {
                        columns: columns.clone(),
                        algo,
                        output: output.clone(),
                    },
                    Declared {
                        input: subset(anys(&columns)),
                        output: Some(SchemaDecl::Relative {
                            adds: vec![ColumnDecl::new(output, DataType::Utf8)],
                            drops: Vec::new(),
                            changes: Vec::new(),
                        }),
                    },
                    1.5,
                )
            }
            "mask" => {
                args::only(name, args, &["columns", "mode", "keep"])?;
                let columns = args::names(name, args, "columns")?;
                let keep = args::optional_u64(name, args, "keep")?;
                let mode = match args::optional_string(name, args, "mode")?.as_deref() {
                    None | Some("redact") => MaskMode::Redact,
                    Some("partial") => MaskMode::Partial(keep.unwrap_or(4) as usize),
                    Some("null") => MaskMode::Null,
                    Some("hash") => MaskMode::Hash,
                    Some(other) => {
                        return Err(bad(
                            name,
                            format!("unknown mode `{other}` (use redact, partial, null or hash)"),
                        ));
                    }
                };
                if keep.is_some() && !matches!(mode, MaskMode::Partial(_)) {
                    return Err(bad(name, "`keep` applies to mode=\"partial\" only"));
                }
                let input = columns
                    .iter()
                    .map(|c| {
                        if mode == MaskMode::Null {
                            ColumnDecl::any(c.clone())
                        } else {
                            ColumnDecl::new(c.clone(), DataType::Utf8)
                        }
                    })
                    .collect();
                (
                    Op::Mask { columns, mode },
                    Declared {
                        input: subset(input),
                        output: unchanged(),
                    },
                    1.0,
                )
            }
            "explode" => {
                args::only(name, args, &["column"])?;
                let column = args::string(name, args, "column")?;
                let list = DataType::List(Arc::new(moruna_kernel::arrow::datatypes::Field::new(
                    "item",
                    DataType::Int64,
                    true,
                )));
                (
                    Op::Explode(column.clone()),
                    Declared {
                        input: subset(vec![ColumnDecl::new(column.clone(), list)]),
                        output: Some(SchemaDecl::Relative {
                            adds: Vec::new(),
                            drops: Vec::new(),
                            changes: vec![ColumnDecl::any(column)],
                        }),
                    },
                    2.0,
                )
            }
            "concat_str" => {
                args::only(name, args, &["columns", "separator", "output"])?;
                let columns = args::names(name, args, "columns")?;
                let separator = args::optional_string(name, args, "separator")?.unwrap_or_default();
                let output =
                    args::optional_string(name, args, "output")?.unwrap_or_else(|| "concat".into());
                (
                    Op::Concat {
                        columns: columns.clone(),
                        separator,
                        output: output.clone(),
                    },
                    Declared {
                        input: subset(anys(&columns)),
                        output: Some(SchemaDecl::Relative {
                            adds: vec![ColumnDecl::new(output, DataType::Utf8)],
                            drops: Vec::new(),
                            changes: Vec::new(),
                        }),
                    },
                    1.0,
                )
            }
            "date_trunc" => {
                args::only(name, args, &["column", "unit", "output"])?;
                let column = args::string(name, args, "column")?;
                let unit = TruncUnit::parse(&args::string(name, args, "unit")?)?;
                let output = args::optional_string(name, args, "output")?;
                let ts = DataType::Timestamp(
                    moruna_kernel::arrow::datatypes::TimeUnit::Microsecond,
                    None,
                );
                let out_decl = match &output {
                    None => unchanged(),
                    Some(o) => Some(SchemaDecl::Relative {
                        adds: vec![ColumnDecl::any(o.clone())],
                        drops: Vec::new(),
                        changes: Vec::new(),
                    }),
                };
                (
                    Op::Trunc {
                        column: column.clone(),
                        unit,
                        output,
                    },
                    Declared {
                        input: subset(vec![ColumnDecl::new(column, ts)]),
                        output: out_decl,
                    },
                    1.0,
                )
            }
            other => {
                return Err(MorunaError::Plan(format!(
                    "moruna.std has no kernel `{other}` (the standard kernels are {})",
                    NAMES.join(", ")
                )));
            }
        };
        let args = serde_json::from_str(&canonical(args)).unwrap_or(Value::Null);
        Ok(StdKernel {
            fingerprint: std_fingerprint(name, &args),
            name: name.to_string(),
            args,
            op,
            declared,
            amplification,
        })
    }

    /// The kernel's name; a fused kernel's is its parts' joined with `+`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The canonical arguments; a fused kernel's are `{"stages": [{"name", "args"}, ...]}`.
    pub fn args(&self) -> &Value {
        &self.args
    }

    /// True when this kernel is two or more fused ones.
    pub fn is_fused(&self) -> bool {
        self.name.contains('+')
    }

    fn parts(&self) -> Vec<Value> {
        if self.is_fused() {
            self.args
                .get("stages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            vec![serde_json::json!({"name": self.name, "args": self.args})]
        }
    }

    /// The output schema for a table input, which every standard kernel computes exactly.
    fn table_output(&self, input: &Schema) -> Result<SchemaRef> {
        match &self.op {
            Op::Project(steps) => ops::projection_schema(steps, input),
            Op::Filter(expr) => {
                expr.validate(input)?;
                Ok(Arc::new(input.clone()))
            }
            Op::Fill(values) => {
                ops::fill_check(values, input)?;
                Ok(Arc::new(input.clone()))
            }
            Op::FillFilter(values, expr) => {
                ops::fill_check(values, input)?;
                expr.validate(input)?;
                Ok(Arc::new(input.clone()))
            }
            Op::Dedupe(keys) => {
                ops::dedupe_check(keys, input)?;
                Ok(Arc::new(input.clone()))
            }
            Op::Hash {
                columns, output, ..
            } => ops::hash_schema(columns, output, input),
            Op::Mask { columns, mode } => ops::mask_schema(columns, *mode, input),
            Op::Explode(column) => ops::explode_schema(column, input),
            Op::Concat {
                columns, output, ..
            } => ops::concat_schema(columns, output, input),
            Op::Trunc { column, output, .. } => ops::trunc_schema(column, output.as_deref(), input),
        }
    }
}

/// Fuse `first` then `second` into one stage when their combination is one operation (MH 4.9):
/// any two projections (`cast`, `rename`, `select`, `drop`, or projections already fused), and
/// `filter` after `fill_null`. `None` when they are not fusable.
pub fn fuse(first: &StdKernel, second: &StdKernel) -> Option<StdKernel> {
    let op = match (&first.op, &second.op) {
        (Op::Project(a), Op::Project(b)) => {
            let mut steps = a.clone();
            steps.extend(b.iter().cloned());
            Op::Project(steps)
        }
        (Op::Fill(values), Op::Filter(expr)) => Op::FillFilter(values.clone(), expr.clone()),
        _ => return None,
    };
    let mut stages = first.parts();
    stages.extend(second.parts());
    let args = serde_json::json!({ "stages": stages });
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"moruna-std-fused\0");
    hasher.update(first.fingerprint.0);
    hasher.update(second.fingerprint.0);
    let name = format!("{}+{}", first.name, second.name);
    let mut fused = StdKernel {
        name,
        args,
        op,
        declared: Declared::default(),
        fingerprint: Fingerprint(hasher.finalize().into()),
        amplification: first.amplification.max(second.amplification),
    };
    fused.declared = fused_declaration(first, second, &fused);
    Some(fused)
}

/// A fused kernel takes the union of what its parts take, less the columns an earlier part adds
/// (a later part reads those from the earlier part, not from the input), and gives what the
/// fused operation gives for the input `moruna check` would generate from that, stated exactly.
fn fused_declaration(first: &StdKernel, second: &StdKernel, fused: &StdKernel) -> Declared {
    let mut columns: Vec<ColumnDecl> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    for part in [first, second] {
        if let Some(SchemaDecl::Subset(cols) | SchemaDecl::Exact(cols)) = &part.declared.input {
            for c in cols {
                if !added.contains(&c.name) && !columns.iter().any(|k| k.name == c.name) {
                    columns.push(c.clone());
                }
            }
        }
        let inputs: Vec<&str> = match &part.declared.input {
            Some(SchemaDecl::Subset(cols) | SchemaDecl::Exact(cols)) => {
                cols.iter().map(|c| c.name.as_str()).collect()
            }
            _ => Vec::new(),
        };
        match &part.declared.output {
            Some(SchemaDecl::Relative { adds, .. }) => {
                added.extend(adds.iter().map(|a| a.name.clone()));
            }
            Some(SchemaDecl::Exact(cols) | SchemaDecl::Subset(cols)) => added.extend(
                cols.iter()
                    .filter(|c| !inputs.contains(&c.name.as_str()))
                    .map(|c| c.name.clone()),
            ),
            None => {}
        }
    }
    let input = Some(SchemaDecl::Subset(columns));
    let output = input
        .as_ref()
        .and_then(|i| i.synthetic_schema().ok())
        .and_then(|schema| fused.table_output(&schema).ok())
        .map(|schema| SchemaDecl::from_schema(&schema));
    Declared { input, output }
}

/// Fuse every adjacent fusable pair of a chain, left to right (MH 4.9). The result computes
/// exactly what the chain computes, in fewer stages.
pub fn fuse_chain(chain: Vec<StdKernel>) -> Vec<StdKernel> {
    let mut out: Vec<StdKernel> = Vec::with_capacity(chain.len());
    for kernel in chain {
        if let Some(last) = out.last()
            && let Some(fused) = fuse(last, &kernel)
        {
            let len = out.len();
            out[len - 1] = fused;
            continue;
        }
        out.push(kernel);
    }
    out
}

/// The state of a `dedupe` instance: the key bytes seen so far.
struct Seen {
    keys: HashSet<Vec<u8>>,
    bytes: u64,
}

impl KernelState for Seen {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// Each key as its length, a little-endian u32, then its bytes, in sorted order so the bytes
    /// are a function of the set.
    fn checkpoint(&mut self) -> Result<Option<Vec<u8>>> {
        let mut keys: Vec<&Vec<u8>> = self.keys.iter().collect();
        keys.sort();
        let mut out = Vec::with_capacity(self.bytes as usize + keys.len() * 4);
        for key in keys {
            out.extend_from_slice(&(key.len() as u32).to_le_bytes());
            out.extend_from_slice(key);
        }
        Ok(Some(out))
    }

    fn footprint(&self) -> Option<u64> {
        Some(self.bytes + self.keys.len() as u64 * 32)
    }
}

impl Kernel for StdKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        match self.op {
            Op::Dedupe(_) => KernelKind::Stateful {
                max_instances: core::num::NonZeroUsize::MIN,
            },
            _ => KernelKind::Stateless,
        }
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: Some(self.amplification),
            uses_device_memory: false,
            releases_gil: Some(true),
            preferred_rows: None,
            resume: match self.op {
                Op::Dedupe(_) => ResumePolicy::Checkpoint,
                _ => ResumePolicy::Reinit,
            },
            state_bytes: None,
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
        match input {
            SourceSchema::Table(schema) => Ok(SourceSchema::Table(self.table_output(schema)?)),
            SourceSchema::Tensor { .. } => Err(MorunaError::Plan(format!(
                "moruna.std.{} takes a table, and the input is a tensor",
                self.name
            ))),
        }
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(match self.op {
            Op::Dedupe(_) => Box::new(Seen {
                keys: HashSet::new(),
                bytes: 0,
            }),
            _ => Box::new(NoState),
        })
    }

    fn restore(&self, _ctx: &InitCtx, state: &[u8]) -> Result<Box<dyn KernelState>> {
        let Op::Dedupe(_) = self.op else {
            return Err(MorunaError::Resume(format!(
                "moruna.std.{} keeps no state to restore",
                self.name
            )));
        };
        let mut keys = HashSet::new();
        let mut bytes = 0u64;
        let mut at = 0usize;
        while at < state.len() {
            let len_bytes: [u8; 4] = state
                .get(at..at + 4)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(|| {
                    MorunaError::Resume("moruna.std.dedupe: truncated checkpoint".into())
                })?;
            let len = u32::from_le_bytes(len_bytes) as usize;
            let key = state.get(at + 4..at + 4 + len).ok_or_else(|| {
                MorunaError::Resume("moruna.std.dedupe: truncated checkpoint".into())
            })?;
            bytes += len as u64;
            keys.insert(key.to_vec());
            at += 4 + len;
        }
        Ok(Box::new(Seen { keys, bytes }))
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Payload::Table(batch, _) = &input else {
            return Err(MorunaError::Plan(format!(
                "moruna.std.{} takes a table, and was given a tensor",
                self.name
            )));
        };
        let out = match &self.op {
            Op::Project(steps) => ops::project(steps, batch)?,
            Op::Filter(expr) => ops::filter(expr, batch)?,
            Op::Fill(values) => ops::fill(values, batch)?,
            Op::FillFilter(values, expr) => ops::fill_then_filter(values, expr, batch)?,
            Op::Dedupe(keys) => {
                let seen = state.as_any_mut().downcast_mut::<Seen>().ok_or_else(|| {
                    MorunaError::Plan("moruna.std.dedupe was given a state it did not make".into())
                })?;
                let before = seen.keys.len();
                let out = ops::dedupe(keys, &mut seen.keys, batch)?;
                if seen.keys.len() != before {
                    seen.bytes = seen.keys.iter().map(|k| k.len() as u64).sum();
                }
                out
            }
            Op::Hash {
                columns,
                algo,
                output,
            } => ops::hash(columns, *algo, output, batch)?,
            Op::Mask { columns, mode } => ops::mask(columns, *mode, batch)?,
            Op::Explode(column) => ops::explode(column, batch)?,
            Op::Concat {
                columns,
                separator,
                output,
            } => ops::concat_str(columns, separator, output, batch)?,
            Op::Trunc {
                column,
                unit,
                output,
            } => ops::date_trunc(column, *unit, output.as_deref(), batch)?,
        };
        drop(input);
        Payload::table(out)
    }
}
