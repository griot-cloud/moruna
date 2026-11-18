//! `moruna.expr`: a tree of elementwise operations, evaluated in one pass (spec/expr.md).
//!
//! The tree has Substrait's shape (literal, column, cast, if-then, switch, function call) and
//! Arrow compute's vocabulary. It is written as JSON, as the `expr` standard kernel's argument:
//!
//! ```json
//! {"outputs": [{"name": "mph", "expr": {"call": "divide", "args": [
//!     {"column": "trip_distance"},
//!     {"call": "divide", "args": [{"column": "duration_s"}, {"literal": 3600.0}]}]}}]}
//! ```
//!
//! [`Tree::from_json`] reads it strictly (an unknown key or a misshapen node is an error naming
//! where it is); [`SCHEMA`] is the same grammar as a JSON Schema. [`Tree::bind`] checks the tree
//! against an input schema and compiles it into a [`Program`]; [`Program::eval`] evaluates every
//! output over a batch in one pass of cache-sized chunks.

mod bind;
mod eval;
pub mod legacy;
pub mod round;
pub mod temporal;

use std::sync::{Arc, Mutex};

use moruna_kernel::arrow::array::ArrayRef;
use moruna_kernel::arrow::datatypes::{DataType, Schema, SchemaRef, TimeUnit};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::declare::{TypeDecl, parse_type, type_name};
use moruna_kernel::{MorunaError, Result};
use serde_json::{Map, Value, json};

pub use bind::{FUNCTIONS, Program};

/// The JSON Schema of the `expr` kernel's arguments (spec/expr.schema.json).
pub const SCHEMA: &str = include_str!("../../spec/expr.schema.json");

/// A node of the tree.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// An input column by name.
    Column(String),
    /// A literal: a JSON scalar, typed by `type` or by its JSON kind.
    Literal {
        /// The value; `null` needs a type.
        value: Value,
        /// The declared type.
        ty: Option<DataType>,
    },
    /// A one-element array taken as a literal of its type. Not written in JSON: the old filter
    /// language's translation makes it, from a literal converted to its column's type.
    Scalar(ArrayRef),
    /// A conversion to another type.
    Cast {
        /// What is converted.
        arg: Box<Node>,
        /// The type it becomes.
        to: DataType,
        /// True: a value that does not convert exactly is an error (Arrow's safe cast).
        safe: bool,
    },
    /// The first clause whose condition is true; a null condition counts as false. Arrow's
    /// `case_when`.
    If {
        /// Each condition and its value.
        clauses: Vec<(Node, Node)>,
        /// The value when no condition is true; null when absent.
        otherwise: Option<Box<Node>>,
    },
    /// The value of the first case whose literal equals the argument; a null argument matches
    /// none.
    Switch {
        /// What is matched.
        arg: Box<Node>,
        /// Each literal and its value.
        cases: Vec<(Node, Node)>,
        /// The value when no case matches; null when absent.
        otherwise: Option<Box<Node>>,
    },
    /// A function of the vocabulary.
    Call {
        /// Its Arrow compute name.
        name: String,
        /// Its arguments.
        args: Vec<Node>,
        /// Its options, by name.
        options: Map<String, Value>,
    },
}

/// One named output of the tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    /// The output column's name.
    pub name: String,
    /// What it computes.
    pub expr: Node,
}

/// The tree: its outputs, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct Tree {
    /// The outputs, which are the output batch's columns in this order.
    pub outputs: Vec<Output>,
}

fn bad(at: &str, msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Plan(format!("moruna.std.expr: {at}: {msg}"))
}

fn object<'a>(at: &str, v: &'a Value) -> Result<&'a Map<String, Value>> {
    v.as_object()
        .ok_or_else(|| bad(at, format!("expected an object, not {v}")))
}

fn only(at: &str, map: &Map<String, Value>, known: &[&str]) -> Result<()> {
    for key in map.keys() {
        if !known.contains(&key.as_str()) {
            return Err(bad(
                at,
                format!("unknown key `{key}` (expected one of {})", known.join(", ")),
            ));
        }
    }
    Ok(())
}

fn required<'a>(at: &str, map: &'a Map<String, Value>, key: &str) -> Result<&'a Value> {
    map.get(key)
        .ok_or_else(|| bad(at, format!("`{key}` is required")))
}

