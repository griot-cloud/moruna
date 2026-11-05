//! A job's run arguments (SDD 22 e.4): the document declares each one, a name and a type, and
//! every run supplies its values. One document can then serve every run, a time window per run
//! for example, and the run's report records what it was given.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

/// The type of a run argument, and so the values a run may give it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArgType {
    /// Any string.
    String,
    /// A whole number, a signed 64-bit integer.
    Int,
    /// A number; a whole number is a float too.
    Float,
    /// `true` or `false`.
    Bool,
    /// A calendar date, `YYYY-MM-DD`.
    Date,
    /// An instant, RFC 3339: `YYYY-MM-DDTHH:MM:SS`, optional fraction, then `Z` or `+HH:MM`.
    Timestamp,
}

/// Every type a declaration may name, as it names it.
pub const ARG_TYPES: [&str; 6] = ["string", "int", "float", "bool", "date", "timestamp"];

impl ArgType {
    /// The type `name` names, or `None`.
    pub fn parse(name: &str) -> Option<ArgType> {
        Some(match name {
            "string" => ArgType::String,
            "int" => ArgType::Int,
            "float" => ArgType::Float,
            "bool" => ArgType::Bool,
            "date" => ArgType::Date,
            "timestamp" => ArgType::Timestamp,
            _ => return None,
        })
    }

    /// The type's name, as a declaration writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            ArgType::String => "string",
            ArgType::Int => "int",
            ArgType::Float => "float",
            ArgType::Bool => "bool",
            ArgType::Date => "date",
            ArgType::Timestamp => "timestamp",
        }
    }
}

/// One declared argument: its name and its type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArgDecl {
    /// 1 to 64 of ASCII letters, digits and `_`, starting with a letter ([`is_arg_name`]).
    pub name: String,
    /// What a run may give it.
    #[serde(rename = "type")]
    pub ty: ArgType,
}

impl ArgDecl {
    /// The argument `name` of type `ty`.
    pub fn new(name: impl Into<String>, ty: ArgType) -> ArgDecl {
        ArgDecl {
            name: name.into(),
            ty,
        }
    }
}

/// Whether `name` is an argument's name: 1 to 64 of ASCII letters, digits and `_`, starting
/// with a letter.
pub fn is_arg_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Every fault of a list of declarations, each naming the declaration at fault: a name that is
/// not one, a name declared twice, the declarations out of name order. Empty when sound.
pub fn declaration_faults(declared: &[ArgDecl]) -> Vec<String> {
    let mut faults = Vec::new();
    for (i, decl) in declared.iter().enumerate() {
        if !is_arg_name(&decl.name) {
            faults.push(format!(
                "args[{i}].name: {:?} is not an argument's name: 1 to 64 of ASCII letters, \
                 digits and `_`, starting with a letter",
                decl.name
            ));
        }
        if let Some(before) = i.checked_sub(1).and_then(|j| declared.get(j)) {
            if before.name == decl.name {
                faults.push(format!("args[{i}].name: `{}` is declared twice", decl.name));
            } else if before.name > decl.name {
                faults.push(format!(
                    "args[{i}].name: `{}` comes after `{}`; a job lists its arguments in name \
                     order",
                    decl.name, before.name
                ));
            }
        }
    }
    faults
}

/// The value `text` gives an argument of type `ty` on a command line (`--arg NAME=VALUE`): the
/// text itself for a string, a date and a timestamp (checked), a number, `true` or `false`.
pub fn parse_value(ty: ArgType, text: &str) -> Result<Value, String> {
    let value = match ty {
        ArgType::String | ArgType::Date | ArgType::Timestamp => Value::String(text.to_string()),
        ArgType::Int => Value::from(
            text.parse::<i64>()
                .map_err(|_| format!("`{text}` is not of type int"))?,
        ),
        ArgType::Float => {
            let number = text
                .parse::<f64>()
                .map_err(|_| format!("`{text}` is not of type float"))?;
            serde_json::Number::from_f64(number)
                .map(Value::Number)
                .ok_or_else(|| format!("`{text}` is not a finite float"))?
        }
        ArgType::Bool => match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(format!("`{text}` is not a bool: true or false")),
        },
    };
    check_value(ty, &value)?;
    Ok(value)
}

