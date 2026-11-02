//! `moruna.net` 3.0 (SDD 31, d.1): the network server's sockets, for the processes of a
//! node that speak to the network. A client connects, listens and accepts, sends and
//! receives TCP bytes with backpressure and closes each half of a connection; binds
//! UDP sockets and sends and receives datagrams; pings; and resolves names.
//!
//! Every connection holds a [`Policy`], fixed when the connection's listener was
//! granted: the boot's configuration gives each listener slot of the server one, and a
//! connection whose policy carries [`rights::GRANT`] mints listeners for narrower ones
//! (`grant`). A request the policy does not permit is refused `NotPermitted` before the
//! server does anything, so a policy is the whole of what a client can reach: which
//! remote addresses and ports it may connect and send to, which names it may resolve
//! and connect to by name, which local addresses and ports it may listen and bind on,
//! and how many sockets it may hold. A node's egress policy is a policy given to the
//! listeners its tasks are linked to.
//!
//! Sockets are numbered per connection and close with it. Requests that wait (open,
//! accept, a receive with nothing queued, a send that finds no room, a resolve, a ping)
//! are answered when what they wait for happens, so a client may have one of each
//! outstanding per socket; a second of the same kind on one socket is `InUse`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod dns;
mod policy;
mod suites;
mod types;

pub use policy::{name_matches, name_valid, normalise_name};
pub use suites::{SUITE_NAME, pair_suite, suite};
pub use types::{
    Addr, Cidr, Endpoint, Info, Interface, NameRule, Opened, Policy, Rule, SocketState,
    SocketStatus, Target,
};

use alloc::{string::String, vec::Vec};

use moruna_svc::{Account, Channel, Handle, Memory, limits};

/// Maximum caller-owned TCP source loan. Bulk bytes never occupy the control message.
pub const SEND_MAX: usize = 1 << 20;
/// Maximum caller-owned stream destination loan; replies carry only a count.
pub const RECEIVE_MAX: usize = 1 << 20;
/// The largest UDP payload the protocol carries; a datagram is also bounded by the
/// MTU of the interface it leaves by, since the server does not fragment.
pub const DATAGRAM_MAX: usize = 65507;
/// The most rules of each kind a policy holds.
pub const RULES_MAX: usize = 64;
/// The longest name `resolve` takes, and the longest pattern of a name rule.
pub const NAME_MAX: usize = 253;
/// The longest listen backlog.
pub const BACKLOG_MAX: u32 = 128;
/// The most bytes a `ping` carries.
pub const PING_MAX: u32 = 1024;

/// What a policy permits beyond its rules (`Policy::rights`).
pub mod rights {
    /// Connect TCP sockets to the remote endpoints the rules permit.
    pub const CONNECT: u32 = 1 << 0;
    /// Listen for TCP connections on the local endpoints the rules permit.
    pub const LISTEN: u32 = 1 << 1;
    /// Bind UDP sockets on permitted local endpoints and send to permitted remotes.
    pub const UDP: u32 = 1 << 2;
    /// Send ICMP echo requests to permitted remote addresses.
    pub const PING: u32 = 1 << 3;
    /// Resolve any name, and connect to a name's address when the address rules
    /// permit it (without this, only the names the name rules match resolve).
    pub const RESOLVE: u32 = 1 << 4;
    /// Mint listeners for policies this one covers (`grant`).
    pub const GRANT: u32 = 1 << 5;
    /// Every right this version defines.
    pub const ALL: u32 = CONNECT | LISTEN | UDP | PING | RESOLVE | GRANT;

    /// The rights' names, in bit order, as the configuration writes them.
    pub const NAMES: [(&str, u32); 6] = [
        ("connect", CONNECT),
        ("listen", LISTEN),
        ("udp", UDP),
        ("ping", PING),
        ("resolve", RESOLVE),
        ("grant", GRANT),
    ];
}

