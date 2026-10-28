//! A validator for the JSON Schema a kind's settings are declared with (SDD 22 e.3).
//!
//! A kind's schema is derived by `schemars` from the type its adapter reads its settings with
//! (SDD 18 KD1), and the check an embedder runs has only the schema, not the type. So this reads
//! exactly the subset of JSON Schema 2020-12 that `schemars` writes for those types, and nothing
//! else: a schema that uses a keyword outside the subset is refused when it enters a
//! [`crate::Catalogue`] ([`SchemaFault`]), so the subset cannot drift without a test failing, and
//! a keyword the validator would otherwise ignore is never ignored silently.
//!
//! The subset: `type` (a name or a list of them), `properties`, `required`,
//! `additionalProperties` (a boolean or a schema), `enum`, `const`, `oneOf`, `anyOf`, `allOf`,
//! `items`, `prefixItems`, `minItems`, `maxItems`, `$ref` to `#/$defs/<name>`, `$defs`,
//! `minimum`, `maximum`, `minLength`, and the annotations `$schema`, `title`, `description`,
//! `examples`, `default` and `format`. A schema may also be `true` or `false`.
//!
//! Two readings are the type's, not the schema's, and are what make the verdict the one
//! `serde` gives: a number is an `integer` only when it is written without a fraction or an
//! exponent (`5`, not `5.0`), and an integer `format` `schemars` writes (`uint8`, `int32`,
//! `uint64`, ...) bounds the integer to that width, since `schemars` writes `minimum: 0` for an
//! unsigned type and no `maximum`. Any other `format` is an annotation and is not checked.

use std::cmp::Ordering;

use serde_json::Value;

/// A keyword every schema may carry and the validator reads.
const VALIDATING: [&str; 17] = [
    "type",
    "properties",
    "required",
    "additionalProperties",
    "enum",
    "const",
    "oneOf",
    "anyOf",
    "allOf",
    "items",
    "prefixItems",
    "minItems",
    "maxItems",
    "$ref",
    "minimum",
    "maximum",
    "minLength",
];

/// A keyword that describes the schema and constrains nothing; `$defs` holds the schemas a
/// `$ref` names.
const ANNOTATING: [&str; 7] = [
    "$schema",
    "$defs",
    "title",
    "description",
    "examples",
    "default",
    "format",
];

/// The names `type` may take.
const TYPES: [&str; 7] = [
    "null", "boolean", "integer", "number", "string", "array", "object",
];

/// Why a schema is not one this crate can check against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaFault {
    /// Where in the schema, as a JSON pointer (`/properties/url`; empty is the schema's root).
    pub at: String,
    /// What is wrong there.
    pub why: String,
}

impl core::fmt::Display for SchemaFault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.at.is_empty() {
            write!(f, "{}", self.why)
        } else {
            write!(f, "at `{}`: {}", self.at, self.why)
        }
    }
}

fn fault(at: &str, why: impl Into<String>) -> SchemaFault {
    SchemaFault {
        at: at.to_string(),
        why: why.into(),
    }
}

/// Whether `root` is a schema in the subset: every keyword one the validator reads or an
/// annotation, every `$ref` one that resolves, every keyword's value the shape it needs.
pub fn admit(root: &Value) -> Result<(), SchemaFault> {
    admit_at(root, root, "")
}

