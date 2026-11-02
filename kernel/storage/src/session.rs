//! Object service on an ACTUALLY admitted connection. The private binder supplies the
//! retained child Account and live authority; neither caller wire Scope nor account
//! names/usage figures can replace kernel object/charged-origin validation.
use crate::{authority::Authority, memory::Loan, protocol::object};
use alloc::{boxed::Box, rc::Rc};
use core::cell::RefCell;
use moruna_storage_format::{
    Scope,
    auth::{
        Action, Admission, Authorization, DecodedAuthorization, Session, SessionRole, Verdict,
        Verifier,
    },
    wire::{self, Operation, Outcome, Refusal, Request, Response},
};
use moruna_svc::{Context, Handle, Memory, PrivateMemory, Rights};

/// Native backend operations use the sole canonical command/metadata/condition types.
/// Production instances own actual device/proxy/cache IO; no permissive/default instance
/// is supplied. Calls occur only after live authority and actual memory-origin checks.
pub trait Backend {
    /// Actual configured/mounted role, not a caller-selected request label.
    fn role(&self) -> SessionRole;
    /// Actual mounted local owner for namespace service. Remote bucket and encrypted
    /// scratch endpoints refuse; no caller-selected provider/role may be promoted.
    fn namespace_volume(&self) -> Result<Rc<RefCell<crate::volume::Volume>>, Refusal> {
        Err(Refusal::Unsupported)
    }
    /// Execute one exact already-authorized command into its retained caller Memory.
    fn execute(
        &mut self,
        request: &Request<'_>,
        account: &Handle,
        payload: Option<Loan>,
        verifier: &dyn Verifier,
    ) -> Result<Outcome, Refusal>;
}
/// Handler created ONLY after actual private admission and authority delegation.
pub struct Bound {
    account: Rc<moruna_sys::Account>,
    admission: Admission,
    authority: Rc<Authority>,
    backend: Box<dyn Backend>,
    scratch: PrivateMemory,
    body_limit: u64,
    _permit: Rc<crate::admission::Permit>,
    pending: Rc<RefCell<crate::admission::Pending>>,
}
impl Bound {
    pub(crate) fn new(
        account: Handle,
        admission: Admission,
        authority: Authority,
        backend: Box<dyn Backend>,
        body_limit: u64,
        permit: Rc<crate::admission::Permit>,
        pending: Rc<RefCell<crate::admission::Pending>>,
    ) -> Result<Self, Refusal> {
        admission.scope.validate().map_err(|_| Refusal::Invalid)?;
        if body_limit == 0 || backend.role() != admission.role {
            return Err(Refusal::Invalid);
        }
        let scratch = PrivateMemory::create_in(wire::MAX_HEADER as u64, &account)
            .map_err(|_| Refusal::Capacity)?;
        Ok(Self {
            account: Rc::new(moruna_sys::Account(account)),
            admission,
            authority: Rc::new(authority),
            backend,
            scratch,
            body_limit,
            _permit: permit,
            pending,
        })
    }
    fn origin(&self, memory: &Memory) -> Result<(), object::Error> {
        match self.account.matches(&memory.0) {
            Ok(true) => Ok(()),
            _ => Err(object::Error::BadMemory),
        }
    }
}
fn namespace_refusal(cause: Refusal) -> object::Error {
    match cause {
        Refusal::Denied => object::Error::Denied,
        Refusal::Unsupported => object::Error::Unsupported,
        Refusal::Capacity => object::Error::Capacity,
        Refusal::Busy => object::Error::Busy,
        Refusal::Invalid => object::Error::BadControl,
        _ => object::Error::Unavailable,
    }
}
fn scope_matches(scope: &Scope, admitted: &Scope) -> Result<(), Refusal> {
    if scope == admitted {
        Ok(())
    } else {
        Err(Refusal::Denied)
    }
}
fn action(operation: &Operation) -> Action {
    match operation {
        Operation::Current
        | Operation::Head { .. }
        | Operation::Read { .. }
        | Operation::CacheRead { .. } => Action::Read,
        Operation::Create { .. } | Operation::CacheFill { .. } => Action::Write,
        Operation::List { .. } => Action::List,
        Operation::Replace { .. } => Action::Publish,
    }
}
impl object::Server for Bound {
    fn session(
        &mut self,
        _: &mut Context<'_>,
        reply: Memory,
        capacity: u32,
    ) -> Result<(u32, Handle), object::Error> {
        // Even metadata/session introspection cannot charge or write another account.
        self.origin(&reply)?;
        if capacity as usize != wire::MAX_HEADER {
            return Err(object::Error::BadMemory);
        }
        let reply =
            Loan::new(reply, 0, capacity as u64, true).map_err(|_| object::Error::BadMemory)?;
        let session = Session {
            admission: self.admission.clone(),
        };
        let length = self
            .scratch
            .with_bytes(|bytes| {
                let length = session
                    .encode_into(bytes)
                    .map_err(|_| object::Error::BadControl)?;
                reply
                    .write(0, &bytes[..length])
                    .map_err(|_| object::Error::BadMemory)?;
                Ok::<_, object::Error>(length)
            })
            .map_err(|_| object::Error::Unavailable)??;
        let account = self
            .account
            .0
            .duplicate(Rights::READ | Rights::TRANSFER)
            .map_err(|_| object::Error::Unavailable)?;
        Ok((length as u32, account))
    }
    fn namespace(
        &mut self,
        _: &mut Context<'_>,
        control: Memory,
        bytes: u32,
    ) -> Result<moruna_svc::Channel, object::Error> {
        self.origin(&control)?;
        if !(wire::PREFIX_BYTES..=wire::MAX_HEADER).contains(&(bytes as usize)) {
            return Err(object::Error::BadMemory);
        }
        if !matches!(
            self.admission.role,
            SessionRole::CheckpointMetadata | SessionRole::SingleNodeHome
        ) {
            return Err(object::Error::Unsupported);
        }
        let control =
            Loan::new(control, 0, u64::from(bytes), false).map_err(|_| object::Error::BadMemory)?;
        let open = self
            .scratch
            .with_bytes(|private| {
                let result = (|| {
                    control
                        .read(0, &mut private[..bytes as usize])
                        .map_err(|_| object::Error::BadMemory)?;
                    moruna_storage_format::namespace::OpenRequest::decode(
                        &private[..bytes as usize],
                    )
                    .map_err(|_| object::Error::BadControl)
                })();
                private.fill(0);
                result
            })
            .map_err(|_| object::Error::Unavailable)??;
        if open.limits.transfer_bytes > self.body_limit {
            return Err(object::Error::BadControl);
        }
        let action = if open.create {
            Action::Write
        } else {
            Action::Read
        };
        let authorization = Authorization::native(&self.admission.scope, &open.root, action)
            .map_err(|_| object::Error::BadControl)?;
        self.authority
            .verify(&authorization)
            .map_err(namespace_refusal)?;
        let volume = self.backend.namespace_volume().map_err(namespace_refusal)?;
        let mut pending = self
            .pending
            .try_borrow_mut()
            .map_err(|_| object::Error::Unavailable)?;
        let owner = pending.namespace_owner(volume).map_err(namespace_refusal)?;
        let permit = pending.reserve().map_err(|_| object::Error::Unavailable)?;
        pending.prepare_enqueue().map_err(namespace_refusal)?;
        let (server, client) =
            moruna_svc::Channel::create().map_err(|_| object::Error::Unavailable)?;
        let handler = owner
            .bind(
                open,
                self.admission.scope.clone(),
                self.account.clone(),
                self.authority.clone(),
                permit,
            )
            .map_err(namespace_refusal)?;
        pending
            .enqueue(server, handler)
            .map_err(namespace_refusal)?;
        Ok(client)
    }
    fn execute(
        &mut self,
        _: &mut Context<'_>,
        control: Memory,
        control_bytes: u32,
        payload: Option<Memory>,
        payload_offset: u64,
        reply: Memory,
        capacity: u32,
    ) -> Result<(u32, u64), object::Error> {
        // Actual kernel Charge owner identity BEFORE mapping, header parsing or IO.
        self.origin(&control)?;
        self.origin(&reply)?;
        if let Some(payload) = &payload {
            self.origin(payload)?;
        }
        if !(wire::PREFIX_BYTES..=wire::MAX_HEADER).contains(&(control_bytes as usize))
            || capacity as usize != wire::MAX_HEADER
        {
            return Err(object::Error::BadMemory);
        }
        let control = Loan::new(control, 0, control_bytes as u64, false)
            .map_err(|_| object::Error::BadMemory)?;
        let reply =
            Loan::new(reply, 0, capacity as u64, true).map_err(|_| object::Error::BadMemory)?;
        let admission = &self.admission;
        let authority = self.authority.as_ref();
        let backend = &mut self.backend;
        let account = &self.account.0;
        let body_limit = self.body_limit;
        self.scratch
            .with_bytes(|bytes| {
                let result = (|| {
                    control
                        .read(0, &mut bytes[..wire::PREFIX_BYTES])
                        .map_err(|_| object::Error::BadMemory)?;
                    if wire::header_bytes(&bytes[..wire::PREFIX_BYTES])
                        .map_err(|_| object::Error::BadControl)?
                        != control_bytes as usize
                    {
                        return Err(object::Error::BadControl);
                    }
                    control
                        .read(0, &mut bytes[..control_bytes as usize])
                        .map_err(|_| object::Error::BadMemory)?;
                    // Authorization is a distinct real control kind, not a synthesized object
                    // request. Unknown/malformed kinds never reach the backend.
                    if let Ok(decoded) =
                        DecodedAuthorization::decode(&bytes[..control_bytes as usize])
                    {
                        if payload.is_some() {
                            return Err(object::Error::BadMemory);
                        }
                        let auth = decoded.authorization();
                        let result = scope_matches(auth.scope(), &admission.scope)
                            .and_then(|()| authority.verify(&auth));
                        let id = decoded.id();
                        drop(decoded);
                        bytes.fill(0);
                        let length = Verdict { id, result }
                            .encode_into(bytes)
                            .map_err(|_| object::Error::BadControl)?;
                        reply
                            .write(0, &bytes[..length])
                            .map_err(|_| object::Error::BadMemory)?;
                        return Ok((length as u32, 0));
                    }
                    let request = Request::decode(&bytes[..control_bytes as usize])
                        .map_err(|_| object::Error::BadControl)?;
                    let id = request.id;
                    let authorization = match request.proof.application() {
                        Some(proof) => Authorization::new(
                            &request.scope,
                            &request.object,
                            action(&request.operation),
                            proof,
                        ),
                        None => Authorization::native(
                            &request.scope,
                            &request.object,
                            action(&request.operation),
                        ),
                    };
                    let authorization =
                        authorization.and_then(|authorization| match &request.operation {
                            Operation::Head { version }
                            | Operation::Read { version, .. }
                            | Operation::CacheRead { version, .. }
                            | Operation::CacheFill { version, .. }
                            | Operation::Replace {
                                condition: wire::Condition::Match(version),
                                ..
                            } => authorization.with_version(version),
                            _ => Ok(authorization),
                        });
                    let authorized = scope_matches(&request.scope, &admission.scope)
                        .and_then(|()| authorization.and_then(|auth| authority.verify(&auth)));
                    let outcome = authorized.and_then(|()| {
                        if request
                            .object
                            .key
                            .starts_with(moruna_storage_format::namespace::ROOT_PREFIX)
                        {
                            return Err(Refusal::Denied);
                        }

                        if request
                            .expected_role
                            .is_some_and(|role| role != admission.role)
                        {
                            return Err(Refusal::Unsupported);
                        }

                        let (length, writable) = match request.operation {
                            Operation::Read { length, .. }
                            | Operation::CacheRead { length, .. } => (length, true),
                            Operation::Create { length }
                            | Operation::Replace { length, .. }
                            | Operation::CacheFill { length, .. } => (length, false),
                            _ => (0, false),
                        };
                        if length > body_limit {
                            return Err(Refusal::Capacity);
                        }
                        let payload = match payload {
                            Some(memory)
                                if matches!(
                                    request.operation,
                                    Operation::Read { .. }
                                        | Operation::CacheRead { .. }
                                        | Operation::CacheFill { .. }
                                        | Operation::Create { .. }
                                        | Operation::Replace { .. }
                                ) =>
                            {
                                Some(
                                    Loan::new(memory, payload_offset, length, writable)
                                        .map_err(|_| Refusal::Invalid)?,
                                )
                            }
                            None if length == 0 => None,
                            _ => return Err(Refusal::Invalid),
                        };
                        backend.execute(&request, account, payload, authority)
                    });
                    let written = match &outcome {
                        Ok(Outcome::Read { length, .. } | Outcome::CacheHit { length, .. }) => {
                            *length
                        }
                        _ => 0,
                    };
                    let outcome = outcome.unwrap_or_else(Outcome::Refused);
                    drop(request);
                    bytes.fill(0);
                    let length = Response { id, outcome }
                        .encode_into(bytes)
                        .map_err(|_| object::Error::BadControl)?;
                    reply
                        .write(0, &bytes[..length])
                        .map_err(|_| object::Error::BadMemory)?;
                    Ok((length as u32, written))
                })();
                bytes.fill(0);
                result
            })
            .map_err(|_| object::Error::Unavailable)?
    }
}
