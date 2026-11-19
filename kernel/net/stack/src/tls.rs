//! Server-owned client and accepted-socket TLS (SDD 31). The crypto, chain/name verification and
//! protocol state machine are rustls/ring; this module only bounds and moves bytes.
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::time::Duration;
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    client::{ClientConnectionData, UnbufferedClientConnection},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::{ServerConnectionData, UnbufferedServerConnection},
    time_provider::TimeProvider,
    unbuffered::{ConnectionState, UnbufferedStatus},
};
use smoltcp::socket::tcp;
use zeroize::Zeroize;

use crate::meter::Meter;

/// The largest application record accepted per send. Larger calls make progress
/// through partial sends, as the unencrypted socket protocol already permits.
const RECORD: usize = 16_384;
const OVERHEAD: usize = 256;
/// One poll must yield even when a peer sends many small handshake records.
const STEPS: usize = 64;

#[cfg(target_os = "none")]
fn secure_random(bytes: &mut [u8]) -> Result<(), getrandom::Error> {
    moruna_svc::random(bytes).map_err(|_| getrandom::Error::UNSUPPORTED)
}
#[cfg(target_os = "none")]
getrandom::register_custom_getrandom!(secure_random);

/// The node's one wall clock, as TLS reads it (system design 3D): on a node, the core's clock
/// page, which the core sets at boot from the machine's clock and `platform` from its cloud's
/// `Date`, before `ready` and every hour. TLS keeps no time of its own: no anchor, no build-time
/// clock, no expiry. While the clock is unset there is no time, so no certificate is judged:
/// `start_tls` is refused and no TLS server is made.
pub struct TrustedTime {
    /// The wall clock now, in nanoseconds since 1970 (UTC); `None` while it is unset.
    wall: Box<dyn Fn() -> Option<u64> + Send + Sync>,
}

impl TrustedTime {
    /// TLS's time from `wall`, which says the wall clock now in nanoseconds since 1970 (UTC), or
    /// `None` while it is unset: on a node, the clock page's realtime.
    pub fn new(wall: impl Fn() -> Option<u64> + Send + Sync + 'static) -> TrustedTime {
        TrustedTime {
            wall: Box::new(wall),
        }
    }
    /// Whether the wall clock is set now.
    pub fn is_set(&self) -> bool {
        self.current_time().is_some()
    }
}
impl core::fmt::Debug for TrustedTime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TrustedTime")
            .field("set", &self.is_set())
            .finish()
    }
}
impl TimeProvider for TrustedTime {
    fn current_time(&self) -> Option<UnixTime> {
        (self.wall)()
            .filter(|ns| *ns > 0)
            .map(|ns| UnixTime::since_unix_epoch(Duration::from_nanos(ns)))
    }
}

/// The most trust roots a server takes: Mozilla's set, as curl published it on 2026-11-05 (the
/// cloud profile's roots), has 118.
pub const ROOTS_MAX: usize = 256;

pub struct Trust {
    pub time: Arc<TrustedTime>,
    config: Arc<ClientConfig>,
}
impl Trust {
    /// DER roots come only from the boot-granted trust capability, never a caller.
    pub fn new(roots: Vec<Vec<u8>>, time: TrustedTime) -> Result<Self, &'static str> {
        if roots.is_empty()
            || roots.len() > ROOTS_MAX
            || roots.iter().map(Vec::len).sum::<usize>() > 1 << 20
        {
            return Err("TLS trust roots exceed their bound or are absent");
        }
        let mut store = RootCertStore::empty();
        for root in roots {
            store
                .add(CertificateDer::from(root))
                .map_err(|_| "invalid TLS root DER")?;
        }
        let time = Arc::new(time);
        let config = ClientConfig::builder_with_details(
            Arc::new(rustls::crypto::ring::default_provider()),
            time.clone(),
        )
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| "TLS 1.3 provider unavailable")?
        .with_root_certificates(store)
        .with_no_client_auth();
        Ok(Self {
            time,
            config: Arc::new(config),
        })
    }
}

