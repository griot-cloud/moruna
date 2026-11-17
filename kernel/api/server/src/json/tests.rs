//! JSON text made one line, and its secrets taken out.

use super::*;
use alloc::{format, string::ToString};
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
    let map: alloc::collections::BTreeMap<String, String> = named
        .iter()
        .enumerate()
        .map(|(n, s)| (format!("s{n}"), s.to_string()))
        .collect();
    Secrets::new(map.iter()).unwrap()
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
fn an_empty_secret_a_bad_name_and_too_many_are_refused() {
    let one = |name: &str, value: &str| {
        let map = alloc::collections::BTreeMap::from([(name.to_string(), value.to_string())]);
        Secrets::new(map.iter()).map(|_| ())
    };
    assert!(one("key", "").is_err());
    for name in ["", "a b", "k/1", "-key", ".key", &"x".repeat(65)] {
        assert!(one(name, "v").is_err(), "{name}");
    }
    for name in ["s3.secret_access_key", "A-1:b", &"x".repeat(64)] {
        assert!(one(name, "v").is_ok(), "{name}");
    }
    let many: alloc::collections::BTreeMap<String, String> = (0..=SECRETS_LIMIT)
        .map(|n| (format!("k{n}"), "v".to_string()))
        .collect();
    assert!(Secrets::new(many.iter()).is_err());
    assert!(Secrets::new(many.iter().take(SECRETS_LIMIT)).is_ok());
    assert_eq!(format!("{:?}", secrets(&["a", "b"])), "Secrets(2)");
}
