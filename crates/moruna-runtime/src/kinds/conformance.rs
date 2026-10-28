//! The conformance suites of the three ports (SDD 18 f.4): what "correct" means for every adapter
//! of a port, written once, before the ports' second adapters (decision AP1). An adapter is
//! accepted when its port's suite passes; the suites read nothing but the port and the
//! declaration, so they hold for a kind that does not exist yet.
//!
//! Every suite checks KD-C1 to KD-C6; the runnable-kind suite adds KD-C7.
//!
//! - **KD-C1** the declaration is one the registry accepts for the port's role (KD-I2);
//! - **KD-C2** it is the same every time it is asked, and its JSON reads back as itself;
//! - **KD-C3** every example of its settings builds, and its entry reads back as itself;
//! - **KD-C4** a setting the schema does not declare is refused, naming the entry;
//! - **KD-C5** a declared setting of a type its schema does not allow is refused, naming the
//!   entry;
//! - **KD-C6** an example without a setting its schema requires is refused, naming the entry;
//! - **KD-C7** a stage whose settings pin a fingerprint is refused, at `<entry>.fingerprint`, for
//!   a pin that is not its kernel's.

use std::sync::Arc;

use moruna_kernel::{MorunaError, Result};
use serde_json::{Map, Value};

use super::{Context, Entry, RunnableKind, SinkKind, SourceKind, Standalone};
use moruna_job::{Declaration, KindDoc, Role};

/// A setting no declaration has.
const UNKNOWN_SETTING: &str = "moruna_conformance_unknown";

/// What a suite builds with: the context of each case and, for a kind whose examples need a
/// fixture (a package file, carried contracts), the examples to use instead of the declared ones.
pub struct Harness {
    make: Box<dyn Fn() -> Context>,
    examples: Option<Vec<Value>>,
}

impl Default for Harness {
    fn default() -> Harness {
        Harness::new()
    }
}

impl Harness {
    /// Each case in a fresh context of a program that supplies nothing, over the declared
    /// examples.
    pub fn new() -> Harness {
        Harness {
            make: Box::new(|| Context::new(Arc::new(Standalone))),
            examples: None,
        }
    }

    /// Each case in a context `make` makes.
    pub fn context(mut self, make: impl Fn() -> Context + 'static) -> Harness {
        self.make = Box::new(make);
        self
    }

    /// These examples rather than the declared ones.
    pub fn examples(mut self, examples: Vec<Value>) -> Harness {
        self.examples = Some(examples);
        self
    }
}

/// What a passing suite did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Passed {
    /// The properties checked, by id.
    pub properties: Vec<&'static str>,
    /// The cases built, refusals included.
    pub cases: usize,
}

/// The first property an adapter broke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The kind, by name.
    pub kind: String,
    /// The property, by id (`KD-C3`).
    pub property: &'static str,
    /// What happened.
    pub detail: String,
}

impl core::fmt::Display for Violation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "`{}` breaks {}: {}",
            self.kind, self.property, self.detail
        )
    }
}

/// The source port's suite.
pub fn source(kind: &dyn SourceKind, harness: &Harness) -> core::result::Result<Passed, Violation> {
    let build = |doc: &KindDoc, cx: &mut Context| {
        kind.source(&Entry::new("source", None, doc), cx)
            .map(|_| ())
    };
    suite(
        kind.declaration(),
        kind.declaration(),
        Role::Source,
        "source",
        &build,
        harness,
    )
}

/// The sink port's suite.
pub fn sink(kind: &dyn SinkKind, harness: &Harness) -> core::result::Result<Passed, Violation> {
    let build =
        |doc: &KindDoc, cx: &mut Context| kind.sink(&Entry::new("sink", None, doc), cx).map(|_| ());
    suite(
        kind.declaration(),
        kind.declaration(),
        Role::Sink,
        "sink",
        &build,
        harness,
    )
}

/// The runnable-kind port's suite.
pub fn kernel(
    kind: &dyn RunnableKind,
    harness: &Harness,
) -> core::result::Result<Passed, Violation> {
    let field = "kernels[0]";
    let build = |doc: &KindDoc, cx: &mut Context| {
        kind.stage(&Entry::new(field, Some(0), doc), cx).map(|_| ())
    };
    let declaration = kind.declaration();
    let mut passed = suite(
        declaration.clone(),
        kind.declaration(),
        Role::Kernel,
        field,
        &build,
        harness,
    )?;
    let violation = |detail: String| Violation {
        kind: declaration.name.clone(),
        property: "KD-C7",
        detail,
    };
    if property_names(&declaration.settings).any(|name| name == "fingerprint") {
        for example in examples_of(&declaration, harness) {
            let mut settings = example;
            settings.insert(
                "fingerprint".into(),
                Value::String(format!("sha256:{}", "0".repeat(64))),
            );
            let doc = KindDoc::new(declaration.name.clone(), settings);
            let at = format!("{field}.fingerprint");
            match build(&doc, &mut (harness.make)()) {
                Err(e) if refused_field(&e).as_deref() == Some(at.as_str()) => {}
                Err(e) => return Err(violation(format!("a wrong pin was refused as `{e}`"))),
                Ok(()) => return Err(violation("a wrong pin built".into())),
            }
            passed.cases += 1;
        }
        passed.properties.push("KD-C7");
    }
    Ok(passed)
}

type Build<'a> = dyn Fn(&KindDoc, &mut Context) -> Result<()> + 'a;

