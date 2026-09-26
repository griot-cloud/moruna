//! Reading a standard kernel's arguments (15 e.6) and writing them canonically.

use moruna_kernel::arrow::datatypes::DataType;
use moruna_kernel::declare::{TypeDecl, json_string, parse_type};
use moruna_kernel::{MorunaError, Result};
use serde_json::{Map, Value};

/// A `Plan` error naming the kernel and the argument.
pub(crate) fn bad(kernel: &str, msg: impl core::fmt::Display) -> MorunaError {
    MorunaError::Plan(format!("moruna.std.{kernel}: {msg}"))
}

/// The arguments as an object; `null` is an empty object.
pub(crate) fn object<'a>(kernel: &str, args: &'a Value) -> Result<Option<&'a Map<String, Value>>> {
    match args {
        Value::Null => Ok(None),
        Value::Object(map) => Ok(Some(map)),
        other => Err(bad(
            kernel,
            format!("arguments must be an object, not {other}"),
        )),
    }
}

/// Every key of `args` must be one of `known`; a misspelt argument is an error, not a default.
pub(crate) fn only(kernel: &str, args: &Value, known: &[&str]) -> Result<()> {
    if let Some(map) = object(kernel, args)? {
        for key in map.keys() {
            if !known.contains(&key.as_str()) {
                return Err(bad(
                    kernel,
                    format!(
                        "unknown argument `{key}` (expected one of {})",
                        known.join(", ")
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn get<'a>(kernel: &str, args: &'a Value, key: &str) -> Result<Option<&'a Value>> {
    Ok(object(kernel, args)?
        .and_then(|m| m.get(key))
        .filter(|v| !v.is_null()))
}

/// A required list of column names, non-empty.
pub(crate) fn names(kernel: &str, args: &Value, key: &str) -> Result<Vec<String>> {
    let names = optional_names(kernel, args, key)?
        .ok_or_else(|| bad(kernel, format!("`{key}` is required")))?;
    if names.is_empty() {
        return Err(bad(kernel, format!("`{key}` names no column")));
    }
    Ok(names)
}

/// An optional list of column names; a single string is a list of one.
pub(crate) fn optional_names(kernel: &str, args: &Value, key: &str) -> Result<Option<Vec<String>>> {
    let Some(value) = get(kernel, args, key)? else {
        return Ok(None);
    };
    match value {
        Value::String(s) => Ok(Some(vec![s.clone()])),
        Value::Array(items) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| bad(kernel, format!("`{key}` must list column names")))
            })
            .collect::<Result<Vec<_>>>()
            .map(Some),
        other => Err(bad(
            kernel,
            format!("`{key}` must list column names, not {other}"),
        )),
    }
}

/// A required string.
pub(crate) fn string(kernel: &str, args: &Value, key: &str) -> Result<String> {
    optional_string(kernel, args, key)?.ok_or_else(|| bad(kernel, format!("`{key}` is required")))
}

/// An optional string.
pub(crate) fn optional_string(kernel: &str, args: &Value, key: &str) -> Result<Option<String>> {
    match get(kernel, args, key)? {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(bad(
            kernel,
            format!("`{key}` must be a string, not {other}"),
        )),
    }
}

/// An optional boolean.
pub(crate) fn optional_bool(kernel: &str, args: &Value, key: &str) -> Result<Option<bool>> {
    match get(kernel, args, key)? {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(bad(
            kernel,
            format!("`{key}` must be true or false, not {other}"),
        )),
    }
}

/// An optional non-negative integer.
pub(crate) fn optional_u64(kernel: &str, args: &Value, key: &str) -> Result<Option<u64>> {
    match get(kernel, args, key)? {
        None => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| {
            bad(
                kernel,
                format!("`{key}` must be a non-negative integer, not {v}"),
            )
        }),
    }
}

/// A required object of column name to something, in key order.
pub(crate) fn mapping<'a>(
    kernel: &str,
    args: &'a Value,
    key: &str,
) -> Result<Vec<(String, &'a Value)>> {
    match get(kernel, args, key)? {
        Some(Value::Object(map)) if !map.is_empty() => {
            Ok(map.iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Some(Value::Object(_)) => Err(bad(kernel, format!("`{key}` names no column"))),
        Some(other) => Err(bad(
            kernel,
            format!("`{key}` must be an object, not {other}"),
        )),
        None => Err(bad(kernel, format!("`{key}` is required"))),
    }
}

/// A type spelling that must be exact (15 e.1).
pub(crate) fn exact_type(kernel: &str, value: &Value) -> Result<DataType> {
    let spelled = value
        .as_str()
        .ok_or_else(|| bad(kernel, format!("a type must be a string, not {value}")))?;
    let parsed = parse_type(spelled).map_err(|e| match e {
        MorunaError::Plan(msg) => bad(kernel, msg),
        other => other,
    })?;
    match parsed {
        TypeDecl::Exact(dt) => Ok(dt),
        TypeDecl::Any => Err(bad(kernel, "`any` is not a type a column can be cast to")),
    }
}

/// `value` as canonical JSON: object keys sorted by their UTF-8 bytes at every depth, no
/// whitespace, numbers as `serde_json` writes them (15 e.6). Written here rather than left to
/// `serde_json::to_string`, whose key order depends on a feature another crate in the build may
/// switch on.
pub fn canonical(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => json_string(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", json_string(k), canonical(&map[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_sorts_keys_at_every_depth() {
        let v = json!({"b": [1, {"z": true, "a": null}], "a": "x\"y"});
        assert_eq!(canonical(&v), r#"{"a":"x\"y","b":[1,{"a":null,"z":true}]}"#);
    }

    #[test]
    fn arguments_are_checked_by_name_and_shape() {
        let args = json!({"columns": ["a"], "n": 3, "flag": true, "s": "x"});
        assert!(only("k", &args, &["columns", "n", "flag", "s"]).is_ok());
        let err = only("k", &args, &["columns"]).expect_err("unknown");
        assert!(err.to_string().contains("unknown argument"), "{err}");
        assert_eq!(names("k", &args, "columns").expect("names"), vec!["a"]);
        assert_eq!(
            names("k", &json!({"columns": "a"}), "columns").expect("one"),
            vec!["a"]
        );
        assert!(names("k", &json!({"columns": []}), "columns").is_err());
        assert!(names("k", &json!({"columns": [1]}), "columns").is_err());
        assert!(names("k", &json!({"columns": 1}), "columns").is_err());
        assert!(names("k", &json!({}), "columns").is_err());
        assert_eq!(optional_u64("k", &args, "n").expect("n"), Some(3));
        assert!(optional_u64("k", &args, "s").is_err());
        assert_eq!(optional_bool("k", &args, "flag").expect("flag"), Some(true));
        assert!(optional_bool("k", &args, "s").is_err());
        assert_eq!(string("k", &args, "s").expect("s"), "x");
        assert!(string("k", &args, "missing").is_err());
        assert!(optional_string("k", &args, "n").is_err());
        assert!(object("k", &json!([1])).is_err());
        assert!(object("k", &Value::Null).expect("null").is_none());
        assert!(mapping("k", &json!({"m": {}}), "m").is_err());
        assert!(mapping("k", &json!({"m": 1}), "m").is_err());
        assert!(mapping("k", &json!({}), "m").is_err());
        assert!(exact_type("k", &json!("any")).is_err());
        assert!(exact_type("k", &json!(3)).is_err());
        assert_eq!(
            exact_type("k", &json!("int32")).expect("t"),
            DataType::Int32
        );
    }
}
