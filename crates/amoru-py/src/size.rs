//! Size strings (`"6GiB"`).
//!
//! f.3 sends every size argument through discovery's parser (03 f.3). That parser is
//! `amoru_discovery::env::parse_size`, which is `pub(crate)`: the surface cannot call it, and an
//! executor does not widen another component's visibility on its own branch. The rule it
//! implements is reproduced here, byte for byte against 03 f.3's own test vectors (`8GiB`, `8GB`,
//! `8589934592`, `8 GiB` rejected, `8gib` accepted, `1.5GiB`), and the escalation asks for the
//! parser to be exported so this module can be deleted.

use amoru_kernel::{AmoruError, Result};

const KIB: u64 = 1024;
const KB: u64 = 1000;

/// Parse a byte count: a bare integer, or a number with a binary (`KiB`, `MiB`, `GiB`, `TiB`) or
/// decimal (`KB`, `MB`, `GB`, `TB`) unit. Case is ignored; an interior space is an error.
pub fn parse_size(name: &'static str, s: &str) -> Result<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed != s {
        return Err(config(name, format!("`{s}` is not a size")));
    }
    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(digits_end);
    let value: f64 = number
        .parse()
        .map_err(|_| config(name, format!("`{s}` is not a size")))?;
    if value < 0.0 {
        return Err(config(name, format!("`{s}` is negative")));
    }
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kib" => KIB,
        "mib" => KIB * KIB,
        "gib" => KIB * KIB * KIB,
        "tib" => KIB * KIB * KIB * KIB,
        "kb" => KB,
        "mb" => KB * KB,
        "gb" => KB * KB * KB,
        "tb" => KB * KB * KB * KB,
        other => {
            return Err(config(
                name,
                format!("unknown unit `{other}` (use KiB, MiB, GiB, TiB, KB, MB, GB or TB)"),
            ));
        }
    };
    let bytes = value * multiplier as f64;
    if !bytes.is_finite() || bytes >= u64::MAX as f64 {
        return Err(config(name, format!("`{s}` does not fit in 64 bits")));
    }
    Ok(bytes as u64)
}

fn config(name: &'static str, msg: String) -> AmoruError {
    AmoruError::Config { name, msg }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 03 f.3's own vectors, so this copy cannot drift from the parser it stands in for.
    #[test]
    fn size_strings() {
        assert_eq!(parse_size("budget", "8GiB").expect("8GiB"), 8 << 30);
        assert_eq!(parse_size("budget", "8gib").expect("8gib"), 8 << 30);
        assert_eq!(parse_size("budget", "8GB").expect("8GB"), 8 * 1_000_000_000);
        assert_eq!(
            parse_size("budget", "8589934592").expect("bare"),
            8_589_934_592
        );
        assert_eq!(
            parse_size("budget", "1.5GiB").expect("fraction"),
            (1.5 * (1u64 << 30) as f64) as u64
        );
        assert_eq!(parse_size("budget", "512B").expect("bytes"), 512);
        for bad in ["8 GiB", "eight", "8GiBx", "", "8Gi", "-1", " 8GiB"] {
            assert!(
                parse_size("budget", bad).is_err(),
                "{bad} should be refused"
            );
        }
        assert!(parse_size("budget", "999999999999999999999999GiB").is_err());
    }
}
