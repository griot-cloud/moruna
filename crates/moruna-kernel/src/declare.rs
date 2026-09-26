//! Declared schemas: what a kernel says it takes and what it gives back (MH 4.9).
//!
//! A declaration is optional. A kernel that declares its input and its output is *checkable*:
//! `moruna check` generates batches from the input declaration and refuses a kernel whose
//! produced schema disagrees with the output declaration, naming the column. The library path
//! does not read declarations at all, so a kernel that declares nothing runs exactly as before.
//!
//! The comparison rules live here, beside the types, because the check harness, the standard
//! kernels and the Python adapter all need the same answer to "does this schema agree with that
//! declaration", and three copies of a rule are three rules.

use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};

use crate::error::MorunaError;

/// The version of the contract a kernel is built against: this crate's `Kernel` trait, the
/// payload crossing rules of 05 e.2 and e.3, and the declaration format of this module. It is
/// part of every kernel fingerprint (MH 4.9), so a kernel checked against one ABI is not
/// mistaken for the same kernel under another. Bumped by hand, in the commit that changes any
/// of the three.
pub const ABI_VERSION: u32 = 1;

/// The type a declared column has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeDecl {
    /// Exactly this Arrow type.
    Exact(DataType),
    /// Any type. `moruna check` generates `Int64` for such a column (MH 4.9); the comparison
    /// accepts whatever is produced.
    Any,
}

impl TypeDecl {
    /// The declared type as the type grammar spells it (MH 4.9): `any`, or the canonical name of
    /// the Arrow type.
    pub fn spelling(&self) -> String {
        match self {
            TypeDecl::Any => "any".to_string(),
            TypeDecl::Exact(dt) => type_name(dt),
        }
    }

    fn accepts(&self, produced: &DataType) -> bool {
        match self {
            TypeDecl::Any => true,
            TypeDecl::Exact(dt) => dt == produced,
        }
    }
}

/// One declared column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDecl {
    /// The column name.
    pub name: String,
    /// Its type.
    pub ty: TypeDecl,
    /// Whether the column may hold nulls. Generation honours it (a non-nullable column is never
    /// null in a synthetic batch); the comparison does not read it (MH 4.9).
    pub nullable: bool,
}

impl ColumnDecl {
    /// A nullable column of an exact type.
    pub fn new(name: impl Into<String>, ty: DataType) -> ColumnDecl {
        ColumnDecl {
            name: name.into(),
            ty: TypeDecl::Exact(ty),
            nullable: true,
        }
    }

    /// A nullable column of any type.
    pub fn any(name: impl Into<String>) -> ColumnDecl {
        ColumnDecl {
            name: name.into(),
            ty: TypeDecl::Any,
            nullable: true,
        }
    }
}

/// A schema declaration (MH 4.9): absolute, a subset, or, for an output, relative to the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaDecl {
    /// Exactly these columns, in this order, and no others.
    Exact(Vec<ColumnDecl>),
    /// At least these columns, with these types, in any position; others may be present.
    Subset(Vec<ColumnDecl>),
    /// The input's columns, less `drops`, with the types of `changes` replacing the input's, plus
    /// `adds`. Meaningful for an output declaration only.
    Relative {
        /// Columns the kernel appends.
        adds: Vec<ColumnDecl>,
        /// Input columns the kernel removes.
        drops: Vec<String>,
        /// Input columns whose type the kernel changes.
        changes: Vec<ColumnDecl>,
    },
}

impl SchemaDecl {
    /// An exact declaration from an Arrow schema, keeping each field's nullability.
    pub fn from_schema(schema: &Schema) -> SchemaDecl {
        SchemaDecl::Exact(
            schema
                .fields()
                .iter()
                .map(|f| ColumnDecl {
                    name: f.name().clone(),
                    ty: TypeDecl::Exact(f.data_type().clone()),
                    nullable: f.is_nullable(),
                })
                .collect(),
        )
    }

