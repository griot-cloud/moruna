//! The type grammar of a schema declaration (MH 4.9), read once and written once, without Arrow.
//!
//! A declared column's type is a string: pyarrow's `str(type)` spelling of every type the grammar
//! has (`int64`, `double`, `string`, `timestamp[us, tz=UTC]`, `decimal128(10, 2)`,
//! `list<item: int64>`, `struct<a: int64, b: string>`), a few aliases (`boolean`, `float32`, `float64`, `utf8`, `large_utf8`,
//! `str`, `date32`, `date64`, `list<int64>`), and `any`. [`parse`] reads a spelling into a
//! [`ColumnType`], a description of the type that needs no Arrow; [`TypeName`]'s `Display` writes
//! the canonical spelling back. Two spellings name the same type exactly when they read as equal
//! descriptions.
//!
//! This is the grammar's only reader. `moruna-kernel` converts a description into the Arrow
//! `DataType` it names (`moruna_kernel::declare::data_type`) and an Arrow type back into the
//! description that names it; `moruna-job` compares descriptions where no Arrow is linked (a
//! served kind's schema rules, SDD 22 e.6). The crate depends on nothing, so both can take it.

#![deny(missing_docs)]

/// A time unit of a timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimeUnit {
    /// `s`.
    Second,
    /// `ms`.
    Millisecond,
    /// `us`.
    Microsecond,
    /// `ns`.
    Nanosecond,
}

impl TimeUnit {
    /// Every unit, coarsest first.
    pub const ALL: [TimeUnit; 4] = [
        TimeUnit::Second,
        TimeUnit::Millisecond,
        TimeUnit::Microsecond,
        TimeUnit::Nanosecond,
    ];

    /// The unit as the grammar spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            TimeUnit::Second => "s",
            TimeUnit::Millisecond => "ms",
            TimeUnit::Microsecond => "us",
            TimeUnit::Nanosecond => "ns",
        }
    }

    fn parse(s: &str) -> Option<TimeUnit> {
        TimeUnit::ALL.into_iter().find(|u| u.as_str() == s.trim())
    }
}

/// A type the grammar names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeName {
    /// `null`.
    Null,
    /// `bool` (`boolean`).
    Bool,
    /// `int8`.
    Int8,
    /// `int16`.
    Int16,
    /// `int32`.
    Int32,
    /// `int64`.
    Int64,
    /// `uint8`.
    UInt8,
    /// `uint16`.
    UInt16,
    /// `uint32`.
    UInt32,
    /// `uint64`.
    UInt64,
    /// `halffloat` (`float16`).
    HalfFloat,
    /// `float` (`float32`).
    Float,
    /// `double` (`float64`).
    Double,
    /// `string` (`utf8`, `str`).
    String,
    /// `large_string` (`large_utf8`).
    LargeString,
    /// `string_view`.
    StringView,
    /// `binary`.
    Binary,
    /// `large_binary`.
    LargeBinary,
    /// `binary_view`.
    BinaryView,
    /// `date32[day]` (`date32`).
    Date32,
    /// `date64[ms]` (`date64`).
    Date64,
    /// `timestamp[<unit>]` or `timestamp[<unit>, tz=<zone>]`.
    Timestamp(TimeUnit, Option<String>),
    /// `decimal128(<precision>, <scale>)`.
    Decimal128(u8, i8),
    /// `list<<name>: <item>>` (`list<<item>>`, whose item is named `item`); the item may be null.
    List(String, Box<TypeName>),
    /// `large_list<<name>: <item>>`, likewise.
    LargeList(String, Box<TypeName>),
    /// `struct<<name>: <type>, ...>`, its fields in order; every field may be null.
    Struct(Vec<(String, TypeName)>),
}

/// A declared column's type: any, or exactly one the grammar names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ColumnType {
    /// `any`: whatever type the column has.
    Any,
    /// Exactly this type.
    Exact(TypeName),
}

