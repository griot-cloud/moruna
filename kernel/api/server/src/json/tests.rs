//! JSON text made one line, and its secrets taken out.

use super::*;
use alloc::{format, string::ToString, vec};
use serde_json::Value;

#[test]
fn compact_takes_out_the_space_between_tokens_only() {
    let written = "{\n  \"a b\" : [ 1,\t2 ],\r\n  \"c\": \"x \\\" y\\\\\",\n \"d\":{ }\n}\n";
    let line = compact(written);
    assert_eq!(line, r#"{"a b":[1,2],"c":"x \" y\\","d":{}}"#);
    let (before, after): (Value, Value) = (
        serde_json::from_str(written).unwrap(),
        serde_json::from_str(&line).unwrap(),
    );
    assert_eq!(before, after);
    assert_eq!(compact("  7 "), "7");
    assert_eq!(
        compact(r#""a\"#),
        r#""a\"#,
        "a string cut short stays as it is"
    );
    assert!(is_object(" \n{\"a\":1}") && !is_object("[{}]") && !is_object(""));
}

fn secrets(named: &[&str]) -> Secrets {
    Secrets::new(&named.iter().map(|s| s.to_string()).collect::<vec::Vec<_>>()).unwrap()
}

#[test]
fn secrets_are_replaced_inside_strings_and_numbers_and_nowhere_else() {
    let secrets = secrets(&["hunter2", "4242", "a\"b\\c", "tr"]);
    let line = r#"{"key hunter2":"x hunter2 y hunter2","n":14242,"m":-4.2,"q":"a\"b\\c!","t":true,"s":"tr\\"}"#;
    let scrubbed = secrets.scrub(line.to_string());
    assert_eq!(
        scrubbed,
        r#"{"key [redacted]":"x [redacted] y [redacted]","n":"[redacted]","m":-4.2,"q":"[redacted]!","t":true,"s":"[redacted]\\"}"#
    );
    let _: Value = serde_json::from_str(&scrubbed).expect("still JSON");
    // What holds no secret comes back as it was.
    let clean = r#"{"a":"b","n":1}"#;
    assert_eq!(secrets.scrub(clean.to_string()), clean);
    assert_eq!(Secrets::default().scrub(line.to_string()), line);
    // A match never begins inside an escape: the secret `"`, written `\"`, is not the second
    // half of `\\` and the quote that ends the string.
    let quote = secrets_of_one("\"");
    assert_eq!(
        quote.scrub(r#"{"a":"b\\","c":"x\"y"}"#.to_string()),
        r#"{"a":"b\\","c":"x[redacted]y"}"#
    );
    // Characters JSON escapes, and ones it writes as they are.
    let odd = secrets_of_one("pa\tss\u{e9}");
    let written = serde_json::to_string("is pa\tss\u{e9}.").unwrap();
    assert_eq!(odd.scrub(written), r#""is [redacted].""#);
}

fn secrets_of_one(secret: &str) -> Secrets {
    secrets(&[secret])
}

#[test]
fn an_empty_secret_and_too_many_are_refused() {
    assert!(Secrets::new(&["".to_string()]).is_err());
    let many = vec!["secret".to_string(); SECRETS_LIMIT + 1];
    assert!(Secrets::new(&many).is_err());
    assert!(Secrets::new(&many[..SECRETS_LIMIT]).is_ok());
    assert_eq!(format!("{:?}", secrets(&["a", "b"])), "Secrets(2)");
}