    /// The input schema `moruna check` generates batches for (MH 4.9): the declared columns in
    /// declaration order, `Any` as `Int64`. A relative declaration describes no input and is a
    /// `Plan` error here.
    pub fn synthetic_schema(&self) -> crate::Result<SchemaRef> {
        let columns = match self {
            SchemaDecl::Exact(c) | SchemaDecl::Subset(c) => c,
            SchemaDecl::Relative { .. } => {
                return Err(MorunaError::Plan(
                    "an input_schema cannot be relative: adds, drops and changes describe an \
                     output"
                        .into(),
                ));
            }
        };
        let mut fields = Vec::with_capacity(columns.len());
        for column in columns {
            let ty = match &column.ty {
                TypeDecl::Exact(dt) => dt.clone(),
                TypeDecl::Any => DataType::Int64,
            };
            fields.push(Field::new(column.name.clone(), ty, column.nullable));
        }
        Ok(Arc::new(Schema::new(fields)))
    }

    /// The expected output for a concrete input (MH 4.9): an exact or subset declaration stands
    /// as it is; a relative one is resolved against `input` into an exact one, with every column
    /// not named keeping the input's type and position and the added columns after them. A
    /// relative declaration that drops or changes a column the input does not have is a `Plan`
    /// error naming it.
    pub fn resolve(&self, input: &Schema) -> crate::Result<Expected> {
        match self {
            SchemaDecl::Exact(c) => Ok(Expected {
                columns: c.clone(),
                ordered: true,
                closed: true,
            }),
            SchemaDecl::Subset(c) => Ok(Expected {
                columns: c.clone(),
                ordered: false,
                closed: false,
            }),
            SchemaDecl::Relative {
                adds,
                drops,
                changes,
            } => {
                for name in drops.iter().chain(changes.iter().map(|c| &c.name)) {
                    if input.field_with_name(name).is_err() {
                        return Err(MorunaError::Plan(format!(
                            "output_schema names column `{name}`, which the input does not have"
                        )));
                    }
                }
                let mut columns = Vec::with_capacity(input.fields().len() + adds.len());
                for field in input.fields() {
                    if drops.iter().any(|d| d == field.name()) {
                        continue;
                    }
                    match changes.iter().find(|c| c.name == *field.name()) {
                        Some(changed) => columns.push(changed.clone()),
                        None => columns.push(ColumnDecl {
                            name: field.name().clone(),
                            ty: TypeDecl::Exact(field.data_type().clone()),
                            nullable: field.is_nullable(),
                        }),
                    }
                }
                columns.extend(adds.iter().cloned());
                Ok(Expected {
                    columns,
                    ordered: false,
                    closed: true,
                })
            }
        }
    }
}

/// A declaration resolved against one input: the columns expected, whether their order is
/// compared, and whether a column not named is a disagreement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expected {
    /// The expected columns.
    pub columns: Vec<ColumnDecl>,
    /// True for an exact declaration: position is compared.
    pub ordered: bool,
    /// True for an exact or relative declaration: an undeclared column disagrees.
    pub closed: bool,
}

/// Why a produced column disagrees with its declaration (MH 4.9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disagreement {
    /// Declared and produced with different types.
    Type {
        /// The column.
        column: String,
        /// The declared type's spelling.
        declared: String,
        /// The produced type's spelling.
        produced: String,
    },
    /// Declared, not produced.
    Missing {
        /// The column.
        column: String,
        /// The declared type's spelling.
        declared: String,
    },
    /// Produced, not declared, under a closed declaration.
    Undeclared {
        /// The column.
        column: String,
        /// The produced type's spelling.
        produced: String,
    },
    /// Declared and produced, at a different position, under an exact declaration.
    Position {
        /// The column.
        column: String,
        /// Its declared index.
        declared: usize,
        /// Its produced index.
        produced: usize,
    },
}

impl Disagreement {
    /// The column the disagreement is about.
    pub fn column(&self) -> &str {
        match self {
            Disagreement::Type { column, .. }
            | Disagreement::Missing { column, .. }
            | Disagreement::Undeclared { column, .. }
            | Disagreement::Position { column, .. } => column,
        }
    }

    /// The reason code of the JSON report (MH 4.9).
    pub fn reason(&self) -> &'static str {
        match self {
            Disagreement::Type { .. } => "type",
            Disagreement::Missing { .. } => "missing",
            Disagreement::Undeclared { .. } => "undeclared",
            Disagreement::Position { .. } => "position",
        }
    }
}