fn admit_at(root: &Value, schema: &Value, at: &str) -> Result<(), SchemaFault> {
    let object = match schema {
        Value::Bool(_) => return Ok(()),
        Value::Object(object) => object,
        other => {
            return Err(fault(
                at,
                format!("a schema is an object or a boolean, not {}", kind_of(other)),
            ));
        }
    };
    for (keyword, value) in object {
        let here = format!("{at}/{}", pointer_escape(keyword));
        if !VALIDATING.contains(&keyword.as_str()) && !ANNOTATING.contains(&keyword.as_str()) {
            return Err(fault(
                &here,
                format!(
                    "the keyword `{keyword}` is outside the subset of JSON Schema this crate checks"
                ),
            ));
        }
        match keyword.as_str() {
            "type" => admit_type(value, &here)?,
            "required" => admit_strings(value, &here, "required")?,
            "enum" => {
                if !value.is_array() {
                    return Err(fault(&here, "`enum` is a list"));
                }
            }
            "minimum" | "maximum" => {
                if !value.is_number() {
                    return Err(fault(&here, format!("`{keyword}` is a number")));
                }
            }
            "minLength" | "minItems" | "maxItems" => {
                if value.as_u64().is_none() {
                    return Err(fault(&here, format!("`{keyword}` is a count")));
                }
            }
            "properties" | "$defs" => {
                let Value::Object(members) = value else {
                    return Err(fault(&here, format!("`{keyword}` is an object of schemas")));
                };
                for (name, member) in members {
                    admit_at(root, member, &format!("{here}/{}", pointer_escape(name)))?;
                }
            }
            "additionalProperties" | "items" => admit_at(root, value, &here)?,
            "prefixItems" | "oneOf" | "anyOf" | "allOf" => {
                let Value::Array(members) = value else {
                    return Err(fault(&here, format!("`{keyword}` is a list of schemas")));
                };
                for (i, member) in members.iter().enumerate() {
                    admit_at(root, member, &format!("{here}/{i}"))?;
                }
            }
            "$ref" => {
                let Some(target) = value.as_str() else {
                    return Err(fault(&here, "`$ref` is a string"));
                };
                resolve(root, target).map_err(|why| fault(&here, why))?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn admit_type(value: &Value, at: &str) -> Result<(), SchemaFault> {
    let names: Vec<&Value> = match value {
        Value::Array(many) => many.iter().collect(),
        one => vec![one],
    };
    for name in names {
        match name.as_str() {
            Some(known) if TYPES.contains(&known) => {}
            _ => {
                return Err(fault(
                    at,
                    format!("`type` names one of {}, not {name}", TYPES.join(", ")),
                ));
            }
        }
    }
    Ok(())
}

fn admit_strings(value: &Value, at: &str, keyword: &str) -> Result<(), SchemaFault> {
    match value.as_array() {
        Some(items) if items.iter().all(Value::is_string) => Ok(()),
        _ => Err(fault(at, format!("`{keyword}` is a list of strings"))),
    }
}

/// The schema a `$ref` names: `#/$defs/<name>` of the root, nothing else.
fn resolve<'a>(root: &'a Value, target: &str) -> Result<&'a Value, String> {
    let name = target.strip_prefix("#/$defs/").ok_or_else(|| {
        format!("`$ref` {target:?} is not `#/$defs/<name>`, the only reference this crate follows")
    })?;
    root.get("$defs")
        .and_then(|defs| defs.get(pointer_unescape(name)))
        .ok_or_else(|| format!("`$ref` {target:?} names a definition the schema does not have"))
}

fn pointer_escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn pointer_unescape(segment: &str) -> String {
    segment.replace("~1", "/").replace("~0", "~")
}

/// One place a value is not what a schema allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The value's place, from the root of what was checked.
    pub path: Vec<Step>,
    /// What is wrong with it.
    pub kind: Wrong,
}

/// One step into a value: an object's key or an array's index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// A key of an object.
    Key(String),
    /// An index of an array.
    Index(usize),
}

/// How a value is wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wrong {
    /// A required key is missing: the violation's path is the key's.
    Missing,
    /// A key no property declares, in an object that allows no other: the path is the key's.
    Undeclared,
    /// The value is of a type the schema does not allow; the names are the allowed ones.
    Type {
        /// What the schema allows.
        allowed: Vec<String>,
        /// What the value is.
        found: &'static str,
    },
    /// The value is not one of the listed ones.
    NotOneOf(Vec<Value>),
    /// The value is not the one allowed.
    NotConst(Value),
    /// A number below what is allowed.
    Below(String),
    /// A number above what is allowed.
    Above(String),
    /// A string shorter than allowed.
    TooShort(u64),
    /// An array with fewer items than allowed.
    TooFewItems(u64),
    /// An array with more items than allowed.
    TooManyItems(u64),
    /// The value matches none of the forms a list of schemas allows.
    NoForm {
        /// What each form allows, in a few words.
        forms: Vec<String>,
        /// What the value is.
        found: &'static str,
    },
    /// The value matches more than one of the forms `oneOf` allows exactly one of.
    ManyForms,
    /// The value is false where the schema is `false`: nothing is allowed there.
    Forbidden,
}

/// Every place `value` is not what `schema` allows, in document order; empty when it is
/// allowed. `root` is the schema `$ref`s are resolved in. A schema that is not in the subset
/// ([`admit`]) is not one this can be asked about.
pub fn validate(root: &Value, schema: &Value, value: &Value) -> Vec<Violation> {
    let mut found = Vec::new();
    walk(root, schema, value, &mut Vec::new(), &mut found, 0);
    found
}

/// A schema may refer to itself through `$ref`; a chain this deep is one that never ends.
const DEPTH: usize = 64;

fn report(found: &mut Vec<Violation>, path: &[Step], kind: Wrong) {
    found.push(Violation {
        path: path.to_vec(),
        kind,
    });
}

