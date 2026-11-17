//! TLS 1.3 for one connection, through rustls's unbuffered server connection (the one rustls
//! offers without std). rustls holds the protocol and the crypto; this module only moves bytes
//! between the transport, rustls and the HTTP session, within fixed bounds.

use crate::{http::Session, identity::Identity};
use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use rustls::{
    ServerConfig,
    pki_types::UnixTime,
    server::{NoServerSessionStorage, UnbufferedServerConnection},
    sign::SingleCertAndKey,
    time_provider::TimeProvider,
    unbuffered::{ConnectionState, EncodeError, EncryptError, InsufficientSizeError},
};

/// The most TLS bytes held from the client before rustls has read them: room for a full record
/// (16 KiB of plaintext and its overhead) several times over.
pub(crate) const INCOMING_LIMIT: usize = 64 * 1024;
/// The most plaintext encrypted at once, one TLS record's worth.
const RECORD: usize = 16 * 1024;
/// A record's overhead beyond its plaintext, rounded up.
const OVERHEAD: usize = 256;
/// The most encrypted bytes held for the client before more of an answer is encrypted.
const OUTGOING_LIMIT: usize = 64 * 1024;
/// The most rustls states handled in one call, so that one connection cannot hold the server.
const STEPS: usize = 64;

/// The wall clock as rustls sees it, set by the server on each poll.
#[derive(Debug, Default)]
pub(crate) struct WallClock(AtomicU64);

impl WallClock {
    pub(crate) fn set(&self, unix_seconds: u64) {
        self.0.store(unix_seconds, Ordering::Relaxed);
    }
}

impl TimeProvider for WallClock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::from_secs(
            self.0.load(Ordering::Relaxed),
        )))
    }
}

/// The server's TLS configuration: TLS 1.3 only, `identity` presented to every client, no
/// client certificates, no resumption and no early data, and HTTP/1.1 as the only application
/// protocol.
pub(crate) fn config(
    identity: &Identity,
    wall: Arc<WallClock>,
) -> Result<Arc<ServerConfig>, rustls::Error> {
    let mut config = ServerConfig::builder_with_details(
        Arc::new(rustls::crypto::ring::default_provider()),
        wall,
    )
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(SingleCertAndKey::from(identity.certified_key())));
    config.max_early_data_size = 0;
    config.send_tls13_tickets = 0;
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Why a connection's TLS failed. Either way the connection is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TlsError {
    /// The client broke the protocol, or rustls refused it.
    Protocol,
    /// The client's handshake does not fit the bounds.
    Limit,
}

/// One connection's TLS state and its buffers.
pub(crate) struct Tls {
    conn: UnbufferedServerConnection,
    /// TLS bytes from the client that rustls has not yet consumed.
    incoming: Vec<u8>,
    /// TLS bytes for the client; `sent` of them have been written.
    outgoing: Vec<u8>,
    sent: usize,
    /// The handshake is done: application data moves.
    ready: bool,
    peer_closed: bool,
    close_queued: bool,
}

impl Tls {
    pub(crate) fn new(config: Arc<ServerConfig>) -> Result<Self, TlsError> {
        Ok(Self {
            conn: UnbufferedServerConnection::new(config).map_err(|_| TlsError::Protocol)?,
            incoming: Vec::new(),
            outgoing: Vec::new(),
            sent: 0,
            ready: false,
            peer_closed: false,
            close_queued: false,
        })
    }

    /// How many more bytes from the client fit.
    pub(crate) fn room(&self) -> usize {
        INCOMING_LIMIT - self.incoming.len()
    }

    /// Bytes read from the client.
    pub(crate) fn receive(&mut self, bytes: &[u8]) {
        let take = bytes.len().min(self.room());
        self.incoming.extend_from_slice(&bytes[..take]);
    }

    /// The encrypted bytes not yet written to the client.
    pub(crate) fn pending(&self) -> &[u8] {
        &self.outgoing[self.sent..]
    }

    /// `count` of the pending bytes have been written.
    pub(crate) fn sent(&mut self, count: usize) {
        self.sent = (self.sent + count).min(self.outgoing.len());
        if self.sent == self.outgoing.len() {
            self.outgoing.clear();
            self.sent = 0;
        }
    }

    /// Whether the handshake is done.
    pub(crate) fn ready(&self) -> bool {
        self.ready
    }

    /// Whether the client has said it sends nothing more (its close_notify).
    pub(crate) fn peer_closed(&self) -> bool {
        self.peer_closed
    }

