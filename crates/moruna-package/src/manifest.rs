//! `moruna.json` (16 e.1) and the declaration as the decorator spells it (16 e.2).

use moruna_kernel::declare::{ABI_VERSION, ColumnDecl, Declared, SchemaDecl, parse_type};
use serde_json::{Map, Value};

use crate::{Format, PackageError, WASM_PLATFORM, WORLD};

/// The largest manifest a reader accepts (16 i).
pub const MANIFEST_MAX_BYTES: usize = 1 << 20;

/// The longest package name (16 e.1).
pub const NAME_MAX_BYTES: usize = 64;

/// The hints a declaration may carry (16 e.2).
pub const HINTS: [&str; 7] = [
    "stateful",
    "args",
    "expected_amplification",
    "preferred_rows",
    "state_bytes",
    "memory_guard",
    "resume",
];

/// A package's manifest (16 e.1).
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    /// The kernel's name; the file is `<name>.<format>`.
    pub name: String,
    /// `<module>:<attribute>` for a `.pex`, `moruna:kernel` for a `.wasm`.
    pub entry: String,
    /// What the kernel takes, gives back and hints.
    pub declaration: DeclarationDoc,
    /// The kernel ABI the package was written for.
    pub abi: u32,
    /// The Moruna version that wrote the package.
    pub moruna: String,
    /// The package's kind.
    pub format: Format,
    /// The targets a `.pex` carries wheels for, sorted; `["wasm32"]` for a `.wasm`.
    pub platforms: Vec<String>,
}

/// A value as pretty JSON (two-space indentation).
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| String::from("null"))
}

/// An object from keys and already-written values, in the order given, two-space indentation.
fn object_text(fields: &[(&str, String)]) -> String {
    let mut out = String::from("{\n");
    for (i, (key, value)) in fields.iter().enumerate() {
        let value = value.replace('\n', "\n  ");
        out.push_str(&format!("  {}: {value}", Value::from(*key)));
        out.push_str(if i + 1 < fields.len() { ",\n" } else { "\n" });
    }
    out.push('}');
    out
}

/// The manifest's `declaration` (16 e.2).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeclarationDoc {
    /// The input schema as the document spells it; `None` is undeclared.
    pub input_schema: Option<Value>,
    /// The output schema as the document spells it; `None` is undeclared.
    pub output_schema: Option<Value>,
    /// The hints.
    pub hints: Hints,
}

/// The hints a package declares (16 e.2).
#[derive(Clone, Debug, PartialEq)]
pub struct Hints {
    /// Keeps state between calls (`open` and `close` for a `.wasm`).
    pub stateful: bool,
    /// Built from arguments (16 b): a `.pex` entry is a factory, a `.wasm` exports `configure`.
    pub args: bool,
    /// Expected peak working set over input bytes.
    pub expected_amplification: Option<f64>,
    /// A row count the kernel would rather receive.
    pub preferred_rows: Option<u64>,
    /// Bytes one instance's state is expected to hold.
    pub state_bytes: Option<u64>,
    /// Whether the allocator guard may refuse the kernel's requests (E13, 05 AD-I10).
    pub memory_guard: bool,
    /// `reinit`, `checkpoint` or `forbid`, a `.pex` only.
    pub resume: Option<String>,
}

impl Default for Hints {
    fn default() -> Hints {
        Hints {
            stateful: false,
            args: false,
            expected_amplification: None,
            preferred_rows: None,
            state_bytes: None,
            memory_guard: true,
            resume: None,
        }
    }
}

fn refuse(field: impl Into<String>, reason: impl Into<String>) -> PackageError {
    PackageError::Format {
        field: field.into(),
        reason: reason.into(),
    }
}

