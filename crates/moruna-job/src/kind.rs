//! The declaration of a kind, what a kind is (SDD 18 e.1), and the listing of them (e.3): the
//! types an embedder reads to know what can run and how to configure it. Nothing here runs a
//! kind; the adapters that do are in `moruna-runtime`.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The format version of `moruna kinds` (the [`Listing`]).
pub const LISTING_VERSION: u32 = 1;

/// The JSON Schema dialect every settings schema is written in (decision KD1).
pub const SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Which part of a run a kind is, and so which port its adapter implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The input ([`SourceKind`]).
    Source,
    /// A stage between them ([`RunnableKind`]).
    Kernel,
    /// The output ([`SinkKind`]).
    Sink,
}

impl Role {
    /// The role's name in a refusal and in the listing.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Source => "source",
            Role::Kernel => "kernel",
            Role::Sink => "sink",
        }
    }

    /// What a kind of this role does, for the sentence naming the kinds of a role: a source
    /// "reads", a kernel "runs", a sink "writes".
    pub fn verb(&self) -> &'static str {
        match self {
            Role::Source => "reads",
            Role::Kernel => "runs",
            Role::Sink => "writes",
        }
    }
}

impl core::fmt::Display for Role {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a kind takes in or gives out (e.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flow {
    /// Arrow record batches (or tensors), as morsels.
    Batches,
    /// Files: what a source reads or a sink writes.
    Files,
    /// Nothing the run carries: an object the caller holds, say.
    Nothing,
}

impl Flow {
    /// The flow's name in a refusal and in the listing.
    pub fn as_str(&self) -> &'static str {
        match self {
            Flow::Batches => "batches",
            Flow::Files => "files",
            Flow::Nothing => "nothing",
        }
    }
}

/// A fact about a kind that holds always, never, or as its settings say (e.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fact {
    /// Always.
    Yes,
    /// Never.
    No,
    /// As the entry's settings say (an object URL needs the network, a projection drops
    /// columns).
    BySettings,
}

/// Where a kind's code comes from and where it runs (e.1, MB3's isolation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Code compiled into the program running the document, in its process.
    Builtin,
    /// Code imported by path from the interpreter's path into the process: what a hosted run
    /// (strict mode) refuses (16 PK-I8, KD-I6).
    Imported,
    /// An object the library caller handed over, run in the caller's process; never named by a
    /// document from a file or a socket.
    Caller,
    /// Code from a package, run apart from the process: a `.wasm` in its sandbox, a `.pex` in a
    /// confined child (17, decision FP1).
    Confined,
}

/// What a kind may need from the machine and the run (e.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Needs {
    /// Connections out of the process (an object store), always reached through the reactor's
    /// clients and never by the kind's own sockets.
    pub network: Fact,
    /// Destinations the kind needs whatever its settings, as `host` or `host:port`; empty for
    /// every kind whose destinations are its settings'.
    pub egress: Vec<String>,
    /// A GPU.
    pub gpu: Fact,
    /// A named port it calls (`call(port, bytes)`, MH 4.10), a model's say.
    pub model_port: Fact,
    /// The contracts the job carries (`contracts`, MH 4.5).
    pub contracts: Fact,
}

impl Needs {
    /// Nothing at all.
    pub fn nothing() -> Needs {
        Needs {
            network: Fact::No,
            egress: Vec::new(),
            gpu: Fact::No,
            model_port: Fact::No,
            contracts: Fact::No,
        }
    }
}

/// What a kind is (e.1): what an embedder reads to know what can run and how to configure it, and
/// what the build and the planner read instead of matching on its name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    /// The name a document's `kind` names it by (KD-I2).
    pub name: String,
    /// The port it implements.
    pub role: Role,
    /// One line on what it is.
    pub summary: String,
    /// Its settings: a JSON Schema (2020-12) of the entry's keys other than `kind`, with
    /// `examples` (e.2, decision KD1).
    pub settings: serde_json::Value,
    /// What it takes in.
    pub inputs: Flow,
    /// What it gives out.
    pub outputs: Flow,
    /// What it may need.
    pub needs: Needs,
    /// Whether the same input and settings give the same output rows.
    pub deterministic: Fact,
    /// Whether the columns it is given pass through unchanged (for a source, whether it yields
    /// the stored columns as stored; for a sink, whether it writes every column it is given).
    pub preserves_columns: Fact,
    /// Where its code comes from and runs.
    pub isolation: Isolation,
}