fn walk(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &mut Vec<Step>,
    found: &mut Vec<Violation>,
    depth: usize,
) {
    let object = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => return report(found, path, Wrong::Forbidden),
        Value::Object(object) => object,
        // `admit` refuses every other shape, so one here is a schema nobody admitted.
        _ => return report(found, path, Wrong::Forbidden),
    };
    if depth > DEPTH {
        return report(found, path, Wrong::Forbidden);
    }
    if let Some(Ok(target)) = object
        .get("$ref")
        .and_then(Value::as_str)
        .map(|target| resolve(root, target))
    {
        walk(root, target, value, path, found, depth + 1);
    }
    if let Some(allowed) = object.get("type") {
        let names = type_names(allowed);
        if !names.iter().any(|name| has_type(value, name)) {
            return report(
                found,
                path,
                Wrong::Type {
                    allowed: names.iter().map(|n| n.to_string()).collect(),
                    found: kind_of(value),
                },
            );
        }
    }
    if let Some(Value::Array(listed)) = object.get("enum")
        && !listed.contains(value)
    {
        report(found, path, Wrong::NotOneOf(listed.clone()));
    }
    if let Some(only) = object.get("const")
        && only != value
    {
        report(found, path, Wrong::NotConst(only.clone()));
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        let Some(Value::Array(forms)) = object.get(keyword) else {
            continue;
        };
        if keyword == "allOf" {
            for form in forms {
                walk(root, form, value, path, found, depth + 1);
            }
            continue;
        }
        let mut refusals = Vec::new();
        let mut matching = 0;
        for form in forms {
            let mut inner = Vec::new();
            walk(root, form, value, path, &mut inner, depth + 1);
            if inner.is_empty() {
                matching += 1;
            } else {
                refusals.push(describe(root, form));
            }
        }
        if matching == 0 {
            report(
                found,
                path,
                Wrong::NoForm {
                    forms: refusals,
                    found: kind_of(value),
                },
            );
        } else if keyword == "oneOf" && matching > 1 {
            report(found, path, Wrong::ManyForms);
        }
    }
    match value {
        Value::Object(members) => walk_object(root, object, members, path, found, depth),
        Value::Array(items) => walk_array(root, object, items, path, found, depth),
        Value::String(text) => {
            if let Some(least) = object.get("minLength").and_then(Value::as_u64)
                && (text.chars().count() as u64) < least
            {
                report(found, path, Wrong::TooShort(least));
            }
        }
        Value::Number(number) => walk_number(object, number, path, found),
        Value::Null | Value::Bool(_) => {}
    }
}

fn walk_object(
    root: &Value,
    schema: &serde_json::Map<String, Value>,
    members: &serde_json::Map<String, Value>,
    path: &mut Vec<Step>,
    found: &mut Vec<Violation>,
    depth: usize,
) {
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(Value::Array(required)) = schema.get("required") {
        for key in required.iter().filter_map(Value::as_str) {
            if !members.contains_key(key) {
                path.push(Step::Key(key.to_string()));
                found.push(Violation {
                    path: path.clone(),
                    kind: Wrong::Missing,
                });
                path.pop();
            }
        }
    }
    // In key order, whatever order the map keeps: with `serde_json`'s `preserve_order` feature,
    // which any crate in a build may switch on, a map keeps the order a key was written in, and a
    // refusal's place in the list must not depend on a feature.
    let mut keys: Vec<&String> = members.keys().collect();
    keys.sort();
    for key in keys {
        let member = &members[key];
        path.push(Step::Key(key.clone()));
        match properties.and_then(|declared| declared.get(key)) {
            Some(declared) => walk(root, declared, member, path, found, depth + 1),
            None => match schema.get("additionalProperties") {
                Some(Value::Bool(false)) => found.push(Violation {
                    path: path.clone(),
                    kind: Wrong::Undeclared,
                }),
                Some(Value::Bool(true)) | None => {}
                Some(other) => walk(root, other, member, path, found, depth + 1),
            },
        }
        path.pop();
    }
}

fn walk_array(
    root: &Value,
    schema: &serde_json::Map<String, Value>,
    items: &[Value],
    path: &mut Vec<Step>,
    found: &mut Vec<Violation>,
    depth: usize,
) {
    if let Some(least) = schema.get("minItems").and_then(Value::as_u64)
        && (items.len() as u64) < least
    {
        found.push(Violation {
            path: path.clone(),
            kind: Wrong::TooFewItems(least),
        });
    }
    if let Some(most) = schema.get("maxItems").and_then(Value::as_u64)
        && (items.len() as u64) > most
    {
        found.push(Violation {
            path: path.clone(),
            kind: Wrong::TooManyItems(most),
        });
    }
    let prefix = schema
        .get("prefixItems")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (i, item) in items.iter().enumerate() {
        let declared = prefix.get(i).or_else(|| schema.get("items"));
        if let Some(declared) = declared {
            path.push(Step::Index(i));
            walk(root, declared, item, path, found, depth + 1);
            path.pop();
        }
    }
}