impl Hints {
    /// The hints of `value`, which must be an object with only the keys of [`HINTS`].
    pub fn parse(value: Option<&Value>, format: Option<Format>) -> Result<Hints, PackageError> {
        let mut hints = Hints::default();
        let Some(value) = value else {
            return Ok(hints);
        };
        if value.is_null() {
            return Ok(hints);
        }
        let object = value
            .as_object()
            .ok_or_else(|| refuse("declaration.hints", "must be an object"))?;
        for (key, v) in object {
            let field = format!("declaration.hints.{key}");
            match key.as_str() {
                "stateful" => {
                    hints.stateful = v
                        .as_bool()
                        .ok_or_else(|| refuse(&field, "must be a bool"))?
                }
                "args" => {
                    hints.args = v
                        .as_bool()
                        .ok_or_else(|| refuse(&field, "must be a bool"))?
                }
                "memory_guard" => {
                    hints.memory_guard = v
                        .as_bool()
                        .ok_or_else(|| refuse(&field, "must be a bool"))?
                }
                "expected_amplification" => {
                    let a = v
                        .as_f64()
                        .filter(|a| a.is_finite() && *a > 0.0)
                        .ok_or_else(|| refuse(&field, "must be a number above 0"))?;
                    hints.expected_amplification = Some(a);
                }
                "preferred_rows" => {
                    let n = v
                        .as_u64()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| refuse(&field, "must be an integer above 0"))?;
                    hints.preferred_rows = Some(n);
                }
                "state_bytes" => {
                    let n = v
                        .as_u64()
                        .ok_or_else(|| refuse(&field, "must be an integer of at least 0"))?;
                    hints.state_bytes = Some(n);
                }
                "resume" => {
                    let r = v
                        .as_str()
                        .filter(|r| matches!(*r, "reinit" | "checkpoint" | "forbid"))
                        .ok_or_else(|| {
                            refuse(&field, "must be \"reinit\", \"checkpoint\" or \"forbid\"")
                        })?;
                    if format == Some(Format::Wasm) {
                        return Err(refuse(
                            &field,
                            "a stateful .wasm always checkpoints through close and open",
                        ));
                    }
                    hints.resume = Some(r.to_string());
                }
                _ => {
                    return Err(refuse(
                        &field,
                        format!("is not a hint; the hints are {}", HINTS.join(", ")),
                    ));
                }
            }
        }
        Ok(hints)
    }

    /// The hints as an object, defaults left out (a reader sorts its keys).
    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        if self.stateful {
            out.insert("stateful".into(), Value::Bool(true));
        }
        if self.args {
            out.insert("args".into(), Value::Bool(true));
        }
        if let Some(a) = self.expected_amplification {
            out.insert("expected_amplification".into(), Value::from(a));
        }
        if let Some(n) = self.preferred_rows {
            out.insert("preferred_rows".into(), Value::from(n));
        }
        if let Some(n) = self.state_bytes {
            out.insert("state_bytes".into(), Value::from(n));
        }
        if !self.memory_guard {
            out.insert("memory_guard".into(), Value::Bool(false));
        }
        if let Some(r) = &self.resume {
            out.insert("resume".into(), Value::from(r.clone()));
        }
        Value::Object(out)
    }
}

/// One declared schema from its spelling (16 e.2): an object is a subset, an array exact, an
/// output object of `adds`, `drops` and `changes` relative.
pub fn schema_decl(value: &Value, field: &str, output: bool) -> Result<SchemaDecl, PackageError> {
    match value {
        Value::Array(items) => {
            let mut columns = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let at = format!("{field}[{i}]");
                let parts = item.as_array().ok_or_else(|| {
                    refuse(
                        &at,
                        "an exact column is [name, type] or [name, type, nullable]",
                    )
                })?;
                let (name, ty, nullable) = match parts.as_slice() {
                    [n, t] => (n, t, true),
                    [n, t, Value::Bool(b)] => (n, t, *b),
                    _ => {
                        return Err(refuse(
                            &at,
                            "an exact column is [name, type] or [name, type, nullable]",
                        ));
                    }
                };
                let name = name
                    .as_str()
                    .ok_or_else(|| refuse(&at, "the column name must be a string"))?;
                columns.push(column(name, ty, &at, nullable)?);
            }
            Ok(SchemaDecl::Exact(columns))
        }
        Value::Object(map) => {
            let relative = !map.is_empty()
                && map
                    .keys()
                    .all(|k| matches!(k.as_str(), "adds" | "drops" | "changes"));
            if relative {
                if !output {
                    return Err(refuse(
                        field,
                        "an input schema cannot be relative: adds, drops and changes describe an \
                         output",
                    ));
                }
                let adds = columns_of(map.get("adds"), &format!("{field}.adds"))?;
                let changes = columns_of(map.get("changes"), &format!("{field}.changes"))?;
                let drops = match map.get("drops") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(names)) => names
                        .iter()
                        .map(|n| {
                            n.as_str().map(str::to_string).ok_or_else(|| {
                                refuse(format!("{field}.drops"), "names must be strings")
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    Some(_) => {
                        return Err(refuse(
                            format!("{field}.drops"),
                            "must be a list of column names",
                        ));
                    }
                };
                return Ok(SchemaDecl::Relative {
                    adds,
                    drops,
                    changes,
                });
            }
            Ok(SchemaDecl::Subset(columns_of(Some(value), field)?))
        }
        _ => Err(refuse(
            field,
            "a schema is an object of column to type, or a list of [name, type] columns",
        )),
    }
}

fn columns_of(value: Option<&Value>, field: &str) -> Result<Vec<ColumnDecl>, PackageError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(name, ty)| column(name, ty, &format!("{field}.{name}"), true))
            .collect(),
        Some(_) => Err(refuse(field, "must be an object of column to type")),
    }
}

