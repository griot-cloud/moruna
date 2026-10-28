//! The ports a stage may call (MH 4.10), as a kind's settings name them: one place for every
//! runnable kind whose code calls `call(port, bytes)`.

use std::collections::BTreeMap;

use moruna_kernel::{Result, Route};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Context, Entry};
use crate::job::refusal;

/// One port a stage may call (`ports.<name>`, MH 4.10): exactly one of `host` and `guest`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PortDoc {
    /// A port the host serves, by the name the host knows it by: the stage's calls go to the
    /// host protocol's peer as `port_call` (MH 4.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// A port answered inside the machine, by the handler the program running the document
    /// registered under this name ([`super::Embedding::guest_port`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<String>,
    /// How long one call may wait for its answer; absent: half an hour
    /// (`moruna_kernel::ports::PORT_DEADLINE_MS`), at most a day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

/// Whether `name` may name a port (MH 4.10): 1 to 64 of ASCII letters, digits, `_`, `-`, `.` and
/// `:`, starting with a letter or a digit.
pub fn is_port_name(name: &str) -> bool {
    name.len() <= 64 && moruna_job::is_task_id(name)
}

/// The routes of an entry's `ports` (MH 4.10): each name valid, each going to exactly one place,
/// each place there to go to.
pub fn routes(
    entry: &Entry<'_>,
    ports: &BTreeMap<String, PortDoc>,
    cx: &Context,
) -> Result<BTreeMap<String, Route>> {
    let mut routes = BTreeMap::new();
    let field = |name: &str| entry.at(&format!("ports.{name}"));
    for (name, port) in ports {
        if !is_port_name(name) {
            return Err(entry.refuse(
                "ports",
                format!(
                    "{name:?} is not a port name: 1 to 64 of ASCII letters, digits, `_`, `-`, `.` \
                     and `:`, starting with a letter or a digit"
                ),
            ));
        }
        let deadline_ms = port
            .deadline_ms
            .unwrap_or(moruna_kernel::ports::PORT_DEADLINE_MS);
        if deadline_ms == 0 || deadline_ms > moruna_kernel::ports::PORT_DEADLINE_MAX_MS {
            return Err(refusal(moruna_job::SpecError::new(
                format!("{}.deadline_ms", field(name)),
                format!(
                    "{deadline_ms} is outside 1 to {} ms",
                    moruna_kernel::ports::PORT_DEADLINE_MAX_MS
                ),
            )));
        }
        let deadline = std::time::Duration::from_millis(deadline_ms);
        let route = match (&port.host, &port.guest) {
            (Some(host_name), None) => {
                if !is_port_name(host_name) {
                    return Err(refusal(moruna_job::SpecError::new(
                        format!("{}.host", field(name)),
                        format!("{host_name:?} is not a port name"),
                    )));
                }
                let Some(hub) = cx.host_port_hub() else {
                    return Err(refusal(moruna_job::SpecError::new(
                        format!("{}.host", field(name)),
                        "a host port needs a peer to serve it: report.socket under moruna run, or \
                         moruna serve",
                    )));
                };
                Route {
                    place: "host",
                    port: hub.port(host_name),
                    deadline,
                }
            }
            (None, Some(handler)) => {
                let Some(answers) = cx.embedding().guest_port(handler) else {
                    return Err(refusal(moruna_job::SpecError::new(
                        format!("{}.guest", field(name)),
                        format!(
                            "no guest handler {handler:?} is registered by the program running \
                             this document"
                        ),
                    )));
                };
                Route {
                    place: "guest",
                    port: moruna_kernel::GuestRoute::new(answers),
                    deadline,
                }
            }
            _ => {
                return Err(refusal(moruna_job::SpecError::new(
                    field(name),
                    "a port goes to exactly one of `host` and `guest`",
                )));
            }
        };
        routes.insert(name.clone(), route);
    }
    Ok(routes)
}