impl core::fmt::Display for Disagreement {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Disagreement::Type {
                column,
                declared,
                produced,
            } => write!(
                f,
                "column `{column}`: declared {declared}, produced {produced}"
            ),
            Disagreement::Missing { column, declared } => {
                write!(f, "column `{column}`: declared {declared}, not produced")
            }
            Disagreement::Undeclared { column, produced } => {
                write!(f, "column `{column}`: produced {produced}, not declared")
            }
            Disagreement::Position {
                column,
                declared,
                produced,
            } => write!(
                f,
                "column `{column}`: declared at position {declared}, produced at {produced}"
            ),
        }
    }
}

impl Expected {
    /// Every way `produced` disagrees with this expectation, in the declared columns' order and
    /// then the produced columns' order; empty when they agree (MH 4.9).
    pub fn compare(&self, produced: &Schema) -> Vec<Disagreement> {
        let mut out = Vec::new();
        for (at, column) in self.columns.iter().enumerate() {
            match produced.index_of(&column.name) {
                Err(_) => out.push(Disagreement::Missing {
                    column: column.name.clone(),
                    declared: column.ty.spelling(),
                }),
                Ok(index) => {
                    let field = produced.field(index);
                    if !column.ty.accepts(field.data_type()) {
                        out.push(Disagreement::Type {
                            column: column.name.clone(),
                            declared: column.ty.spelling(),
                            produced: type_name(field.data_type()),
                        });
                    } else if self.ordered && index != at {
                        out.push(Disagreement::Position {
                            column: column.name.clone(),
                            declared: at,
                            produced: index,
                        });
                    }
                }
            }
        }
        if self.closed {
            for field in produced.fields() {
                if !self.columns.iter().any(|c| c.name == *field.name()) {
                    out.push(Disagreement::Undeclared {
                        column: field.name().clone(),
                        produced: type_name(field.data_type()),
                    });
                }
            }
        }
        out
    }
}

/// What a kernel declares about its schemas (MH 4.9). Both halves are optional; a kernel that
/// declares an input is checkable, and one that also declares an output is compared.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Declared {
    /// What the kernel takes.
    pub input: Option<SchemaDecl>,
    /// What it gives back.
    pub output: Option<SchemaDecl>,
}

impl Declared {
    /// True when both halves are declared, which is what `moruna check` calls checkable.
    pub fn is_checkable(&self) -> bool {
        self.input.is_some() && self.output.is_some()
    }

    /// The declaration as canonical JSON, with a fixed key order and the type grammar's
    /// spellings, for fingerprints (05 e.4).
    pub fn canonical_json(&self) -> String {
        format!(
            "{{\"input\":{},\"output\":{}}}",
            optional_decl_json(self.input.as_ref()),
            optional_decl_json(self.output.as_ref())
        )
    }
}

fn optional_decl_json(decl: Option<&SchemaDecl>) -> String {
    match decl {
        None => "null".into(),
        Some(SchemaDecl::Exact(c)) => format!("{{\"exact\":{}}}", columns_json(c)),
        Some(SchemaDecl::Subset(c)) => format!("{{\"subset\":{}}}", columns_json(c)),
        Some(SchemaDecl::Relative {
            adds,
            drops,
            changes,
        }) => {
            let drops: Vec<String> = drops.iter().map(|d| json_string(d)).collect();
            format!(
                "{{\"adds\":{},\"changes\":{},\"drops\":[{}]}}",
                columns_json(adds),
                columns_json(changes),
                drops.join(",")
            )
        }
    }
}

fn columns_json(columns: &[ColumnDecl]) -> String {
    let parts: Vec<String> = columns
        .iter()
        .map(|c| {
            format!(
                "[{},{},{}]",
                json_string(&c.name),
                json_string(&c.ty.spelling()),
                c.nullable
            )
        })
        .collect();
    format!("[{}]", parts.join(","))
}

