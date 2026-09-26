//! The canonical form of a document and its digest (MH 4.1).
//!
//! Written here rather than left to `serde_json`'s own writer, because whether that writer
//! sorts object keys depends on a cargo feature (`preserve_order`) that any crate in the graph
//! may switch on for everyone. The canonical form is: object keys sorted by their UTF-8 bytes,
//! no whitespace anywhere, strings escaped exactly as `serde_json` escapes them, integers in
//! decimal, other numbers in `serde_json`'s shortest round-trip form.

use sha2::{Digest, Sha256};

/// The canonical text of `value` (MH 4.1).
pub fn to_string(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        serde_json::Value::String(s) => out.push_str(&string(s)),
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&string(key));
                out.push(':');
                if let Some(item) = map.get(key) {
                    write(item, out);
                }
            }
            out.push('}');
        }
    }
}

/// A JSON string literal, escaped as `serde_json` escapes it.
fn string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// `sha256:` followed by the 64 lowercase hex characters of the SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys sort by bytes at every depth, and nothing is added between tokens (MH 4.1).
    #[test]
    fn keys_sort_and_nothing_is_added() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{ "b": [1, 2.5, "x\"y"], "a": {"z": null, "B": true, "a": false}, "é": -3 }"#,
        )
        .expect("json");
        assert_eq!(
            to_string(&value),
            r#"{"a":{"B":true,"a":false,"z":null},"b":[1,2.5,"x\"y"],"é":-3}"#
        );
    }

    /// The digest of the empty string is SHA-256's published one.
    #[test]
    fn sha256_is_sha256() {
        assert_eq!(
            sha256_hex(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