mod transport {
    use super::*;
    moruna_svc::protocol! {
        /// The sole net3 wire schema, behind the Account-bound public client.
        pub protocol wire {
            name = "moruna.net";
            major = 3;
            minor = 0;
            requests {
                /// The interfaces, their addresses and links, the resolver's servers, and
                /// this connection's policy.
                1 => fn info() -> Info;
                /// A TCP connection to `to`, from `from` when given (which picks the
                /// interface), answered once it is established or refused.
                2 => fn open(to: Target, from: Option<Addr>) -> Opened;
                /// A TCP listener on `at` (port 0: the server picks one), holding at most
                /// `backlog` connections not yet accepted; answered with its socket and the
                /// endpoint it listens on.
                3 => fn listen(at: Endpoint, backlog: u32) -> (u32, Endpoint);
                /// The next connection a listener has established.
                4 => fn accept(listener: u32) -> Opened;
                /// Queues bytes on a TCP socket: answered with how many were taken, at least
                /// one, once there is room for any.
                5 => fn send_from(socket:u32,source:Memory,offset:u64,length:u32) -> u32;
                /// Fill at most `max` bytes of the caller's charged destination at
                /// `offset`; reply with count, zero only at authenticated stream EOF.
                /// The transferred loan requires READ|WRITE|MAP|TRANSFER under the
                /// native mapping ABI. The caller retains its original memory handle.
                /// Invalid rights/ranges are refused before consuming stream bytes.
                6 => fn receive_into(socket: u32, destination: Memory, offset: u64, max: u32) -> u32;
                /// Finishes the sending half of a TCP socket: queued bytes go, then a FIN.
                /// The receiving half stays open.
                7 => fn shutdown(socket: u32) -> ();
                /// Closes a socket of any kind; requests waiting on it are answered `Closed`.
                8 => fn close(socket: u32) -> ();
                /// A UDP socket bound to `at` (port 0: the server picks one).
                9 => fn bind(at: Endpoint) -> (u32, Endpoint);
                /// Sends one datagram from a UDP socket, once there is room for it.
                10 => fn send_to(socket: u32, to: Endpoint, bytes: Vec<u8>) -> ();
                /// The next datagram a UDP socket received, and who sent it.
                11 => fn receive_from(socket: u32) -> (Endpoint, Vec<u8>);
                /// A name's IPv4 and IPv6 addresses, through the server's resolver.
                12 => fn resolve(name: String) -> Vec<Addr>;
                /// An ICMP echo of `size` bytes to `to`: the round trip in nanoseconds.
                13 => fn ping(to: Addr, size: u32) -> u64;
                /// A listener whose every connection holds `policy`, which this
                /// connection's policy must cover.
                14 => fn grant(policy: Policy) -> Channel;
                /// A socket's state, endpoints and queues.
                15 => fn status(socket: u32) -> SocketStatus;
                /// Upgrades a pristine established outbound TCP socket to verified TLS
                /// 1.3 under the boot's trust roots and UTC clock. No plaintext fallback.
                16 => fn start_tls(socket:u32,server_name:String) -> ();
                /// One actual immutable Account binding before any network operation.
                17 => fn bind_account(account:Handle) -> ();
                /// Bounded public cert/ALPN/lifetime control and separate private PKCS8 loan.
                18 => fn install_server_identity(control:Memory,control_bytes:u32,key:Memory,key_bytes:u32) -> u32;
                /// TLS1.3 on a fresh accepted socket; queued ClientHello is supported.
                19 => fn start_tls_server(socket:u32,identity:u32) -> ();
                /// Revoke identity and close all associated sockets before dropping it.
                20 => fn release_server_identity(identity:u32) -> ();
                /// Public negotiated ALPN after authenticated handshake; no peer identity claim.
                21 => fn tls_alpn(socket:u32) -> Option<Vec<u8>>;
            }
            events {}
            errors {
                /// The connection's policy does not permit the request.
                1 => NotPermitted;
                /// An address or endpoint that cannot be used: unspecified, multicast or
                /// broadcast where one host is needed, port 0 for a remote, a source that
                /// is not one of the server's, or a malformed policy.
                2 => BadAddress;
                /// No interface reaches the address.
                3 => NoRoute;
                /// The peer refused the connection.
                4 => Refused;
                /// The connection or the name was not established in time.
                5 => TimedOut;
                /// The peer reset the connection.
                6 => Reset;
                /// The connection holds no socket of that number.
                7 => NoSocket;
                /// The socket is not of the kind the request needs.
                8 => WrongKind;
                /// A request of the same kind already waits on the socket.
                9 => InUse;
                /// The connection holds as many sockets as its policy allows, or the server
                /// as many as its configuration allows.
                10 => TooMany;
                /// The socket is not connected, or its sending half has been shut down.
                11 => NotConnected;
                /// The name has no address.
                12 => NotFound;
                /// The resolver has no server, or the name is not one a resolver takes.
                13 => NoResolver;
                /// The socket was closed while the request waited.
                14 => Closed;
                /// A datagram larger than its interface carries, or bytes over the bound.
                15 => TooLarge;
                /// The local endpoint is taken.
                16 => AddressInUse;
                /// The policy is malformed: a prefix past its family's bits, a port range
                /// that ends before it starts, a pattern that is not a name, unknown rights
                /// or too many rules.
                17 => BadPolicy;
                /// A count out of its range: a backlog of 0 or over `BACKLOG_MAX`, a
                /// receive of 0 bytes, a ping over `PING_MAX`.
                18 => BadArgument;
                /// No trusted roots/UTC anchor, or the anchor has expired.
                19 => TlsUnavailable;
                /// The TLS handshake, chain, name, record or authenticated close failed.
                20 => TlsFailed;
            }
        }
    }
}