/// A string as a JSON string literal.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The canonical spelling of an Arrow type in the type grammar (MH 4.9). It is pyarrow's
/// `str(type)` for every type the grammar has, so a declaration written in either language reads
/// the same in a report.
pub fn type_name(dt: &DataType) -> String {
    match dt {
        DataType::Null => "null".into(),
        DataType::Boolean => "bool".into(),
        DataType::Int8 => "int8".into(),
        DataType::Int16 => "int16".into(),
        DataType::Int32 => "int32".into(),
        DataType::Int64 => "int64".into(),
        DataType::UInt8 => "uint8".into(),
        DataType::UInt16 => "uint16".into(),
        DataType::UInt32 => "uint32".into(),
        DataType::UInt64 => "uint64".into(),
        DataType::Float16 => "halffloat".into(),
        DataType::Float32 => "float".into(),
        DataType::Float64 => "double".into(),
        DataType::Utf8 => "string".into(),
        DataType::LargeUtf8 => "large_string".into(),
        DataType::Utf8View => "string_view".into(),
        DataType::Binary => "binary".into(),
        DataType::LargeBinary => "large_binary".into(),
        DataType::BinaryView => "binary_view".into(),
        DataType::Date32 => "date32[day]".into(),
        DataType::Date64 => "date64[ms]".into(),
        DataType::Timestamp(unit, tz) => match tz {
            None => format!("timestamp[{}]", unit_name(unit)),
            Some(tz) => format!("timestamp[{}, tz={tz}]", unit_name(unit)),
        },
        DataType::Decimal128(p, s) => format!("decimal128({p}, {s})"),
        DataType::List(item) => format!("list<item: {}>", type_name(item.data_type())),
        DataType::LargeList(item) => {
            format!("large_list<item: {}>", type_name(item.data_type()))
        }
        other => format!("{other}"),
    }
}

fn unit_name(unit: &TimeUnit) -> &'static str {
    match unit {
        TimeUnit::Second => "s",
        TimeUnit::Millisecond => "ms",
        TimeUnit::Microsecond => "us",
        TimeUnit::Nanosecond => "ns",
    }
}

fn parse_unit(s: &str) -> Option<TimeUnit> {
    Some(match s.trim() {
        "s" => TimeUnit::Second,
        "ms" => TimeUnit::Millisecond,
        "us" => TimeUnit::Microsecond,
        "ns" => TimeUnit::Nanosecond,
        _ => return None,
    })
}