impl Declaration {
    /// Whether this declares a kind of `role` the registry accepts (KD-I2): a kind name; the
    /// role; a summary; flows a part of that role can have; a closed object schema in the
    /// dialect of KD1 with at least one example, none of whose properties is `kind`.
    pub fn check(&self, role: Role) -> core::result::Result<(), String> {
        if !is_kind_name(&self.name) {
            return Err(format!(
                "{:?} is not a kind name: 1 to 64 of lowercase ASCII letters, digits, `_`, `-` \
                 and `.`, starting with a letter",
                self.name
            ));
        }
        if self.role != role {
            return Err(format!(
                "`{}` declares the role {} and is registered as a {}",
                self.name,
                self.role.as_str(),
                role.as_str()
            ));
        }
        if self.summary.trim().is_empty() {
            return Err(format!("`{}` has no summary", self.name));
        }
        let flows_fit = match role {
            Role::Source => self.inputs != Flow::Batches && self.outputs == Flow::Batches,
            Role::Kernel => self.inputs == Flow::Batches && self.outputs == Flow::Batches,
            Role::Sink => self.inputs == Flow::Batches && self.outputs != Flow::Batches,
        };
        if !flows_fit {
            return Err(format!(
                "`{}` takes {} and gives {}, which a {} does not: a source gives batches, a \
                 kernel takes and gives batches, a sink takes batches",
                self.name,
                self.inputs.as_str(),
                self.outputs.as_str(),
                role.as_str()
            ));
        }
        let schema = &self.settings;
        let fail = |what: &str| Err(format!("`{}`'s settings schema {what}", self.name));
        if schema.get("$schema").and_then(|v| v.as_str()) != Some(SCHEMA_DIALECT) {
            return fail(&format!("is not in the dialect {SCHEMA_DIALECT}"));
        }
        if schema.get("type").and_then(|v| v.as_str()) != Some("object") {
            return fail("does not describe an object");
        }
        if schema.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
            return fail("is not closed (`additionalProperties: false`)");
        }
        if schema
            .get("properties")
            .and_then(|p| p.as_object())
            .is_some_and(|p| p.contains_key("kind"))
        {
            return fail("has a property `kind`, which is the document's, not a setting");
        }
        match schema.get("examples").and_then(|v| v.as_array()) {
            Some(examples) if !examples.is_empty() && examples.iter().all(|e| e.is_object()) => {
                Ok(())
            }
            _ => fail("has no `examples`, or one that is not an object"),
        }
    }

    /// The examples of its settings schema.
    pub fn examples(&self) -> Vec<serde_json::Value> {
        self.settings
            .get("examples")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }
}

/// The settings schema of a kind that reads its settings as `T` (e.2): JSON Schema 2020-12
/// derived from `T`, titled with the kind's name, with `examples`.
pub fn settings_schema<T: schemars::JsonSchema>(
    name: &str,
    examples: Vec<serde_json::Value>,
) -> serde_json::Value {
    let mut schema = schemars::SchemaGenerator::default().into_root_schema_for::<T>();
    if let Some(object) = schema.as_object_mut() {
        object.insert("title".into(), serde_json::Value::String(name.to_string()));
        object.insert("examples".into(), serde_json::Value::Array(examples));
    }
    schema.to_value()
}

/// Whether `name` may name a kind (KD-I2): 1 to 64 of lowercase ASCII letters, digits, `_`, `-`
/// and `.`, starting with a letter.
pub fn is_kind_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

/// Files or prefixes: one string or a list of them (MH 4.1). Written as a string when it is one
/// and as a list when it is several, and read either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Urls(pub Vec<String>);

impl Serialize for Urls {
    fn serialize<S: Serializer>(&self, s: S) -> core::result::Result<S::Ok, S::Error> {
        match self.0.as_slice() {
            [one] => s.serialize_str(one),
            many => many.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Urls {
    fn deserialize<D: Deserializer<'de>>(d: D) -> core::result::Result<Urls, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }
        Ok(match OneOrMany::deserialize(d)? {
            OneOrMany::One(one) => Urls(vec![one]),
            OneOrMany::Many(many) => Urls(many),
        })
    }
}

impl schemars::JsonSchema for Urls {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Urls".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut any_of = serde_json::Map::new();
        any_of.insert(
            "anyOf".into(),
            serde_json::json!([
                {"type": "string"},
                {"type": "array", "items": {"type": "string"}}
            ]),
        );
        any_of.insert(
            "description".into(),
            serde_json::Value::String("One location, or a list of them.".into()),
        );
        schemars::Schema::from(any_of)
    }
}

/// Every declaration of a registry (e.3): what `moruna kinds` prints, and what an embedder reads
/// to know what can run and how to configure it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listing {
    /// The listing's format version ([`LISTING_VERSION`]).
    pub moruna_kinds: u32,
    /// The Moruna that listed them.
    pub moruna: String,
    /// The source kinds.
    pub sources: Vec<Declaration>,
    /// The runnable kinds.
    pub kernels: Vec<Declaration>,
    /// The sink kinds.
    pub sinks: Vec<Declaration>,
}

impl Listing {
    /// The listing as indented JSON, as [`crate::JobSpec::to_json_pretty`] writes a document.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }
}