/// A type by its spelling: `moruna check`'s (`int64`, `double`, `timestamp[us, tz=UTC]`), and
/// `duration[s|ms|us|ns]`.
pub fn parse_data_type(spelled: &str) -> Result<DataType> {
    let t = spelled.trim();
    if let Some(unit) = t
        .strip_prefix("duration[")
        .and_then(|r| r.strip_suffix(']'))
    {
        let unit = match unit.trim() {
            "s" => TimeUnit::Second,
            "ms" => TimeUnit::Millisecond,
            "us" => TimeUnit::Microsecond,
            "ns" => TimeUnit::Nanosecond,
            other => {
                return Err(MorunaError::Plan(format!(
                    "unknown duration unit `{other}`"
                )));
            }
        };
        return Ok(DataType::Duration(unit));
    }
    match parse_type(t)? {
        TypeDecl::Exact(dt) => Ok(dt),
        TypeDecl::Any => Err(MorunaError::Plan("`any` is not a type a value has".into())),
    }
}

/// A type's spelling, the inverse of [`parse_data_type`].
pub fn spell_data_type(dt: &DataType) -> String {
    match dt {
        DataType::Duration(unit) => format!(
            "duration[{}]",
            match unit {
                TimeUnit::Second => "s",
                TimeUnit::Millisecond => "ms",
                TimeUnit::Microsecond => "us",
                TimeUnit::Nanosecond => "ns",
            }
        ),
        other => type_name(other),
    }
}

