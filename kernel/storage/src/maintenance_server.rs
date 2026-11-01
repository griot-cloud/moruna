//! Privileged, bounded maintenance aggregation. Each local owner reaches a real result
//! after copied data/journal/root durability; no tenant Scope/root list enters this port.
use crate::{
    admission::Pending as Registrations,
    namespace_server::{Maintenance, Owner},
    protocol::maintenance,
};
use alloc::{boxed::Box, rc::Rc, vec::Vec};
use core::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    task::{Context as TaskContext, Poll, Waker},
};
use moruna_storage_format::{config::MaintenanceConfig, wire::Refusal};
use moruna_svc::{
    Completions, Context, Decoder, Handle, Handler, Outcome, Pending, ProtocolError, ProtocolId,
    Rights, Signals, Watch,
};
struct Server {
    owners: Vec<Rc<Owner>>,
    account: Rc<moruna_sys::Account>,
    policy: MaintenanceConfig,
    registrations: Rc<RefCell<Registrations>>,
    running: Rc<Cell<bool>>,
}
struct Aggregate {
    owners: Vec<Rc<Owner>>,
    account: Rc<moruna_sys::Account>,
    policy: MaintenanceConfig,
    current: Maintenance,
    next: usize,
    handle: Handle,
    pending: Rc<RefCell<Option<Pending>>>,
    running: Rc<Cell<bool>>,
}
impl Drop for Aggregate {
    fn drop(&mut self) {
        self.running.set(false);
    }
}
fn failure(code: Refusal) -> Outcome {
    let error = match code {
        Refusal::Denied => maintenance::Error::Denied,
        Refusal::Busy | Refusal::Conflict => maintenance::Error::Busy,
        Refusal::Capacity => maintenance::Error::Capacity,
        Refusal::Invalid | Refusal::Unsupported => maintenance::Error::Invalid,
        Refusal::Integrity => maintenance::Error::Integrity,
        Refusal::Unavailable | Refusal::Missing => maintenance::Error::Unavailable,
    };
    Outcome::Failure(error.code())
}
/// Assemble only from actual configured local owners, the service's OWN carved Account,
/// readonly validated policy and a boot-granted connected private maintenance endpoint.
pub fn handler(
    owners: Vec<Rc<Owner>>,
    account: Rc<moruna_sys::Account>,
    policy: MaintenanceConfig,
    registrations: Rc<RefCell<Registrations>>,
) -> Result<Box<dyn Handler>, Refusal> {
    policy.validate().map_err(|_| Refusal::Invalid)?;
    if owners.is_empty() {
        return Err(Refusal::Unsupported);
    }
    Ok(Box::new(Server {
        owners,
        account,
        policy,
        registrations,
        running: Rc::new(Cell::new(false)),
    }))
}
impl Handler for Server {
    fn protocol(&self) -> ProtocolId {
        maintenance::ID
    }
    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, body: &mut Decoder<'_>) -> Outcome {
        if ordinal != 1 {
            return Outcome::Status(moruna_svc::Status::UnknownMethod);
        }
        if body.finish().is_err() {
            return failure(Refusal::Invalid);
        }
        if self.running.get() {
            return failure(Refusal::Busy);
        }
        let mut registrations = match self.registrations.try_borrow_mut() {
            Ok(value) => value,
            Err(_) => return failure(Refusal::Busy),
        };
        let permit = match registrations.reserve() {
            Ok(value) => value,
            Err(error) => return failure(error),
        };
        if let Err(error) = registrations.prepare_watch() {
            return failure(error);
        }
        let limits = match self.policy.limits() {
            Ok(value) => value,
            Err(_) => return failure(Refusal::Invalid),
        };
        let current = match self.owners[0].request_maintenance(
            self.account.clone(),
            limits,
            self.policy.max_objects,
            self.policy.max_roots,
        ) {
            Ok(value) => value,
            Err(error) => return failure(error),
        };
        let handle = match current.wait_handle().duplicate(Rights::WAIT) {
            Ok(value) => value,
            Err(_) => return failure(Refusal::Unavailable),
        };
        let pending = Rc::new(RefCell::new(None));
        let watch = Aggregate {
            owners: self.owners.clone(),
            account: self.account.clone(),
            policy: self.policy.clone(),
            current,
            next: 1,
            handle,
            pending: pending.clone(),
            running: self.running.clone(),
        };
        self.running.set(true);
        if let Err(error) = registrations.enqueue_reserved_watch(Box::new(watch), permit) {
            return failure(error);
        }
        // Every fallible queue/resource step precedes defer, so no untracked SDK request
        // remains if admission fails. The real owner independently retains accepted IO.
        *pending.borrow_mut() = Some(cx.defer());
        Outcome::Deferred
    }
}
impl Aggregate {
    fn finish(&mut self, done: &mut Completions, result: Result<(), Refusal>) -> bool {
        if let Some(pending) = self.pending.borrow_mut().take() {
            done.complete(
                pending,
                match result {
                    Ok(()) => Outcome::reply(()),
                    Err(error) => failure(error),
                },
            );
        }
        false
    }
}
impl Watch for Aggregate {
    fn handle(&self) -> &Handle {
        &self.handle
    }
    fn signals(&self) -> Signals {
        Signals::READABLE | Signals::PEER_CLOSED
    }
    fn signaled(&mut self, _: Signals, done: &mut Completions) -> bool {
        // At most the finite configured local owner count can complete in one turn.
        loop {
            let mut context = TaskContext::from_waker(Waker::noop());
            match Pin::new(&mut self.current).poll(&mut context) {
                Poll::Pending => return true,
                Poll::Ready(Err(error)) => return self.finish(done, Err(error)),
                Poll::Ready(Ok(())) => {
                    if self.next == self.owners.len() {
                        return self.finish(done, Ok(()));
                    }
                    let limits = match self.policy.limits() {
                        Ok(value) => value,
                        Err(_) => return self.finish(done, Err(Refusal::Invalid)),
                    };
                    let current = match self.owners[self.next].request_maintenance(
                        self.account.clone(),
                        limits,
                        self.policy.max_objects,
                        self.policy.max_roots,
                    ) {
                        Ok(value) => value,
                        Err(error) => return self.finish(done, Err(error)),
                    };
                    let handle = match current.wait_handle().duplicate(Rights::WAIT) {
                        Ok(value) => value,
                        Err(_) => return self.finish(done, Err(Refusal::Unavailable)),
                    };
                    self.current = current;
                    self.handle = handle;
                    self.next += 1;
                }
            }
        }
    }
}
