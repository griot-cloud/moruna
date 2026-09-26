//! The host protocol's messages (MH 4.3): newline-delimited JSON, one object per line,
//! `type` first. Moruna sends `hello`, `heartbeat`, `limits_changed`, `report` and `exit`; it
//! reads `spec`, `cancel` and `checkpoint`, and nothing else. No message a peer can send
//! enlarges a run.

use moruna_kernel::Limits;
use serde::{Deserialize, Serialize};

/// The limits as the protocol carries them (MH 4.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LimitsMsg {
    /// The host memory ceiling in bytes.
    pub memory_ceiling: u64,
    /// Where the host would kill the process, when known.
    pub memory_kill: Option<u64>,
    /// CPUs, in cores.
    pub cpu_quota: f64,
    /// `Cgroup`, `Os` or `Explicit`.
    pub source: String,
}

impl LimitsMsg {
    /// The protocol's view of discovered limits.
    pub fn of(limits: &Limits) -> LimitsMsg {
        LimitsMsg {
            memory_ceiling: limits.memory_ceiling,
            memory_kill: limits.memory_kill,
            cpu_quota: limits.cpu_quota,
            source: format!("{:?}", limits.source),
        }
    }
}

/// One heartbeat (MH 4.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Milliseconds since the process began the run.
    pub t_ms: u64,
    /// The sink's commit watermark.
    pub committed_seq: Option<u64>,
    /// Rows the last kernel stage produced so far.
    pub rows_out: u64,
    /// Bytes the last kernel stage produced so far.
    pub bytes_out: u64,
    /// Workers allowed to take tasks.
    pub active_workers: u16,
    /// The memory ceiling in force.
    pub ceiling_bytes: Option<u64>,
    /// The process's anonymous memory.
    pub anon_bytes: Option<u64>,
    /// The controller's latest classification.
    pub bottleneck: Option<String>,
    /// Bytes on the disk tier.
    pub staging_bytes: u64,
}

impl Heartbeat {
    /// A heartbeat from what the observer knows, at `t_ms`.
    pub fn of(progress: &crate::observe::Progress, t_ms: u64) -> Heartbeat {
        Heartbeat {
            t_ms,
            committed_seq: progress.committed_seq,
            rows_out: progress.rows_out,
            bytes_out: progress.bytes_out,
            active_workers: progress.active_workers,
            ceiling_bytes: progress.ceiling_bytes,
            anon_bytes: progress.anon_bytes,
            bottleneck: progress.bottleneck.clone(),
            staging_bytes: progress.staging_bytes,
        }
    }
}

/// A message from Moruna to its peer (MH 4.3).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outbound {
    /// First, on connect.
    Hello {
        /// `moruna --version`.
        moruna_version: String,
        /// The job document's digest; `null` in serve mode, where no document has arrived yet.
        spec_digest: Option<String>,
        /// The limits as discovered; `null` when discovery failed.
        limits: Option<LimitsMsg>,
    },
    /// At least every 2 s while the process lives.
    Heartbeat(Heartbeat),
    /// When discovery observes a change (MH 4.4; sent by the elastic budget, F8.2).
    LimitsChanged {
        /// Before.
        old: LimitsMsg,
        /// After.
        new: LimitsMsg,
        /// `memory` or `cpu`.
        reason: String,
    },
    /// The full run report, before `exit`, whenever the run produced one.
    Report {
        /// `RunReport` as JSON (04 d.1).
        report: serde_json::Value,
    },
    /// Last.
    Exit {
        /// The process's exit code (MH 4.2).
        code: i32,
        /// Why, when the code is not 0.
        diagnostic: Option<String>,
    },
}

impl Outbound {
    /// The message as one line, newline included.
    pub fn line(&self) -> String {
        let mut text = serde_json::to_string(self)
            .unwrap_or_else(|e| format!("{{\"type\":\"exit\",\"code\":1,\"diagnostic\":\"{e}\"}}"));
        text.push('\n');
        text
    }
}

/// A message from the peer to Moruna (MH 4.3).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Inbound {
    /// The job document, in serve mode; once.
    Spec {
        /// The document.
        spec: serde_json::Value,
    },
    /// Cancel the run: the manifest is written and the process exits 130.
    Cancel,
    /// Write the manifest now.
    Checkpoint,
}

impl Inbound {
    /// Read one line. An unreadable line is an `Err` with the reason, which the session notes
    /// and otherwise ignores.
    pub fn parse(line: &str) -> Result<Inbound, String> {
        serde_json::from_str(line.trim_end()).map_err(|e| e.to_string())
    }
}

/// The longest line read before a `spec` has arrived, which is the document itself (MH 4.3).
pub const MAX_SPEC_LINE: usize = 16 << 20;
/// The longest line read once the run is going: a `cancel` or a `checkpoint` is a few bytes.
pub const MAX_CONTROL_LINE: usize = 64 << 10;

