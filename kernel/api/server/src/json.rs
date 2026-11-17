//! JSON as text, read where it stands: made one line, and its secrets taken out (system design,
//! section 3B). The texts are whole JSON values as `serde_json` writes them, or as a client
//! wrote them; nothing here builds a tree.

use alloc::{string::String, vec::Vec};

/// What stands where a secret was.
pub(crate) const REDACTED: &str = "[redacted]";

/// The most secrets one job names.
pub(crate) const SECRETS_LIMIT: usize = 32;

/// Whether a byte is the white space JSON allows between its tokens.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

/// `text`, a JSON value, with the white space between its tokens taken out: one line, since a
/// JSON string holds no raw line break.
pub(crate) fn compact(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if in_string {
            out.push(byte);
            if byte == b'\\' {
                if let Some(next) = bytes.get(at + 1) {
                    out.push(*next);
                }
                at += 1;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if !is_space(byte) {
            in_string = byte == b'"';
            out.push(byte);
        }
        at += 1;
    }
    // Only ASCII white space outside strings was left out, so the text is still UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// Whether `text` is a JSON object, by how it begins.
pub(crate) fn is_object(text: &str) -> bool {
    text.bytes().find(|b| !is_space(*b)) == Some(b'{')
}

/// The strings a job says are secret, each as JSON writes it inside a string. Their bytes are
/// wiped when the job has ended and this is dropped.
#[derive(Default)]
pub(crate) struct Secrets {
    forms: Vec<Vec<u8>>,
}

impl Secrets {
    /// The secrets `named`, or why they are refused: an empty one, or more than
    /// [`SECRETS_LIMIT`].
    pub(crate) fn new(named: &[String]) -> Result<Self, &'static str> {
        if named.len() > SECRETS_LIMIT {
            return Err("a job names at most 32 secrets");
        }
        let mut forms = Vec::with_capacity(named.len());
        for secret in named {
            if secret.is_empty() {
                return Err("a secret is not empty");
            }
            // A string as JSON writes it, without its quotes.
            let written = serde_json::to_vec(secret).map_err(|_| "a secret cannot be written")?;
            forms.push(written[1..written.len() - 1].to_vec());
        }
        Ok(Self { forms })
    }

    /// The secret that begins `bytes`, by its length.
    fn begins(&self, bytes: &[u8]) -> Option<usize> {
        self.forms
            .iter()
            .find(|form| bytes.starts_with(form))
            .map(Vec::len)
    }

    /// Whether `bytes` holds a secret anywhere.
    fn holds(&self, bytes: &[u8]) -> bool {
        self.forms.iter().any(|form| {
            bytes
                .windows(form.len())
                .any(|window| window == form.as_slice())
        })
    }

    /// `json`, a JSON value as text, with every secret inside a string replaced by
    /// `[redacted]`, and every number that holds one by the string `"[redacted]"`.
    pub(crate) fn scrub(&self, json: String) -> String {
        if self.forms.is_empty() || !self.holds(json.as_bytes()) {
            return json;
        }
        let bytes = json.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut in_string = false;
        let mut at = 0;
        while at < bytes.len() {
            let byte = bytes[at];
            if in_string {
                // Each step begins at a character or at an escape, as each secret's form does,
                // so a match never begins inside an escape.
                if let Some(length) = self.begins(&bytes[at..]) {
                    out.extend_from_slice(REDACTED.as_bytes());
                    at += length;
                } else if byte == b'\\' {
                    out.extend_from_slice(&bytes[at..(at + 2).min(bytes.len())]);
                    at += 2;
                } else {
                    in_string = byte != b'"';
                    out.push(byte);
                    at += 1;
                }
            } else if byte == b'-' || byte.is_ascii_digit() {
                let length = bytes[at..]
                    .iter()
                    .take_while(|b| matches!(b, b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E'))
                    .count();
                let number = &bytes[at..at + length];
                if self.holds(number) {
                    out.push(b'"');
                    out.extend_from_slice(REDACTED.as_bytes());
                    out.push(b'"');
                } else {
                    out.extend_from_slice(number);
                }
                at += length;
            } else {
                in_string = byte == b'"';
                out.push(byte);
                at += 1;
            }
        }
        // Whole secrets, which are whole characters, were replaced by ASCII.
        String::from_utf8(out).unwrap_or_default()
    }
}

impl Drop for Secrets {
    fn drop(&mut self) {
        for form in &mut self.forms {
            form.fill(0);
        }
    }
}

impl core::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Secrets({})", self.forms.len())
    }
}

#[cfg(test)]
mod tests;