/// Account-bound canonical client; no copied TCP send or unbound production facade.
pub mod net {
    pub use super::transport::wire::{Error, ID, REQUESTS, Server};
    use super::*;
    /// Owns exactly the supplied caller Account, never a service-pool substitute.
    pub struct Client {
        transport: super::transport::wire::Client,
        account: Account,
    }
    impl Client {
        /// Major3 hello followed by mandatory Account bind within the same deadline.
        pub fn connect(
            channel: Channel,
            deadline: u64,
            account: Account,
        ) -> Result<Self, moruna_svc::CallError<Error>> {
            let loan = account
                .0
                .duplicate(
                    moruna_svc::Rights::READ
                        | moruna_svc::Rights::WRITE
                        | moruna_svc::Rights::TRANSFER,
                )
                .map_err(moruna_svc::CallError::Kernel)?;
            let mut transport = super::transport::wire::Client::connect(channel, deadline)?;
            let remaining = deadline
                .checked_sub(moruna_svc::now())
                .filter(|n| *n > 0)
                .ok_or(moruna_svc::CallError::Kernel(moruna_svc::Error::TimedOut))?;
            transport.core().set_timeout(remaining);
            transport.bind_account(loan)?;
            Ok(Self { transport, account })
        }
        /// Same explicitly supplied actual owner for every stream/credential loan.
        pub fn account(&self) -> &Account {
            &self.account
        }
        /// Canonical timeout/channel/outstanding-request interface.
        pub fn core(&mut self) -> &mut moruna_svc::ClientCore {
            self.transport.core()
        }
        /// Transfer actual transport; server keeps its bound Account until closure.
        pub fn into_core(self) -> moruna_svc::ClientCore {
            self.transport.into_core()
        }
        /// Canonical `info` request from the single schema.
        pub fn info(&mut self) -> Result<Info, moruna_svc::CallError<Error>> {
            self.transport.info()
        }
        /// Canonical `open` request from the single schema.
        pub fn open(
            &mut self,
            to: Target,
            from: Option<Addr>,
        ) -> Result<Opened, moruna_svc::CallError<Error>> {
            self.transport.open(to, from)
        }
        /// Canonical `listen` request from the single schema.
        pub fn listen(
            &mut self,
            at: Endpoint,
            backlog: u32,
        ) -> Result<(u32, Endpoint), moruna_svc::CallError<Error>> {
            self.transport.listen(at, backlog)
        }
        /// Canonical `accept` request from the single schema.
        pub fn accept(&mut self, listener: u32) -> Result<Opened, moruna_svc::CallError<Error>> {
            self.transport.accept(listener)
        }
        /// Canonical `send_from` request from the single schema.
        pub fn send_from(
            &mut self,
            socket: u32,
            source: Memory,
            offset: u64,
            length: u32,
        ) -> Result<u32, moruna_svc::CallError<Error>> {
            self.transport.send_from(socket, source, offset, length)
        }
        /// Canonical `receive_into` request from the single schema.
        pub fn receive_into(
            &mut self,
            socket: u32,
            destination: Memory,
            offset: u64,
            max: u32,
        ) -> Result<u32, moruna_svc::CallError<Error>> {
            self.transport
                .receive_into(socket, destination, offset, max)
        }
        /// Canonical `shutdown` request from the single schema.
        pub fn shutdown(&mut self, socket: u32) -> Result<(), moruna_svc::CallError<Error>> {
            self.transport.shutdown(socket)
        }
        /// Canonical `close` request from the single schema.
        pub fn close(&mut self, socket: u32) -> Result<(), moruna_svc::CallError<Error>> {
            self.transport.close(socket)
        }
        /// Canonical `bind` request from the single schema.
        pub fn bind(
            &mut self,
            at: Endpoint,
        ) -> Result<(u32, Endpoint), moruna_svc::CallError<Error>> {
            self.transport.bind(at)
        }
        /// Canonical `send_to` request from the single schema.
        pub fn send_to(
            &mut self,
            socket: u32,
            to: Endpoint,
            bytes: Vec<u8>,
        ) -> Result<(), moruna_svc::CallError<Error>> {
            self.transport.send_to(socket, to, bytes)
        }
        /// Canonical `receive_from` request from the single schema.
        pub fn receive_from(
            &mut self,
            socket: u32,
        ) -> Result<(Endpoint, Vec<u8>), moruna_svc::CallError<Error>> {
            self.transport.receive_from(socket)
        }
        /// Canonical `resolve` request from the single schema.
        pub fn resolve(&mut self, name: String) -> Result<Vec<Addr>, moruna_svc::CallError<Error>> {
            self.transport.resolve(name)
        }
        /// Canonical `ping` request from the single schema.
        pub fn ping(&mut self, to: Addr, size: u32) -> Result<u64, moruna_svc::CallError<Error>> {
            self.transport.ping(to, size)
        }
        /// Canonical `grant` request from the single schema.
        pub fn grant(&mut self, policy: Policy) -> Result<Channel, moruna_svc::CallError<Error>> {
            self.transport.grant(policy)
        }
        /// Canonical `status` request from the single schema.
        pub fn status(
            &mut self,
            socket: u32,
        ) -> Result<SocketStatus, moruna_svc::CallError<Error>> {
            self.transport.status(socket)
        }
        /// Canonical `start_tls` request from the single schema.
        pub fn start_tls(
            &mut self,
            socket: u32,
            server_name: String,
        ) -> Result<(), moruna_svc::CallError<Error>> {
            self.transport.start_tls(socket, server_name)
        }
        /// Canonical `install_server_identity` request from the single schema.
        pub fn install_server_identity(
            &mut self,
            control: Memory,
            control_bytes: u32,
            key: Memory,
            key_bytes: u32,
        ) -> Result<u32, moruna_svc::CallError<Error>> {
            self.transport
                .install_server_identity(control, control_bytes, key, key_bytes)
        }
        /// Canonical `start_tls_server` request from the single schema.
        pub fn start_tls_server(
            &mut self,
            socket: u32,
            identity: u32,
        ) -> Result<(), moruna_svc::CallError<Error>> {
            self.transport.start_tls_server(socket, identity)
        }
        /// Canonical `release_server_identity` request from the single schema.
        pub fn release_server_identity(
            &mut self,
            identity: u32,
        ) -> Result<(), moruna_svc::CallError<Error>> {
            self.transport.release_server_identity(identity)
        }
        /// Canonical `tls_alpn` request from the single schema.
        pub fn tls_alpn(
            &mut self,
            socket: u32,
        ) -> Result<Option<Vec<u8>>, moruna_svc::CallError<Error>> {
            self.transport.tls_alpn(socket)
        }
    }
}