impl Node {
    /// A node from its JSON form; `at` names where it is, for errors.
    pub fn from_json(at: &str, v: &Value) -> Result<Node> {
        let map = object(at, v)?;
        let kinds = ["column", "literal", "cast", "if", "switch", "call"];
        let present: Vec<&str> = kinds
            .iter()
            .copied()
            .filter(|k| map.contains_key(*k))
            .collect();
        let kind = match present.as_slice() {
            [one] => *one,
            [] => {
                return Err(bad(at, format!("a node is one of {}", kinds.join(", "))));
            }
            many => {
                return Err(bad(
                    at,
                    format!("a node is one kind, and this names {}", many.join(" and ")),
                ));
            }
        };
        Ok(match kind {
            "column" => {
                only(at, map, &["column"])?;
                let name = map["column"]
                    .as_str()
                    .ok_or_else(|| bad(at, "`column` names a column with a string"))?;
                Node::Column(name.to_string())
            }
            "literal" => {
                only(at, map, &["literal", "type"])?;
                let value = map["literal"].clone();
                if value.is_array() || value.is_object() {
                    return Err(bad(
                        at,
                        "a literal is a number, a string, a boolean or null",
                    ));
                }
                let ty = match map.get("type") {
                    None => None,
                    Some(Value::String(s)) => {
                        Some(parse_data_type(s).map_err(|e| bad(at, e.to_string()))?)
                    }
                    Some(other) => return Err(bad(at, format!("`type` is a string, not {other}"))),
                };
                if value.is_null() && ty.is_none() {
                    return Err(bad(at, "a null literal needs a `type`"));
                }
                Node::Literal { value, ty }
            }
            "cast" => {
                only(at, map, &["cast", "to", "safe"])?;
                let to = required(at, map, "to")?
                    .as_str()
                    .ok_or_else(|| bad(at, "`to` is a type's spelling"))?;
                let safe = match map.get("safe") {
                    None => true,
                    Some(Value::Bool(b)) => *b,
                    Some(other) => {
                        return Err(bad(at, format!("`safe` is true or false, not {other}")));
                    }
                };
                Node::Cast {
                    arg: Box::new(Node::from_json(&format!("{at}.cast"), &map["cast"])?),
                    to: parse_data_type(to).map_err(|e| bad(at, e.to_string()))?,
                    safe,
                }
            }
            "if" => {
                only(at, map, &["if", "else"])?;
                let items = map["if"]
                    .as_array()
                    .filter(|a| !a.is_empty())
                    .ok_or_else(|| {
                        bad(at, "`if` lists one or more {\"when\", \"then\"} clauses")
                    })?;
                let mut clauses = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    let here = format!("{at}.if[{i}]");
                    let clause = object(&here, item)?;
                    only(&here, clause, &["when", "then"])?;
                    clauses.push((
                        Node::from_json(&format!("{here}.when"), required(&here, clause, "when")?)?,
                        Node::from_json(&format!("{here}.then"), required(&here, clause, "then")?)?,
                    ));
                }
                Node::If {
                    clauses,
                    otherwise: Self::otherwise(at, map)?,
                }
            }
            "switch" => {
                only(at, map, &["switch", "cases", "else"])?;
                let items = required(at, map, "cases")?
                    .as_array()
                    .filter(|a| !a.is_empty())
                    .ok_or_else(|| bad(at, "`cases` lists one or more {\"match\", \"then\"}"))?;
                let mut cases = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    let here = format!("{at}.cases[{i}]");
                    let case = object(&here, item)?;
                    only(&here, case, &["match", "then"])?;
                    let matched =
                        Node::from_json(&format!("{here}.match"), required(&here, case, "match")?)?;
                    if !matches!(matched, Node::Literal { .. }) {
                        return Err(bad(&here, "`match` is a literal"));
                    }
                    cases.push((
                        matched,
                        Node::from_json(&format!("{here}.then"), required(&here, case, "then")?)?,
                    ));
                }
                Node::Switch {
                    arg: Box::new(Node::from_json(&format!("{at}.switch"), &map["switch"])?),
                    cases,
                    otherwise: Self::otherwise(at, map)?,
                }
            }
            _ => {
                only(at, map, &["call", "args", "options"])?;
                let name = map["call"]
                    .as_str()
                    .ok_or_else(|| bad(at, "`call` names a function with a string"))?;
                let args = match map.get("args") {
                    None => Vec::new(),
                    Some(Value::Array(items)) => items
                        .iter()
                        .enumerate()
                        .map(|(i, a)| Node::from_json(&format!("{at}.args[{i}]"), a))
                        .collect::<Result<Vec<_>>>()?,
                    Some(other) => return Err(bad(at, format!("`args` is a list, not {other}"))),
                };
                let options = match map.get("options") {
                    None => Map::new(),
                    Some(Value::Object(o)) => o.clone(),
                    Some(other) => {
                        return Err(bad(at, format!("`options` is an object, not {other}")));
                    }
                };
                Node::Call {
                    name: name.to_string(),
                    args,
                    options,
                }
            }
        })
    }

    fn otherwise(at: &str, map: &Map<String, Value>) -> Result<Option<Box<Node>>> {
        map.get("else")
            .map(|e| Node::from_json(&format!("{at}.else"), e).map(Box::new))
            .transpose()
    }

    /// The node's JSON form. A [`Node::Scalar`] is written as a literal of its type whose value
    /// is its text, for reading only.
    pub fn to_json(&self) -> Value {
        match self {
            Node::Column(name) => json!({ "column": name }),
            Node::Literal { value, ty } => match ty {
                Some(ty) => json!({ "literal": value, "type": spell_data_type(ty) }),
                None => json!({ "literal": value }),
            },
            Node::Scalar(array) => {
                let text = moruna_kernel::arrow::util::display::array_value_to_string(array, 0)
                    .unwrap_or_default();
                json!({ "literal": text, "type": spell_data_type(array.data_type()) })
            }
            Node::Cast { arg, to, safe } => {
                let mut m = json!({ "cast": arg.to_json(), "to": spell_data_type(to) });
                if !safe {
                    m["safe"] = Value::Bool(false);
                }
                m
            }
            Node::If { clauses, otherwise } => {
                let clauses: Vec<Value> = clauses
                    .iter()
                    .map(|(w, t)| json!({ "when": w.to_json(), "then": t.to_json() }))
                    .collect();
                let mut m = json!({ "if": clauses });
                if let Some(e) = otherwise {
                    m["else"] = e.to_json();
                }
                m
            }
            Node::Switch {
                arg,
                cases,
                otherwise,
            } => {
                let cases: Vec<Value> = cases
                    .iter()
                    .map(|(w, t)| json!({ "match": w.to_json(), "then": t.to_json() }))
                    .collect();
                let mut m = json!({ "switch": arg.to_json(), "cases": cases });
                if let Some(e) = otherwise {
                    m["else"] = e.to_json();
                }
                m
            }
            Node::Call {
                name,
                args,
                options,
            } => {
                let mut m = json!({ "call": name });
                if !args.is_empty() {
                    m["args"] = Value::Array(args.iter().map(Node::to_json).collect());
                }
                if !options.is_empty() {
                    m["options"] = Value::Object(options.clone());
                }
                m
            }
        }
    }

    /// The input columns the node reads, first mention first.
    pub fn columns(&self, out: &mut Vec<String>) {
        match self {
            Node::Column(name) => {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
            Node::Literal { .. } | Node::Scalar(_) => {}
            Node::Cast { arg, .. } => arg.columns(out),
            Node::If { clauses, otherwise } => {
                for (w, t) in clauses {
                    w.columns(out);
                    t.columns(out);
                }
                if let Some(e) = otherwise {
                    e.columns(out);
                }
            }
            Node::Switch {
                arg,
                cases,
                otherwise,
            } => {
                arg.columns(out);
                for (_, t) in cases {
                    t.columns(out);
                }
                if let Some(e) = otherwise {
                    e.columns(out);
                }
            }
            Node::Call { args, .. } => {
                for a in args {
                    a.columns(out);
                }
            }
        }
    }

    /// A call of `name` on `args`, with no options.
    pub fn call(name: &str, args: Vec<Node>) -> Node {
        Node::Call {
            name: name.to_string(),
            args,
            options: Map::new(),
        }
    }

    /// A column.
    pub fn col(name: &str) -> Node {
        Node::Column(name.to_string())
    }

    /// A literal of its JSON kind's type.
    pub fn lit(value: impl Into<Value>) -> Node {
        Node::Literal {
            value: value.into(),
            ty: None,
        }
    }
}