    /// Whether this side's close_notify has been queued and every byte before it written.
    pub(crate) fn closed(&self) -> bool {
        self.close_queued && self.outgoing.is_empty()
    }

    /// Runs rustls as far as it can go now: the handshake, decrypting the client's records into
    /// `session` while it has room, encrypting `session`'s answer, and this side's close_notify
    /// once the session is closing and its answer is all encrypted. Says whether anything moved.
    pub(crate) fn process(&mut self, session: &mut Session) -> Result<bool, TlsError> {
        let mut progress = false;
        for _ in 0..STEPS {
            let status = self.conn.process_tls_records(&mut self.incoming);
            let mut discard = status.discard;
            let mut wait = false;
            match status.state.map_err(|_| TlsError::Protocol)? {
                ConnectionState::EncodeTlsData(mut encode) => {
                    append(&mut self.outgoing, 4096, |out| match encode.encode(out) {
                        Ok(n) => Ok(n),
                        Err(EncodeError::InsufficientSize(InsufficientSizeError {
                            required_size,
                        })) => Err(Some(required_size)),
                        Err(_) => Err(None),
                    })?;
                    progress = true;
                }
                // The encoded records are queued in `outgoing`, ahead of anything later.
                ConnectionState::TransmitTlsData(transmit) => transmit.done(),
                // rustls decrypts one record at a time and offers nothing else until it is
                // taken, so every record is taken here. What bounds the session's input is that
                // the server reads from the transport only while the session wants more.
                ConnectionState::ReadTraffic(mut read) => {
                    while let Some(record) = read.next_record() {
                        let record = record.map_err(|_| TlsError::Protocol)?;
                        discard += record.discard;
                        session.receive(record.payload);
                        progress = true;
                    }
                }
                ConnectionState::WriteTraffic(mut write) => {
                    self.ready = true;
                    let answer = session.pending();
                    if !answer.is_empty() && self.outgoing.len() < OUTGOING_LIMIT {
                        let chunk = &answer[..answer.len().min(RECORD)];
                        append(
                            &mut self.outgoing,
                            chunk.len() + OVERHEAD,
                            |out| match write.encrypt(chunk, out) {
                                Ok(n) => Ok(n),
                                Err(EncryptError::InsufficientSize(InsufficientSizeError {
                                    required_size,
                                })) => Err(Some(required_size)),
                                Err(_) => Err(None),
                            },
                        )?;
                        let taken = chunk.len();
                        session.take(taken);
                        progress = true;
                    } else if answer.is_empty() && session.closing() && !self.close_queued {
                        append(&mut self.outgoing, OVERHEAD, |out| {
                            match write.queue_close_notify(out) {
                                Ok(n) => Ok(n),
                                Err(EncryptError::InsufficientSize(InsufficientSizeError {
                                    required_size,
                                })) => Err(Some(required_size)),
                                Err(_) => Err(None),
                            }
                        })?;
                        self.close_queued = true;
                        progress = true;
                    } else {
                        wait = true;
                    }
                }
                ConnectionState::PeerClosed => {
                    if !self.peer_closed {
                        progress = true;
                    }
                    self.peer_closed = true;
                }
                ConnectionState::Closed => {
                    self.peer_closed = true;
                    wait = true;
                }
                ConnectionState::BlockedHandshake => {
                    if self.incoming.len() - discard.min(self.incoming.len()) >= INCOMING_LIMIT {
                        return Err(TlsError::Limit);
                    }
                    wait = true;
                }
                // Early data is never accepted, and rustls adds no other state without a new
                // version of this crate.
                _ => return Err(TlsError::Protocol),
            }
            if discard > self.incoming.len() {
                return Err(TlsError::Protocol);
            }
            self.incoming.drain(..discard);
            if wait {
                break;
            }
        }
        Ok(progress)
    }
}

/// Appends to `out` what `encode` writes into a fresh tail of `size` bytes, growing the tail to
/// the size rustls asks for when it is too small.
fn append(
    out: &mut Vec<u8>,
    size: usize,
    mut encode: impl FnMut(&mut [u8]) -> Result<usize, Option<usize>>,
) -> Result<(), TlsError> {
    let at = out.len();
    let mut size = size;
    for _ in 0..2 {
        out.resize(at + size, 0);
        match encode(&mut out[at..]) {
            Ok(written) => {
                out.truncate(at + written);
                return Ok(());
            }
            Err(Some(required)) if required > size => size = required,
            Err(_) => break,
        }
    }
    out.truncate(at);
    Err(TlsError::Protocol)
}

#[cfg(test)]
mod tests;
