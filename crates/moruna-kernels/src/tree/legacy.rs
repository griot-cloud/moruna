//! The old `moruna.std.filter` language (`crate::expr`) as an `expr` tree (spec/expr.md, "The
//! filter language").
//!
//! A document written in the old language keeps its rows: each construct becomes the tree that
//! computes what the old evaluator computed.
//!
//! - `a op b` is the comparison of the same name. A literal is converted to the type of the
//!   column it is compared with exactly as before (Arrow's cast, an error where it does not
//!   convert), and two columns must have one type, as before.
//! - Floats compare with `"order": "total"`: the old evaluator compared them in IEEE 754 total
//!   order (arrow-rs), so NaN equals NaN and is above every number, and -0.0 is below 0.0.
//! - `and`, `or` and `not` are `and_kleene`, `or_kleene` and `invert`; `is_null(c)` and
//!   `is_not_null(c)` are `is_null` and `is_valid`.

use std::sync::Arc;

use moruna_kernel::arrow::array::{AsArray, BooleanArray};
use moruna_kernel::arrow::datatypes::{DataType, Schema, SchemaRef};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{MorunaError, Result};
use serde_json::{Map, Value};

use super::{Node, Output, Program, ProgramCache, Tree};
use crate::expr::{Expr, Op, Operand};

fn plan(msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Plan(format!("moruna.std.filter: {msg}"))
}

fn column_type(schema: &Schema, name: &str) -> Result<DataType> {
    schema
        .field_with_name(name)
        .map(|f| f.data_type().clone())
        .map_err(|_| plan(format!("no column `{name}`")))
}

fn is_float(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Float16 | DataType::Float32 | DataType::Float64
    )
}

fn operand(schema: &Schema, this: &Operand, other: &Operand) -> Result<(Node, DataType)> {
    match this {
        Operand::Column(c) => Ok((Node::Column(c.clone()), column_type(schema, c)?)),
        Operand::Literal(lit) => {
            let Operand::Column(c) = other else {
                return Err(plan("a comparison needs at least one column"));
            };
            let ty = column_type(schema, c)?;
            Ok((Node::Scalar(lit.scalar_of(&ty)?.into_inner()), ty))
        }
    }
}

/// The tree that computes `expr` over batches of `schema`.
pub fn translate(expr: &Expr, schema: &Schema) -> Result<Node> {
    Ok(match expr {
        Expr::Cmp(l, op, r) => {
            let (left, lt) = operand(schema, l, r)?;
            let (right, rt) = operand(schema, r, l)?;
            if lt != rt {
                return Err(plan(format!(
                    "a comparison of a {lt} with a {rt}; the filter language compares one type"
                )));
            }
            let name = match op {
                Op::Eq => "equal",
                Op::Ne => "not_equal",
                Op::Lt => "less",
                Op::Le => "less_equal",
                Op::Gt => "greater",
                Op::Ge => "greater_equal",
            };
            let mut options = Map::new();
            if is_float(&lt) {
                options.insert("order".into(), Value::String("total".into()));
            }
            Node::Call {
                name: name.into(),
                args: vec![left, right],
                options,
            }
        }
        Expr::IsNull(c) => {
            column_type(schema, c)?;
            Node::call("is_null", vec![Node::Column(c.clone())])
        }
        Expr::IsNotNull(c) => {
            column_type(schema, c)?;
            Node::call("is_valid", vec![Node::Column(c.clone())])
        }
        Expr::And(a, b) => Node::call(
            "and_kleene",
            vec![translate(a, schema)?, translate(b, schema)?],
        ),
        Expr::Or(a, b) => Node::call(
            "or_kleene",
            vec![translate(a, schema)?, translate(b, schema)?],
        ),
        Expr::Not(a) => Node::call("invert", vec![translate(a, schema)?]),
    })
}

/// The one-output tree whose output, `keep`, is the filter's mask.
pub fn tree(expr: &Expr, schema: &Schema) -> Result<Tree> {
    Ok(Tree {
        outputs: vec![Output {
            name: "keep".into(),
            expr: translate(expr, schema)?,
        }],
    })
}

/// The translation as JSON, as a document would write it.
pub fn to_json(expr: &Expr, schema: &Schema) -> Result<Value> {
    Ok(translate(expr, schema)?.to_json())
}

/// The old filter's predicate, evaluated as its translation, compiled once per input schema.
#[derive(Clone, Debug)]
pub struct Legacy {
    /// The predicate as the old language parsed it.
    pub expr: Expr,
    cache: ProgramCache,
}

impl Legacy {
    /// The predicate, not yet bound to a schema.
    pub fn new(expr: Expr) -> Legacy {
        Legacy {
            expr,
            cache: ProgramCache::default(),
        }
    }

    /// The program of the translation for `schema`.
    pub fn program(&self, schema: &SchemaRef) -> Result<Arc<Program>> {
        self.cache
            .get_or_bind(schema, |s| tree(&self.expr, s)?.bind(s))
    }

    /// The mask: true where the predicate is, null where it is unknown.
    pub fn mask(&self, batch: &RecordBatch) -> Result<BooleanArray> {
        let out = self.program(&batch.schema())?.eval(batch)?;
        Ok(out.column(0).as_boolean().clone())
    }
}
