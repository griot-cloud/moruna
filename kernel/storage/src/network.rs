//! Actual KE2 major-2 TLS socket ingress into retained caller Memory. TCP/TLS internal
//! staging remains bounded/charged by KE2; these calls do not claim NIC DMA or zero-copy
//! encryption. TCP sends transfer actual account-bound Memory loans, not payload Vecs.
use crate::memory::Loan;
use alloc::{boxed::Box, string::String};
use moruna_svc::{CallError, Channel, DEADLINE_INFINITE, Encoder, Memory, Rights, Wire};
use netstack_protocol::{RECEIVE_MAX, SEND_MAX, Target, net};

/// Actual network transport diagnostic, not a fabricated empty successful object.
#[derive(Debug)]
pub enum Error {
    /// Original policy, TLS, socket, timeout or SDK refusal.
    Network(CallError<net::Error>),
    /// Actual Memory mapping/rights/bounds refusal.
    Memory(moruna_svc::Error),
    /// Peer ended before the exact admitted object/control bytes arrived.
    Incomplete,
    /// Invalid bound, count or state from the peer.
    Invalid,
    /// Connection was closed after an uncertain operation or uncompleted socket owner.
    Closed,
    /// Cleanup also failed; both original diagnostics remain available.
    Cleanup {
        /// Original failure.
        primary: Box<Error>,
        /// Real close failure.
        cleanup: Box<Error>,
    },
}
/// Explicit already-connected KE2 endpoint, never listener guessing or ambient egress.
pub struct Network {
    client: Option<net::Client>,
    sent: u64,
    received: u64,
    timeout_ns: u64,
}
impl Network {
    /// Hello on the actual boot-granted connected network capability with finite timeout.
    pub fn new(
        channel: Channel,
        timeout_ns: u64,
        account: &moruna_svc::Handle,
    ) -> Result<Self, Error> {
        if timeout_ns == 0 || timeout_ns == u64::MAX {
            return Err(Error::Invalid);
        }
        let bound = moruna_svc::Account(
            account
                .duplicate(Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER)
                .map_err(Error::Memory)?,
        );
        let mut client = net::Client::connect(channel, moruna_svc::deadline_in(timeout_ns), bound)
            .map_err(Error::Network)?;
        client.core().set_timeout(timeout_ns);
        Ok(Self {
            client: Some(client),
            sent: 0,
            received: 0,
            timeout_ns,
        })
    }
    /// Actual accepted source-Memory bytes and successful caller-Memory ingress bytes.
    /// KE2 TCP/TLS staging remains separately charged; this is not NIC DMA proof.
    pub fn bytes(&self) -> (u64, u64) {
        (self.sent, self.received)
    }
    /// Establish explicit target then verified TLS1.3 under KE2's real roots/clock/policy.
    /// Missing TLS or rejected identity fails; plaintext is never used as a fallback.
    pub fn open(&mut self, target: Target, server_name: String) -> Result<Socket<'_>, Error> {
        if server_name.is_empty() {
            return Err(Error::Invalid);
        }
        let client = self.client.as_mut().ok_or(Error::Closed)?;
        let opened = match client.open(target, None) {
            Ok(opened) => opened,
            Err(cause) => {
                self.client = None;
                return Err(Error::Network(cause));
            }
        };
        if let Err(primary) = client.start_tls(opened.socket, server_name) {
            let close = client.close(opened.socket);
            if let Err(cleanup) = close {
                self.client = None;
                return Err(Error::Cleanup {
                    primary: Box::new(Error::Network(primary)),
                    cleanup: Box::new(Error::Network(cleanup)),
                });
            }
            return Err(Error::Network(primary));
        }
        Ok(Socket {
            network: self,
            id: opened.socket,
            closed: false,
        })
    }
}
/// One owned exact-response stream. Dropping before explicit close closes the whole
/// connection rather than silently leaving a live untracked socket for account recycle.
pub struct Socket<'a> {
    network: &'a mut Network,
    id: u32,
    closed: bool,
}
impl Socket<'_> {
    /// Send exact admitted source bytes through bounded SDK messages, retaining the
    /// original source owner in the caller. No body-size allocation occurs here.
    pub fn send(&mut self, source: &Loan, offset: u64, bytes: u64) -> Result<(), Error> {
        let result = self.send_inner(source, offset, bytes);
        if result.is_err() {
            self.network.client = None;
        }
        result
    }
    fn send_inner(&mut self, source: &Loan, offset: u64, bytes: u64) -> Result<(), Error> {
        if offset
            .checked_add(bytes)
            .is_none_or(|end| end > source.len())
        {
            return Err(Error::Invalid);
        }
        let mut at = 0;
        while at < bytes {
            let count = (bytes - at).min(SEND_MAX as u64) as usize;
            let loan = source
                .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
                .map_err(Error::Memory)?;
            let source_offset = source
                .offset()
                .checked_add(offset)
                .and_then(|n| n.checked_add(at))
                .ok_or(Error::Invalid)?;
            let taken = self
                .network
                .client
                .as_mut()
                .ok_or(Error::Closed)?
                .send_from(self.id, loan, source_offset, count as u32)
                .map_err(Error::Network)?;
            if taken == 0 || taken as usize > count {
                return Err(Error::Invalid);
            }
            self.network.sent = self
                .network
                .sent
                .checked_add(taken as u64)
                .ok_or(Error::Invalid)?;
            at += taken as u64;
        }
        Ok(())
    }
    /// Receive exactly into validated caller-owned charged Memory; no payload Vec or
    /// aliased Rust slice is created. EOF and overlarge peer counts refuse explicitly.
    pub fn receive(&mut self, destination: &Loan, offset: u64, bytes: u64) -> Result<(), Error> {
        let result = self.receive_inner(destination, offset, bytes);
        if result.is_err() {
            self.network.client = None;
        }
        result
    }
    fn receive_inner(&mut self, destination: &Loan, offset: u64, bytes: u64) -> Result<(), Error> {
        if offset
            .checked_add(bytes)
            .is_none_or(|end| end > destination.len())
        {
            return Err(Error::Invalid);
        }
        let rights = Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER;
        let mut at = 0;
        while at < bytes {
            let count = (bytes - at).min(RECEIVE_MAX as u64) as u32;
            let loan = destination.duplicate(rights).map_err(Error::Memory)?;
            let at_memory = destination
                .offset()
                .checked_add(offset)
                .and_then(|n| n.checked_add(at))
                .ok_or(Error::Invalid)?;
            let received = self.receive_chunk(loan, at_memory, count)?;
            if received == 0 {
                return Err(Error::Incomplete);
            }
            if received > count {
                return Err(Error::Invalid);
            }
            self.network.received = self
                .network
                .received
                .checked_add(received as u64)
                .ok_or(Error::Invalid)?;
            at += received as u64;
        }
        Ok(())
    }
    fn receive_chunk(&mut self, loan: Memory, offset: u64, max: u32) -> Result<u32, Error> {
        let client = self.network.client.as_mut().ok_or(Error::Closed)?;
        let mut body = Encoder::new();
        self.id.encode(&mut body).map_err(|_| Error::Invalid)?;
        loan.encode(&mut body).map_err(|_| Error::Invalid)?;
        offset.encode(&mut body).map_err(|_| Error::Invalid)?;
        max.encode(&mut body).map_err(|_| Error::Invalid)?;
        let id = client
            .core()
            .send(6, body)
            .map_err(|e| Error::Network(e.typed()))?;
        let mut timed_out = false;
        let first = client
            .core()
            .reply(id, moruna_svc::deadline_in(self.network.timeout_ns));
        let mut frame = match first {
            Err(CallError::TimedOut) => {
                timed_out = true;
                let mut close = Encoder::new();
                self.id.encode(&mut close).map_err(|_| Error::Invalid)?;
                let close_id = match client.core().send(8, close) {
                    Ok(close_id) => close_id,
                    Err(close_error) => {
                        // Failed cancellation submission says NOTHING about the
                        // original mapping lifetime. Keep waiting its actual result.
                        let original = client.core().reply(id, DEADLINE_INFINITE);
                        return match original {
                            Ok(_) => Err(Error::Network(close_error.typed())),
                            Err(original_error) => Err(Error::Cleanup {
                                primary: Box::new(Error::Network(original_error.typed())),
                                cleanup: Box::new(Error::Network(close_error.typed())),
                            }),
                        };
                    }
                };
                // A close ACK alone cannot release the destination. Consume the
                // ORIGINAL receive reply first; KE2 cancels its owned mapping before
                // answering that request. CPU-only copying has no device DMA into it.
                let original = client.core().reply(id, DEADLINE_INFINITE);
                let close = client.core().reply(close_id, DEADLINE_INFINITE);
                if let Err(error) = close {
                    return Err(Error::Network(error.typed()));
                }
                original
            }
            other => other,
        }
        .map_err(|e| Error::Network(e.typed()))?;
        if timed_out {
            return Err(Error::Network(CallError::TimedOut));
        }
        let mut decoder = frame.decoder();
        let count = u32::decode(&mut decoder).map_err(|_| Error::Invalid)?;
        decoder.finish().map_err(|_| Error::Invalid)?;
        Ok(count)
    }
    /// Finish this owned socket, reporting actual close failures rather than discarding.
    pub fn close(mut self) -> Result<(), Error> {
        let result = self
            .network
            .client
            .as_mut()
            .ok_or(Error::Closed)?
            .close(self.id)
            .map_err(Error::Network);
        self.closed = result.is_ok();
        if result.is_err() {
            self.network.client = None;
        }
        result
    }
}
impl Drop for Socket<'_> {
    fn drop(&mut self) {
        if !self.closed {
            self.network.client = None;
        }
    }
}