/// Sole bounded public server control codec; never contains private key bytes.
pub struct ServerIdentitySpec {
    /// Public DER chain, at most8 certificates/48KiB.
    pub certificates: Vec<Vec<u8>>,
    /// Explicit public ALPN identifiers.
    pub alpn: Vec<Vec<u8>>,
    /// Finite installation-relative lifetime, 1ns..24h. Expiry revokes all associated sockets.
    pub lifetime_ns: u64,
}
impl ServerIdentitySpec {
    /// Encode using the same SDK wire codec, without transferred handles.
    pub fn encode(self) -> Result<Vec<u8>, moruna_svc::WireError> {
        self.validate()?;
        let mut encoder = moruna_svc::Encoder::new();
        moruna_svc::Wire::encode(
            (self.certificates, self.alpn, self.lifetime_ns),
            &mut encoder,
        )?;
        encoder.into_control_bytes()
    }
    /// Decode one exact control body; trailing bytes and impossible bounds refuse.
    pub fn decode(bytes: &[u8]) -> Result<Self, moruna_svc::WireError> {
        if bytes.len() > limits::CHANNEL_MAX_BYTES {
            return Err(moruna_svc::WireError::TooLarge);
        }
        let mut handles = [];
        let mut decoder = moruna_svc::Decoder::new(bytes, &mut handles);
        let (certificates, alpn, lifetime_ns) = moruna_svc::Wire::decode(&mut decoder)?;
        decoder.finish()?;
        let spec = Self {
            certificates,
            alpn,
            lifetime_ns,
        };
        spec.validate()?;
        Ok(spec)
    }
    fn validate(&self) -> Result<(), moruna_svc::WireError> {
        if self.lifetime_ns == 0
            || self.lifetime_ns > 86_400_000_000_000
            || self.certificates.is_empty()
            || self.certificates.len() > 8
            || self.certificates.iter().any(Vec::is_empty)
            || self.certificates.iter().map(Vec::len).sum::<usize>() > 48 * 1024
            || self.alpn.len() > 16
            || self.alpn.iter().any(|p| p.is_empty() || p.len() > 255)
        {
            return Err(moruna_svc::WireError::TooLarge);
        }
        Ok(())
    }
}
#[cfg(test)]
mod server_control_tests {
    use super::*;
    #[test]
    fn public_server_control_lifetime_framing_and_bounds_are_exact() {
        let spec = ServerIdentitySpec {
            certificates: alloc::vec![alloc::vec![1, 2, 3]],
            alpn: alloc::vec![b"h2".to_vec()],
            lifetime_ns: 1000,
        };
        let bytes = spec.encode().unwrap();
        let decoded = ServerIdentitySpec::decode(&bytes).unwrap();
        assert_eq!(decoded.lifetime_ns, 1000);
        assert_eq!(decoded.alpn, [b"h2".to_vec()]);
        let mut trailing = bytes;
        trailing.push(0);
        assert!(ServerIdentitySpec::decode(&trailing).is_err());
        for lifetime_ns in [0, u64::MAX, 86_400_000_000_001] {
            assert!(
                ServerIdentitySpec {
                    certificates: alloc::vec![alloc::vec![1]],
                    alpn: Vec::new(),
                    lifetime_ns
                }
                .encode()
                .is_err()
            );
        }
    }
}