fn column(name: &str, ty: &Value, field: &str, nullable: bool) -> Result<ColumnDecl, PackageError> {
    let spelling = ty
        .as_str()
        .ok_or_else(|| refuse(field, "a type is a string of the type grammar (MH 4.9)"))?;
    let ty = parse_type(spelling).map_err(|e| refuse(field, e.to_string()))?;
    Ok(ColumnDecl {
        name: name.to_string(),
        ty,
        nullable,
    })
}

/// Whether `name` is a package name (16 e.1).
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= NAME_MAX_BYTES
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `entry` is `<module>:<attribute>` with dotted identifiers on both sides.
pub fn valid_pex_entry(entry: &str) -> bool {
    let Some((module, attribute)) = entry.split_once(':') else {
        return false;
    };
    let dotted = |s: &str| !s.is_empty() && s.split('.').all(valid_identifier);
    dotted(module) && dotted(attribute)
}

fn valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Whether `target` is a `.pex` target this Moruna can run: `cp3NN-manylinux_<a>_<b>_<arch>`
/// with `arch` `x86_64` or `aarch64` (16 e.1, f.4).
pub fn valid_pex_target(target: &str) -> bool {
    parse_target(target).is_some()
}

/// A `.pex` target's parts: the interpreter tag's minor version, the glibc version, the
/// architecture.
pub fn parse_target(target: &str) -> Option<(u32, (u32, u32), &str)> {
    let (interpreter, platform) = target.split_once('-')?;
    let minor = interpreter.strip_prefix("cp3")?.parse::<u32>().ok()?;
    if interpreter != format!("cp3{minor}") {
        return None;
    }
    let rest = platform.strip_prefix("manylinux_")?;
    let mut parts = rest.splitn(3, '_');
    let major = parts.next()?.parse::<u32>().ok()?;
    let glibc_minor = parts.next()?.parse::<u32>().ok()?;
    let arch = parts.next()?;
    if !matches!(arch, "x86_64" | "aarch64") {
        return None;
    }
    if platform != format!("manylinux_{major}_{glibc_minor}_{arch}") {
        return None;
    }
    Some((minor, (major, glibc_minor), arch))
}