fn suite(
    declaration: Declaration,
    again: Declaration,
    role: Role,
    field: &str,
    build: &Build<'_>,
    harness: &Harness,
) -> core::result::Result<Passed, Violation> {
    let name = declaration.name.clone();
    let violation = |property: &'static str, detail: String| Violation {
        kind: name.clone(),
        property,
        detail,
    };
    let mut cases = 0;

    // KD-C1.
    declaration.check(role).map_err(|e| violation("KD-C1", e))?;

    // KD-C2.
    if again != declaration {
        return Err(violation("KD-C2", "two calls declared two kinds".into()));
    }
    let text =
        serde_json::to_string(&declaration).map_err(|e| violation("KD-C2", e.to_string()))?;
    let read: Declaration =
        serde_json::from_str(&text).map_err(|e| violation("KD-C2", e.to_string()))?;
    if read != declaration {
        return Err(violation("KD-C2", "its JSON reads back as another".into()));
    }

    let examples = examples_of(&declaration, harness);
    let refused = |settings: Map<String, Value>,
                   property: &'static str,
                   what: &str|
     -> core::result::Result<(), Violation> {
        let doc = KindDoc::new(name.clone(), settings);
        match build(&doc, &mut (harness.make)()) {
            Err(e) if refused_field(&e).is_some_and(|at| under(&at, field)) => Ok(()),
            Err(e) => Err(violation(property, format!("{what} was refused as `{e}`"))),
            Ok(()) => Err(violation(property, format!("{what} built"))),
        }
    };
    for example in examples {
        // KD-C3.
        let doc = KindDoc::new(name.clone(), example.clone());
        let written = serde_json::to_value(&doc).map_err(|e| violation("KD-C3", e.to_string()))?;
        let back: KindDoc =
            serde_json::from_value(written).map_err(|e| violation("KD-C3", e.to_string()))?;
        if back != doc {
            return Err(violation("KD-C3", "an entry reads back as another".into()));
        }
        build(&doc, &mut (harness.make)())
            .map_err(|e| violation("KD-C3", format!("the example {example:?} was refused: {e}")))?;
        cases += 1;

        // KD-C4.
        let mut unknown = example.clone();
        unknown.insert(UNKNOWN_SETTING.into(), Value::Bool(true));
        refused(unknown, "KD-C4", "an undeclared setting")?;
        cases += 1;

        // KD-C5.
        for (setting, wrong) in wrong_values(&declaration.settings) {
            let mut mistyped = example.clone();
            mistyped.insert(setting.clone(), wrong.clone());
            refused(mistyped, "KD-C5", &format!("`{setting}` as {wrong}"))?;
            cases += 1;
        }

        // KD-C6.
        for required in required_names(&declaration.settings) {
            let mut missing = example.clone();
            if missing.remove(&required).is_some() {
                refused(missing, "KD-C6", &format!("an entry without `{required}`"))?;
                cases += 1;
            }
        }
    }
    Ok(Passed {
        properties: vec!["KD-C1", "KD-C2", "KD-C3", "KD-C4", "KD-C5", "KD-C6"],
        cases,
    })
}

fn examples_of(declaration: &Declaration, harness: &Harness) -> Vec<Map<String, Value>> {
    harness
        .examples
        .clone()
        .unwrap_or_else(|| declaration.examples())
        .into_iter()
        .filter_map(|example| match example {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .collect()
}

/// The field a document refusal names (`spec refused: <field>: <reason>`).
pub fn refused_field(error: &MorunaError) -> Option<String> {
    let MorunaError::Config { name: "spec", msg } = error else {
        return None;
    };
    let rest = msg.strip_prefix("spec refused: ")?;
    rest.split_once(": ").map(|(field, _)| field.to_string())
}

/// Whether `at` is `field` or a setting under it.
fn under(at: &str, field: &str) -> bool {
    at == field
        || at
            .strip_prefix(field)
            .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
}

fn property_names(schema: &Value) -> impl Iterator<Item = &String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|properties| properties.keys())
}

fn required_names(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(str::to_string))
        .collect()
}

/// For each declared setting whose schema allows only some JSON types, a value of a type it
/// does not allow.
fn wrong_values(schema: &Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return out;
    };
    for (name, property) in properties {
        let Some(types) = types_of(schema, property, 0) else {
            continue;
        };
        let candidates = [
            ("object", serde_json::json!({UNKNOWN_SETTING: true})),
            ("number", serde_json::json!(7.5)),
            ("string", serde_json::json!("moruna-conformance")),
            ("boolean", Value::Bool(true)),
            ("array", serde_json::json!([])),
        ];
        if let Some((_, wrong)) = candidates
            .into_iter()
            .find(|(ty, _)| !types.iter().any(|t| t == ty))
        {
            out.push((name.clone(), wrong));
        }
    }
    out
}

/// The JSON types a subschema allows, `null` aside; `None` when it allows any.
fn types_of(root: &Value, schema: &Value, depth: usize) -> Option<Vec<String>> {
    if depth > 8 {
        return None;
    }
    let Value::Object(schema) = schema else {
        // `true` (or `false`, which no setting is) allows anything.
        return None;
    };
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let target = root.pointer(reference.strip_prefix('#')?)?;
        return types_of(root, target, depth + 1);
    }
    for combinator in ["anyOf", "oneOf"] {
        if let Some(members) = schema.get(combinator).and_then(Value::as_array) {
            let mut all = Vec::new();
            for member in members {
                all.extend(types_of(root, member, depth + 1)?);
            }
            return Some(all);
        }
    }
    let types: Vec<String> = match schema.get("type")? {
        Value::String(one) => vec![one.clone()],
        Value::Array(many) => many
            .iter()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect(),
        _ => return None,
    };
    Some(types.into_iter().filter(|t| t != "null").collect())
}
