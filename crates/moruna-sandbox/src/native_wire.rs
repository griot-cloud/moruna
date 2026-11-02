//! Native PEX transport, version 1: the authoritative sandbox logical commands
//! and replies carried by one checked Arrow record, not a second kernel ABI.
//! All byte offsets in ToChild/FromChild refer to the single actual binary body.
use crate::protocol::{FRAME_MAX_BYTES, FromChild, ToChild};
use moruna_kernel::arrow::{
    array::{Array, BinaryArray},
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
const KEY: &str = "moruna.native_pex";
/// Native record protocol major. Unknown versions fail before calling package code.
pub const VERSION: u32 = 1;
/// An initial real package snapshot, or an existing canonical sandbox command.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "native", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Hash-checked package loading inside the actual child.
    Load {
        /// Fingerprint of the exact package bytes in the record body.
        fingerprint: String,
        /// Canonical invocation arguments.
        args: Option<serde_json::Value>,
        /// Explicit read-only image asset directory, never a host fallback.
        assets: Option<String>,
    },
    /// Existing sandbox command; offsets and lengths refer to this record's body.
    Execute {
        /// Sole sandbox logical control codec.
        command: ToChild,
    },
}
/// One correlated real command.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Protocol major.
    pub v: u32,
    /// Strictly increasing nonzero request sequence.
    pub seq: u64,
    /// The canonical logical command.
    pub command: Command,
}
/// An actual command-thread output write, never a platform event.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Log {
    /// Original stream: stdout=1, stderr=2, structured=3.
    pub stream: u8,
    /// Structured level from trace=0 through critical=5.
    pub level: Option<u8>,
    /// Exact bytes written.
    pub bytes: Vec<u8>,
}
/// One actual child reply, including its real optional state-footprint reading.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// Protocol major.
    pub v: u32,
    /// Exact original request sequence.
    pub seq: u64,
    /// Sole sandbox logical reply codec.
    pub reply: FromChild,
    /// Actual package state's footprint, absent only when unsupported.
    pub state_bytes: Option<u64>,
    /// Actual bounded command-thread output.
    pub logs: Vec<Log>,
}
fn encode<T: Serialize>(header: &T, body: &[u8]) -> Result<RecordBatch, String> {
    if body.len() > i32::MAX as usize {
        return Err("native PEX binary offset domain exceeded".into());
    }
    let text = serde_json::to_string(header).map_err(|e| e.to_string())?;
    if text.len() > FRAME_MAX_BYTES {
        return Err("native PEX header exceeds canonical bound".into());
    }
    let mut metadata = HashMap::new();
    metadata.insert(KEY.into(), text);
    let schema = std::sync::Arc::new(Schema::new_with_metadata(
        vec![Field::new("body", DataType::Binary, false)],
        metadata,
    ));
    RecordBatch::try_new(
        schema,
        vec![std::sync::Arc::new(BinaryArray::from(vec![body]))],
    )
    .map_err(|e| e.to_string())
}
fn decode<T: serde::de::DeserializeOwned>(batch: &RecordBatch) -> Result<(T, Vec<u8>), String> {
    let schema = batch.schema();
    if batch.num_rows() != 1
        || batch.num_columns() != 1
        || schema.fields()[0].name() != "body"
        || schema.fields()[0].is_nullable()
        || schema.metadata().len() != 1
    {
        return Err("native PEX record geometry/schema differs".into());
    }
    let header = schema
        .metadata()
        .get(KEY)
        .ok_or("native PEX header missing")?;
    if header.len() > FRAME_MAX_BYTES {
        return Err("native PEX header exceeds canonical bound".into());
    }
    let body = batch
        .column(0)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .ok_or("native PEX body is not binary")?;
    if body.is_null(0) {
        return Err("native PEX body is null".into());
    }
    Ok((
        serde_json::from_str(header).map_err(|e| e.to_string())?,
        body.value(0).to_vec(),
    ))
}
fn request_body(header: &Request, body: &[u8]) -> Result<(), String> {
    match &header.command {
        Command::Load { .. } if body.is_empty() => Err("empty package snapshot".into()),
        Command::Load { .. } => Ok(()),
        Command::Execute {
            command:
                ToChild::Call {
                    lease,
                    offset,
                    len,
                    out_offset,
                    out_len,
                },
        } if *lease == header.seq
            && *offset == 0
            && *len == body.len() as u64
            && *out_offset == 0
            && *out_len > 0 =>
        {
            Ok(())
        }
        Command::Execute {
            command: ToChild::Open { offset, len },
        } if *offset == 0 && *len == body.len() as u64 => Ok(()),
        Command::Execute {
            command: ToChild::Checkpoint { offset, len },
        } if *offset == 0 && *len > 0 && body.is_empty() => Ok(()),
        Command::Execute {
            command: ToChild::Release | ToChild::Stop,
        } if body.is_empty() => Ok(()),
        _ => Err("native PEX control range differs from actual record body".into()),
    }
}
fn response_body(header: &Response, body: &[u8]) -> Result<(), String> {
    let mut used = 0usize;
    for log in &header.logs {
        used = used
            .checked_add(log.bytes.len().saturating_add(64))
            .ok_or("native output size overflow")?;
        if used > 8192
            || !(1..=3).contains(&log.stream)
            || log.level.is_some_and(|l| l > 5)
            || (log.stream != 3 && log.level.is_some())
            || (log.stream == 3 && std::str::from_utf8(&log.bytes).is_err())
        {
            return Err("native output geometry/stream/level/budget".into());
        }
    }
    match &header.reply {
        FromChild::Done {
            offset,
            len,
            released,
            ..
        }
        | FromChild::State {
            offset,
            len,
            released,
        } if *offset == 0 && *len == body.len() as u64 && released.is_empty() => Ok(()),
        FromChild::Error { released, .. } | FromChild::Released { released }
            if body.is_empty() && released.is_empty() =>
        {
            Ok(())
        }
        FromChild::Ready | FromChild::Opened | FromChild::Failed { .. } if body.is_empty() => {
            Ok(())
        }
        _ => Err("native PEX reply range/lease claim differs from actual body".into()),
    }
}
/// Encode one real command into the existing checked record format.
pub fn request(header: &Request, body: &[u8]) -> Result<RecordBatch, String> {
    if header.v != VERSION || header.seq == 0 {
        return Err("native PEX request version/sequence".into());
    }
    request_body(header, body)?;
    encode(header, body)
}
/// Strictly decode a command using the sole shared codec.
pub fn read_request(batch: &RecordBatch) -> Result<(Request, Vec<u8>), String> {
    let (header, body): (Request, Vec<u8>) = decode(batch)?;
    if header.v != VERSION || header.seq == 0 {
        return Err("native PEX request version/sequence".into());
    }
    request_body(&header, &body)?;
    Ok((header, body))
}
/// Encode an actual child reply.
pub fn response(header: &Response, body: &[u8]) -> Result<RecordBatch, String> {
    if header.v != VERSION || header.seq == 0 {
        return Err("native PEX reply version/sequence".into());
    }
    response_body(header, body)?;
    encode(header, body)
}
/// Strictly decode an actual reply using the sole shared codec.
pub fn read_response(batch: &RecordBatch) -> Result<(Response, Vec<u8>), String> {
    let (header, body): (Response, Vec<u8>) = decode(batch)?;
    if header.v != VERSION || header.seq == 0 {
        return Err("native PEX reply version/sequence".into());
    }
    response_body(&header, &body)?;
    Ok((header, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_control_body_and_state_counts_roundtrip_without_lease_invention() {
        let control = Request {
            v: VERSION,
            seq: 7,
            command: Command::Execute {
                command: ToChild::Open { offset: 0, len: 3 },
            },
        };
        let batch = request(&control, b"abc").unwrap();
        let (decoded, bytes) = read_request(&batch).unwrap();
        assert_eq!(decoded.seq, 7);
        assert_eq!(bytes, b"abc");
        assert!(matches!(
            decoded.command,
            Command::Execute {
                command: ToChild::Open { offset: 0, len: 3 }
            }
        ));
        let response = Response {
            v: VERSION,
            seq: 7,
            reply: FromChild::State {
                offset: 0,
                len: 3,
                released: Vec::new(),
            },
            state_bytes: Some(4096),
            logs: Vec::new(),
        };
        let batch = super::response(&response, b"xyz").unwrap();
        let (decoded, bytes) = read_response(&batch).unwrap();
        assert_eq!(decoded.seq, 7);
        assert_eq!(decoded.state_bytes, Some(4096));
        assert_eq!(bytes, b"xyz");
        let forged = Response {
            reply: FromChild::State {
                offset: 0,
                len: 3,
                released: vec![99],
            },
            ..response
        };
        assert!(super::response(&forged, b"xyz").is_err());
    }
    #[test]
    fn unknown_version_fields_bad_geometry_and_false_byte_ranges_refuse() {
        let mut header = Request {
            v: VERSION,
            seq: 1,
            command: Command::Load {
                fingerprint: "actual-package-hash".into(),
                args: None,
                assets: None,
            },
        };
        let valid = request(&header, b"package snapshot").unwrap();
        header.v = VERSION + 1;
        assert!(request(&header, b"package snapshot").is_err());
        let mut metadata = valid.schema().metadata().clone();
        let mut value: serde_json::Value = serde_json::from_str(&metadata[KEY]).unwrap();
        value["undeclared"] = serde_json::json!(true);
        metadata.insert(KEY.into(), value.to_string());
        let changed = RecordBatch::try_new(
            std::sync::Arc::new(Schema::new_with_metadata(
                valid.schema().fields().clone(),
                metadata,
            )),
            valid.columns().to_vec(),
        )
        .unwrap();
        assert!(read_request(&changed).is_err());
        let invalid = Request {
            v: VERSION,
            seq: 2,
            command: Command::Execute {
                command: ToChild::Open { offset: 0, len: 9 },
            },
        };
        assert!(request(&invalid, b"abc").is_err());
        let mut call = Request {
            v: VERSION,
            seq: 3,
            command: Command::Execute {
                command: ToChild::Call {
                    lease: 3,
                    offset: 0,
                    len: 3,
                    out_offset: 0,
                    out_len: 4096,
                },
            },
        };
        assert!(read_request(&request(&call, b"abc").unwrap()).is_ok());
        if let Command::Execute {
            command: ToChild::Call { lease, .. },
        } = &mut call.command
        {
            *lease = 2;
        }
        assert!(request(&call, b"abc").is_err());
        let header = Response {
            v: VERSION,
            seq: 2,
            reply: FromChild::Done {
                offset: 0,
                len: 99,
                alloc: crate::protocol::AllocDoc::default(),
                released: Vec::new(),
            },
            state_bytes: None,
            logs: Vec::new(),
        };
        assert!(super::response(&header, b"abc").is_err());
    }
}