/// Explicit server certificate capability. It authenticates this endpoint only;
/// peer application identity/authorization remains the embedding application's.
pub struct ServerIdentity {
    config: Arc<ServerConfig>,
    time: Arc<TrustedTime>,
}
impl ServerIdentity {
    pub fn clock_usable(&self) -> bool {
        self.time.current_time().is_some()
    }
    /// Parse borrowed PKCS8 without an un-wiped owned DER copy. The caller owns and
    /// wipes its secret Memory staging. Opaque ring allocations have provider lifetime,
    /// not a claimed universal zeroization guarantee.
    pub fn new(
        chain: Vec<Vec<u8>>,
        pkcs8: &[u8],
        alpn: Vec<Vec<u8>>,
        time: Arc<TrustedTime>,
    ) -> Result<Self, Error> {
        if time.current_time().is_none() {
            return Err(Error::Unavailable);
        }
        if chain.is_empty()
            || chain.len() > 8
            || chain.iter().any(Vec::is_empty)
            || chain.iter().map(Vec::len).sum::<usize>() > 65536
            || pkcs8.is_empty()
            || pkcs8.len() > 16384
            || alpn.len() > 16
            || alpn.iter().any(|p| p.is_empty() || p.len() > 255)
        {
            return Err(Error::Limit);
        }
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8));
        let key =
            rustls::crypto::ring::sign::any_supported_type(&der).map_err(|_| Error::Protocol)?;
        let certified = rustls::sign::CertifiedKey::new(
            chain.into_iter().map(CertificateDer::from).collect(),
            key,
        );
        certified.keys_match().map_err(|_| Error::Protocol)?;
        let mut config = ServerConfig::builder_with_details(
            Arc::new(rustls::crypto::ring::default_provider()),
            time.clone(),
        )
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| Error::Unavailable)?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified)));
        config.max_early_data_size = 0;
        config.send_tls13_tickets = 0;
        config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        config.alpn_protocols = alpn;
        Ok(Self {
            config: Arc::new(config),
            time,
        })
    }
}
trait Connection {
    type Data;
    fn alpn(&self) -> Option<&[u8]>;
    fn records<'c, 'i>(&'c mut self, input: &'i mut [u8]) -> UnbufferedStatus<'c, 'i, Self::Data>;
}
impl Connection for UnbufferedClientConnection {
    type Data = ClientConnectionData;
    fn alpn(&self) -> Option<&[u8]> {
        self.alpn_protocol()
    }
    fn records<'c, 'i>(&'c mut self, input: &'i mut [u8]) -> UnbufferedStatus<'c, 'i, Self::Data> {
        self.process_tls_records(input)
    }
}
impl Connection for UnbufferedServerConnection {
    type Data = ServerConnectionData;
    fn alpn(&self) -> Option<&[u8]> {
        self.alpn_protocol()
    }
    fn records<'c, 'i>(&'c mut self, input: &'i mut [u8]) -> UnbufferedStatus<'c, 'i, Self::Data> {
        self.process_tls_records(input)
    }
}
/// Fixed initialized storage: consumed plaintext is wiped before compaction and
/// every populated allocation byte is wiped on drop, including former used tails.
struct SecretBuffer {
    bytes: Vec<u8>,
    limit: usize,
    used: usize,
}
impl SecretBuffer {
    fn new(capacity: usize) -> Result<Self, Error> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Limit)?;
        bytes.resize(bytes.capacity(), 0);
        Ok(Self {
            bytes,
            limit: capacity,
            used: 0,
        })
    }
    fn len(&self) -> usize {
        self.used
    }
    fn is_empty(&self) -> bool {
        self.used == 0
    }
    fn resize(&mut self, n: usize) -> Result<(), Error> {
        if n > self.limit {
            return Err(Error::Limit);
        }
        self.truncate(n);
        self.used = n;
        Ok(())
    }
    fn truncate(&mut self, n: usize) {
        if n < self.used {
            self.bytes[n..self.used].zeroize();
            self.used = n;
        }
    }
    fn clear(&mut self) {
        self.bytes.as_mut_slice().zeroize();
        self.used = 0;
    }
    fn extend_from_slice(&mut self, input: &[u8]) -> Result<(), Error> {
        let end = self
            .used
            .checked_add(input.len())
            .filter(|n| *n <= self.limit)
            .ok_or(Error::Limit)?;
        self.bytes[self.used..end].copy_from_slice(input);
        self.used = end;
        Ok(())
    }
    fn discard(&mut self, n: usize) -> Result<(), Error> {
        if n > self.used {
            return Err(Error::Protocol);
        }
        self.bytes.copy_within(n..self.used, 0);
        let next = self.used - n;
        self.bytes[next..self.used].zeroize();
        self.used = next;
        Ok(())
    }
}
impl core::ops::Deref for SecretBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes[..self.used]
    }
}
impl core::ops::DerefMut for SecretBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[..self.used]
    }
}
impl Drop for SecretBuffer {
    fn drop(&mut self) {
        self.bytes.as_mut_slice().zeroize();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    Protocol,
    Limit,
    TimedOut,
}

/// Once installed this marker is never removed, including after failure. No
/// send/receive can silently fall back to the TCP plaintext path.
struct State<C> {
    conn: C,
    time: Arc<TrustedTime>,
    incoming: SecretBuffer,
    outgoing: SecretBuffer,
    offset: usize,
    plain_in: SecretBuffer,
    plain_out: SecretBuffer,
    limit: usize,
    handshake_bytes: usize,
    deadline: u64,
    ready: bool,
    peer_closed: bool,
    closing: bool,
    close_queued: bool,
    close_deadline: u64,
    failure: Option<Error>,
    work: bool,
}
enum Mode {
    Client(State<UnbufferedClientConnection>),
    Server(State<UnbufferedServerConnection>),
}
/// One TLS mode is retained after failure; plaintext fallback is impossible.
pub struct Tls(Mode);
macro_rules! state {
    ($this:expr,$v:ident=>$body:expr) => {
        match &$this.0 {
            Mode::Client($v) => $body,
            Mode::Server($v) => $body,
        }
    };
}
macro_rules! state_mut {
    ($this:expr,$v:ident=>$body:expr) => {
        match &mut $this.0 {
            Mode::Client($v) => $body,
            Mode::Server($v) => $body,
        }
    };
}
impl Tls {
    pub fn new(trust: &Trust, name: &str, limit: usize, deadline: u64) -> Result<Self, Error> {
        if trust.time.current_time().is_none() {
            return Err(Error::Unavailable);
        }
        let name =
            ServerName::try_from(alloc::string::String::from(name)).map_err(|_| Error::Protocol)?;
        let conn = UnbufferedClientConnection::new(trust.config.clone(), name)
            .map_err(|_| Error::Protocol)?;
        State::new(conn, trust.time.clone(), limit, deadline).map(|state| Self(Mode::Client(state)))
    }
    pub fn server(identity: &ServerIdentity, limit: usize, deadline: u64) -> Result<Self, Error> {
        if identity.time.current_time().is_none() {
            return Err(Error::Unavailable);
        }
        let conn = UnbufferedServerConnection::new(identity.config.clone())
            .map_err(|_| Error::Protocol)?;
        State::new(conn, identity.time.clone(), limit, deadline)
            .map(|state| Self(Mode::Server(state)))
    }
    pub fn alpn(&self) -> Option<&[u8]> {
        state!(self,s=>s.conn.alpn())
    }
    pub fn ready(&self) -> bool {
        state!(self,s=>s.ready())
    }
    pub fn error(&self) -> Option<Error> {
        state!(self,s=>s.error())
    }
    pub fn needs_poll(&self) -> bool {
        state!(self,s=>s.needs_poll())
    }
    pub fn deadline(&self) -> Option<u64> {
        state!(self,s=>s.deadline())
    }
    pub fn close_sent(&self) -> bool {
        state!(self,s=>s.close_sent())
    }
    pub fn shutdown(&mut self, deadline: u64) {
        state_mut!(self,s=>s.shutdown(deadline))
    }
    pub fn send_from<E>(
        &mut self,
        max: usize,
        read: impl FnOnce(&mut [u8]) -> Result<(), E>,
    ) -> Option<Result<Result<u32, E>, Error>> {
        state_mut!(self,s=>s.send_from(max,read))
    }
    pub fn receive_to<E>(
        &mut self,
        max: usize,
        write: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Option<Result<Result<u32, E>, Error>> {
        state_mut!(self,s=>s.receive_to(max,write))
    }
    /// Whether [`Tls::receive_to`] answers now (work item 6.3ad): plaintext waits, or the
    /// stream's end, or a failure.
    pub fn receivable(&self) -> bool {
        state!(self,s=>s.receivable())
    }
    /// Moves the socket's records, timed and counted on `meter` (work item 6.3ab).
    pub fn poll(&mut self, socket: &mut tcp::Socket<'static>, now: u64, meter: &mut Meter) {
        state_mut!(self,s=>s.poll(socket,now,meter))
    }
}
impl<C: Connection> State<C> {
    fn new(conn: C, time: Arc<TrustedTime>, limit: usize, deadline: u64) -> Result<Self, Error> {
        if !(RECORD + OVERHEAD..=1 << 20).contains(&limit) {
            return Err(Error::Limit);
        }
        Ok(Self {
            conn,
            time,
            incoming: SecretBuffer::new(limit)?,
            outgoing: SecretBuffer::new(limit)?,
            offset: 0,
            plain_in: SecretBuffer::new(limit)?,
            plain_out: SecretBuffer::new(RECORD)?,
            limit,
            handshake_bytes: 0,
            deadline,
            ready: false,
            peer_closed: false,
            closing: false,
            close_queued: false,
            close_deadline: 0,
            failure: None,
            work: true,
        })
    }
    pub fn ready(&self) -> bool {
        self.ready && self.failure.is_none()
    }
    pub fn error(&self) -> Option<Error> {
        self.failure
    }
    pub fn needs_poll(&self) -> bool {
        self.work && self.failure.is_none()
    }
    pub fn deadline(&self) -> Option<u64> {
        if self.failure.is_some() {
            None
        } else if self.closing && !self.close_sent() {
            Some(self.close_deadline)
        } else {
            (!self.ready).then_some(self.deadline)
        }
    }
    pub fn close_sent(&self) -> bool {
        self.close_queued
            && self.outgoing.is_empty()
            && self.plain_out.is_empty()
            && self.failure.is_none()
    }
    pub fn shutdown(&mut self, deadline: u64) {
        if !self.closing {
            self.close_deadline = deadline;
        }
        self.closing = true;
        self.work = true;
    }
    pub fn send_from<E>(
        &mut self,
        max: usize,
        read: impl FnOnce(&mut [u8]) -> Result<(), E>,
    ) -> Option<Result<Result<u32, E>, Error>> {
        if let Some(error) = self.failure {
            return Some(Err(error));
        }
        if self.closing {
            return Some(Err(Error::Protocol));
        }
        if !self.ready || !self.plain_out.is_empty() {
            return None;
        }
        let count = max.min(RECORD);
        if let Err(error) = self.plain_out.resize(count) {
            return Some(Err(error));
        }
        if let Err(error) = read(&mut self.plain_out) {
            self.plain_out.clear();
            return Some(Ok(Err(error)));
        }
        self.work = true;
        Some(Ok(Ok(count as u32)))
    }
    pub fn receive_to<E>(
        &mut self,
        max: usize,
        write: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Option<Result<Result<u32, E>, Error>> {
        // Plaintext already decrypted came in records each authenticated on its own: it is
        // delivered whole, and a failure behind it (a FIN with no close_notify, a bad record) is
        // reported on the read past its last byte. An HTTPS server that omits close_notify after
        // a Content-Length body is read to the body's end.
        if !self.plain_in.is_empty() {
            let count = max.min(self.plain_in.len());
            if let Err(error) = write(&self.plain_in[..count]) {
                return Some(Ok(Err(error)));
            }
            if let Err(error) = self.plain_in.discard(count) {
                return Some(Err(error));
            }
            self.work = true;
            return Some(Ok(Ok(count as u32)));
        }
        if let Some(error) = self.failure {
            return Some(Err(error));
        }
        self.peer_closed.then(|| Ok(Ok(0)))
    }
    pub fn receivable(&self) -> bool {
        !self.plain_in.is_empty() || self.failure.is_some() || self.peer_closed
    }
    pub fn poll(&mut self, socket: &mut tcp::Socket<'static>, now: u64, meter: &mut Meter) {
        if self.failure.is_some() {
            return;
        }
        self.work = false;
        let result = if !self.ready && self.time.current_time().is_none() {
            Err(Error::Unavailable)
        } else {
            self.advance(socket, now, meter)
        };
        if let Err(e) = result {
            self.failure = Some(e);
            socket.abort();
        }
    }
    fn advance(
        &mut self,
        socket: &mut tcp::Socket<'static>,
        now: u64,
        meter: &mut Meter,
    ) -> Result<(), Error> {
        if !self.ready && now >= self.deadline {
            return Err(Error::TimedOut);
        }
        if self.closing && !self.close_sent() && now >= self.close_deadline {
            return Err(Error::TimedOut);
        }
        for _ in 0..STEPS {
            if self.offset < self.outgoing.len() {
                if !socket.may_send() {
                    return Err(Error::Protocol);
                }
                let sent = socket
                    .send_slice(&self.outgoing[self.offset..])
                    .map_err(|_| Error::Protocol)?;
                meter.counts.tls_out_bytes += sent as u64;
                self.offset += sent;
                if self.offset < self.outgoing.len() {
                    return Ok(());
                }
                self.outgoing.clear();
                self.offset = 0;
            }
            if socket.can_recv() && self.incoming.len() < self.limit {
                let n = (self.limit - self.incoming.len()).min(socket.recv_queue());
                let at = self.incoming.len();
                self.incoming.resize(at + n)?;
                let n = socket
                    .recv_slice(&mut self.incoming[at..])
                    .map_err(|_| Error::Protocol)?;
                self.incoming.truncate(at + n);
                meter.counts.tls_in_bytes += n as u64;
                if !self.ready {
                    self.handshake_bytes =
                        self.handshake_bytes.checked_add(n).ok_or(Error::Limit)?;
                    if self.handshake_bytes > self.limit {
                        return Err(Error::Limit);
                    }
                }
            }
            // rustls opens every whole record it holds as it processes them: timed from here
            // to their plaintext taken, when there is input to open.
            let opening = (!self.incoming.is_empty()).then(|| meter.now());
            let status = self.conn.records(&mut self.incoming);
            let mut discard = status.discard;
            let mut wait = false;
            // Waiting only because the caller has not read what is decrypted: the records
            // behind it, a close_notify among them, are still to come.
            let mut held_back = false;
            match status.state.map_err(|_| Error::Protocol)? {
                ConnectionState::EncodeTlsData(mut encode) => {
                    self.outgoing.resize(self.limit)?;
                    let n = encode
                        .encode(&mut self.outgoing)
                        .map_err(|_| Error::Limit)?;
                    self.outgoing.truncate(n);
                }
                ConnectionState::TransmitTlsData(transmit) => transmit.done(),
                ConnectionState::ReadTraffic(mut read) => {
                    while let Some(size) = read.peek_len() {
                        if self.plain_in.len() + size.get() > self.limit {
                            wait = true;
                            held_back = true;
                            break;
                        }
                        let record = read
                            .next_record()
                            .ok_or(Error::Protocol)?
                            .map_err(|_| Error::Protocol)?;
                        discard += record.discard;
                        self.plain_in.extend_from_slice(record.payload)?;
                        meter.counts.opened_bytes += record.payload.len() as u64;
                    }
                    if let Some(start) = opening {
                        meter.counts.open_ns += meter.since(start);
                    }
                }
                ConnectionState::WriteTraffic(mut write) => {
                    self.ready = true;
                    if !self.plain_out.is_empty() {
                        self.outgoing.resize(self.plain_out.len() + OVERHEAD)?;
                        let sealing = meter.now();
                        let n = write
                            .encrypt(&self.plain_out, &mut self.outgoing)
                            .map_err(|_| Error::Protocol)?;
                        meter.counts.seal_ns += meter.since(sealing);
                        meter.counts.sealed_bytes += self.plain_out.len() as u64;
                        self.outgoing.truncate(n);
                        self.plain_out.clear();
                    } else if self.closing && !self.close_queued {
                        self.outgoing.resize(OVERHEAD)?;
                        let n = write
                            .queue_close_notify(&mut self.outgoing)
                            .map_err(|_| Error::Protocol)?;
                        self.outgoing.truncate(n);
                        self.close_queued = true;
                    } else {
                        wait = true;
                    }
                }
                ConnectionState::PeerClosed => {
                    self.peer_closed = true;
                }
                ConnectionState::Closed => {
                    self.peer_closed = true;
                    wait = true;
                }
                ConnectionState::BlockedHandshake => {
                    if self.incoming.len().saturating_sub(discard) == self.limit {
                        return Err(Error::Limit);
                    }
                    wait = true;
                }
                _ => return Err(Error::Protocol),
            }
            if discard > self.incoming.len() {
                return Err(Error::Protocol);
            }
            self.incoming.discard(discard)?;
            if self.close_queued && self.outgoing.is_empty() {
                socket.close();
            }
            if wait {
                // The peer's FIN with no close_notify before it is a truncated stream, but only
                // once every record it sent has been read: while some wait behind a full buffer,
                // the close_notify may be among them.
                if !socket.may_recv() && !self.peer_closed && !held_back {
                    return Err(Error::Protocol);
                }
                if socket.can_recv()
                    && self.incoming.len() < self.limit
                    && self.plain_in.len() < self.limit
                {
                    continue;
                }
                return Ok(());
            }
        }
        self.work = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secret_staging_wipes_consumed_and_former_used_capacity_without_growth() {
        let mut buffer = SecretBuffer::new(32).unwrap();
        let allocated = buffer.bytes.capacity();
        buffer.extend_from_slice(b"secret").unwrap();
        buffer.discard(2).unwrap();
        assert_eq!(&*buffer, b"cret");
        assert!(buffer.bytes[4..].iter().all(|b| *b == 0));
        buffer.resize(16).unwrap();
        buffer[..].fill(7);
        buffer.truncate(3);
        assert!(buffer.bytes[3..].iter().all(|b| *b == 0));
        buffer.clear();
        assert!(buffer.bytes.iter().all(|b| *b == 0));
        assert_eq!(buffer.bytes.len(), allocated);
        assert!(buffer.resize(33).is_err());
        assert_eq!(buffer.bytes.capacity(), allocated);
    }
    #[test]
    fn tls_has_the_wall_clock_while_it_is_set_and_no_time_while_it_is_not() {
        use core::sync::atomic::{AtomicU64, Ordering};
        // The clock page's realtime, as a test sets it: zero is unset.
        let page = Arc::new(AtomicU64::new(0));
        let wall = page.clone();
        let time = TrustedTime::new(move || match wall.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(ns),
        });
        assert!(!time.is_set());
        assert!(time.current_time().is_none());
        assert_eq!(alloc::format!("{time:?}"), "TrustedTime { set: false }");
        // Set at boot from the machine's clock, or by platform: TLS reads it as it is.
        page.store(1_800_000_000_500_000_000, Ordering::Relaxed);
        assert!(time.is_set());
        assert_eq!(time.current_time().unwrap().as_secs(), 1_800_000_000);
        // Set again, earlier or later, as platform does every hour: TLS follows it, with no
        // expiry and no rollback of its own.
        page.store(1_799_999_990_000_000_000, Ordering::Relaxed);
        assert_eq!(time.current_time().unwrap().as_secs(), 1_799_999_990);
        page.store(1_900_000_000_000_000_000, Ordering::Relaxed);
        assert_eq!(time.current_time().unwrap().as_secs(), 1_900_000_000);
        // A clock that says 1970 itself is no time.
        let zero = TrustedTime::new(|| Some(0));
        assert!(!zero.is_set());
    }

    #[test]
    fn roots_are_bounded_in_number_and_bytes() {
        let time = || TrustedTime::new(|| Some(1_800_000_000_000_000_000));
        assert!(Trust::new(Vec::new(), time()).is_err());
        let too_many = alloc::vec![alloc::vec![0u8; 8]; ROOTS_MAX + 1];
        assert_eq!(
            Trust::new(too_many, time()).err(),
            Some("TLS trust roots exceed their bound or are absent")
        );
        let too_large = alloc::vec![alloc::vec![0u8; 1 << 16]; 17];
        assert!(Trust::new(too_large, time()).is_err());
        assert_eq!(
            Trust::new(alloc::vec![alloc::vec![1u8, 2, 3]], time()).err(),
            Some("invalid TLS root DER")
        );
    }
}
