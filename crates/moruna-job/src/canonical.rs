//! The canonical form of a document (MH 4.1); its digest, which needs a hash, is `moruna-runtime`'s.
//!
//! Written here rather than left to `serde_json`'s own writer, because whether that writer
//! sorts object keys depends on a cargo feature (`preserve_order`) that any crate in the graph
//! may switch on for everyone. The canonical form is: object keys sorted by their UTF-8 bytes,
//! no whitespace anywhere, strings escaped exactly as `serde_json` escapes them, integers in
//! decimal, and every other number in its shortest round-trip digits, written as Python's
//! `json` writes a float: positional with a `.0` when its exponent is from -4 to 15, else
//! `d.ddde±XX` with at least two exponent digits. So the bytes Moruna writes and the bytes
//! `moruna.RunReport.bytes()` writes of one document are the same.

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
        serde_json::Value::Number(n) => out.push_str(&number(n)),
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

/// An integer in decimal; any other number as [`float`] writes it.
fn number(n: &serde_json::Number) -> String {
    match n.as_f64() {
        Some(f) if !(n.is_i64() || n.is_u64()) => float(f),
        _ => n.to_string(),
    }
}

/// A finite float in its shortest round-trip digits, positioned as Python's `repr` does.
fn float(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    // Rust's float Display rounds a shortest-decimal tie away from zero; Python
    // chooses the even last digit. serde_json's shortest writer chooses the same
    // digits as Python. Only the position of those digits needs normalising.
    let shortest = serde_json::json!(f).to_string();
    let (mantissa, exponent) =
        shortest
            .split_once('e')
            .map_or((shortest.as_str(), 0), |(mantissa, exponent)| {
                (
                    mantissa,
                    exponent
                        .parse::<i64>()
                        .expect("a float's exponent is an integer"),
                )
            });
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let point = mantissa.find('.').unwrap_or(mantissa.len()) as i64;
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let first = digits.len() - digits.trim_start_matches('0').len();
    let exponent = exponent + point - first as i64 - 1;
    let digits = digits[first..].trim_end_matches('0');
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let count = digits.len() as i64;
        if point <= 0 {
            format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
        } else if point >= count {
            format!("{sign}{digits}{}.0", "0".repeat((point - count) as usize))
        } else {
            let (whole, fraction) = digits.split_at(point as usize);
            format!("{sign}{whole}.{fraction}")
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() {
            first.to_string()
        } else {
            format!("{first}.{rest}")
        };
        let mark = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{mantissa}e{mark}{:02}", exponent.abs())
    }
}

/// A JSON string literal, escaped as `serde_json` escapes it.
fn string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
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

    /// A float is written as Python's `json` writes it, so a report's bytes are the same from
    /// either side; an integer stays an integer.
    #[test]
    fn floats_are_written_as_python_writes_them() {
        let floats = [
            2.52e-05,
            1500.0,
            0.0001,
            1e-05,
            1e16,
            1.5e16,
            123456789012345.6,
            -0.0,
            0.1,
            1e22,
            -3.25e-07,
            9999999999999998.0,
            5e-324,
            1.797_693_134_862_315_7e308,
            // Exact halfway decimals: Rust's Display chooses the odd digit.
            f64::from_bits(0x3e60_0000_0000_0000),
            f64::from_bits(0xc2e7_03c1_5756_ea84),
        ];
        let value = serde_json::Value::Array(floats.iter().map(|f| serde_json::json!(f)).collect());
        // What Python's `json.dumps(floats, separators=(",", ":"))` writes.
        assert_eq!(
            to_string(&value),
            "[2.52e-05,1500.0,0.0001,1e-05,1e+16,1.5e+16,123456789012345.6,-0.0,0.1,1e+22,\
             -3.25e-07,9999999999999998.0,5e-324,1.7976931348623157e+308,\
             2.9802322387695312e-08,-202439168538452.12]"
        );
        assert_eq!(
            to_string(&serde_json::json!([3, -7, u64::MAX])),
            "[3,-7,18446744073709551615]"
        );
    }
}