/// Why `value` is not a value of `ty`, or nothing when it is one.
pub fn check_value(ty: ArgType, value: &Value) -> Result<(), String> {
    let fits = match (ty, value) {
        (ArgType::String, Value::String(_)) => true,
        (ArgType::Int, Value::Number(n)) => n.is_i64(),
        (ArgType::Float, Value::Number(_)) => true,
        (ArgType::Bool, Value::Bool(_)) => true,
        (ArgType::Date, Value::String(s)) => {
            return date(s)
                .map(|_| ())
                .ok_or_else(|| format!("{s:?} is not a date: YYYY-MM-DD, a day of its month"));
        }
        (ArgType::Timestamp, Value::String(s)) => {
            return timestamp(s).then_some(()).ok_or_else(|| {
                format!(
                    "{s:?} is not a timestamp: RFC 3339, YYYY-MM-DDTHH:MM:SS, an optional \
                     fraction, then Z or +HH:MM"
                )
            });
        }
        _ => false,
    };
    if fits {
        Ok(())
    } else {
        Err(format!("{value} is not of type {}", ty.as_str()))
    }
}

/// The values a run gives the job's `declared` arguments, checked: every declared argument
/// given, each a value of its type, nothing given that is not declared. Every refusal is named,
/// not only the first; the values come back in name order.
pub fn supply(
    declared: &[ArgDecl],
    given: &BTreeMap<String, Value>,
) -> Result<Map<String, Value>, Vec<String>> {
    let mut refusals = Vec::new();
    let mut values = Map::new();
    for decl in declared {
        match given.get(&decl.name) {
            None => refusals.push(format!(
                "args.{}: the job declares it ({}) and the run gives no value",
                decl.name,
                decl.ty.as_str()
            )),
            Some(value) => match check_value(decl.ty, value) {
                Ok(()) => {
                    values.insert(decl.name.clone(), value.clone());
                }
                Err(why) => refusals.push(format!("args.{}: {why}", decl.name)),
            },
        }
    }
    for name in given.keys() {
        if !declared.iter().any(|d| &d.name == name) {
            refusals.push(format!(
                "args.{name}: the job declares no such argument{}",
                if declared.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; it declares {}",
                        declared
                            .iter()
                            .map(|d| d.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            ));
        }
    }
    if refusals.is_empty() {
        Ok(values)
    } else {
        Err(refusals)
    }
}

fn digits(s: &str) -> Option<u32> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// `(year, month, day)` of a `YYYY-MM-DD` date that exists.
fn date(s: &str) -> Option<(u32, u32, u32)> {
    let (year, rest) = s.split_at_checked(4)?;
    let rest = rest.strip_prefix('-')?;
    let (month, rest) = rest.split_at_checked(2)?;
    let day = rest.strip_prefix('-')?;
    if day.len() != 2 {
        return None;
    }
    let (year, month, day) = (digits(year)?, digits(month)?, digits(day)?);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    (1..=days).contains(&day).then_some((year, month, day))
}

/// Whether `s` is an RFC 3339 timestamp.
fn timestamp(s: &str) -> bool {
    let Some((day, time)) = s.split_once(['T', 't']) else {
        return false;
    };
    if date(day).is_none() || time.len() < 9 {
        return false;
    }
    let (clock, zone) = time.split_at(8);
    let parts: Vec<&str> = clock.split(':').collect();
    let clock_ok = matches!(parts.as_slice(), [h, m, s]
        if h.len() == 2 && m.len() == 2 && s.len() == 2
            && digits(h).is_some_and(|h| h < 24)
            && digits(m).is_some_and(|m| m < 60)
            && digits(s).is_some_and(|s| s < 61));
    if !clock_ok {
        return false;
    }
    let zone = match zone.strip_prefix('.') {
        Some(fraction) => {
            let end = fraction
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(fraction.len());
            if end == 0 {
                return false;
            }
            &fraction[end..]
        }
        None => zone,
    };
    match zone {
        "Z" | "z" => true,
        offset => {
            let Some(rest) = offset.strip_prefix(['+', '-']) else {
                return false;
            };
            matches!(rest.split_once(':'), Some((h, m))
                if h.len() == 2 && m.len() == 2
                    && digits(h).is_some_and(|h| h < 24)
                    && digits(m).is_some_and(|m| m < 60))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn window() -> Vec<ArgDecl> {
        vec![
            ArgDecl::new("end", ArgType::Timestamp),
            ArgDecl::new("limit", ArgType::Int),
            ArgDecl::new("start", ArgType::Timestamp),
        ]
    }

    #[test]
    fn a_run_gives_every_argument_of_its_type() {
        let given: BTreeMap<String, Value> = [
            ("start", json!("2026-11-01T00:00:00Z")),
            ("end", json!("2026-11-02T00:00:00.5+03:00")),
            ("limit", json!(10)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let values = supply(&window(), &given).expect("supplied");
        assert_eq!(values.keys().collect::<Vec<_>>(), ["end", "limit", "start"]);
    }

    #[test]
    fn every_refusal_is_named() {
        let given: BTreeMap<String, Value> = [
            ("start", json!("yesterday")),
            ("limit", json!(1.5)),
            ("colour", json!("red")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let refused = supply(&window(), &given).expect_err("refused").join("; ");
        assert!(
            refused.contains("args.end: the job declares it (timestamp)"),
            "{refused}"
        );
        assert!(
            refused.contains("args.limit: 1.5 is not of type int"),
            "{refused}"
        );
        assert!(
            refused.contains("args.start: \"yesterday\" is not a timestamp"),
            "{refused}"
        );
        assert!(
            refused.contains("args.colour: the job declares no such argument"),
            "{refused}"
        );
    }

    #[test]
    fn values_from_a_command_line() {
        assert_eq!(parse_value(ArgType::Int, "42").unwrap(), json!(42));
        assert_eq!(parse_value(ArgType::Float, "2").unwrap(), json!(2.0));
        assert_eq!(parse_value(ArgType::Bool, "true").unwrap(), json!(true));
        assert_eq!(
            parse_value(ArgType::Date, "2024-02-29").unwrap(),
            json!("2024-02-29")
        );
        assert!(parse_value(ArgType::Date, "2023-02-29").is_err());
        assert!(parse_value(ArgType::Bool, "yes").is_err());
        assert!(parse_value(ArgType::Int, "4.0").is_err());
        assert!(parse_value(ArgType::Float, "NaN").is_err());
        assert!(parse_value(ArgType::Timestamp, "2026-11-01T25:00:00Z").is_err());
        assert!(parse_value(ArgType::Timestamp, "2026-11-01T10:00:00").is_err());
        assert!(parse_value(ArgType::Timestamp, "2026-11-01t10:00:00.123z").is_ok());
    }

    #[test]
    fn declarations_are_named_once_in_order() {
        assert!(declaration_faults(&window()).is_empty());
        let faults = declaration_faults(&[
            ArgDecl::new("b", ArgType::Int),
            ArgDecl::new("a", ArgType::Int),
            ArgDecl::new("a", ArgType::Int),
            ArgDecl::new("9x", ArgType::Int),
        ])
        .join("; ");
        assert!(faults.contains("`a` comes after `b`"), "{faults}");
        assert!(faults.contains("`a` is declared twice"), "{faults}");
        assert!(
            faults.contains("\"9x\" is not an argument's name"),
            "{faults}"
        );
        assert_eq!(ArgType::parse("timestamp"), Some(ArgType::Timestamp));
        assert_eq!(ArgType::parse("datetime"), None);
        for name in ARG_TYPES {
            assert_eq!(ArgType::parse(name).map(ArgType::as_str), Some(name));
        }
    }
}
