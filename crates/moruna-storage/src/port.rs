//! Real whole-object/range port over an owned deadline-bound host socket. Headers use
//! the sole no_std codec; read bodies land directly in the caller's charged destination.
//! This does not eliminate host bucket SDK/TCP staging or claim DMA.
#![forbid(unsafe_code)]
use crate::{
    Access, Action, Authorizer, Backend, Condition, Credential, Error, Governed, Metadata, Object,
    Operation, Result, Scope, Version, io, volume::Arenas,
};
use moruna_kernel::{Allocator, Buffer, BufferView};
use moruna_storage_format::wire::{
    self, Operation as Command, Outcome, Refusal, Request, Response,
};
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
/// Owned full duplex transport. An injected connector must set finite IO deadlines.
pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}
/// Only explicitly configured socket addresses; no default service or cloud fallback.
pub trait Connector: Send + Sync {
    /// Establish an owned socket with finite read/write deadlines. Cancellation of the
    /// consuming future may detach its worker, but original IO still owns all buffers.
    fn connect(&self) -> Result<Box<dyn Stream>>;
}
/// Unix-domain host proxy connector with mandatory positive timeout.
pub struct Unix {
    path: PathBuf,
    timeout: Duration,
}
impl Unix {
    /// Configure an absolute owned host proxy endpoint and finite IO timeout.
    pub fn new(path: PathBuf, timeout: Duration) -> Result<Self> {
        if !path.is_absolute() || timeout.is_zero() {
            return Err(Error::Invalid("object socket path/deadline"));
        }
        Ok(Self { path, timeout })
    }
}
impl Connector for Unix {
    fn connect(&self) -> Result<Box<dyn Stream>> {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .map_err(|e| io("create object Unix socket", e))?;
        let address =
            socket2::SockAddr::unix(&self.path).map_err(|e| io("object Unix socket address", e))?;
        socket
            .connect_timeout(&address, self.timeout)
            .map_err(|e| io("connect object port", e))?;
        let fd: std::os::fd::OwnedFd = socket.into();
        let socket = std::os::unix::net::UnixStream::from(fd);
        socket
            .set_read_timeout(Some(self.timeout))
            .and_then(|_| socket.set_write_timeout(Some(self.timeout)))
            .map_err(|e| io("set object socket deadline", e))?;
        Ok(Box::new(socket))
    }
}
/// Linux host-proxy vsock connector with explicit CID/port and finite deadlines.
#[cfg(target_os = "linux")]
pub struct Vsock {
    cid: u32,
    port: u32,
    timeout: Duration,
}
#[cfg(target_os = "linux")]
impl Vsock {
    /// Configure a real host proxy, never defaulting to a protected socket or endpoint.
    pub fn new(cid: u32, port: u32, timeout: Duration) -> Result<Self> {
        if port == 0 || timeout.is_zero() {
            return Err(Error::Invalid("object vsock endpoint/deadline"));
        }
        Ok(Self { cid, port, timeout })
    }
}
#[cfg(target_os = "linux")]
impl Connector for Vsock {
    fn connect(&self) -> Result<Box<dyn Stream>> {
        let socket = socket2::Socket::new(socket2::Domain::VSOCK, socket2::Type::STREAM, None)
            .map_err(|e| io("create object vsock", e))?;
        socket
            .connect_timeout(&socket2::SockAddr::vsock(self.cid, self.port), self.timeout)
            .map_err(|e| io("connect object vsock", e))?;
        socket
            .set_read_timeout(Some(self.timeout))
            .and_then(|_| socket.set_write_timeout(Some(self.timeout)))
            .map_err(|e| io("set object vsock deadline", e))?;
        let fd: std::os::fd::OwnedFd = socket.into();
        Ok(Box::new(std::os::unix::net::UnixStream::from(fd)))
    }
}
/// A requester-bound object client. Capability is shared through Arc, never secret Clone
/// or hidden credentials on disk. Inject the same exact verifier into run-facing Governed.
pub struct Client {
    connector: Arc<dyn Connector>,
    access: Arc<Access>,
    arenas: Arc<dyn Arenas>,
}
impl Client {
    /// Bind one actual requesting ledger and opaque capability; no permissive default.
    pub fn new(
        connector: Arc<dyn Connector>,
        access: Arc<Access>,
        arenas: Arc<dyn Arenas>,
    ) -> Result<Self> {
        Scope::validate_parts(&access.workspace, &access.run)?;
        Ok(Self {
            connector,
            access,
            arenas,
        })
    }
    /// Construct the run-facing governed port with actual remote exact-action verification,
    /// not an allow-all local adapter. The object service independently verifies again.
    pub fn governed(self: Arc<Self>) -> Result<Governed> {
        Governed::new(self.clone(), self)
    }
    async fn authorize_remote(
        &self,
        access: &Access,
        object: &Object,
        action: Action,
        version: Option<&Version>,
    ) -> Result<()> {
        use moruna_storage_format::auth::{Authorization, Verdict};
        let scope = Scope {
            workspace: access.workspace.clone(),
            run: access.run.clone(),
        };
        self.scope(&scope)?;
        let arena = self.arenas.allocator(&scope)?;
        let mut control = SecretControl(allocate(arena.as_ref(), wire::MAX_HEADER)?);
        let authorization = Authorization::new(
            &scope,
            object,
            action,
            access.credential.application().ok_or(Error::Denied)?,
        )
        .map_err(from_refusal)?;
        let authorization = match version {
            Some(version) => authorization.with_version(version).map_err(from_refusal)?,
            None => authorization,
        };
        let length = authorization.encode_into(1, control.as_mut())?;
        let mut reply = allocate(arena.as_ref(), wire::MAX_HEADER)?;
        let connector = self.connector.clone();
        tokio::task::spawn_blocking(move || {
            let mut socket = connector.connect()?;
            socket
                .write_all(&control.as_ref()[..length])
                .map_err(|e| io("send exact object authorization", e))?;
            control.as_mut().fill(0);
            let length = read_header(socket.as_mut(), reply.as_mut())?;
            let verdict = Verdict::decode(&reply.as_ref()[..length])?;
            if verdict.id != 1 {
                return Err(Error::Integrity);
            }
            verdict.result.map_err(from_refusal)
        })
        .await
        .map_err(|_| Error::Unavailable("owned object authority worker"))?
    }
    fn scope(&self, scope: &Scope) -> Result<()> {
        scope.validate()?;
        if scope.workspace != self.access.workspace || scope.run != self.access.run {
            return Err(Error::Denied);
        }
        Ok(())
    }
    fn call(
        &self,
        scope: &Scope,
        object: &Object,
        command: Command,
        body: Option<BufferView>,
        destination: Option<Buffer>,
    ) -> Operation<'static, (Outcome, Option<Buffer>)> {
        let check = self.scope(scope);
        let scope = scope.clone();
        let object = object.clone();
        let connector = self.connector.clone();
        let access = self.access.clone();
        let arenas = self.arenas.clone();
        Box::pin(async move {
            check?;
            if body.as_ref().is_some_and(|b| b.as_host_slice().is_none())
                || destination.as_ref().is_some_and(|d| d.host_ptr().is_none())
            {
                return Err(Error::Invalid("object port host payload"));
            }
            let arena = arenas.allocator(&scope)?;
            let header = allocate(arena.as_ref(), wire::MAX_HEADER)?;
            let reply = allocate(arena.as_ref(), wire::MAX_HEADER)?;
            tokio::task::spawn_blocking(move || {
                let mut header = SecretControl(header);
                let mut reply = reply;
                let id = 1; // One request per owned socket; no multiplexed id reuse.
                let request = Request {
                    id,
                    scope,
                    object,
                    expected_role: None,
                    proof: moruna_storage_format::auth::Proof::Application(
                        access.credential.application().ok_or(Error::Denied)?,
                    ),
                    operation: command,
                };
                let encoded = request.encode_into(header.as_mut())?;
                let mut socket = connector.connect()?;
                socket
                    .write_all(&header.as_ref()[..encoded])
                    .map_err(|e| io("send object header", e))?;
                // Wipe control credentials as soon as their send has completed.
                header.as_mut().fill(0);
                if let Some(src) = body {
                    read_header(socket.as_mut(), reply.as_mut())?;
                    let ready =
                        Response::decode(&reply.as_ref()[..header_length(reply.as_ref())?])?;
                    if ready.id != id {
                        return Err(Error::Integrity);
                    }
                    match ready.outcome {
                        Outcome::Ready { length } if length == src.len() as u64 => {}
                        Outcome::Refused(code) => return Err(from_refusal(code)),
                        _ => return Err(Error::Integrity),
                    }
                    socket
                        .write_all(
                            src.as_host_slice()
                                .ok_or(Error::Invalid("object port host upload"))?,
                        )
                        .map_err(|e| io("send object body", e))?;
                }
                read_header(socket.as_mut(), reply.as_mut())?;
                let response = Response::decode(&reply.as_ref()[..header_length(reply.as_ref())?])?;
                if response.id != id {
                    return Err(Error::Integrity);
                }
                if let Outcome::Refused(refusal) = response.outcome {
                    return Err(from_refusal(refusal));
                }
                validate_reply(&request, &response.outcome)?;
                let destination = match (destination, &response.outcome) {
                    (Some(mut dst), Outcome::Read { length, .. })
                        if dst.len() as u64 == *length =>
                    {
                        socket
                            .read_exact(dst.as_mut())
                            .map_err(|e| io("receive object body into arena", e))?;
                        Some(dst)
                    }
                    (None, Outcome::Read { .. }) => return Err(Error::Integrity),
                    (None, _) => None,
                    _ => return Err(Error::Integrity),
                };
                Ok((response.outcome, destination))
            })
            .await
            .map_err(|_| Error::Unavailable("owned object socket worker"))?
        })
    }
}
fn validate_reply(request: &Request<'_>, outcome: &Outcome) -> Result<()> {
    match (&request.operation, outcome) {
        (Command::Current, Outcome::Head(metadata)) if metadata.object == request.object => Ok(()),
        (Command::Head { version }, Outcome::Head(metadata))
            if metadata.object == request.object && metadata.version == *version =>
        {
            Ok(())
        }
        (
            Command::Read {
                version,
                offset,
                length,
            },
            Outcome::Read {
                metadata,
                offset: got,
                length: bytes,
            },
        ) if metadata.object == request.object
            && metadata.version == *version
            && got == offset
            && bytes == length =>
        {
            Ok(())
        }
        (
            Command::Create { length } | Command::Replace { length, .. },
            Outcome::Written(metadata),
        ) if metadata.object == request.object && metadata.bytes == *length => Ok(()),
        (Command::List { limit, after }, Outcome::Listed { entries, next }) => {
            if entries.len() > *limit as usize {
                return Err(Error::Integrity);
            }
            let mut previous = after.as_deref();
            for entry in entries {
                if entry.object.workspace != request.object.workspace
                    || entry.object.bucket != request.object.bucket
                    || !entry.object.key.starts_with(&request.object.key)
                    || previous.is_some_and(|p| p >= entry.object.key.as_str())
                {
                    return Err(Error::Integrity);
                }
                previous = Some(&entry.object.key);
            }
            if next.is_some() && next.as_deref() != entries.last().map(|m| m.object.key.as_str()) {
                return Err(Error::Integrity);
            }
            Ok(())
        }
        _ => Err(Error::Integrity),
    }
}
fn allocate(arena: &dyn Allocator, bytes: usize) -> Result<Buffer> {
    crate::allocate_exact(arena, bytes)
}
fn header_length(header: &[u8]) -> Result<usize> {
    wire::header_bytes(header).map_err(Into::into)
}
fn read_header(stream: &mut dyn Stream, destination: &mut [u8]) -> Result<usize> {
    if destination.len() < 16 {
        return Err(Error::Invalid("object header destination"));
    }
    stream
        .read_exact(&mut destination[..16])
        .map_err(|e| io("receive object header prefix", e))?;
    let len = header_length(destination)?;
    stream
        .read_exact(
            destination
                .get_mut(16..len)
                .ok_or(Error::Invalid("object header capacity"))?,
        )
        .map_err(|e| io("receive object header", e))?;
    Ok(len)
}
impl Authorizer for Client {
    fn verify<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        action: Action,
        version: Option<&'a Version>,
    ) -> Operation<'a, ()> {
        Box::pin(self.authorize_remote(access, object, action, version))
    }
}
impl Backend for Client {
    fn role(&self) -> Option<moruna_storage_format::auth::SessionRole> {
        None
    }

