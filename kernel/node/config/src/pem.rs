//! The armour of PEM text (RFC 7468), checked for its structure only: which blocks the text holds,
//! that each begins and ends with its own label, and that each body is base64. What a block's
//! bytes mean (a certificate, a key) is checked by whoever uses them.

use alloc::{string::String, vec::Vec};

use crate::base64;

/// Why text is not a run of PEM blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PemError {
    /// Something other than blank space stands outside the blocks.
    Outside,
    /// A block begins and does not end with the `END` line of its own label.
    Unfinished,
    /// A block's body is empty or is not base64.
    Body,
}

/// The label of a `-----BEGIN label-----` line (`word` is `BEGIN`) or of an `END` one.
fn boundary<'a>(line: &'a str, word: &str) -> Option<&'a str> {
    let label = line
        .strip_prefix("-----")?
        .strip_prefix(word)?
        .strip_prefix(' ')?
        .strip_suffix("-----")?;
    let written = |b: u8| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b' ';
    (!label.is_empty() && label.bytes().all(written)).then_some(label)
}

/// The labels of the PEM blocks `text` holds, in order. Lines end in LF or CRLF; blank lines may
/// stand between blocks, and nothing else may.
pub fn labels(text: &str) -> Result<Vec<&str>, PemError> {
    let mut labels = Vec::new();
    let mut open: Option<(&str, String)> = None;
    for line in text.lines().map(str::trim) {
        open = match open {
            None if line.is_empty() => None,
            None => {
                let label = boundary(line, "BEGIN").ok_or(PemError::Outside)?;
                Some((label, String::new()))
            }
            Some((label, body)) => match boundary(line, "END") {
                Some(end) if end != label => return Err(PemError::Unfinished),
                Some(_) => {
                    if body.is_empty() || base64::decode(body.as_bytes()).is_none() {
                        return Err(PemError::Body);
                    }
                    labels.push(label);
                    None
                }
                None => Some((label, body + line)),
            },
        };
    }
    match open {
        None => Ok(labels),
        Some(_) => Err(PemError::Unfinished),
    }
}