impl ColumnType {
    /// The canonical spelling: `any`, or the type's.
    pub fn spelling(&self) -> String {
        match self {
            ColumnType::Any => "any".to_string(),
            ColumnType::Exact(name) => name.to_string(),
        }
    }

    /// Whether a column declared `self` accepts one known to be `given`: `any` on either side
    /// accepts, and two exact types accept each other when they are the same type.
    pub fn accepts(&self, given: &ColumnType) -> bool {
        match (self, given) {
            (ColumnType::Exact(a), ColumnType::Exact(b)) => a == b,
            _ => true,
        }
    }
}

/// A spelling the grammar does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownType(pub String);

impl core::fmt::Display for UnknownType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "unknown type `{}` in a schema declaration", self.0)
    }
}

impl std::error::Error for UnknownType {}

/// The column type `spelling` names, surrounding space ignored.
pub fn parse(spelling: &str) -> Result<ColumnType, UnknownType> {
    let t = spelling.trim();
    if t == "any" {
        return Ok(ColumnType::Any);
    }
    name(t)
        .map(ColumnType::Exact)
        .ok_or_else(|| UnknownType(spelling.to_string()))
}

/// The type `t` (trimmed) names; `any` is no type.
fn name(t: &str) -> Option<TypeName> {
    let simple = match t {
        "null" => Some(TypeName::Null),
        "bool" | "boolean" => Some(TypeName::Bool),
        "int8" => Some(TypeName::Int8),
        "int16" => Some(TypeName::Int16),
        "int32" => Some(TypeName::Int32),
        "int64" => Some(TypeName::Int64),
        "uint8" => Some(TypeName::UInt8),
        "uint16" => Some(TypeName::UInt16),
        "uint32" => Some(TypeName::UInt32),
        "uint64" => Some(TypeName::UInt64),
        "halffloat" | "float16" => Some(TypeName::HalfFloat),
        "float" | "float32" => Some(TypeName::Float),
        "double" | "float64" => Some(TypeName::Double),
        "string" | "utf8" | "str" => Some(TypeName::String),
        "large_string" | "large_utf8" => Some(TypeName::LargeString),
        "string_view" => Some(TypeName::StringView),
        "binary" => Some(TypeName::Binary),
        "large_binary" => Some(TypeName::LargeBinary),
        "binary_view" => Some(TypeName::BinaryView),
        "date32" | "date32[day]" => Some(TypeName::Date32),
        "date64" | "date64[ms]" => Some(TypeName::Date64),
        _ => None,
    };
    if simple.is_some() {
        return simple;
    }
    if let Some(inner) = t
        .strip_prefix("timestamp[")
        .and_then(|r| r.strip_suffix(']'))
    {
        let mut parts = inner.splitn(2, ',');
        let unit = TimeUnit::parse(parts.next()?)?;
        let zone = match parts.next() {
            None => None,
            Some(rest) => {
                let zone = rest.trim().strip_prefix("tz=")?.trim();
                if zone.is_empty() {
                    return None;
                }
                Some(zone.to_string())
            }
        };
        return Some(TypeName::Timestamp(unit, zone));
    }
    if let Some(inner) = t
        .strip_prefix("decimal128(")
        .and_then(|r| r.strip_suffix(')'))
    {
        let (precision, scale) = inner.split_once(',')?;
        return Some(TypeName::Decimal128(
            precision.trim().parse().ok()?,
            scale.trim().parse().ok()?,
        ));
    }
    if let Some(inner) = t.strip_prefix("struct<").and_then(|r| r.strip_suffix('>')) {
        if inner.trim().is_empty() {
            return Some(TypeName::Struct(Vec::new()));
        }
        let mut fields = Vec::new();
        for part in top_level(inner) {
            let (field, item) = part.split_once(':')?;
            let field = field.trim();
            if field.is_empty() || field.contains(['<', '[', '(']) {
                return None;
            }
            fields.push((field.to_string(), name(item.trim())?));
        }
        return Some(TypeName::Struct(fields));
    }
    for (prefix, large) in [("list<", false), ("large_list<", true)] {
        if let Some(inner) = t.strip_prefix(prefix).and_then(|r| r.strip_suffix('>')) {
            let (field, item) = match inner.split_once(':') {
                Some((field, item)) if !field.contains('<') => (field.trim(), item.trim()),
                _ => ("item", inner.trim()),
            };
            if field.is_empty() {
                return None;
            }
            let item = Box::new(name(item)?);
            return Some(if large {
                TypeName::LargeList(field.to_string(), item)
            } else {
                TypeName::List(field.to_string(), item)
            });
        }
    }
    None
}

