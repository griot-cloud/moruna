//! A port a run calls (MH 4.10), as a document or a declaration names one: where the name goes,
//! and how long one call may wait for its answer.
//!
//! Here, beside the declaration, because two things name ports: a `package` entry's `ports`
//! (SDD 16 d.4) and a kind its embedder serves (`Declaration::served`, SDD 18 e.4). Both read the
//! same document and are refused by the same rules; the runtime makes the route.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A port's deadline when it names none: half an hour.
pub const PORT_DEADLINE_MS: u64 = 30 * 60 * 1000;

/// The longest deadline a port may name: a day.
pub const PORT_DEADLINE_MAX_MS: u64 = 24 * 60 * 60 * 1000;

/// One port a stage may call (`ports.<name>`, MH 4.10): exactly one of `host` and `guest`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PortDoc {
    /// A port the host serves, by the name the host knows it by: the stage's calls go to the
    /// host protocol's peer as `port_call` (MH 4.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// A port answered inside the machine, by the handler the program running the document
    /// registered under this name (`Embedding::guest_port`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<String>,
    /// How long one call may wait for its answer; absent: half an hour
    /// (`moruna_job::PORT_DEADLINE_MS`), at most a day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

/// Where a port's calls go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place<'a> {
    /// To the host protocol's peer, under this name.
    Host(&'a str),
    /// To the handler the program running the document registered under this name.
    Guest(&'a str),
}

impl Place<'_> {
    /// `host` or `guest`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Place::Host(_) => "host",
            Place::Guest(_) => "guest",
        }
    }

    /// The name the place knows the port by.
    pub fn name(&self) -> &str {
        match self {
            Place::Host(name) | Place::Guest(name) => name,
        }
    }
}

/// Why a port document is refused: the key at fault (`host`, `guest`, `deadline_ms`; `None` for
/// the port as a whole) and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortFault {
    /// The key of the port document at fault, or `None` for the document as a whole.
    pub key: Option<&'static str>,
    /// Why, in one sentence.
    pub reason: String,
}

impl PortDoc {
    /// A port the host serves as `name`.
    pub fn host(name: impl Into<String>) -> PortDoc {
        PortDoc {
            host: Some(name.into()),
            guest: None,
            deadline_ms: None,
        }
    }

    /// A port the program running the document answers as `name`.
    pub fn guest(name: impl Into<String>) -> PortDoc {
        PortDoc {
            host: None,
            guest: Some(name.into()),
            deadline_ms: None,
        }
    }

    /// The same port with a deadline of `ms` for one call.
    pub fn with_deadline_ms(mut self, ms: u64) -> PortDoc {
        self.deadline_ms = Some(ms);
        self
    }

    /// Where its calls go: exactly one of `host` and `guest`, the host's name a port name.
    pub fn place(&self) -> Result<Place<'_>, PortFault> {
        match (&self.host, &self.guest) {
            (Some(host), None) => {
                if !is_port_name(host) {
                    return Err(PortFault {
                        key: Some("host"),
                        reason: format!("{host:?} is not a port name"),
                    });
                }
                Ok(Place::Host(host))
            }
            (None, Some(guest)) => Ok(Place::Guest(guest)),
            _ => Err(PortFault {
                key: None,
                reason: "a port goes to exactly one of `host` and `guest`".to_string(),
            }),
        }
    }

    /// How long one call may wait, in milliseconds: its `deadline_ms`, or [`PORT_DEADLINE_MS`],
    /// within 1 to [`PORT_DEADLINE_MAX_MS`].
    pub fn deadline(&self) -> Result<u64, PortFault> {
        let ms = self.deadline_ms.unwrap_or(PORT_DEADLINE_MS);
        if ms == 0 || ms > PORT_DEADLINE_MAX_MS {
            return Err(PortFault {
                key: Some("deadline_ms"),
                reason: format!("{ms} is outside 1 to {PORT_DEADLINE_MAX_MS} ms"),
            });
        }
        Ok(ms)
    }
}

/// Whether `name` may name a port (MH 4.10): 1 to 64 of ASCII letters, digits, `_`, `-`, `.` and
/// `:`, starting with a letter or a digit.
pub fn is_port_name(name: &str) -> bool {
    name.len() <= 64 && crate::doc::is_task_id(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_goes_to_exactly_one_place() {
        assert_eq!(PortDoc::host("mcp").place(), Ok(Place::Host("mcp")));
        assert_eq!(PortDoc::guest("echo").place(), Ok(Place::Guest("echo")));
        let both = PortDoc {
            host: Some("a".into()),
            guest: Some("b".into()),
            deadline_ms: None,
        };
        assert_eq!(both.place().unwrap_err().key, None);
        let neither = PortDoc {
            host: None,
            guest: None,
            deadline_ms: None,
        };
        assert_eq!(
            neither.place().unwrap_err().reason,
            "a port goes to exactly one of `host` and `guest`"
        );
        let bad = PortDoc::host("no spaces");
        assert_eq!(
            bad.place().unwrap_err(),
            PortFault {
                key: Some("host"),
                reason: "\"no spaces\" is not a port name".into()
            }
        );
        assert_eq!(Place::Host("x").as_str(), "host");
        assert_eq!(Place::Guest("y").name(), "y");
    }

    #[test]
    fn a_deadline_is_a_millisecond_to_a_day() {
        assert_eq!(PortDoc::host("a").deadline(), Ok(PORT_DEADLINE_MS));
        assert_eq!(PortDoc::host("a").with_deadline_ms(1).deadline(), Ok(1));
        assert_eq!(
            PortDoc::host("a")
                .with_deadline_ms(PORT_DEADLINE_MAX_MS)
                .deadline(),
            Ok(PORT_DEADLINE_MAX_MS)
        );
        for ms in [0, PORT_DEADLINE_MAX_MS + 1] {
            let fault = PortDoc::host("a")
                .with_deadline_ms(ms)
                .deadline()
                .unwrap_err();
            assert_eq!(fault.key, Some("deadline_ms"));
            assert_eq!(
                fault.reason,
                format!("{ms} is outside 1 to {PORT_DEADLINE_MAX_MS} ms")
            );
        }
    }

    #[test]
    fn port_names() {
        assert!(is_port_name("mcp"));
        assert!(is_port_name("a.b-c_d:9"));
        assert!(!is_port_name(""));
        assert!(!is_port_name("-a"));
        assert!(!is_port_name(&"a".repeat(65)));
        assert!(is_port_name(&"a".repeat(64)));
    }
}
