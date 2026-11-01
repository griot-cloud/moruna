//! Actual connected namespace consumer. SDK staging copies are real and explicit: these
//! are NOT caller-arena DMA claims. Dropping a request closes a real cancellation peer;
//! its owned worker and service keep original loans until reply or safe driver drain.
use super::{Client, rounded};
use crate::{Access, Buffer, Error, Result, Scope};
use moruna_storage_format::{
    auth::SessionRole,
    namespace as format,
    wire::{self, Refusal},
};
use moruna_storage_service::protocol::{namespace as protocol, object};
use moruna_svc::{Channel, ClientCore, Encoder, Handle, Memory, ObjectType, Rights, Wire};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
/// An actual connected namespace, returned ONLY after account/scope/role/root attestation.
pub struct Namespace {
    parent: Arc<Client>,
    core: Arc<Mutex<Option<ClientCore>>>,
    session: format::Session,
    receipt: Mutex<crate::Metadata>,
    inflight: std::sync::atomic::AtomicUsize,
}
/// Per-step actual facts plus original charged caller allocation. A transport success
/// does NOT imply all steps succeeded: Refused/Cancelled/Skipped remain in the response.
pub struct Transfer {
    /// Exact submitted operations, retained with partial/short outcomes.
    pub request: format::Request,
    /// Canonical actual outcomes and last real root receipt.
    pub response: format::Response,
    /// Original caller allocation, with valid read prefixes named by actual byte counts.
    pub buffer: Option<Buffer>,
}
impl Transfer {
    /// First actual failure/cancellation, without erasing this transfer's partial facts.
    pub fn failure(&self) -> Option<Error> {
        self.request
            .steps
            .iter()
            .zip(&self.response.outcomes)
            .find_map(|(step, outcome)| match (&step.command, outcome) {
                (
                    format::Command::WriteAt { length, .. },
                    format::Outcome::Io(format::Io::Done(bytes)),
                ) if bytes < length => Some(Error::Io {
                    operation: "namespace short write",
                    source: std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        format!("{bytes} of {length} bytes"),
                    ),
                }),
                (
                    _,
                    format::Outcome::Refused(code)
                    | format::Outcome::Io(format::Io::Failed(_, code)),
                ) => Some(crate::port::from_refusal(*code)),
                (_, format::Outcome::Cancelled | format::Outcome::Io(format::Io::Cancelled(_))) => {
                    Some(Error::Cancelled)
                }
                (_, format::Outcome::Skipped) => Some(Error::Unavailable(
                    "linked namespace step was not submitted",
                )),
                _ => None,
            })
    }
}
struct Submission(Arc<Namespace>);
impl Drop for Submission {
    fn drop(&mut self) {
        self.0
            .inflight
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
struct Cancel {
    peer: Mutex<Option<Channel>>,
}
impl Cancel {
    fn cancel(&self) {
        if let Ok(mut peer) = self.peer.lock() {
            peer.take();
        }
    }
}
/// Awaitable actual request. Cancel is intent only; awaiting still obtains the ORIGINAL
/// server outcome. A dropped receiver never fabricates a completion or reuses DMA memory.
pub struct Pending {
    cancel: Arc<Cancel>,
    future: Pin<Box<dyn Future<Output = Result<Transfer>> + Send>>,
}
impl Pending {
    /// Close the actual cancellation peer; no shared mapped flag or fake CQE is used.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}
impl Future for Pending {
    type Output = Result<Transfer>;
    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(context)
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl Client {
    /// Open through the existing actual local bound service. BucketHome/cache providers
    /// refuse; no socket, file path, default key or new credential is substituted.
    pub async fn open_namespace(
        self: &Arc<Self>,
        access: &Access,
        open: format::OpenRequest,
        timeout_ns: u64,
    ) -> Result<Arc<Namespace>> {
        self.check(&Scope {
            workspace: access.workspace.clone(),
            run: access.run.clone(),
        })?;
        if !access.credential.contains_native(&self.connection)
            || !matches!(
                self.role(),
                SessionRole::SingleNodeHome | SessionRole::CheckpointMetadata
            )
        {
            return Err(Error::Denied);
        }
        if timeout_ns == 0 || timeout_ns == u64::MAX {
            return Err(Error::Invalid("namespace timeout"));
        }
        let arena = self.arenas.allocator(&self.scope)?;
        let mut control = crate::allocate_exact(arena.as_ref(), wire::MAX_HEADER)?;
        let length = open.encode_into(control.as_mut())?;
        let parent = self.clone();
        tokio::task::spawn_blocking(move || {
            let account = &parent.connection.account.handle.0;
            let memory = Memory::create_in(length.max(1) as u64, account).map_err(super::sdk)?;
            memory
                .map(0, rounded(length)?, moruna_svc::map::WRITE)
                .map_err(super::sdk)?
                .write(0, &control.as_ref()[..length])
                .map_err(super::sdk)?;
            let mut body = Encoder::new();
            Memory(
                memory
                    .0
                    .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
                    .map_err(super::sdk)?,
            )
            .encode(&mut body)
            .map_err(|_| Error::Integrity)?;
            (length as u32)
                .encode(&mut body)
                .map_err(|_| Error::Integrity)?;
            let channel = {
                let mut guard = parent
                    .connection
                    .core
                    .lock()
                    .map_err(|_| Error::Unavailable("namespace parent channel poisoned"))?;
                let core = guard
                    .as_mut()
                    .ok_or(Error::Unavailable("namespace parent channel closed"))?;
                let frame = core.call(3, body);
                let mut frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => {
                        let known = matches!(&error, moruna_svc::CallError::Remote(_));
                        let result = open_error(error.typed());
                        if !known {
                            *guard = None;
                        }
                        return Err(result);
                    }
                };
                let mut decoder = frame.decoder();
                let channel = Channel::decode(&mut decoder).map_err(|_| Error::Integrity)?;
                decoder.finish().map_err(|_| Error::Integrity)?;
                channel
            };
            let mut core =
                ClientCore::connect(channel, protocol::ID, moruna_svc::deadline_in(timeout_ns))
                    .map_err(|_| Error::Unavailable("namespace handshake unacknowledged"))?;
            core.set_timeout(timeout_ns);
            let reply = Memory::create_in(wire::MAX_HEADER as u64, account).map_err(super::sdk)?;
            let mapping = reply
                .map(0, wire::MAX_HEADER as u64, 0)
                .map_err(super::sdk)?;
            let mut body = Encoder::new();
            Memory(
                reply
                    .0
                    .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
                    .map_err(super::sdk)?,
            )
            .encode(&mut body)
            .map_err(|_| Error::Integrity)?;
            (wire::MAX_HEADER as u32)
                .encode(&mut body)
                .map_err(|_| Error::Integrity)?;
            let mut frame = core.call(2, body).map_err(|error| remote(error.typed()))?;
            let mut decoder = frame.decoder();
            let length = u32::decode(&mut decoder).map_err(|_| Error::Integrity)? as usize;
            let bound = Handle::decode(&mut decoder).map_err(|_| Error::Integrity)?;
            decoder.finish().map_err(|_| Error::Integrity)?;
            let (kind, rights) = bound.info().map_err(super::sdk)?;
            if kind != ObjectType::Account
                || !rights.contains(Rights::READ)
                || !parent
                    .connection
                    .account
                    .handle
                    .matches(&bound)
                    .map_err(super::sdk)?
                || length > wire::MAX_HEADER
            {
                return Err(Error::Denied);
            }
            mapping
                .read(0, &mut control.as_mut()[..length])
                .map_err(super::sdk)?;
            let session = format::Session::decode(&control.as_ref()[..length])?;
            if session.admission.scope != parent.scope
                || session.admission.role != parent.role()
                || session.receipt.object != open.root
                || session.limits != open.limits
            {
                return Err(Error::Denied);
            }
            Ok(Arc::new(Namespace {
                receipt: Mutex::new(session.receipt.clone()),
                session,
                parent,
                core: Arc::new(Mutex::new(Some(core))),
                inflight: std::sync::atomic::AtomicUsize::new(0),
            }))
        })
        .await
        .map_err(|_| Error::Unavailable("namespace opener owned worker"))?
    }
}
impl Namespace {
    /// Actual attested root/limits/scope, never caller labels minted into permissions.
    pub fn session(&self) -> &format::Session {
        &self.session
    }
    /// Exact last acknowledged root, retaining partial batch facts in Transfer as well.
    pub fn receipt(&self) -> Result<crate::Metadata> {
        self.receipt
            .lock()
            .map(|receipt| receipt.clone())
            .map_err(|_| Error::Unavailable("namespace receipt poisoned"))
    }
    /// Admit one bounded actual request; the caller supplies the SAME charged payload
    /// allocation named by canonical range offsets. Uses the existing Tokio owned-worker
    /// path of the native SDK; no nested runtime is constructed on an engine worker.
    pub fn submit(
        self: &Arc<Self>,
        access: Arc<Access>,
        request: format::Request,
        buffer: Option<Buffer>,
    ) -> Result<Pending> {
        self.parent.check(&Scope {
            workspace: access.workspace.clone(),
            run: access.run.clone(),
        })?;
        if !access.credential.contains_native(&self.parent.connection) {
            return Err(Error::Denied);
        }
        self.inflight
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |count| {
                    count
                        .checked_add(1)
                        .filter(|next| *next <= format::MAX_STEPS)
                },
            )
            .map_err(|_| Error::Remote(Refusal::Busy))?;
        let permit = Submission(self.clone());
        let (bytes, writable) = request.payload()?;
        if bytes > self.session.limits.transfer_bytes
            || buffer.as_ref().map(Buffer::len).unwrap_or(0) as u64 != bytes
        {
            return Err(Error::Invalid("namespace charged payload span"));
        }
        if buffer
            .as_ref()
            .is_some_and(|buffer| buffer.host_ptr().is_none())
        {
            return Err(Error::Invalid("namespace host staging"));
        }
        let arena = self.parent.arenas.allocator(&self.parent.scope)?;
        let mut control = crate::allocate_exact(arena.as_ref(), wire::MAX_HEADER)?;
        let reply = crate::allocate_exact(arena.as_ref(), wire::MAX_HEADER)?;
        let length = request.encode_into(control.as_mut())?;
        let (client_cancel, server_cancel) = Channel::create().map_err(super::sdk)?;
        let cancel = Arc::new(Cancel {
            peer: Mutex::new(Some(client_cancel)),
        });
        let namespace = self.clone();
        let owned_cancel = cancel.clone();
        let future = Box::pin(async move {
            // Cancellation peer stays caller-owned. The detached blocking worker retains
            // the original payload/headers/actual Account independently of this receiver.
            let result = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                namespace.exchange(
                    request,
                    control,
                    length,
                    reply,
                    buffer,
                    writable,
                    server_cancel,
                )
            })
            .await
            .map_err(|_| Error::Unavailable("namespace original owned worker"))?;
            owned_cancel.cancel();
            result
        });
        Ok(Pending { cancel, future })
    }
    /// Await one request without discarding per-step refusals or cancellation facts.
    pub async fn execute(
        self: &Arc<Self>,
        access: Arc<Access>,
        request: format::Request,
        buffer: Option<Buffer>,
    ) -> Result<Transfer> {
        self.submit(access, request, buffer)?.await
    }
    fn exchange(
        &self,
        request: format::Request,
        control: Buffer,
        length: usize,
        mut reply: Buffer,
        mut buffer: Option<Buffer>,
        writable: bool,
        cancel: Channel,
    ) -> Result<Transfer> {
        let account = &self.parent.connection.account.handle.0;
        let header = Memory::create_in(length.max(1) as u64, account).map_err(super::sdk)?;
        header
            .map(0, rounded(length)?, moruna_svc::map::WRITE)
            .map_err(super::sdk)?
            .write(0, &control.as_ref()[..length])
            .map_err(super::sdk)?;
        let response = Memory::create_in(wire::MAX_HEADER as u64, account).map_err(super::sdk)?;
        let response_map = response
            .map(0, wire::MAX_HEADER as u64, 0)
            .map_err(super::sdk)?;
        let payload = buffer
            .as_ref()
            .map(|buffer| {
                Memory::create_dma_in(buffer.len().max(1) as u64, account).map_err(super::sdk)
            })
            .transpose()?;
        let payload_map = payload
            .as_ref()
            .map(|payload| {
                payload
                    .map(
                        0,
                        rounded(buffer.as_ref().map_or(0, Buffer::len))?,
                        moruna_svc::map::WRITE,
                    )
                    .map_err(super::sdk)
            })
            .transpose()?;
        if !writable {
            if let (Some(mapping), Some(buffer)) = (&payload_map, &buffer) {
                mapping.write(0, buffer.as_ref()).map_err(super::sdk)?;
                self.parent
                    .connection
                    .copied_in
                    .fetch_add(buffer.len() as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let mut body = Encoder::new();
        Memory(
            header
                .0
                .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
                .map_err(super::sdk)?,
        )
        .encode(&mut body)
        .map_err(|_| Error::Integrity)?;
        (length as u32)
            .encode(&mut body)
            .map_err(|_| Error::Integrity)?;
        let rights = Rights::READ
            | Rights::MAP
            | Rights::TRANSFER
            | Rights::DUPLICATE
            | if writable {
                Rights::WRITE
            } else {
                Rights::READ
            };
        payload
            .as_ref()
            .map(|payload| payload.0.duplicate(rights).map(Memory).map_err(super::sdk))
            .transpose()?
            .encode(&mut body)
            .map_err(|_| Error::Integrity)?;
        0u64.encode(&mut body).map_err(|_| Error::Integrity)?;
        cancel.encode(&mut body).map_err(|_| Error::Integrity)?;
        Memory(
            response
                .0
                .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
                .map_err(super::sdk)?,
        )
        .encode(&mut body)
        .map_err(|_| Error::Integrity)?;
        (wire::MAX_HEADER as u32)
            .encode(&mut body)
            .map_err(|_| Error::Integrity)?;
        let mut guard = self
            .core
            .lock()
            .map_err(|_| Error::Unavailable("namespace channel poisoned"))?;
        let core = guard.as_mut().ok_or(Error::Unavailable(
            "namespace channel closed after uncertain operation",
        ))?;
        let mut frame = match core.call(1, body) {
            Ok(frame) => frame,
            Err(error) => {
                let known = matches!(&error, moruna_svc::CallError::Remote(_));
                let error = remote(error.typed());
                if !known {
                    *guard = None;
                }
                return Err(error);
            }
        };
        let result = (|| {
            let mut decoder = frame.decoder();
            let length = u32::decode(&mut decoder).map_err(|_| Error::Integrity)? as usize;
            let span = u64::decode(&mut decoder).map_err(|_| Error::Integrity)?;
            decoder.finish().map_err(|_| Error::Integrity)?;
            if length > wire::MAX_HEADER
                || span > buffer.as_ref().map_or(0, Buffer::len) as u64
                || !writable && span != 0
            {
                return Err(Error::Integrity);
            }
            response_map
                .read(0, &mut reply.as_mut()[..length])
                .map_err(super::sdk)?;
            let response = format::Response::decode(&reply.as_ref()[..length])?;
            checked(
                &request,
                &response,
                &self.session.receipt.object,
                buffer.as_ref().map_or(0, Buffer::len) as u64,
                writable,
                span,
            )?;
            if let (Some(mapping), Some(buffer)) = (&payload_map, &mut buffer) {
                if writable && span > 0 {
                    mapping
                        .read(0, &mut buffer.as_mut()[..span as usize])
                        .map_err(super::sdk)?;
                    self.parent
                        .connection
                        .copied_out
                        .fetch_add(span, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Ok(response)
        })();
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                *guard = None;
                return Err(error);
            }
        };
        *self
            .receipt
            .lock()
            .map_err(|_| Error::Unavailable("namespace receipt poisoned"))? =
            response.receipt.clone();
        Ok(Transfer {
            request,
            response,
            buffer,
        })
    }
}
fn remote(error: moruna_svc::CallError<protocol::Error>) -> Error {
    match error {
        moruna_svc::CallError::Remote(code) => crate::port::from_refusal(match code {
            protocol::Error::BadMemory | protocol::Error::BadControl => Refusal::Invalid,
            protocol::Error::Unavailable => Refusal::Unavailable,
            protocol::Error::Denied => Refusal::Denied,
            protocol::Error::Capacity => Refusal::Capacity,
            protocol::Error::Conflict => Refusal::Conflict,
            protocol::Error::Unsupported => Refusal::Unsupported,
            protocol::Error::Missing => Refusal::Missing,
            protocol::Error::Busy => Refusal::Busy,
        }),
        moruna_svc::CallError::Malformed => Error::Integrity,
        _ => Error::Unavailable("native namespace original reply unacknowledged"),
    }
}
fn open_error(error: moruna_svc::CallError<object::Error>) -> Error {
    match error {
        moruna_svc::CallError::Remote(code) => crate::port::from_refusal(match code {
            object::Error::BadMemory | object::Error::BadControl => Refusal::Invalid,
            object::Error::Unavailable => Refusal::Unavailable,
            object::Error::Denied => Refusal::Denied,
            object::Error::Unsupported => Refusal::Unsupported,
            object::Error::Capacity => Refusal::Capacity,
            object::Error::Busy => Refusal::Busy,
        }),
        moruna_svc::CallError::Malformed => Error::Integrity,
        _ => Error::Unavailable("native namespace opener unacknowledged"),
    }
}

fn checked(
    request: &format::Request,
    response: &format::Response,
    root: &crate::Object,
    payload_bytes: u64,
    writable: bool,
    span: u64,
) -> Result<()> {
    if response.id != request.id
        || response.outcomes.len() != request.steps.len()
        || response.receipt.object != *root
        || span > payload_bytes
        || !writable && span != 0
    {
        return Err(Error::Integrity);
    }
    let mut expected = 0;
    for (step, outcome) in request.steps.iter().zip(&response.outcomes) {
        let compatible = match outcome {
            format::Outcome::Io(_) => matches!(
                step.command,
                format::Command::ReadAt { .. } | format::Command::WriteAt { .. }
            ),
            format::Outcome::Opened(_) => matches!(
                step.command,
                format::Command::Open { .. } | format::Command::Stat { .. }
            ),
            format::Outcome::Done => !matches!(
                step.command,
                format::Command::Open { .. }
                    | format::Command::Stat { .. }
                    | format::Command::ReadAt { .. }
                    | format::Command::WriteAt { .. }
            ),
            format::Outcome::Refused(_) | format::Outcome::Cancelled | format::Outcome::Skipped => {
                true
            }
        };
        if !compatible {
            return Err(Error::Integrity);
        }
        match (&step.command, outcome) {
            (
                format::Command::ReadAt {
                    length,
                    payload_offset,
                    ..
                },
                format::Outcome::Io(
                    format::Io::Done(n) | format::Io::Cancelled(n) | format::Io::Failed(n, _),
                ),
            ) => {
                if n > length {
                    return Err(Error::Integrity);
                }
                if *n > 0 {
                    expected =
                        expected.max(payload_offset.checked_add(*n).ok_or(Error::Integrity)?);
                }
            }
            (
                format::Command::WriteAt { length, .. },
                format::Outcome::Io(
                    format::Io::Done(n) | format::Io::Cancelled(n) | format::Io::Failed(n, _),
                ),
            ) if n > length => return Err(Error::Integrity),
            (_, format::Outcome::Opened(facts))
                if facts.handle == 0
                    || facts.inode == 0
                    || facts.directory && facts.length != 0 =>
            {
                return Err(Error::Integrity);
            }
            _ => {}
        }
    }
    if span != expected {
        return Err(Error::Integrity);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response(outcome: format::Outcome) -> format::Response {
        format::Response {
            id: 9,
            receipt: crate::Metadata {
                object: crate::Object {
                    workspace: "owner".into(),
                    bucket: "home".into(),
                    key: ".moruna-ns/test/root".into(),
                },
                version: crate::Version {
                    etag: Some("actual-version".into()),
                    generation: None,
                },
                bytes: 128,
            },
            outcomes: vec![outcome],
        }
    }
    fn request(read: bool) -> format::Request {
        let command = if read {
            format::Command::ReadAt {
                handle: 1,
                offset: 0,
                length: 16,
                payload_offset: 4,
            }
        } else {
            format::Command::WriteAt {
                handle: 1,
                offset: 0,
                length: 16,
                payload_offset: 0,
            }
        };
        format::Request {
            id: 9,
            steps: vec![format::Step {
                command,
                link: false,
            }],
        }
    }
    #[test]
    fn partial_reads_require_exact_acknowledged_span_and_bound_each_count() {
        let request = request(true);
        let mut reply = response(format::Outcome::Io(format::Io::Cancelled(7)));
        let root = reply.receipt.object.clone();
        assert!(checked(&request, &reply, &root, 20, true, 11).is_ok());
        assert!(checked(&request, &reply, &root, 20, true, 12).is_err());
        reply.outcomes[0] = format::Outcome::Io(format::Io::Done(17));
        assert!(checked(&request, &reply, &root, 21, true, 21).is_err());
        reply.outcomes[0] = format::Outcome::Done;
        assert!(checked(&request, &reply, &root, 20, true, 0).is_err());
        reply.id += 1;
        assert!(checked(&request, &reply, &root, 20, true, 0).is_err());
    }
    #[test]
    fn short_write_is_retained_as_actual_fact_and_reported_as_failure() {
        let request = request(false);
        let reply = response(format::Outcome::Io(format::Io::Done(7)));
        assert!(checked(&request, &reply, &reply.receipt.object, 16, false, 0).is_ok());
        let transfer = Transfer {
            request,
            response: reply,
            buffer: None,
        };
        assert!(
            matches!(transfer.failure(), Some(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::WriteZero)
        );
        assert!(matches!(
            transfer.response.outcomes[0],
            format::Outcome::Io(format::Io::Done(7))
        ));
    }
}