impl Tree {
    /// The tree of the `expr` kernel's arguments, read strictly.
    pub fn from_json(args: &Value) -> Result<Tree> {
        let map = object("arguments", args)?;
        only("arguments", map, &["outputs"])?;
        let items = required("arguments", map, "outputs")?
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| bad("outputs", "lists one or more {\"name\", \"expr\"}"))?;
        let mut outputs: Vec<Output> = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let at = format!("outputs[{i}]");
            let o = object(&at, item)?;
            only(&at, o, &["name", "expr"])?;
            let name = required(&at, o, "name")?
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad(&at, "`name` is a non-empty string"))?;
            if outputs.iter().any(|p| p.name == name) {
                return Err(bad(&at, format!("two outputs are named `{name}`")));
            }
            outputs.push(Output {
                name: name.to_string(),
                expr: Node::from_json(&format!("{at}.expr"), required(&at, o, "expr")?)?,
            });
        }
        Ok(Tree { outputs })
    }

    /// The tree as the `expr` kernel's arguments.
    pub fn to_json(&self) -> Value {
        let outputs: Vec<Value> = self
            .outputs
            .iter()
            .map(|o| json!({ "name": o.name, "expr": o.expr.to_json() }))
            .collect();
        json!({ "outputs": outputs })
    }

    /// The input columns the tree reads, first mention first.
    pub fn columns(&self) -> Vec<String> {
        let mut out = Vec::new();
        for o in &self.outputs {
            o.expr.columns(&mut out);
        }
        out
    }

    /// Check the tree against `input` and compile it (spec/expr.md, "Types").
    pub fn bind(&self, input: &moruna_kernel::arrow::datatypes::Schema) -> Result<Arc<Program>> {
        bind::bind(self, input).map(Arc::new)
    }
}

/// A program compiled for the last input schema it was asked for: the batches of a stage share
/// one schema, so a tree is bound once per stage, not once per batch.
#[derive(Clone, Debug, Default)]
pub struct ProgramCache(Arc<Mutex<Option<BoundProgram>>>);

/// A program and the schema it was bound for.
type BoundProgram = (SchemaRef, Arc<Program>);

impl ProgramCache {
    /// The program for `schema`: the cached one when it was compiled for an equal schema, else
    /// `bind`'s, which is cached in its place.
    pub fn get_or_bind(
        &self,
        schema: &SchemaRef,
        bind: impl FnOnce(&Schema) -> Result<Arc<Program>>,
    ) -> Result<Arc<Program>> {
        if let Ok(guard) = self.0.lock()
            && let Some((cached, program)) = guard.as_ref()
            && (Arc::ptr_eq(cached, schema) || cached.as_ref() == schema.as_ref())
        {
            return Ok(Arc::clone(program));
        }
        let program = bind(schema)?;
        if let Ok(mut guard) = self.0.lock() {
            *guard = Some((Arc::clone(schema), Arc::clone(&program)));
        }
        Ok(program)
    }
}

/// The `expr` kernel's tree, compiled once per input schema.
#[derive(Clone, Debug)]
pub struct Compiled {
    /// The tree, as its arguments wrote it.
    pub tree: Tree,
    cache: ProgramCache,
}

impl Compiled {
    /// The tree, not yet bound to a schema.
    pub fn new(tree: Tree) -> Compiled {
        Compiled {
            tree,
            cache: ProgramCache::default(),
        }
    }

    /// The program for `schema`.
    pub fn program(&self, schema: &SchemaRef) -> Result<Arc<Program>> {
        self.cache.get_or_bind(schema, |s| self.tree.bind(s))
    }

    /// Every output over `batch`.
    pub fn eval(&self, batch: &RecordBatch) -> Result<RecordBatch> {
        self.program(&batch.schema())?.eval(batch)
    }
}

#[cfg(test)]
mod tests;