/// `s` cut at each comma outside `<>`, `[]` and `()`: a struct's fields, whose types may hold
/// commas of their own (`timestamp[us, tz=UTC]`, `decimal128(10, 2)`, a nested struct).
fn top_level(s: &str) -> Vec<&str> {
    let (mut depth, mut start, mut parts) = (0i32, 0, Vec::new());
    for (at, c) in s.char_indices() {
        match c {
            '<' | '[' | '(' => depth += 1,
            '>' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&s[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

impl core::fmt::Display for TypeName {
    /// The canonical spelling: pyarrow's `str(type)`, which [`parse`] reads back as itself.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let simple = match self {
            TypeName::Null => "null",
            TypeName::Bool => "bool",
            TypeName::Int8 => "int8",
            TypeName::Int16 => "int16",
            TypeName::Int32 => "int32",
            TypeName::Int64 => "int64",
            TypeName::UInt8 => "uint8",
            TypeName::UInt16 => "uint16",
            TypeName::UInt32 => "uint32",
            TypeName::UInt64 => "uint64",
            TypeName::HalfFloat => "halffloat",
            TypeName::Float => "float",
            TypeName::Double => "double",
            TypeName::String => "string",
            TypeName::LargeString => "large_string",
            TypeName::StringView => "string_view",
            TypeName::Binary => "binary",
            TypeName::LargeBinary => "large_binary",
            TypeName::BinaryView => "binary_view",
            TypeName::Date32 => "date32[day]",
            TypeName::Date64 => "date64[ms]",
            TypeName::Timestamp(unit, None) => return write!(f, "timestamp[{}]", unit.as_str()),
            TypeName::Timestamp(unit, Some(zone)) => {
                return write!(f, "timestamp[{}, tz={zone}]", unit.as_str());
            }
            TypeName::Decimal128(p, s) => return write!(f, "decimal128({p}, {s})"),
            TypeName::List(field, item) => return write!(f, "list<{field}: {item}>"),
            TypeName::LargeList(field, item) => return write!(f, "large_list<{field}: {item}>"),
            TypeName::Struct(fields) => {
                f.write_str("struct<")?;
                for (at, (field, item)) in fields.iter().enumerate() {
                    if at > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{field}: {item}")?;
                }
                return f.write_str(">");
            }
        };
        f.write_str(simple)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every type the grammar has, nested lists and every unit with and without a zone included.
    fn every() -> Vec<TypeName> {
        let mut out = vec![
            TypeName::Null,
            TypeName::Bool,
            TypeName::Int8,
            TypeName::Int16,
            TypeName::Int32,
            TypeName::Int64,
            TypeName::UInt8,
            TypeName::UInt16,
            TypeName::UInt32,
            TypeName::UInt64,
            TypeName::HalfFloat,
            TypeName::Float,
            TypeName::Double,
            TypeName::String,
            TypeName::LargeString,
            TypeName::StringView,
            TypeName::Binary,
            TypeName::LargeBinary,
            TypeName::BinaryView,
            TypeName::Date32,
            TypeName::Date64,
            TypeName::Decimal128(10, 2),
            TypeName::Decimal128(38, -3),
        ];
        for unit in TimeUnit::ALL {
            out.push(TypeName::Timestamp(unit, None));
            out.push(TypeName::Timestamp(unit, Some("Europe/Berlin".into())));
        }
        let leaves = out.clone();
        for leaf in &leaves {
            out.push(TypeName::List("item".into(), Box::new(leaf.clone())));
            out.push(TypeName::LargeList(
                "element".into(),
                Box::new(leaf.clone()),
            ));
        }
        out.push(TypeName::List(
            "item".into(),
            Box::new(TypeName::LargeList("v".into(), Box::new(TypeName::Bool))),
        ));
        let record = TypeName::Struct(
            leaves
                .iter()
                .enumerate()
                .map(|(at, leaf)| (format!("f{at}"), leaf.clone()))
                .collect(),
        );
        out.push(TypeName::Struct(Vec::new()));
        out.push(TypeName::List("item".into(), Box::new(record.clone())));
        out.push(TypeName::Struct(vec![
            ("inner".into(), record),
            (
                "tags".into(),
                TypeName::List("item".into(), Box::new(TypeName::String)),
            ),
        ]));
        out
    }

    #[test]
    fn every_type_reads_back_from_its_spelling() {
        for ty in every() {
            let spelled = ty.to_string();
            assert_eq!(
                parse(&spelled),
                Ok(ColumnType::Exact(ty.clone())),
                "{spelled}"
            );
            assert_eq!(ColumnType::Exact(ty).spelling(), spelled);
        }
        assert_eq!(parse(" any "), Ok(ColumnType::Any));
        assert_eq!(ColumnType::Any.spelling(), "any");
    }

    #[test]
    fn aliases_read_as_their_names() {
        for (alias, canonical) in [
            ("boolean", "bool"),
            (" float64 ", "double"),
            ("float32", "float"),
            ("float16", "halffloat"),
            ("utf8", "string"),
            ("str", "string"),
            ("large_utf8", "large_string"),
            ("date32", "date32[day]"),
            ("date64", "date64[ms]"),
            ("timestamp[ us ]", "timestamp[us]"),
            ("timestamp[ms,tz= UTC ]", "timestamp[ms, tz=UTC]"),
            ("decimal128( 10 , 2 )", "decimal128(10, 2)"),
            ("list<int64>", "list<item: int64>"),
            ("list< x : utf8 >", "list<x: string>"),
            (
                "large_list<list<double>>",
                "large_list<item: list<item: double>>",
            ),
        ] {
            assert_eq!(
                parse(alias).map(|t| t.spelling()).as_deref(),
                Ok(canonical),
                "{alias}"
            );
        }
    }

    #[test]
    fn what_is_not_in_the_grammar_is_refused_by_its_spelling() {
        for bad in [
            "",
            "integer",
            "Int64",
            "tensor",
            "timestamp[h]",
            "timestamp[s, zone=UTC]",
            "timestamp[s, utc]",
            "timestamp[us, tz=]",
            "decimal128(x, 2)",
            "decimal128(10)",
            "decimal128(10, 2, 9)",
            "decimal128(300, 2)",
            "list<any>",
            "list<nope>",
            "list<>",
            "list<: int64>",
            "map<string, int64>",
            "struct<a>",
            "struct<a: nope>",
            "struct<: int64>",
            "struct<a: int64,>",
            "struct<a: int64 not null>",
        ] {
            let refused = parse(bad).expect_err(bad);
            assert_eq!(
                refused.to_string(),
                format!("unknown type `{bad}` in a schema declaration")
            );
        }
    }

    #[test]
    fn any_accepts_and_exact_types_accept_themselves() {
        let int = ColumnType::Exact(TypeName::Int64);
        assert!(ColumnType::Any.accepts(&int));
        assert!(int.accepts(&ColumnType::Any));
        assert!(int.accepts(&int.clone()));
        assert!(!int.accepts(&ColumnType::Exact(TypeName::String)));
        let named =
            |n: &str| ColumnType::Exact(TypeName::List(n.into(), Box::new(TypeName::Int64)));
        assert!(
            !named("item").accepts(&named("element")),
            "a list's item name is its type's"
        );
    }
}
