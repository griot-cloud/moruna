//! Server-owned client and accepted-socket TLS (SDD 31). The crypto, chain/name verification and
//! protocol state machine are rustls/ring; this module only bounds and moves bytes.
use alloc::{sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};
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

/// A trusted boot-provided UTC anchor. Monotonic time is only elapsed duration;
/// it never becomes an invented Unix timestamp. Expiry/rollback fails closed.
#[derive(Debug)]
pub struct TrustedTime {
    unix_seconds: u64,
    monotonic_start: u64,
    valid_ns: u64,
    monotonic_now: AtomicU64,
    invalid: AtomicBool,
}
impl TrustedTime {
    pub fn new(unix_seconds: u64, monotonic_start: u64, valid_ns: u64) -> Option<Self> {
        (unix_seconds > 0 && valid_ns > 0 && valid_ns <= 86_400_000_000_000).then_some(Self {
            unix_seconds,
            monotonic_start,
            valid_ns,
            monotonic_now: AtomicU64::new(monotonic_start),
            invalid: AtomicBool::new(false),
        })
    }
    pub fn update(&self, now: u64) {
        let previous = self.monotonic_now.fetch_max(now, Ordering::Relaxed);
        if now < previous {
            self.invalid.store(true, Ordering::Relaxed);
        }
    }
}
impl TimeProvider for TrustedTime {
    fn current_time(&self) -> Option<UnixTime> {
        if self.invalid.load(Ordering::Relaxed) {
            return None;
        }
        let elapsed = self
            .monotonic_now
            .load(Ordering::Relaxed)
            .checked_sub(self.monotonic_start)?;
        if elapsed >= self.valid_ns {
            return None;
        }
        Some(UnixTime::since_unix_epoch(Duration::from_secs(
            self.unix_seconds.checked_add(elapsed / 1_000_000_000)?,
        )))
    }
}

pub struct Trust {
    pub time: Arc<TrustedTime>,
    config: Arc<ClientConfig>,
}
impl Trust {
    /// DER roots come only from the boot-granted trust capability, never a caller.
    pub fn new(roots: Vec<Vec<u8>>, time: TrustedTime) -> Result<Self, &'static str> {
        if roots.is_empty()
            || roots.len() > 64
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
    pub fn poll(&mut self, socket: &mut tcp::Socket<'static>, now: u64) {
        state_mut!(self,s=>s.poll(socket,now))
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
        if let Some(error) = self.failure {
            return Some(Err(error));
        }
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
        self.peer_closed.then(|| Ok(Ok(0)))
    }
    pub fn poll(&mut self, socket: &mut tcp::Socket<'static>, now: u64) {
        if self.failure.is_some() {
            return;
        }
        self.work = false;
        self.time.update(now);
        let result = if !self.ready && self.time.current_time().is_none() {
            Err(Error::Unavailable)
        } else {
            self.advance(socket, now)
        };
        if let Err(e) = result {
            self.failure = Some(e);
            socket.abort();
        }
    }
    fn advance(&mut self, socket: &mut tcp::Socket<'static>, now: u64) -> Result<(), Error> {
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
                if !self.ready {
                    self.handshake_bytes =
                        self.handshake_bytes.checked_add(n).ok_or(Error::Limit)?;
                    if self.handshake_bytes > self.limit {
                        return Err(Error::Limit);
                    }
                }
            }
            let status = self.conn.records(&mut self.incoming);
            let mut discard = status.discard;
            let mut wait = false;
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
                            break;
                        }
                        let record = read
                            .next_record()
                            .ok_or(Error::Protocol)?
                            .map_err(|_| Error::Protocol)?;
                        discard += record.discard;
                        self.plain_in.extend_from_slice(record.payload)?;
                    }
                }
                ConnectionState::WriteTraffic(mut write) => {
                    self.ready = true;
                    if !self.plain_out.is_empty() {
                        self.outgoing.resize(self.plain_out.len() + OVERHEAD)?;
                        let n = write
                            .encrypt(&self.plain_out, &mut self.outgoing)
                            .map_err(|_| Error::Protocol)?;
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
                if !socket.may_recv() && !self.peer_closed {
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
    fn trusted_clock_failure_cannot_be_undone_by_rewinding() {
        let clock = TrustedTime::new(1_700_000_000, 10, 100).unwrap();
        clock.update(110);
        assert!(clock.current_time().is_none());
        clock.update(10);
        clock.update(20);
        assert!(clock.current_time().is_none());
        let clock = TrustedTime::new(1_700_000_000, 10, 100).unwrap();
        clock.update(20);
        clock.update(19);
        clock.update(21);
        assert!(clock.current_time().is_none());
    }

    #[test]
    fn trusted_clock_expires_and_never_invents_utc_on_rollback() {
        assert!(TrustedTime::new(0, 0, 100).is_none());
        let clock = TrustedTime::new(1_700_000_000, 100, 2_000_000_000).unwrap();
        assert_eq!(clock.current_time().unwrap().as_secs(), 1_700_000_000);
        clock.update(1_000_000_100);
        assert_eq!(clock.current_time().unwrap().as_secs(), 1_700_000_001);
        clock.update(2_000_000_100);
        assert!(clock.current_time().is_none());
        clock.update(99);
        assert!(clock.current_time().is_none());
    }
}
