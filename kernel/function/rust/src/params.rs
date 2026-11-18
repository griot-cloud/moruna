//! A function's params and the run's arguments (item 5.5): the JSON objects the runner gives
//! every function's process as `MORUNA_PARAMS` and `MORUNA_ARGS` (`docs/images.md`), a native
//! function's as an image function's. On the core std reads them from the process's start slot
//! `env`; on a host, from the process's environment.

use serde_json::{Map, Value};

use crate::FunctionError;

/// The variable that holds the function's params.
pub(crate) const PARAMS: &str = "MORUNA_PARAMS";
/// The variable that holds the run's arguments.
pub(crate) const ARGS: &str = "MORUNA_ARGS";

/// The function's params, the JSON object its task was given (`{}` when it was given none, or
/// when the process runs outside a job). An error names the variable when it is not a JSON
/// object.
///
/// ```no_run
/// # use moruna_function::{Emit, FunctionError, RecordBatch};
/// # fn f(batch: RecordBatch, out: &mut Emit) -> Result<(), FunctionError> {
/// let threshold = moruna_function::params()?
///     .get("threshold")
///     .and_then(|t| t.as_f64())
///     .unwrap_or(0.5);
/// # Ok(()) }
/// ```
pub fn params() -> Result<Map<String, Value>, FunctionError> {
    object(PARAMS, std::env::var_os(PARAMS))
}

/// The run's arguments, the JSON object the run was given (`{}` when it was given none, or when
/// the process runs outside a job). An error names the variable when it is not a JSON object.
pub fn args() -> Result<Map<String, Value>, FunctionError> {
    object(ARGS, std::env::var_os(ARGS))
}

/// The object the variable `name` holds as `text`; `{}` when it is unset.
fn object(
    name: &str,
    text: Option<std::ffi::OsString>,
) -> Result<Map<String, Value>, FunctionError> {
    let Some(text) = text else {
        return Ok(Map::new());
    };
    let text = text
        .into_string()
        .map_err(|_| FunctionError::new(format!("{name} is not UTF-8")))?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(other) => Err(FunctionError::new(format!(
            "{name} is a JSON object, not {}",
            kind(&other)
        ))),
        Err(e) => Err(FunctionError::new(format!("{name} is not JSON: {e}"))),
    }
}

/// What a JSON value is, as an error names it.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn text(s: &str) -> Option<std::ffi::OsString> {
        Some(std::ffi::OsString::from(s))
    }

    #[test]
    fn an_object_is_read_and_an_unset_variable_is_empty() {
        let read = object(PARAMS, text(r#"{"threshold":0.5,"labels":["a"]}"#)).unwrap();
        assert_eq!(read["threshold"], json!(0.5));
        assert_eq!(read["labels"], json!(["a"]));
        assert_eq!(object(ARGS, None).unwrap(), Map::new());
        assert_eq!(object(ARGS, text("{}")).unwrap(), Map::new());
    }

    #[test]
    fn anything_else_is_an_error_naming_the_variable() {
        let said = |t: &str| object(PARAMS, text(t)).unwrap_err().message().to_string();
        assert_eq!(said("[1]"), "MORUNA_PARAMS is a JSON object, not an array");
        assert_eq!(said("null"), "MORUNA_PARAMS is a JSON object, not null");
        assert_eq!(
            said("true"),
            "MORUNA_PARAMS is a JSON object, not a boolean"
        );
        assert_eq!(said("1"), "MORUNA_PARAMS is a JSON object, not a number");
        assert_eq!(
            said("\"x\""),
            "MORUNA_PARAMS is a JSON object, not a string"
        );
        assert!(
            said("{").starts_with("MORUNA_PARAMS is not JSON: "),
            "{}",
            said("{")
        );
        assert_eq!(kind(&json!({})), "an object");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let bad = std::ffi::OsString::from_vec(vec![0xff]);
            let e = object(ARGS, Some(bad)).unwrap_err();
            assert_eq!(e.message(), "MORUNA_ARGS is not UTF-8");
        }
    }

    #[test]
    fn the_process_environment_is_read() {
        // This test's process is given no params: none of its tests sets the variables.
        if std::env::var_os(PARAMS).is_none() {
            assert_eq!(params().unwrap(), Map::new());
        }
        if std::env::var_os(ARGS).is_none() {
            assert_eq!(args().unwrap(), Map::new());
        }
    }
}