    fn current<'a>(&'a self, scope: &'a Scope, object: &'a Object) -> Operation<'a, Metadata> {
        let call = self.call(scope, object, Command::Current, None, None);
        Box::pin(async move {
            match call.await?.0 {
                Outcome::Head(metadata) => Ok(metadata),
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
        let operation = self.call(
            scope,
            object,
            Command::Head {
                version: version.clone(),
            },
            None,
            None,
        );
        Box::pin(async move {
            match operation.await?.0 {
                Outcome::Head(metadata) => Ok(metadata),
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
        let operation = self.call(
            scope,
            object,
            Command::Read {
                version: version.clone(),
                offset,
                length: dst.len() as u64,
            },
            None,
            Some(dst),
        );
        Box::pin(async move { operation.await?.1.ok_or(Error::Integrity) })
    }
    fn write<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        src: BufferView,
    ) -> Operation<'a, Metadata> {
        let operation = self.call(
            scope,
            object,
            Command::Create {
                length: src.len() as u64,
            },
            Some(src),
            None,
        );
        Box::pin(async move {
            match operation.await?.0 {
                Outcome::Written(metadata) => Ok(metadata),
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
        let operation = self.call(
            scope,
            object,
            Command::Replace {
                condition: match condition {
                    Condition::Absent => wire::Condition::Absent,
                    Condition::Match(v) => wire::Condition::Match(v),
                },
                length: src.len() as u64,
            },
            Some(src),
            None,
        );
        Box::pin(async move {
            match operation.await?.0 {
                Outcome::Written(metadata) => Ok(metadata),
                _ => Err(Error::Integrity),
            }
        })
    }
    fn list<'a>(&'a self, scope: &'a Scope, prefix: &'a Object) -> Operation<'a, Vec<Metadata>> {
        Box::pin(async move {
            let mut entries = Vec::new();
            let mut after = None;
            loop {
                let response = self
                    .call(
                        scope,
                        prefix,
                        Command::List {
                            limit: 64,
                            after: after.clone(),
                        },
                        None,
                        None,
                    )
                    .await?
                    .0;
                match response {
                    Outcome::Listed {
                        entries: page,
                        next,
                    } => {
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
/// Host service for the same governed port. Call on an owned blocking socket worker with
/// a live async runtime handle; never on the async executor thread. Transport owner sets
/// finite IO deadlines and admits `control` before accepting any untrusted header.
pub struct Service {
    store: Arc<Governed>,
    arenas: Arc<dyn Arenas>,
    body_limit: u64,
}
impl Service {
    /// Bound whole-object request allocation before accepting payload bytes.
    pub fn new(store: Arc<Governed>, arenas: Arc<dyn Arenas>, body_limit: u64) -> Result<Self> {
        if body_limit == 0 {
            return Err(Error::Invalid("object service body ceiling"));
        }
        Ok(Self {
            store,
            arenas,
            body_limit,
        })
    }
    /// Serve one owned connection, with exact live verification before body/object IO.
    /// On failure a typed refusal is sent and the original diagnostic is returned.
    pub async fn serve(self: Arc<Self>, socket: Box<dyn Stream>, control: Buffer) -> Result<()> {
        if control.host_ptr().is_none() || control.len() < wire::MAX_HEADER {
            return Err(Error::Invalid("object service charged control buffer"));
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Unavailable("object service async runtime"))?;
        tokio::task::spawn_blocking(move || self.serve_one(&runtime, socket, control))
            .await
            .map_err(|_| Error::Unavailable("owned object service worker"))?
    }
    fn serve_one(
        &self,
        runtime: &tokio::runtime::Handle,
        mut socket: Box<dyn Stream>,
        control: Buffer,
    ) -> Result<()> {
        let mut control = SecretControl(control);
        let len = read_header(socket.as_mut(), control.as_mut())?;
        if let Ok(query) =
            moruna_storage_format::provider::Request::decode(&control.as_ref()[..len])
        {
            let bytes = moruna_storage_format::provider::Description {
                id: query.id,
                role: self.store.role(),
            }
            .encode_into(control.as_mut())?;
            socket
                .write_all(&control.as_ref()[..bytes])
                .map_err(|e| io("send actual provider description", e))?;
            return Ok(());
        }
        if control.as_ref().get(16) == Some(&3) {
            return self.authorize_one(runtime, socket.as_mut(), control, len);
        }
        let request = Request::decode(&control.as_ref()[..len])?;
        if request.proof.native_session() {
            let id = request.id;
            drop(request);
            control.as_mut().fill(0);
            let size = Response {
                id,
                outcome: Outcome::Refused(Refusal::Denied),
            }
            .encode_into(control.as_mut())?;
            socket
                .write_all(&control.as_ref()[..size])
                .map_err(|e| io("refuse socket native-session object", e))?;
            return Err(Error::Denied);
        }
        let id = request.id;
        let outcome = self.execute(runtime, socket.as_mut(), &request);
        let arena = self.arenas.allocator(&request.scope)?;
        let mut reply = allocate(arena.as_ref(), wire::MAX_HEADER)?;
        match outcome {
            Ok((outcome, body)) => {
                let len = Response { id, outcome }.encode_into(reply.as_mut())?;
                socket
                    .write_all(&reply.as_ref()[..len])
                    .map_err(|e| io("send object response", e))?;
                if let Some(body) = body {
                    socket
                        .write_all(body.as_ref())
                        .map_err(|e| io("send object response body", e))?;
                }
                control.as_mut().fill(0);
                Ok(())
            }
            Err(primary) => {
                let len = Response {
                    id,
                    outcome: Outcome::Refused(refusal(&primary)),
                }
                .encode_into(reply.as_mut())?;
                if let Err(e) = socket.write_all(&reply.as_ref()[..len]) {
                    return Err(Error::Cleanup {
                        primary: Box::new(primary),
                        cleanup: Box::new(io("send object refusal", e)),
                    });
                }
                control.as_mut().fill(0);
                Err(primary)
            }
        }
    }
    fn authorize_one(
        &self,
        runtime: &tokio::runtime::Handle,
        socket: &mut dyn Stream,
        mut control: SecretControl,
        length: usize,
    ) -> Result<()> {
        use moruna_storage_format::auth::{DecodedAuthorization, Verdict};
        let decoded = DecodedAuthorization::decode(&control.as_ref()[..length])?;
        let authorization = decoded.authorization();
        if authorization.proof().native_session() {
            let id = decoded.id();
            drop(decoded);
            control.as_mut().fill(0);
            let size = Verdict {
                id,
                result: Err(Refusal::Denied),
            }
            .encode_into(control.as_mut())?;
            socket
                .write_all(&control.as_ref()[..size])
                .map_err(|e| io("refuse socket native-session marker", e))?;
            return Err(Error::Denied);
        }
        let access = Access {
            workspace: authorization.scope().workspace.clone(),
            run: authorization.scope().run.clone(),
            credential: Credential::new(
                authorization
                    .proof()
                    .application()
                    .ok_or(Error::Denied)?
                    .to_vec(),
            )?,
        };
        let result = runtime.block_on(self.store.authorize(
            &access,
            authorization.object(),
            authorization.action(),
            authorization.version(),
        ));
        let verdict = Verdict {
            id: decoded.id(),
            result: result.as_ref().map(|_| ()).map_err(refusal),
        };
        drop(decoded);
        control.as_mut().fill(0);
        let encoded = verdict.encode_into(control.as_mut())?;
        socket
            .write_all(&control.as_ref()[..encoded])
            .map_err(|error| io("send exact object authority verdict", error))?;
        result
    }
    fn execute(
        &self,
        runtime: &tokio::runtime::Handle,
        socket: &mut dyn Stream,
        request: &Request<'_>,
    ) -> Result<(Outcome, Option<Buffer>)> {
        if request
            .expected_role
            .is_some_and(|role| self.store.role() != Some(role))
        {
            return Err(Error::Remote(Refusal::Unsupported));
        }
        let access = Access {
            workspace: request.scope.workspace.clone(),
            run: request.scope.run.clone(),
            credential: Credential::new(
                request.proof.application().ok_or(Error::Denied)?.to_vec(),
            )?,
        };
        let action = match request.operation {
            Command::Current
            | Command::Head { .. }
            | Command::Read { .. }
            | Command::CacheRead { .. } => Action::Read,
            Command::Create { .. } | Command::CacheFill { .. } => Action::Write,
            Command::List { .. } => Action::List,
            Command::Replace { .. } => Action::Publish,
        };
        runtime.block_on(self.store.authorize(
            &access,
            &request.object,
            action,
            match &request.operation {
                Command::Head { version }
                | Command::Read { version, .. }
                | Command::Replace {
                    condition: wire::Condition::Match(version),
                    ..
                } => Some(version),
                _ => None,
            },
        ))?;
        let arena = self.arenas.allocator(&request.scope)?;
        match &request.operation {
            Command::CacheRead { .. } | Command::CacheFill { .. } => {
                Err(Error::Remote(Refusal::Unsupported))
            }
            Command::Current => Ok((
                Outcome::Head(runtime.block_on(self.store.current(&access, &request.object))?),
                None,
            )),
            Command::Head { version } => Ok((
                Outcome::Head(runtime.block_on(self.store.head(
                    &access,
                    &request.object,
                    version,
                ))?),
                None,
            )),
            Command::Read {
                version,
                offset,
                length,
            } => {
                if *length > self.body_limit {
                    return Err(Error::Unavailable("object range body ceiling"));
                }
                let metadata =
                    runtime.block_on(self.store.head(&access, &request.object, version))?;
                let bytes =
                    usize::try_from(*length).map_err(|_| Error::Invalid("object range width"))?;
                let body = runtime.block_on(self.store.read(
                    &access,
                    &request.object,
                    version,
                    *offset,
                    allocate(arena.as_ref(), bytes)?,
                ))?;
                Ok((
                    Outcome::Read {
                        metadata,
                        offset: *offset,
                        length: *length,
                    },
                    Some(body),
                ))
            }
            Command::Create { length } | Command::Replace { length, .. } => {
                if *length > self.body_limit {
                    return Err(Error::Unavailable("object write body ceiling"));
                }
                let bytes =
                    usize::try_from(*length).map_err(|_| Error::Invalid("object write width"))?;
                let mut body = allocate(arena.as_ref(), bytes)?;
                let mut ready = allocate(arena.as_ref(), wire::MAX_HEADER)?;
                let ready_len = Response {
                    id: request.id,
                    outcome: Outcome::Ready { length: *length },
                }
                .encode_into(ready.as_mut())?;
                socket
                    .write_all(&ready.as_ref()[..ready_len])
                    .map_err(|e| io("admit object request body", e))?;
                socket
                    .read_exact(body.as_mut())
                    .map_err(|e| io("receive object write into arena", e))?;
                let body = Arc::new(body);
                let metadata = match &request.operation {
                    Command::Create { .. } => {
                        runtime.block_on(self.store.write(&access, &request.object, body.view()))?
                    }
                    Command::Replace { condition, .. } => runtime.block_on(self.store.replace(
                        &access,
                        &request.object,
                        match condition {
                            wire::Condition::Absent => Condition::Absent,
                            wire::Condition::Match(v) => Condition::Match(v.clone()),
                        },
                        body.view(),
                    ))?,
                    _ => return Err(Error::Integrity),
                };
                Ok((Outcome::Written(metadata), None))
            }
            Command::List { limit, after } => {
                let mut all = runtime.block_on(self.store.list(&access, &request.object))?;
                all.sort_by(|a, b| a.object.key.cmp(&b.object.key));
                let mut rows = all
                    .into_iter()
                    .filter(|m| after.as_deref().is_none_or(|p| m.object.key.as_str() > p));
                let mut entries = Vec::new();
                for _ in 0..*limit {
                    if let Some(row) = rows.next() {
                        entries.push(row)
                    } else {
                        break;
                    }
                }
                let next = rows
                    .next()
                    .and_then(|_| entries.last().map(|m| m.object.key.clone()));
                Ok((Outcome::Listed { entries, next }, None))
            }
        }
    }
}
/// Canonical public refusal classification for native transport and CLI/SDK/MCP adapters.
/// The compact wire code does not invent capacity numbers or checkpoint receipts; inspect
/// Error::capacity and Error::local_checkpoint for those actual typed facts when supported.
pub fn refusal(error: &Error) -> Refusal {
    match error {
        Error::Denied => Refusal::Denied,
        Error::Missing => Refusal::Missing,
        Error::Conflict => Refusal::Conflict,
        Error::Integrity | Error::Format(moruna_storage_format::Error::Integrity) => {
            Refusal::Integrity
        }
        Error::Invalid(_) | Error::Format(_) => Refusal::Invalid,
        Error::CaptureCapacity { .. }
        | Error::StagingRefused { .. }
        | Error::StagingPending { .. } => Refusal::Capacity,
        Error::Remote(code) => *code,
        _ => Refusal::Unavailable,
    }
}

// Every return path wipes the complete credential-bearing arena page before release.
struct SecretControl(Buffer);
impl SecretControl {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
    fn as_mut(&mut self) -> &mut [u8] {
        self.0.as_mut()
    }
}
impl Drop for SecretControl {
    fn drop(&mut self) {
        self.0.as_mut().fill(0);
    }
}

/// Canonical wire refusal conversion shared by native/socket/SDK consumers. Only a
/// genuine Missing becomes Error::Missing; denial/failure never becomes absence.
pub fn from_refusal(code: Refusal) -> Error {
    match code {
        Refusal::Denied => Error::Denied,
        Refusal::Missing => Error::Missing,
        Refusal::Conflict => Error::Conflict,
        Refusal::Integrity => Error::Integrity,
        _ => Error::Remote(code),
    }
}
