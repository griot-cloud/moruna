//! What ends a task: a function's error, and the SDK's own.

use std::fmt;

use arrow_schema::ArrowError;

use crate::code;

/// Why a function did not answer a batch. Returned by a function (or passed on from
/// [`crate::Emit::batch`]), it ends the task: the message goes to the task's log, after the
/// task's name, and the task's exit code says what kind of failure it was ([`crate::code`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionError {
    message: String,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Refused,
    WindowTooSmall,
    Rings,
}

impl FunctionError {
    /// A function's refusal of a batch, with the sentence that says why.
    pub fn new(message: impl Into<String>) -> FunctionError {
        FunctionError {
            message: message.into(),
            kind: Kind::Refused,
        }
    }

    /// The sentence the task's log gets.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// One row of an output batch takes `need` bytes as a record and a window holds `have`.
    pub(crate) fn window_too_small(need: u64, have: u64) -> FunctionError {
        FunctionError {
            message: format!("the output record takes {need} bytes and the window has {have}"),
            kind: Kind::WindowTooSmall,
        }
    }

    /// The rings broke.
    pub(crate) fn rings(what: impl fmt::Display) -> FunctionError {
        FunctionError {
            message: format!("the rings: {what}"),
            kind: Kind::Rings,
        }
    }

    /// The exit code a task that ends with this error says.
    pub(crate) fn code(&self) -> i64 {
        match self.kind {
            Kind::Refused => code::REFUSED,
            Kind::WindowTooSmall => code::WINDOW_SMALL,
            Kind::Rings => code::RINGS,
        }
    }
}

impl fmt::Display for FunctionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FunctionError {}

impl From<ArrowError> for FunctionError {
    fn from(e: ArrowError) -> FunctionError {
        FunctionError::new(e.to_string())
    }
}

impl From<String> for FunctionError {
    fn from(message: String) -> FunctionError {
        FunctionError::new(message)
    }
}

impl From<&str> for FunctionError {
    fn from(message: &str) -> FunctionError {
        FunctionError::new(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_says_its_code_and_its_message() {
        let e = FunctionError::new("no column x");
        assert_eq!((e.message(), e.code()), ("no column x", code::REFUSED));
        assert_eq!(e.to_string(), "no column x");
        let e = FunctionError::window_too_small(9000, 4096);
        assert_eq!(e.code(), code::WINDOW_SMALL);
        assert_eq!(
            e.message(),
            "the output record takes 9000 bytes and the window has 4096"
        );
        let e = FunctionError::rings("an input past the region");
        assert_eq!(
            (e.message(), e.code()),
            ("the rings: an input past the region", code::RINGS)
        );
    }

    #[test]
    fn strings_and_arrow_errors_are_refusals() {
        let a: FunctionError = ArrowError::ComputeError(String::from("overflow")).into();
        assert_eq!(a.code(), code::REFUSED);
        assert!(a.message().contains("overflow"), "{a}");
        let s: FunctionError = String::from("owned").into();
        let r: FunctionError = "borrowed".into();
        assert_eq!((s.message(), r.message()), ("owned", "borrowed"));
        let boxed: Box<dyn std::error::Error> = Box::new(r);
        assert_eq!(boxed.to_string(), "borrowed");
    }
}