/// Parse a type in the grammar of MH 4.9: pyarrow's `str(type)` spellings (`int64`, `double`,
/// `string`, `timestamp[us, tz=UTC]`, `list<item: int64>`, `decimal128(10, 2)`), the aliases
/// `boolean`, `float32`, `float64`, `utf8`, `large_utf8`, `str`, `date32`, `date64`,
/// `list<int64>`, and `any`. An unknown spelling is a `Plan` error naming it.
pub fn parse_type(s: &str) -> crate::Result<TypeDecl> {
    let t = s.trim();
    let unknown = || MorunaError::Plan(format!("unknown type `{s}` in a schema declaration"));
    let simple = match t {
        "any" => return Ok(TypeDecl::Any),
        "null" => Some(DataType::Null),
        "bool" | "boolean" => Some(DataType::Boolean),
        "int8" => Some(DataType::Int8),
        "int16" => Some(DataType::Int16),
        "int32" => Some(DataType::Int32),
        "int64" => Some(DataType::Int64),
        "uint8" => Some(DataType::UInt8),
        "uint16" => Some(DataType::UInt16),
        "uint32" => Some(DataType::UInt32),
        "uint64" => Some(DataType::UInt64),
        "halffloat" | "float16" => Some(DataType::Float16),
        "float" | "float32" => Some(DataType::Float32),
        "double" | "float64" => Some(DataType::Float64),
        "string" | "utf8" | "str" => Some(DataType::Utf8),
        "large_string" | "large_utf8" => Some(DataType::LargeUtf8),
        "string_view" => Some(DataType::Utf8View),
        "binary" => Some(DataType::Binary),
        "large_binary" => Some(DataType::LargeBinary),
        "binary_view" => Some(DataType::BinaryView),
        "date32" | "date32[day]" => Some(DataType::Date32),
        "date64" | "date64[ms]" => Some(DataType::Date64),
        _ => None,
    };
    if let Some(dt) = simple {
        return Ok(TypeDecl::Exact(dt));
    }
    if let Some(inner) = t
        .strip_prefix("timestamp[")
        .and_then(|r| r.strip_suffix(']'))
    {
        let mut parts = inner.splitn(2, ',');
        let unit = parts.next().and_then(parse_unit).ok_or_else(unknown)?;
        let tz = match parts.next() {
            None => None,
            Some(rest) => {
                let tz = rest.trim().strip_prefix("tz=").ok_or_else(unknown)?;
                Some(Arc::from(tz.trim()))
            }
        };
        return Ok(TypeDecl::Exact(DataType::Timestamp(unit, tz)));
    }
    if let Some(inner) = t
        .strip_prefix("decimal128(")
        .and_then(|r| r.strip_suffix(')'))
    {
        let mut parts = inner.split(',');
        let p: u8 = parts
            .next()
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(unknown)?;
        let s: i8 = parts
            .next()
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(unknown)?;
        return Ok(TypeDecl::Exact(DataType::Decimal128(p, s)));
    }
    for (prefix, large) in [("list<", false), ("large_list<", true)] {
        if let Some(inner) = t.strip_prefix(prefix).and_then(|r| r.strip_suffix('>')) {
            let (name, item) = match inner.split_once(':') {
                Some((name, item)) if !name.contains('<') => (name.trim(), item),
                _ => ("item", inner),
            };
            let TypeDecl::Exact(item) = parse_type(item)? else {
                return Err(unknown());
            };
            let field = Arc::new(Field::new(name, item, true));
            return Ok(TypeDecl::Exact(if large {
                DataType::LargeList(field)
            } else {
                DataType::List(field)
            }));
        }
    }
    Err(unknown())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_round_trips_through_the_grammar() {
        let tz: Arc<str> = Arc::from("UTC");
        let types = [
            DataType::Null,
            DataType::Boolean,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::UInt64,
            DataType::Float16,
            DataType::Float32,
            DataType::Float64,
            DataType::Utf8,
            DataType::LargeUtf8,
            DataType::Utf8View,
            DataType::Binary,
            DataType::LargeBinary,
            DataType::BinaryView,
            DataType::Date32,
            DataType::Date64,
            DataType::Timestamp(TimeUnit::Second, None),
            DataType::Timestamp(TimeUnit::Millisecond, None),
            DataType::Timestamp(TimeUnit::Microsecond, Some(tz)),
            DataType::Timestamp(TimeUnit::Nanosecond, None),
            DataType::Decimal128(10, 2),
            DataType::List(Arc::new(Field::new("item", DataType::Int64, true))),
            DataType::LargeList(Arc::new(Field::new("item", DataType::Utf8, true))),
        ];
        for dt in types {
            let spelled = type_name(&dt);
            assert_eq!(
                parse_type(&spelled).expect(&spelled),
                TypeDecl::Exact(dt),
                "{spelled}"
            );
        }
        for (alias, dt) in [
            ("boolean", DataType::Boolean),
            ("float64", DataType::Float64),
            ("float32", DataType::Float32),
            ("float16", DataType::Float16),
            ("utf8", DataType::Utf8),
            ("str", DataType::Utf8),
            ("large_utf8", DataType::LargeUtf8),
            ("date32", DataType::Date32),
            ("date64", DataType::Date64),
            (
                "list<int64>",
                DataType::List(Arc::new(Field::new("item", DataType::Int64, true))),
            ),
        ] {
            assert_eq!(parse_type(alias).expect(alias), TypeDecl::Exact(dt));
        }
        assert_eq!(parse_type(" any ").expect("any"), TypeDecl::Any);
        assert_eq!(TypeDecl::Any.spelling(), "any");
        for bad in [
            "tensor",
            "timestamp[h]",
            "timestamp[s, zone=UTC]",
            "decimal128(x, 2)",
            "decimal128(10)",
            "list<any>",
            "list<nope>",
        ] {
            assert!(parse_type(bad).is_err(), "{bad}");
        }
        assert!(type_name(&DataType::Time32(TimeUnit::Second)).contains("Time32"));
    }

    fn input() -> Schema {
        Schema::new(vec![
            Field::new("a", DataType::Int64, false),
            Field::new("b", DataType::Utf8, true),
        ])
    }

    #[test]
    fn exact_subset_and_relative_resolve_and_compare() {
        let exact = SchemaDecl::from_schema(&input());
        let e = exact.resolve(&input()).expect("exact");
        assert!(e.ordered && e.closed);
        assert!(e.compare(&input()).is_empty());
        let swapped = Schema::new(vec![
            Field::new("b", DataType::Utf8, true),
            Field::new("a", DataType::Int64, false),
        ]);
        let d = e.compare(&swapped);
        assert_eq!(d[0].reason(), "position");
        assert!(d[0].to_string().contains("position 0"));

        let subset = SchemaDecl::Subset(vec![ColumnDecl::any("a")]);
        let s = subset.resolve(&input()).expect("subset");
        assert!(
            s.compare(&swapped).is_empty(),
            "a subset allows other columns anywhere"
        );
        let only_b = Schema::new(vec![Field::new("b", DataType::Utf8, true)]);
        let d = s.compare(&only_b);
        assert_eq!(d[0].reason(), "missing");
        assert_eq!(d[0].column(), "a");
        assert!(d[0].to_string().contains("not produced"));

        let relative = SchemaDecl::Relative {
            adds: vec![ColumnDecl::new("c", DataType::Boolean)],
            drops: vec!["b".into()],
            changes: vec![ColumnDecl::new("a", DataType::Float64)],
        };
        let r = relative.resolve(&input()).expect("relative");
        let good = Schema::new(vec![
            Field::new("c", DataType::Boolean, true),
            Field::new("a", DataType::Float64, true),
        ]);
        assert!(r.compare(&good).is_empty(), "relative ignores order");
        let bad = Schema::new(vec![
            Field::new("a", DataType::Int64, true),
            Field::new("b", DataType::Utf8, true),
        ]);
        let reasons: Vec<&str> = r.compare(&bad).iter().map(|d| d.reason()).collect();
        assert_eq!(reasons, vec!["type", "missing", "undeclared"]);
        let text: Vec<String> = r.compare(&bad).iter().map(|d| d.to_string()).collect();
        assert_eq!(text[0], "column `a`: declared double, produced int64");
        assert_eq!(text[2], "column `b`: produced string, not declared");

        let ghost = SchemaDecl::Relative {
            adds: Vec::new(),
            drops: vec!["ghost".into()],
            changes: Vec::new(),
        };
        assert!(ghost.resolve(&input()).is_err());
        assert!(ghost.synthetic_schema().is_err());
    }

    #[test]
    fn synthetic_schemas_and_canonical_json() {
        let decl = SchemaDecl::Subset(vec![
            ColumnDecl::any("a"),
            ColumnDecl {
                name: "b\"q".into(),
                ty: TypeDecl::Exact(DataType::Utf8),
                nullable: false,
            },
        ]);
        let schema = decl.synthetic_schema().expect("schema");
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
        assert!(!schema.field(1).is_nullable());
        let declared = Declared {
            input: Some(decl),
            output: Some(SchemaDecl::Relative {
                adds: vec![ColumnDecl::any("x")],
                drops: vec!["a".into()],
                changes: Vec::new(),
            }),
        };
        assert!(declared.is_checkable());
        assert!(!Declared::default().is_checkable());
        assert_eq!(
            declared.canonical_json(),
            r#"{"input":{"subset":[["a","any",true],["b\"q","string",false]]},"output":{"adds":[["x","any",true]],"changes":[],"drops":["a"]}}"#
        );
        let exact = Declared {
            input: Some(SchemaDecl::Exact(vec![ColumnDecl::any("a")])),
            output: None,
        };
        assert_eq!(
            exact.canonical_json(),
            r#"{"input":{"exact":[["a","any",true]]},"output":null}"#
        );
        assert_eq!(json_string("a\\\n\r\t\u{1}"), "\"a\\\\\\n\\r\\t\\u0001\"");
    }
}