/// Read one line of at most `max` bytes. `Ok(None)` at the end of the stream; a longer line is
/// an error, so a peer cannot make the process hold an unbounded buffer.
pub fn read_line(reader: &mut dyn std::io::BufRead, max: usize) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&buf).into_owned())
            });
        }
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (at + 1, true),
            None => (available.len(), false),
        };
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if buf.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("a line longer than {max} bytes"),
            ));
        }
        if done {
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MH 4.3, byte for byte: `type` first, then the fields in the order the table lists them,
    /// nulls written out.
    #[test]
    fn ho_t15_protocol_is_byte_exact() {
        let hello = Outbound::Hello {
            moruna_version: "0.1.1".into(),
            spec_digest: None,
            limits: Some(LimitsMsg {
                memory_ceiling: 1024,
                memory_kill: None,
                cpu_quota: 2.0,
                source: "Explicit".into(),
            }),
        };
        assert_eq!(
            hello.line(),
            "{\"type\":\"hello\",\"moruna_version\":\"0.1.1\",\"spec_digest\":null,\"limits\":\
             {\"memory_ceiling\":1024,\"memory_kill\":null,\"cpu_quota\":2.0,\"source\":\"Explicit\"}}\n"
        );
        let beat = Outbound::Heartbeat(Heartbeat {
            t_ms: 1000,
            committed_seq: Some(3),
            rows_out: 10,
            bytes_out: 80,
            active_workers: 2,
            ceiling_bytes: Some(1024),
            anon_bytes: Some(512),
            bottleneck: Some("Compute".into()),
            staging_bytes: 0,
        });
        assert_eq!(
            beat.line(),
            "{\"type\":\"heartbeat\",\"t_ms\":1000,\"committed_seq\":3,\"rows_out\":10,\
             \"bytes_out\":80,\"active_workers\":2,\"ceiling_bytes\":1024,\"anon_bytes\":512,\
             \"bottleneck\":\"Compute\",\"staging_bytes\":0}\n"
        );
        let limits = LimitsMsg {
            memory_ceiling: 1,
            memory_kill: Some(2),
            cpu_quota: 0.5,
            source: "Os".into(),
        };
        let changed = Outbound::LimitsChanged {
            old: limits.clone(),
            new: limits,
            reason: "memory".into(),
        };
        assert!(
            changed
                .line()
                .starts_with("{\"type\":\"limits_changed\",\"old\":{\"memory_ceiling\":1,")
        );
        assert!(changed.line().ends_with(",\"reason\":\"memory\"}\n"));
        let report = Outbound::Report {
            report: serde_json::json!({"run_id": "ab"}),
        };
        assert_eq!(
            report.line(),
            "{\"type\":\"report\",\"report\":{\"run_id\":\"ab\"}}\n"
        );
        let exit = Outbound::Exit {
            code: 130,
            diagnostic: Some("cancelled".into()),
        };
        assert_eq!(
            exit.line(),
            "{\"type\":\"exit\",\"code\":130,\"diagnostic\":\"cancelled\"}\n"
        );
    }

    /// The three inbound messages and nothing else; an unknown type is refused.
    #[test]
    fn inbound_is_three_messages() {
        assert_eq!(
            Inbound::parse("{\"type\":\"cancel\"}\n"),
            Ok(Inbound::Cancel)
        );
        assert_eq!(
            Inbound::parse("{\"type\":\"checkpoint\"}"),
            Ok(Inbound::Checkpoint)
        );
        assert_eq!(
            Inbound::parse("{\"type\":\"spec\",\"spec\":{\"a\":1}}"),
            Ok(Inbound::Spec {
                spec: serde_json::json!({"a": 1})
            })
        );
        assert!(Inbound::parse("{\"type\":\"resize\",\"memory\":1}").is_err());
        // A field beside `cancel` or `checkpoint` changes nothing: the message is what it says.
        assert_eq!(
            Inbound::parse("{\"type\":\"cancel\",\"budget\":1}"),
            Ok(Inbound::Cancel)
        );
        assert!(Inbound::parse("{\"type\":\"spec\",\"spec\":{},\"budget\":1}").is_err());
        assert!(Inbound::parse("not json").is_err());
    }

    #[test]
    fn lines_are_bounded() {
        let mut short = std::io::Cursor::new(b"one\ntwo".to_vec());
        assert_eq!(
            read_line(&mut short, 16).expect("one"),
            Some("one\n".into())
        );
        assert_eq!(read_line(&mut short, 16).expect("two"), Some("two".into()));
        assert_eq!(read_line(&mut short, 16).expect("end"), None);
        let mut long = std::io::Cursor::new(vec![b'x'; 100]);
        assert!(read_line(&mut long, 16).is_err());
    }
}
