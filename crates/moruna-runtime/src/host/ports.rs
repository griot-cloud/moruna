//! Host ports (MH 4.10): a stage's calls to ports the host serves, carried on the host protocol's
//! own connection as `port_call`, answered by the peer's `port_reply`, and given up with
//! `port_cancel`.
//!
//! One hub per connection. A call that cannot be sent, a peer that closes with calls waiting, and
//! a reply Moruna cannot read end those calls as lost, and their stages fail with the cause; a
//! reply to a call nobody waits for any more is a note in the report.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use moruna_kernel::{Abandon, Answer, Port, PortCall};

use super::protocol::{Outbound, PORTS_VERSION};
use super::session::Wire;

/// The calls one connection's stages have waiting on the host.
pub struct HostPorts {
    wire: Arc<Wire>,
    waiting: Mutex<BTreeMap<u64, Sender<Answer>>>,
    /// Why no call can reach the host any more, once that is so.
    closed: Mutex<Option<String>>,
}

impl HostPorts {
    /// The hub of the connection `wire` writes to.
    pub fn new(wire: Arc<Wire>) -> Arc<HostPorts> {
        Arc::new(HostPorts {
            wire,
            waiting: Mutex::new(BTreeMap::new()),
            closed: Mutex::new(None),
        })
    }

    /// The port the host knows as `name`.
    pub fn port(self: &Arc<Self>, name: &str) -> Arc<dyn Port> {
        Arc::new(HostPort {
            hub: Arc::clone(self),
            name: name.to_string(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Sender<Answer>>> {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The peer's `port_reply`.
    pub fn reply(&self, v: u32, call: u64, ok: Option<String>, error: Option<String>) {
        let Some(answers) = self.lock().remove(&call) else {
            self.wire.note(format!(
                "a port_reply for call {call}, which nothing waits for (answered, given up, or \
                 never made), was ignored"
            ));
            return;
        };
        let answer = if v != PORTS_VERSION {
            Answer::Lost(format!(
                "the host answered with port messages of version {v}; this build reads version \
                 {PORTS_VERSION}"
            ))
        } else {
            match (ok, error) {
                (Some(bytes), None) => {
                    match base64::engine::general_purpose::STANDARD.decode(&bytes) {
                        Ok(bytes) => Answer::Ok(bytes),
                        Err(e) => Answer::Lost(format!("the host's answer is not base64: {e}")),
                    }
                }
                (None, Some(said)) => Answer::Error(said),
                _ => Answer::Lost(
                    "the host's port_reply has both or neither of `ok` and `error`".to_string(),
                ),
            }
        };
        let _ = answers.send(answer);
    }

    /// The peer is gone: every call waiting is lost, and no later one starts.
    pub fn close(&self, reason: String) {
        *self.closed.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.clone());
        let waiting = std::mem::take(&mut *self.lock());
        for (_, answers) in waiting {
            let _ = answers.send(Answer::Lost(reason.clone()));
        }
    }

    fn closed(&self) -> Option<String> {
        self.closed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// One host port, by the host's name for it.
struct HostPort {
    hub: Arc<HostPorts>,
    name: String,
}

impl Port for HostPort {
    fn begin(&self, call: PortCall) -> Result<Receiver<Answer>, String> {
        if let Some(reason) = self.hub.closed() {
            return Err(reason);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.hub.lock().insert(call.id, tx);
        let message = Outbound::PortCall {
            v: PORTS_VERSION,
            call: call.id,
            stage: call.stage,
            port: self.name.clone(),
            deadline_ms: call.deadline.as_millis() as u64,
            request: base64::engine::general_purpose::STANDARD.encode(&call.request),
        };
        if let Err(reason) = self.hub.wire.try_send(&message) {
            self.hub.lock().remove(&call.id);
            return Err(format!("the call could not be sent to the host: {reason}"));
        }
        Ok(rx)
    }

    fn abandon(&self, id: u64, why: Abandon) {
        if self.hub.lock().remove(&id).is_some() {
            self.hub.wire.send(&Outbound::PortCancel {
                v: PORTS_VERSION,
                call: id,
                reason: why.name().to_string(),
            });
        }
    }
}