fn walk_number(
    schema: &serde_json::Map<String, Value>,
    number: &serde_json::Number,
    path: &[Step],
    found: &mut Vec<Violation>,
) {
    let value = Wide::of(number);
    let mut floor = schema.get("minimum").and_then(Wide::from_value);
    let mut ceiling = schema.get("maximum").and_then(Wide::from_value);
    if matches!(value, Wide::Int(_))
        && let Some((low, high)) = schema.get("format").and_then(Value::as_str).and_then(width)
    {
        floor = Some(floor.map_or(low, |f| f.larger(low)));
        ceiling = Some(ceiling.map_or(high, |c| c.smaller(high)));
    }
    if let Some(floor) = floor
        && value.order(floor) == Some(Ordering::Less)
    {
        report(found, path, Wrong::Below(floor.show()));
    }
    if let Some(ceiling) = ceiling
        && value.order(ceiling) == Some(Ordering::Greater)
    {
        report(found, path, Wrong::Above(ceiling.show()));
    }
}

/// The range an integer `format` of `schemars` bounds a value to, when it is one.
fn width(format: &str) -> Option<(Wide, Wide)> {
    let signed = |bits: u32| {
        (
            Wide::Int(-(1i128 << (bits - 1))),
            Wide::Int((1i128 << (bits - 1)) - 1),
        )
    };
    let unsigned = |bits: u32| (Wide::Int(0), Wide::Int((1i128 << bits) - 1));
    Some(match format {
        "int8" => signed(8),
        "int16" => signed(16),
        "int32" => signed(32),
        "int64" | "int" => signed(64),
        "uint8" => unsigned(8),
        "uint16" => unsigned(16),
        "uint32" => unsigned(32),
        "uint64" | "uint" => unsigned(64),
        _ => return None,
    })
}

/// A JSON number held wide enough to compare an integer of either sign at its full width.
#[derive(Clone, Copy, Debug)]
enum Wide {
    Int(i128),
    Float(f64),
}

impl Wide {
    fn of(number: &serde_json::Number) -> Wide {
        if let Some(i) = number.as_i64() {
            Wide::Int(i128::from(i))
        } else if let Some(u) = number.as_u64() {
            Wide::Int(i128::from(u))
        } else {
            Wide::Float(number.as_f64().unwrap_or(f64::NAN))
        }
    }

    fn from_value(value: &Value) -> Option<Wide> {
        value.as_number().map(Wide::of)
    }

    fn float(self) -> f64 {
        match self {
            Wide::Int(i) => i as f64,
            Wide::Float(f) => f,
        }
    }

    fn order(self, other: Wide) -> Option<Ordering> {
        match (self, other) {
            (Wide::Int(a), Wide::Int(b)) => Some(a.cmp(&b)),
            (a, b) => a.float().partial_cmp(&b.float()),
        }
    }

    fn larger(self, other: Wide) -> Wide {
        if other.order(self) == Some(Ordering::Greater) {
            other
        } else {
            self
        }
    }

    fn smaller(self, other: Wide) -> Wide {
        if other.order(self) == Some(Ordering::Less) {
            other
        } else {
            self
        }
    }

    fn show(self) -> String {
        match self {
            Wide::Int(i) => i.to_string(),
            Wide::Float(f) => f.to_string(),
        }
    }
}

fn type_names(allowed: &Value) -> Vec<&str> {
    match allowed {
        Value::String(one) => vec![one.as_str()],
        Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Whether `value` is of the JSON type `name`, as `serde` reads one: an integer is a number
/// written without a fraction or an exponent.
fn has_type(value: &Value, name: &str) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

/// The name of a value's JSON type, for a sentence.
pub fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "an integer",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// What a schema allows, in a few words, for the sentence that says a value matched none of the
/// forms a list of schemas gave.
fn describe(root: &Value, schema: &Value) -> String {
    let Value::Object(object) = schema else {
        return "anything".to_string();
    };
    if let Some(target) = object.get("$ref").and_then(Value::as_str)
        && let Ok(found) = resolve(root, target)
    {
        return describe(root, found);
    }
    if let Some(only) = object.get("const") {
        return only.to_string();
    }
    let article = |name: &str| match name {
        "array" | "object" | "integer" => format!("an {name}"),
        "null" => "null".to_string(),
        other => format!("a {other}"),
    };
    match object.get("type").map(type_names) {
        Some(names) if !names.is_empty() => names
            .iter()
            .map(|n| article(n))
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "another form".to_string(),
    }
}
