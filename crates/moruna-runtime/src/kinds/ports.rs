//! The ports a stage may call (MH 4.10), as a kind's settings or its declaration name them: one
//! place where a port document becomes the route its calls take, for every runnable kind whose
//! stage calls `call(port, bytes)` (a `package`) or whose every batch is a call (a served kind,
//! SDD 18 e.4).

use std::collections::BTreeMap;
use std::sync::Arc;

use moruna_job::{Place, PortDoc, SpecError, is_port_name};
use moruna_kernel::{Result, Route, StagePorts};

use super::{Context, Entry};
use crate::job::refusal;

/// The ports of a stage and the hand-off of them to its kernel (MH 4.10): the build makes the
/// stage's [`StagePorts`] from `routes` once its id is known, gives them to `bind`, and binds them
/// to the run when it starts, so every call answers to the run's cancel and is told to its waits.
pub struct StageCalls {
    /// Each name the stage may call, and where it goes.
    pub routes: BTreeMap<String, Route>,
    /// Gives the kernel its ports.
    pub bind: Box<dyn FnOnce(Arc<StagePorts>) + Send>,
}

/// The routes of an entry's `ports` (MH 4.10): each name valid, each going to exactly one place,
/// each place there to go to.
pub fn routes(
    entry: &Entry<'_>,
    ports: &BTreeMap<String, PortDoc>,
    cx: &Context,
) -> Result<BTreeMap<String, Route>> {
    let mut routes = BTreeMap::new();
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
        let route = route(&entry.at(&format!("ports.{name}")), port, cx)?;
        routes.insert(name.clone(), route);
    }
    Ok(routes)
}

/// The route of one port document at `field`: refused at the field, or the key under it, when it
/// does not go to exactly one place, when its deadline is out of range, and when the place is not
/// there: a host port with no peer, a guest port whose handler the program running the document
/// did not register.
pub fn route(field: &str, port: &PortDoc, cx: &Context) -> Result<Route> {
    route_with(port, cx, &|key, reason| {
        let at = key.map_or_else(|| field.to_string(), |k| format!("{field}.{k}"));
        refusal(SpecError::new(at, reason))
    })
}

/// [`route`] with its refusals made by `refuse`, from the key of the port document at fault
/// (`None`: the document) and why: for a port a declaration names rather than an entry.
pub fn route_with(
    port: &PortDoc,
    cx: &Context,
    refuse: &dyn Fn(Option<&str>, String) -> moruna_kernel::MorunaError,
) -> Result<Route> {
    let deadline_ms = port.deadline().map_err(|f| refuse(f.key, f.reason))?;
    let deadline = std::time::Duration::from_millis(deadline_ms);
    match port.place().map_err(|f| refuse(f.key, f.reason))? {
        Place::Host(name) => {
            let Some(hub) = cx.host_port_hub() else {
                return Err(refuse(
                    Some("host"),
                    "a host port needs a peer to serve it: report.socket under moruna run, or \
                     moruna serve"
                        .to_string(),
                ));
            };
            Ok(Route {
                place: "host",
                port: hub.port(name),
                deadline,
            })
        }
        Place::Guest(name) => {
            let Some(answers) = cx.embedding().guest_port(name) else {
                return Err(refuse(
                    Some("guest"),
                    format!(
                        "no guest handler {name:?} is registered by the program running this \
                         document"
                    ),
                ));
            };
            Ok(Route {
                place: "guest",
                port: moruna_kernel::GuestRoute::new(answers),
                deadline,
            })
        }
    }
}
