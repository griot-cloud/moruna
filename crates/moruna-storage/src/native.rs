//! Actual kernel-startup storage grants for the Linux personality. No host simulator,
//! opaque-token stand-in or wire-scope admission is used here. SDK payload memory is
//! charged to the explicitly granted child account; copies to/from arena bytes are real
//! staging copies, not a zero-copy claim. The service independently verifies every action.
#![forbid(unsafe_code)]
/// Actual connected durable namespace client; no host filesystem fallback.
#[path = "native_namespace.rs"]
pub mod namespace;
use crate::{
    Access, Action, Authorizer, Backend, Buffer, BufferView, Condition, Credential, Error,
    Governed, Metadata, Object, Operation, Result, Scope, Version, volume::Arenas,
};
use moruna_storage_format::{
    auth::{Authorization, Proof, Session, SessionRole, Verdict},
    wire::{self, Outcome, Request, Response},
};
use moruna_storage_service::protocol::object;
use moruna_svc::{Channel, ClientCore, Encoder, Memory, ObjectType, Rights, Wire};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

/// Real child-account grant, consumed once from the kernel startup page. Sharing this
/// owner permits explicit home/local roles without taking the same startup slot twice.
pub struct Account {
    handle: moruna_sys::Account,
}
impl Account {
    /// Consume an actual Account grant. Off-kernel execution and wrong object types refuse.
    pub fn from_slot(slot: &str) -> Result<Arc<Self>> {
        let handle = moruna_micro::start::take(slot)
            .ok_or(Error::Unavailable("native child account grant"))?;
        let (kind, _) = handle.info().map_err(sdk)?;
        if kind != ObjectType::Account {
            return Err(Error::Denied);
        }
        let handle = moruna_sys::Account(handle);
        handle.usage().map_err(sdk)?;
        Ok(Arc::new(Self { handle }))
    }
}
/// Sealed actual handshaken connection identity. Only startup consumption constructs it;
/// a requested NativeSession wire marker cannot construct or substitute this owner.
pub(crate) struct Connection {
    core: Mutex<Option<ClientCore>>,
    next: AtomicU64,
    account: Arc<Account>,
    copied_in: AtomicU64,
    copied_out: AtomicU64,
}
/// One actual admitted storage session and its caller-charged control allocator.
pub struct Client {
    connection: Arc<Connection>,
    scope: Scope,
    role: SessionRole,
    arenas: Arc<dyn Arenas>,
}
impl Client {
    /// Consume a connected `moruna.storage` startup grant and perform the real SDK hello.
    /// Scope is an asserted request identity only; the designated live authority must
    /// match it to the channel/account admission on every request. Timeout poisons this
    /// connection: it cannot replay an uncertain publication or reuse reply memory.
    pub fn from_slot(
        slot: &str,
        scope: Scope,
        account: Arc<Account>,
        arenas: Arc<dyn Arenas>,
        timeout_ns: u64,
    ) -> Result<Arc<Self>> {
        scope.validate()?;
        if timeout_ns == 0 || timeout_ns == u64::MAX {
            return Err(Error::Invalid("native storage timeout"));
        }
        arenas.allocator(&scope)?;
        let handle = moruna_micro::start::take(slot)
            .ok_or(Error::Unavailable("native storage connected grant"))?;
        let (kind, _) = handle.info().map_err(sdk)?;
        if kind != ObjectType::Channel {
            return Err(Error::Denied);
        }
        let mut core = ClientCore::connect(
            Channel(handle),
            object::ID,
            moruna_svc::deadline_in(timeout_ns),
        )
        .map_err(|_| Error::Unavailable("native storage handshake"))?;
        core.set_timeout(timeout_ns);
        // Actual service-owned facts, correlated by the SDK call. Neither the startup
        // slot name nor this constructor's asserted Scope is a role attestation.
        let reply = Memory::create_in(wire::MAX_HEADER as u64, &account.handle.0).map_err(sdk)?;
        let mapping = reply.map(0, wire::MAX_HEADER as u64, 0).map_err(sdk)?;
        let rights = Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER;
        let mut body = Encoder::new();
        Memory(reply.0.duplicate(rights).map_err(sdk)?)
            .encode(&mut body)
            .map_err(|_| Error::Integrity)?;
        (wire::MAX_HEADER as u32)
            .encode(&mut body)
            .map_err(|_| Error::Integrity)?;
        let mut frame = core
            .call(2, body)
            .map_err(|_| Error::Unavailable("native storage bound session"))?;
        let mut decoder = frame.decoder();
        let length = u32::decode(&mut decoder).map_err(|_| Error::Integrity)? as usize;
        let bound_account =
            moruna_svc::Handle::decode(&mut decoder).map_err(|_| Error::Integrity)?;
        decoder.finish().map_err(|_| Error::Integrity)?;
        let (kind, rights) = bound_account.info().map_err(sdk)?;
        if kind != ObjectType::Account
            || !rights.contains(Rights::READ)
            || !account.handle.matches(&bound_account).map_err(sdk)?
        {
            return Err(Error::Denied);
        }
        if length > wire::MAX_HEADER {
            return Err(Error::Integrity);
        }
        let arena = arenas.allocator(&scope)?;
        let mut control = crate::allocate_exact(arena.as_ref(), wire::MAX_HEADER)?;
        mapping
            .read(0, &mut control.as_mut()[..length])
            .map_err(sdk)?;
        let session = Session::decode(&control.as_ref()[..length])?;
        if session.admission.scope != scope {
            return Err(Error::Denied);
        }
        let role = session.admission.role;
        Ok(Arc::new(Self {
            connection: Arc::new(Connection {
                core: Mutex::new(Some(core)),
                next: AtomicU64::new(1),
                account,
                copied_in: AtomicU64::new(0),
                copied_out: AtomicU64::new(0),
            }),
            scope,
            role,
            arenas,
        }))
    }
    /// Request scope asserted by this actual admitted session, not an owner-equality grant.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
    /// Backend role reported by the actual admitted service, never inferred from slot names.
    pub fn role(&self) -> SessionRole {
        self.role
    }
    /// Shared original child account used by all explicit backend roles.
    pub fn account(&self) -> &Arc<Account> {
        &self.connection.account
    }
    pub(crate) fn connection(&self) -> &Arc<Connection> {
        &self.connection
    }
    /// Credential sealed to this actual connection; never serialized as application bytes.
    pub fn credential(&self) -> Credential {
        Credential::from_native(self.connection.clone())
    }
    /// Same governed API as socket/bucket/raw storage, with actual remote live authorization.
    pub fn governed(self: Arc<Self>) -> Result<Governed> {
        Governed::new(self.clone(), self)
    }
    /// Actual payload staging bytes copied into and out of SDK memory, excluding controls.
    pub fn copied_bytes(&self) -> (u64, u64) {
        (
            self.connection.copied_in.load(Ordering::Relaxed),
            self.connection.copied_out.load(Ordering::Relaxed),
        )
    }
    /// Lookup original HOME identity/range through the real encrypted local service.
    /// Caller MUST freshly authorize/head the home version first; miss retains exactly
    /// the same destination, while corruption/key/origin/transport errors remain errors.
    pub async fn cache_lookup(
        &self,
        access: &Access,
        object: &Object,
        version: &Version,
        offset: u64,
        dst: Buffer,
    ) -> Result<crate::cache::Lookup> {
        if self.role != SessionRole::CacheScratch {
            return Err(Error::Invalid("native cache role"));
        }
        Authorizer::verify(self, access, object, Action::Read, Some(version)).await?;
        let scope = Scope {
            workspace: access.workspace.clone(),
            run: access.run.clone(),
        };
        let length = dst.len() as u64;
        let (outcome, dst) = self
            .call(
                &scope,
                object,
                wire::Operation::CacheRead {
                    version: version.clone(),
                    offset,
                    length,
                },
                None,
                Some(dst),
            )
            .await?;
        let dst = dst.ok_or(Error::Integrity)?;
        match outcome {
            Outcome::CacheHit {
                offset: actual,
                length: bytes,
            } if actual == offset && bytes == length => Ok(crate::cache::Lookup::Hit(dst)),
            Outcome::CacheMiss => Ok(crate::cache::Lookup::Miss(dst)),
            _ => Err(Error::Integrity),
        }
    }
    /// Fill only actual home-read bytes under an explicit trusted-engine CacheScratch
    /// WRITE grant for the original pinned version/range. No substitute version is minted.
    pub async fn cache_fill(
        &self,
        access: &Access,
        object: &Object,
        version: &Version,
        offset: u64,
        src: BufferView,
    ) -> Result<()> {
        if self.role != SessionRole::CacheScratch {
            return Err(Error::Invalid("native cache role"));
        }
        Authorizer::verify(self, access, object, Action::Write, Some(version)).await?;
        let scope = Scope {
            workspace: access.workspace.clone(),
            run: access.run.clone(),
        };
        let length = src.len() as u64;
        match self
            .call(
                &scope,
                object,
                wire::Operation::CacheFill {
                    version: version.clone(),
                    offset,
                    length,
                },
                Some(src),
                None,
            )
            .await?
            .0
        {
            Outcome::Cached => Ok(()),
            _ => Err(Error::Integrity),
        }
    }
    fn check(&self, scope: &Scope) -> Result<()> {
        scope.validate()?;
        if scope != &self.scope {
            return Err(Error::Denied);
        }
        Ok(())
    }
    fn id(&self) -> Result<u64> {
        self.connection
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Unavailable("native storage request identity exhausted"))
    }
    async fn call(
        &self,
        scope: &Scope,
        object: &Object,
        command: wire::Operation,
        body: Option<BufferView>,
        destination: Option<Buffer>,
    ) -> Result<(Outcome, Option<Buffer>)> {
        self.check(scope)?;
        let arena = self.arenas.allocator(scope)?;
        let mut control = crate::allocate_exact(arena.as_ref(), wire::MAX_HEADER)?;
        let reply = crate::allocate_exact(arena.as_ref(), wire::MAX_HEADER)?;
        let id = self.id()?;
        let request = Request {
            id,
            scope: scope.clone(),
            object: object.clone(),
            expected_role: Some(self.role),
            proof: Proof::NativeSession,
            operation: command,
        };
        let length = request.encode_into(control.as_mut())?;
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut reply = reply;
            let mut destination = destination;
            let response_bytes = exchange(
                &connection,
                &control.as_ref()[..length],
                reply.as_mut(),
                body.as_ref(),
                destination.as_mut(),
            )?;
            let response = Response::decode(&reply.as_ref()[..response_bytes])?;
            if response.id != id {
                return Err(Error::Integrity);
            }
            match &response.outcome {
                Outcome::Refused(code) => return Err(crate::port::from_refusal(*code)),
                Outcome::Read {
                    offset: _,
                    length,
                    metadata,
                } => {
                    let dst = destination.as_ref().ok_or(Error::Integrity)?;
                    if *length != dst.len() as u64 || metadata.object != request.object {
                        return Err(Error::Integrity);
                    }
                }
                Outcome::CacheHit { length, .. }
                    if destination
                        .as_ref()
                        .is_some_and(|dst| dst.len() as u64 == *length) => {}
                Outcome::CacheMiss => {}
                _ if destination.is_some() => return Err(Error::Integrity),
                _ => {}
            }
            Ok((response.outcome, destination))
        })
        .await
        .map_err(|_| Error::Unavailable("native storage owned worker"))?
    }
}
fn sdk(_: moruna_svc::Error) -> Error {
    Error::Unavailable("native storage SDK operation")
}
/// Each fresh staging Memory is owned by the child account. A timed-out peer retains
/// its transferred loan through actual service/driver IO; no mapping is reused. We never
/// create a Rust slice over shared mappings and never release a peer's original IO pin.
fn exchange(
    connection: &Connection,
    control: &[u8],
    reply: &mut [u8],
    source: Option<&BufferView>,
    destination: Option<&mut Buffer>,
) -> Result<usize> {
    let account = &connection.account.handle.0;
    let input = Memory::create_in(control.len().max(1) as u64, account).map_err(sdk)?;
    let output = Memory::create_in(reply.len() as u64, account).map_err(sdk)?;
    let input_map = input
        .map(0, rounded(control.len())?, moruna_svc::map::WRITE)
        .map_err(sdk)?;
    input_map.write(0, control).map_err(sdk)?;
    let output_map = output.map(0, rounded(reply.len())?, 0).map_err(sdk)?;
    let bytes = source
        .map(BufferView::len)
        .or_else(|| destination.as_ref().map(|b| b.len()))
        .unwrap_or(0);
    let payload = if source.is_some() || destination.is_some() {
        Some(Memory::create_dma_in(bytes.max(1) as u64, account).map_err(sdk)?)
    } else {
        None
    };
    let payload_map = payload
        .as_ref()
        .map(|p| {
            p.map(0, rounded(bytes)?, moruna_svc::map::WRITE)
                .map_err(sdk)
        })
        .transpose()?;
    if let Some(source) = source {
        let bytes = source
            .as_host_slice()
            .ok_or(Error::Invalid("native storage host source"))?;
        payload_map
            .as_ref()
            .ok_or(Error::Integrity)?
            .write(0, bytes)
            .map_err(sdk)?;
        connection
            .copied_in
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
    }
    let read_rights = Rights::READ | Rights::MAP | Rights::TRANSFER;
    let write_rights = read_rights | Rights::WRITE;
    let mut body = Encoder::new();
    Memory(input.0.duplicate(read_rights).map_err(sdk)?)
        .encode(&mut body)
        .map_err(|_| Error::Integrity)?;
    u32::try_from(control.len())
        .map_err(|_| Error::Integrity)?
        .encode(&mut body)
        .map_err(|_| Error::Integrity)?;
    let loan = payload
        .as_ref()
        .map(|p| {
            p.0.duplicate(if destination.is_some() {
                write_rights
            } else {
                read_rights
            })
            .map(Memory)
            .map_err(sdk)
        })
        .transpose()?;
    loan.encode(&mut body).map_err(|_| Error::Integrity)?;
    0u64.encode(&mut body).map_err(|_| Error::Integrity)?;
    Memory(output.0.duplicate(write_rights).map_err(sdk)?)
        .encode(&mut body)
        .map_err(|_| Error::Integrity)?;
    (wire::MAX_HEADER as u32)
        .encode(&mut body)
        .map_err(|_| Error::Integrity)?;
    let mut guard = connection
        .core
        .lock()
        .map_err(|_| Error::Unavailable("native storage connection poisoned"))?;
    let result = guard
        .as_mut()
        .ok_or(Error::Unavailable("native storage connection closed"))?
        .call(1, body);
    let mut frame = match result {
        Ok(frame) => frame,
        Err(_) => {
            *guard = None;
            return Err(Error::Unavailable(
                "native storage reply uncertain; connection closed",
            ));
        }
    };
    let mut decoder = frame.decoder();
    let length = u32::decode(&mut decoder).map_err(|_| Error::Integrity)? as usize;
    let written = u64::decode(&mut decoder).map_err(|_| Error::Integrity)?;
    decoder.finish().map_err(|_| Error::Integrity)?;
    if length > reply.len() {
        return Err(Error::Integrity);
    }
    output_map.read(0, &mut reply[..length]).map_err(sdk)?;
    // Only a successful, correlated canonical response permits a payload to escape.
    // Read validation occurs before copying because a refusal must not expose peer bytes.
    match Response::decode(&reply[..length]) {
        Ok(Response {
            id,
            outcome:
                Outcome::Read {
                    metadata,
                    offset,
                    length,
                },
            ..
        }) if length == written => {
            let request = Request::decode(control)?;
            let wire::Operation::Read {
                version,
                offset: expected_offset,
                length: expected_length,
            } = request.operation
            else {
                return Err(Error::Integrity);
            };
            if id != request.id
                || metadata.object != request.object
                || metadata.version != version
                || offset != expected_offset
                || length != expected_length
                || destination
                    .as_ref()
                    .is_none_or(|dst| dst.len() as u64 != length)
            {
                return Err(Error::Integrity);
            }
            let dst = destination.ok_or(Error::Integrity)?;
            payload_map
                .as_ref()
                .ok_or(Error::Integrity)?
                .read(0, dst.as_mut())
                .map_err(sdk)?;
            connection.copied_out.fetch_add(written, Ordering::Relaxed);
        }
        Ok(Response {
            id,
            outcome: Outcome::CacheHit { offset, length },
        }) if length == written => {
            let request = Request::decode(control)?;
            let wire::Operation::CacheRead {
                offset: expected_offset,
                length: expected_length,
                ..
            } = request.operation
            else {
                return Err(Error::Integrity);
            };
            if id != request.id
                || offset != expected_offset
                || length != expected_length
                || destination
                    .as_ref()
                    .is_none_or(|dst| dst.len() as u64 != length)
            {
                return Err(Error::Integrity);
            }
            let dst = destination.ok_or(Error::Integrity)?;
            payload_map
                .as_ref()
                .ok_or(Error::Integrity)?
                .read(0, dst.as_mut())
                .map_err(sdk)?;
            connection.copied_out.fetch_add(written, Ordering::Relaxed);
        }
        Ok(Response {
            id,
            outcome: Outcome::CacheMiss,
        }) if written == 0 => {
            let request = Request::decode(control)?;
            if id != request.id || !matches!(request.operation, wire::Operation::CacheRead { .. }) {
                return Err(Error::Integrity);
            }
        }
        Ok(Response {
            outcome: Outcome::Refused(_),
            ..
        }) if written == 0 => {}
        Ok(_) if destination.is_none() && written == 0 => {}
        Err(_) if destination.is_none() && written == 0 => {
            Verdict::decode(&reply[..length])?;
        }
        _ => return Err(Error::Integrity),
    }
    Ok(length)
}
fn rounded(bytes: usize) -> Result<u64> {
    (bytes.max(1) as u64)
        .checked_add(4095)
        .map(|n| n / 4096 * 4096)
        .ok_or(Error::Invalid("native storage mapping size"))
}
impl Authorizer for Client {
    fn verify<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        action: Action,
        version: Option<&'a Version>,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            let scope = Scope {
                workspace: access.workspace.clone(),
                run: access.run.clone(),
            };
            self.check(&scope)?;
            if !access.credential.contains_native(&self.connection) {
                return Err(Error::Denied);
            }
            let arena = self.arenas.allocator(&scope)?;
            let mut control = crate::allocate_exact(arena.as_ref(), wire::MAX_HEADER)?;
            let mut reply = crate::allocate_exact(arena.as_ref(), wire::MAX_HEADER)?;
            let id = self.id()?;
            let authorization =
                Authorization::native(&scope, object, action).map_err(crate::port::from_refusal)?;
            let authorization = match version {
                Some(version) => authorization
                    .with_version(version)
                    .map_err(crate::port::from_refusal)?,
                None => authorization,
            };
            let length = authorization.encode_into(id, control.as_mut())?;
            let connection = self.connection.clone();
            tokio::task::spawn_blocking(move || {
                let size = exchange(
                    &connection,
                    &control.as_ref()[..length],
                    reply.as_mut(),
                    None,
                    None,
                )?;
                let verdict = Verdict::decode(&reply.as_ref()[..size])?;
                if verdict.id != id {
                    return Err(Error::Integrity);
                }
                verdict.result.map_err(crate::port::from_refusal)
            })
            .await
            .map_err(|_| Error::Unavailable("native storage authority worker"))?
        })
    }
}
impl Backend for Client {
    fn role(&self) -> Option<moruna_storage_format::auth::SessionRole> {
        Some(self.role())
    }

