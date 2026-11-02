//! Actual native bucket-home object proxy. The explicitly configured TLS endpoint runs
//! the same governed object codec backed by its real bucket adapter. Deployment and
//! application-issued credentials are required integrations, never ambient defaults.
use crate::{
    memory::Loan,
    network::{self, Network, Socket},
    session::Backend,
};
use alloc::string::String;
use moruna_storage_format::{
    Metadata,
    auth::{Proof, SessionRole},
    wire::{self, Operation, Outcome, Refusal, Request, Response},
};
use moruna_svc::{Handle, Memory, PrivateMemory};
use netstack_protocol::Target;

/// Startup diagnostic preserves actual transport/memory/control failure or wrong provider.
#[derive(Debug)]
pub enum Startup {
    /// Exact source Memory/SDK refusal.
    Memory(moruna_svc::Error),
    /// Actual network/TLS/cleanup failure.
    Network(network::Error),
    /// Canonical provider control invalid or unexpectedly correlated.
    Control(moruna_storage_format::Error),
    /// Actual remote backend is not a bucket home (unknown/fixtures refuse).
    Role(Option<SessionRole>),
    /// Invalid explicit configuration.
    Invalid,
}
/// Explicit bucket proxy and real opaque credential Memory grant. This does not issue
/// credentials or interpret their syntax. Native session requests are reauthorized by
/// the designated local authority AND by the remote application's actual verifier.
pub struct Proxy {
    network: Network,
    target: Target,
    server_name: String,
    credential: Loan,
    last_error: Option<network::Error>,
    last_receipt: Option<Metadata>,
}
impl Proxy {
    /// Require actual connected network, TLS identity and explicitly granted nonempty
    /// bounded opaque credential bytes. No local-disk or plaintext fallback exists.
    pub fn new(
        mut network: Network,
        target: Target,
        server_name: String,
        credential: Memory,
        credential_bytes: u64,
        account: &Handle,
    ) -> Result<Self, Startup> {
        if server_name.is_empty()
            || credential_bytes == 0
            || credential_bytes > wire::MAX_CAPABILITY as u64
        {
            return Err(Startup::Invalid);
        }
        probe(&mut network, target.clone(), server_name.clone(), account)?;
        let credential =
            Loan::new(credential, 0, credential_bytes, false).map_err(Startup::Memory)?;
        Ok(Self {
            network,
            target,
            server_name,
            credential,
            last_error: None,
            last_receipt: None,
        })
    }
    /// Actual transport/cleanup error retained for the supervising process; no token bytes.
    pub fn take_error(&mut self) -> Option<network::Error> {
        self.last_error.take()
    }
    /// Actual durable acknowledgement, retained even if subsequent socket cleanup failed.
    pub fn last_receipt(&self) -> Option<&Metadata> {
        self.last_receipt.as_ref()
    }
    fn exchange(
        &mut self,
        request: &Request<'_>,
        account: &Handle,
        payload: Option<Loan>,
    ) -> Result<Outcome, Refusal> {
        let mut token = PrivateMemory::create_in(self.credential.len(), account)
            .map_err(|_| Refusal::Capacity)?;
        let mut header = PrivateMemory::create_in(wire::MAX_HEADER as u64, account)
            .map_err(|_| Refusal::Capacity)?;
        let length = token
            .with_bytes(|secret| {
                let result = (|| {
                    let n = self.credential.len() as usize;
                    self.credential
                        .read(0, &mut secret[..n])
                        .map_err(|_| Refusal::Unavailable)?;
                    header
                        .with_bytes(|bytes| {
                            Request {
                                id: request.id,
                                scope: request.scope.clone(),
                                object: request.object.clone(),
                                expected_role: Some(
                                    moruna_storage_format::auth::SessionRole::BucketHome,
                                ),
                                proof: Proof::Application(&secret[..n]),
                                operation: request.operation.clone(),
                            }
                            .encode_into(bytes)
                            .map_err(|_| Refusal::Invalid)
                        })
                        .map_err(|_| Refusal::Unavailable)?
                })();
                secret.fill(0);
                result
            })
            .map_err(|_| Refusal::Unavailable)??;
        let header = Loan::new(header.into_memory(), 0, wire::MAX_HEADER as u64, true)
            .map_err(|_| Refusal::Unavailable)?;
        let reply = Loan::new(
            Memory::create_in(wire::MAX_HEADER as u64, account).map_err(|_| Refusal::Capacity)?,
            0,
            wire::MAX_HEADER as u64,
            true,
        )
        .map_err(|_| Refusal::Unavailable)?;
        let mut parse = PrivateMemory::create_in(wire::MAX_HEADER as u64, account)
            .map_err(|_| Refusal::Capacity)?;
        let mut socket = match self
            .network
            .open(self.target.clone(), self.server_name.clone())
        {
            Ok(socket) => socket,
            Err(cause) => {
                self.last_error = Some(cause);
                return Err(Refusal::Unavailable);
            }
        };
        let sent = socket.send(&header, 0, length as u64);
        // Scrub credential-bearing exported header even on failed send. Its original
        // private owner was consumed before export; no aliased mutable slice is formed.
        let zero = [0u8; 256];
        for at in (0..length).step_by(zero.len()) {
            header
                .write(at as u64, &zero[..(length - at).min(zero.len())])
                .map_err(|_| Refusal::Unavailable)?;
        }
        if let Err(cause) = sent {
            self.last_error = Some(cause);
            return Err(Refusal::Unavailable);
        }
        let mut response = read_response(&mut socket, &reply, &mut parse).map_err(|cause| {
            self.last_error = Some(cause);
            Refusal::Unavailable
        })?;
        if response.id != request.id {
            return Err(Refusal::Integrity);
        }
        if matches!(
            request.operation,
            Operation::Create { .. } | Operation::Replace { .. }
        ) {
            match response.outcome {
                Outcome::Ready { length }
                    if payload.as_ref().map(|body| body.len()).unwrap_or(0) == length =>
                {
                    if let Some(body) = payload.as_ref() {
                        socket.send(body, 0, body.len()).map_err(|cause| {
                            self.last_error = Some(cause);
                            Refusal::Unavailable
                        })?;
                    }
                    response = read_response(&mut socket, &reply, &mut parse).map_err(|cause| {
                        self.last_error = Some(cause);
                        Refusal::Unavailable
                    })?;
                    if response.id != request.id {
                        return Err(Refusal::Integrity);
                    }
                }
                Outcome::Refused(code) => return Err(code),
                _ => return Err(Refusal::Integrity),
            }
        }
        match (&request.operation, &response.outcome) {
            (_, Outcome::Refused(code)) => return Err(*code),
            (
                Operation::Read {
                    version,
                    offset,
                    length,
                },
                Outcome::Read {
                    metadata,
                    offset: actual_offset,
                    length: actual_length,
                },
            ) => {
                if metadata.object != request.object
                    || &metadata.version != version
                    || actual_offset != offset
                    || actual_length != length
                {
                    return Err(Refusal::Integrity);
                }
                if *length == 0 && payload.is_none() {
                    // Canonical empty object/range needs no payload Memory allocation.
                } else {
                    let destination = payload.as_ref().ok_or(Refusal::Invalid)?;
                    if destination.len() != *length {
                        return Err(Refusal::Invalid);
                    }
                    socket.receive(destination, 0, *length).map_err(|cause| {
                        self.last_error = Some(cause);
                        Refusal::Unavailable
                    })?;
                }
            }
            (Operation::Current, Outcome::Head(metadata)) if metadata.object == request.object => {}
            (Operation::Head { version }, Outcome::Head(metadata))
                if metadata.object == request.object && &metadata.version == version => {}
            (
                Operation::Create { length } | Operation::Replace { length, .. },
                Outcome::Written(metadata),
            ) if metadata.object == request.object && metadata.bytes == *length => {
                self.last_receipt = Some(metadata.clone());
            }
            (Operation::List { .. }, Outcome::Listed { .. }) => {}
            _ => return Err(Refusal::Integrity),
        }
        socket.close().map_err(|cause| {
            self.last_error = Some(cause);
            Refusal::Unavailable
        })?;
        Ok(response.outcome)
    }
}
fn probe(
    network: &mut Network,
    target: Target,
    tls_name: String,
    account: &Handle,
) -> Result<(), Startup> {
    use moruna_storage_format::provider;
    let mut control =
        PrivateMemory::create_in(wire::MAX_HEADER as u64, account).map_err(Startup::Memory)?;
    let length = control
        .with_bytes(|bytes| provider::Request { id: 1 }.encode_into(bytes))
        .map_err(Startup::Memory)?
        .map_err(Startup::Control)?;
    let request = Loan::new(control.into_memory(), 0, wire::MAX_HEADER as u64, false)
        .map_err(Startup::Memory)?;
    let reply = Loan::new(
        Memory::create_in(wire::MAX_HEADER as u64, account).map_err(Startup::Memory)?,
        0,
        wire::MAX_HEADER as u64,
        true,
    )
    .map_err(Startup::Memory)?;
    let mut parse =
        PrivateMemory::create_in(wire::MAX_HEADER as u64, account).map_err(Startup::Memory)?;
    let mut socket = network.open(target, tls_name).map_err(Startup::Network)?;
    socket
        .send(&request, 0, length as u64)
        .map_err(Startup::Network)?;
    socket
        .receive(&reply, 0, wire::PREFIX_BYTES as u64)
        .map_err(Startup::Network)?;
    let mut prefix = [0; wire::PREFIX_BYTES];
    reply.read(0, &mut prefix).map_err(Startup::Memory)?;
    let length = wire::header_bytes(&prefix).map_err(Startup::Control)?;
    socket
        .receive(
            &reply,
            wire::PREFIX_BYTES as u64,
            (length - wire::PREFIX_BYTES) as u64,
        )
        .map_err(Startup::Network)?;
    let description = parse
        .with_bytes(|bytes| {
            reply
                .read(0, &mut bytes[..length])
                .map_err(Startup::Memory)?;
            provider::Description::decode(&bytes[..length]).map_err(Startup::Control)
        })
        .map_err(Startup::Memory)??;
    socket.close().map_err(Startup::Network)?;
    if description.id != 1 {
        return Err(Startup::Invalid);
    }
    if description.role != Some(SessionRole::BucketHome) {
        return Err(Startup::Role(description.role));
    }
    Ok(())
}
fn read_response(
    socket: &mut Socket<'_>,
    reply: &Loan,
    parse: &mut PrivateMemory,
) -> Result<Response, network::Error> {
    socket.receive(reply, 0, wire::PREFIX_BYTES as u64)?;
    let mut prefix = [0; wire::PREFIX_BYTES];
    reply.read(0, &mut prefix).map_err(network::Error::Memory)?;
    let length = wire::header_bytes(&prefix).map_err(|_| network::Error::Invalid)?;
    socket.receive(
        reply,
        wire::PREFIX_BYTES as u64,
        (length - wire::PREFIX_BYTES) as u64,
    )?;
    parse
        .with_bytes(|bytes| {
            reply
                .read(0, &mut bytes[..length])
                .map_err(network::Error::Memory)?;
            Response::decode(&bytes[..length]).map_err(|_| network::Error::Invalid)
        })
        .map_err(network::Error::Memory)?
}
impl Backend for Proxy {
    fn role(&self) -> SessionRole {
        SessionRole::BucketHome
    }
    fn execute(
        &mut self,
        request: &Request<'_>,
        account: &Handle,
        payload: Option<Loan>,
        _verifier: &dyn moruna_storage_format::auth::Verifier,
    ) -> Result<Outcome, Refusal> {
        self.exchange(request, account, payload)
    }
}
