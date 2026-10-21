//! The world's `call` import (MH 4.10): a component hands a named port a request and waits for
//! the answer, without a socket, a file or a credential of its own.
//!
//! `call(port: string, request: list<u8>) -> result<list<u8>, string>`. The port's answer is
//! `ok`; the port's own refusal is `err`, for the component to decide about. Everything else ends
//! the call in a trap that fails the stage with its cause: a name the stage's entry does not list,
//! a request past the bound, no answer before the port's deadline, an answer that never came, and
//! the run's cancel, which ends the run as cancelled.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moruna_kernel::{Answer, CallFailure, StagePorts};
use wasmtime::StoreContextMut;

use crate::limiter::Limits;

/// The world's call import.
pub const CALL: &str = "call";

/// What `call` reaches from one instance.
pub(crate) struct Calls {
    /// The stage's ports.
    pub(crate) ports: Arc<StagePorts>,
    /// The instance's number in its stage.
    pub(crate) instance: usize,
    /// The instance's state when the `process` in progress began: kept for a stateful stage that
    /// lists ports, so a checkpoint taken while it waits has a state to write.
    pub(crate) entry: Option<Vec<u8>>,
    /// The entry states of the stage's instances that are waiting on a port now, by instance.
    pub(crate) waiting: Arc<Mutex<BTreeMap<usize, Vec<u8>>>>,
}

/// A call that fails the stage, carried through wasmtime's trap to the stage's error.
#[derive(Debug)]
pub(crate) struct CallTrap(pub(crate) CallFailure);

impl std::fmt::Display for CallTrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::error::Error for CallTrap {}

/// The host side of `call`.
pub(crate) fn call(
    store: StoreContextMut<'_, Limits>,
    port: String,
    request: Vec<u8>,
) -> wasmtime::Result<(Result<Vec<u8>, String>,)> {
    let Some(calls) = store.data().calls.as_ref() else {
        return Err(wasmtime::Error::new(CallTrap(CallFailure::Unbound {
            name: port,
        })));
    };
    let published = match &calls.entry {
        Some(state) => {
            calls
                .waiting
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(calls.instance, state.clone());
            true
        }
        None => false,
    };
    let answered = calls.ports.call(&port, request);
    if published {
        calls
            .waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&calls.instance);
    }
    match answered {
        Ok(Answer::Ok(bytes)) => Ok((Ok(bytes),)),
        Ok(Answer::Error(said)) => Ok((Err(said),)),
        Ok(Answer::Lost(reason)) => Err(wasmtime::Error::new(CallTrap(CallFailure::Lost {
            name: port,
            reason,
        }))),
        Err(failure) => Err(wasmtime::Error::new(CallTrap(failure))),
    }
}