impl Manifest {
    /// Parse and validate a manifest (16 PK-I3).
    pub fn parse(bytes: &[u8]) -> Result<Manifest, PackageError> {
        if bytes.len() > MANIFEST_MAX_BYTES {
            return Err(refuse(
                "moruna.json",
                format!(
                    "{} bytes is over the {MANIFEST_MAX_BYTES} a manifest may have",
                    bytes.len()
                ),
            ));
        }
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|e| refuse("moruna.json", format!("not JSON: {e}")))?;
        Manifest::from_value(&value)
    }

    /// Validate a manifest already parsed as JSON.
    pub fn from_value(value: &Value) -> Result<Manifest, PackageError> {
        let object = value
            .as_object()
            .ok_or_else(|| refuse("moruna.json", "must be an object"))?;
        const FIELDS: [&str; 7] = [
            "name",
            "entry",
            "declaration",
            "abi",
            "moruna",
            "format",
            "platforms",
        ];
        for key in object.keys() {
            if !FIELDS.contains(&key.as_str()) {
                return Err(refuse(
                    key.clone(),
                    format!(
                        "is not a manifest field; the fields are {}",
                        FIELDS.join(", ")
                    ),
                ));
            }
        }
        let field =
            |name: &'static str| object.get(name).ok_or_else(|| refuse(name, "is required"));
        let string = |name: &'static str| -> Result<String, PackageError> {
            field(name)?
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| refuse(name, "must be a string"))
        };
        let format = match string("format")?.as_str() {
            "pex" => Format::Pex,
            "wasm" => Format::Wasm,
            other => return Err(refuse("format", format!("`{other}` is not pex or wasm"))),
        };
        let name = string("name")?;
        if !valid_name(&name) {
            return Err(refuse(
                "name",
                format!(
                    "`{name}` is not a package name: a letter or `_`, then letters, digits or \
                     `_`, at most {NAME_MAX_BYTES} bytes"
                ),
            ));
        }
        let entry = string("entry")?;
        match format {
            Format::Wasm if entry != WORLD => {
                return Err(refuse("entry", format!("a .wasm's entry is `{WORLD}`")));
            }
            Format::Pex if !valid_pex_entry(&entry) => {
                return Err(refuse(
                    "entry",
                    format!("`{entry}` is not <module>:<attribute>"),
                ));
            }
            _ => {}
        }
        let abi = field("abi")?
            .as_u64()
            .ok_or_else(|| refuse("abi", "must be an integer"))?;
        if abi != u64::from(ABI_VERSION) {
            return Err(refuse(
                "abi",
                format!(
                    "the package was written for kernel ABI {abi} and this Moruna's is {ABI_VERSION}"
                ),
            ));
        }
        let moruna = string("moruna")?;
        let platforms: Vec<String> = field("platforms")?
            .as_array()
            .ok_or_else(|| refuse("platforms", "must be a list of strings"))?
            .iter()
            .map(|p| {
                p.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| refuse("platforms", "must be a list of strings"))
            })
            .collect::<Result<_, _>>()?;
        match format {
            Format::Wasm => {
                if platforms != [WASM_PLATFORM] {
                    return Err(refuse(
                        "platforms",
                        format!("a .wasm's platforms are [\"{WASM_PLATFORM}\"]"),
                    ));
                }
            }
            Format::Pex => {
                if platforms.is_empty() {
                    return Err(refuse("platforms", "a .pex carries at least one target"));
                }
                let mut sorted = platforms.clone();
                sorted.sort();
                sorted.dedup();
                if sorted != platforms {
                    return Err(refuse("platforms", "must be sorted and without repeats"));
                }
                if let Some(bad) = platforms.iter().find(|p| !valid_pex_target(p)) {
                    return Err(refuse(
                        "platforms",
                        format!("`{bad}` is not cp3NN-manylinux_<major>_<minor>_<x86_64|aarch64>"),
                    ));
                }
            }
        }
        let declaration = field("declaration")?
            .as_object()
            .ok_or_else(|| refuse("declaration", "must be an object"))?;
        for key in declaration.keys() {
            if !matches!(key.as_str(), "input_schema" | "output_schema" | "hints") {
                return Err(refuse(
                    format!("declaration.{key}"),
                    "is not a declaration field; they are input_schema, output_schema, hints",
                ));
            }
        }
        let schema = |name: &str| declaration.get(name).filter(|v| !v.is_null()).cloned();
        let declaration = DeclarationDoc {
            input_schema: schema("input_schema"),
            output_schema: schema("output_schema"),
            hints: Hints::parse(declaration.get("hints"), Some(format))?,
        };
        let manifest = Manifest {
            name,
            entry,
            declaration,
            abi: ABI_VERSION,
            moruna,
            format,
            platforms,
        };
        // Every schema is parsed now, so a package that cannot be declared is refused when it is
        // opened rather than when it is checked.
        manifest.declared()?;
        Ok(manifest)
    }

    /// The manifest as 16 e.1 writes it: keys in that order, two-space indentation, a final
    /// newline. Objects inside the declaration have their keys sorted, which is the order a
    /// reader keeps (a subset and a relative declaration do not compare column order).
    pub fn to_json(&self) -> String {
        let mut declaration: Vec<(&str, Value)> = Vec::new();
        if let Some(v) = &self.declaration.input_schema {
            declaration.push(("input_schema", v.clone()));
        }
        if let Some(v) = &self.declaration.output_schema {
            declaration.push(("output_schema", v.clone()));
        }
        declaration.push(("hints", self.declaration.hints.to_value()));
        let declaration = object_text(
            declaration
                .iter()
                .map(|(k, v)| (*k, pretty(v)))
                .collect::<Vec<_>>()
                .as_slice(),
        );
        let platforms = pretty(&Value::Array(
            self.platforms.iter().cloned().map(Value::from).collect(),
        ));
        let mut text = object_text(&[
            ("name", pretty(&Value::from(self.name.clone()))),
            ("entry", pretty(&Value::from(self.entry.clone()))),
            ("declaration", declaration),
            ("abi", self.abi.to_string()),
            ("moruna", pretty(&Value::from(self.moruna.clone()))),
            ("format", pretty(&Value::from(self.format.as_str()))),
            ("platforms", platforms),
        ]);
        text.push('\n');
        text
    }

    /// The declaration as the kernel contract's type (MH 4.9).
    pub fn declared(&self) -> Result<Declared, PackageError> {
        let input = match &self.declaration.input_schema {
            Some(v) => Some(schema_decl(v, "declaration.input_schema", false)?),
            None => None,
        };
        let output = match &self.declaration.output_schema {
            Some(v) => Some(schema_decl(v, "declaration.output_schema", true)?),
            None => None,
        };
        Ok(Declared { input, output })
    }

    /// The hints.
    pub fn hints(&self) -> &Hints {
        &self.declaration.hints
    }
}