    fn current<'a>(&'a self, scope: &'a Scope, object: &'a Object) -> Operation<'a, Metadata> {
        Box::pin(async move {
            match self
                .call(scope, object, wire::Operation::Current, None, None)
                .await?
                .0
            {
                Outcome::Head(metadata) if metadata.object == *object => Ok(metadata),
                _ => Err(Error::Integrity),
            }
        })
    }

    fn head<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        version: &'a Version,
    ) -> Operation<'a, Metadata> {
        Box::pin(async move {
            match self
                .call(
                    scope,
                    object,
                    wire::Operation::Head {
                        version: version.clone(),
                    },
                    None,
                    None,
                )
                .await?
                .0
            {
                Outcome::Head(m) => Ok(m),
                _ => Err(Error::Integrity),
            }
        })
    }
    fn read<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        dst: Buffer,
    ) -> Operation<'a, Buffer> {
        Box::pin(async move {
            let bytes = dst.len() as u64;
            self.call(
                scope,
                object,
                wire::Operation::Read {
                    version: version.clone(),
                    offset,
                    length: bytes,
                },
                None,
                Some(dst),
            )
            .await?
            .1
            .ok_or(Error::Integrity)
        })
    }
    fn write<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        src: BufferView,
    ) -> Operation<'a, Metadata> {
        Box::pin(async move {
            let n = src.len() as u64;
            match self
                .call(
                    scope,
                    object,
                    wire::Operation::Create { length: n },
                    Some(src),
                    None,
                )
                .await?
                .0
            {
                Outcome::Written(m) => Ok(m),
                _ => Err(Error::Integrity),
            }
        })
    }
    fn replace<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        condition: Condition,
        src: BufferView,
    ) -> Operation<'a, Metadata> {
        Box::pin(async move {
            let n = src.len() as u64;
            let condition = match condition {
                Condition::Absent => wire::Condition::Absent,
                Condition::Match(v) => wire::Condition::Match(v),
            };
            match self
                .call(
                    scope,
                    object,
                    wire::Operation::Replace {
                        condition,
                        length: n,
                    },
                    Some(src),
                    None,
                )
                .await?
                .0
            {
                Outcome::Written(m) => Ok(m),
                _ => Err(Error::Integrity),
            }
        })
    }
    fn list<'a>(&'a self, scope: &'a Scope, prefix: &'a Object) -> Operation<'a, Vec<Metadata>> {
        Box::pin(async move {
            let mut entries = Vec::new();
            let mut after = None;
            loop {
                match self
                    .call(
                        scope,
                        prefix,
                        wire::Operation::List {
                            limit: 64,
                            after: after.clone(),
                        },
                        None,
                        None,
                    )
                    .await?
                    .0
                {
                    Outcome::Listed {
                        entries: page,
                        next,
                    } => {
                        if let Some(next) = &next
                            && (after.as_ref().is_some_and(|old| next <= old)
                                || page.last().is_none_or(|last| &last.object.key != next))
                        {
                            return Err(Error::Integrity);
                        }
                        entries.extend(page);
                        match next {
                            Some(next) => after = Some(next),
                            None => return Ok(entries),
                        }
                    }
                    _ => return Err(Error::Integrity),
                }
            }
        })
    }
}
