//! The call a served kind makes (SDD 18 e.5): for each batch of a task whose kind its embedder
//! serves, the bytes the embedder's port is handed, and what it answers with.
//!
//! A request is the magic `MSK1`, the length of a header as a little-endian `u32`, the header,
//! then the batch as one Arrow IPC stream (the schema, one record batch, the end marker). The
//! header is the canonical JSON (MH 4.1) of [`ServedCall`]: the format's version, the kind, the
//! task's settings and the input port the batch came in on. The answer is an Arrow IPC stream of
//! one record batch or more, the task's output for that batch; the port's own refusal is the
//! port's error, never bytes.
//!
//! Here, without Arrow, so an embedder that serves a kind reads the header with the crate it
//! checks its jobs with; the batch is the embedder's to read with the Arrow it has.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The first four bytes of every request.
pub const CALL_MAGIC: [u8; 4] = *b"MSK1";

/// The version of [`ServedCall`] this build writes and reads.
pub const CALL_VERSION: u32 = 1;

/// The most bytes a header may hold: a task's settings are a document's, and a document is not
/// this large.
pub const CALL_HEADER_MAX: usize = 16 << 20;

/// What a request says besides its batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServedCall {
    /// [`CALL_VERSION`].
    pub v: u32,
    /// The kind the task is.
    pub kind: String,
    /// The task's settings, which its kind's settings schema accepted.
    pub settings: Map<String, Value>,
    /// The input port the batch came in on, numbered from 0 in the order the task lists its
    /// inputs.
    pub input: u16,
}

/// Why bytes are not a request.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// They do not start with [`CALL_MAGIC`] and a header length.
    #[error("a served kind's request starts with MSK1 and a header length")]
    Magic,
    /// The header is longer than the bytes, or than [`CALL_HEADER_MAX`].
    #[error("the header declares {declared} bytes and {left} follow")]
    Length {
        /// What the length says.
        declared: usize,
        /// What follows it.
        left: usize,
    },
    /// The header is not a [`ServedCall`].
    #[error("the header is not a served kind's call: {0}")]
    Header(String),
    /// The header is of another version.
    #[error("the request is of version {0}, and this build reads version {CALL_VERSION}")]
    Version(u32),
}

impl ServedCall {
    /// The call of a task of `kind` with `settings`, for a batch on input `input`.
    pub fn new(kind: impl Into<String>, settings: Map<String, Value>, input: u16) -> ServedCall {
        ServedCall {
            v: CALL_VERSION,
            kind: kind.into(),
            settings,
            input,
        }
    }

    /// The header's bytes: the call's canonical JSON.
    pub fn header(&self) -> Vec<u8> {
        let value = serde_json::json!({
            "v": self.v,
            "kind": self.kind,
            "settings": Value::Object(self.settings.clone()),
            "input": self.input,
        });
        crate::canonical::to_string(&value).into_bytes()
    }

    /// The request for `batch`, an Arrow IPC stream.
    pub fn encode(&self, batch: &[u8]) -> Vec<u8> {
        let header = self.header();
        let mut out = Vec::with_capacity(8 + header.len() + batch.len());
        out.extend_from_slice(&CALL_MAGIC);
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(batch);
        out
    }

    /// The call a request makes and its batch's bytes.
    pub fn decode(bytes: &[u8]) -> Result<(ServedCall, &[u8]), CallError> {
        let (magic, rest) = bytes.split_at_checked(4).ok_or(CallError::Magic)?;
        if magic != CALL_MAGIC {
            return Err(CallError::Magic);
        }
        let (len, rest) = rest.split_at_checked(4).ok_or(CallError::Magic)?;
        let declared = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
        if declared > rest.len() || declared > CALL_HEADER_MAX {
            return Err(CallError::Length {
                declared,
                left: rest.len(),
            });
        }
        let (header, batch) = rest.split_at(declared);
        let value: Value =
            serde_json::from_slice(header).map_err(|e| CallError::Header(e.to_string()))?;
        if let Some(v) = value.get("v").and_then(Value::as_u64)
            && v != u64::from(CALL_VERSION)
        {
            return Err(CallError::Version(u32::try_from(v).unwrap_or(u32::MAX)));
        }
        let call: ServedCall =
            serde_json::from_value(value).map_err(|e| CallError::Header(e.to_string()))?;
        Ok((call, batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings() -> Map<String, Value> {
        json!({"prompt": "say \"hi\"", "n": 3})
            .as_object()
            .cloned()
            .unwrap()
    }

    #[test]
    fn a_request_reads_back_as_itself() {
        let call = ServedCall::new("judge", settings(), 2);
        let bytes = call.encode(b"arrow bytes");
        assert_eq!(&bytes[..4], b"MSK1");
        let (back, batch) = ServedCall::decode(&bytes).expect("a request");
        assert_eq!(back, call);
        assert_eq!(batch, b"arrow bytes");
        // The header is the canonical form: the same call is the same bytes.
        assert_eq!(
            String::from_utf8(call.header()).unwrap(),
            r#"{"input":2,"kind":"judge","settings":{"n":3,"prompt":"say \"hi\""},"v":1}"#
        );
    }

    #[test]
    fn what_is_not_a_request_is_refused_by_what_it_is() {
        assert_eq!(ServedCall::decode(b"MSK"), Err(CallError::Magic));
        assert_eq!(ServedCall::decode(b"NOPE\0\0\0\0"), Err(CallError::Magic));
        assert_eq!(ServedCall::decode(b"MSK1\x01"), Err(CallError::Magic));
        assert_eq!(
            ServedCall::decode(b"MSK1\x09\0\0\0{}"),
            Err(CallError::Length {
                declared: 9,
                left: 2
            })
        );
        let mut big = b"MSK1".to_vec();
        big.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            ServedCall::decode(&big),
            Err(CallError::Length { .. })
        ));
        let text = |header: &str| {
            let mut out = b"MSK1".to_vec();
            out.extend_from_slice(&(header.len() as u32).to_le_bytes());
            out.extend_from_slice(header.as_bytes());
            out
        };
        assert!(matches!(
            ServedCall::decode(&text("not json")),
            Err(CallError::Header(_))
        ));
        assert_eq!(
            ServedCall::decode(&text(r#"{"v":2,"kind":"k","settings":{},"input":0}"#)),
            Err(CallError::Version(2))
        );
        assert!(matches!(
            ServedCall::decode(&text(r#"{"v":1,"kind":"k","settings":{},"input":0,"x":1}"#)),
            Err(CallError::Header(_))
        ));
        let cut = &r#"{"v":1,"kind":"k","settings":{},"input":0}"#[..10];
        assert!(matches!(
            ServedCall::decode(&text(cut)),
            Err(CallError::Header(_))
        ));
    }
}
