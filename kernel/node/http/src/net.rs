//! A connection through the network server's sockets (`moruna.net`), as a [`Stream`]: to an
//! address (`platform`, to instance data), or to a name the server resolves under the client's
//! policy, with TLS verified for that name (`node-admission`, to a function image's host; system
//! design 3D). The bytes move through memory of the client's own account, lent for each call,
//! as `moruna.net` requires.

use alloc::{format, string::String};

use moruna_svc::{CallError, DEADLINE_INFINITE, Error, Memory, Rights, map, now};
use netstack_protocol::{Target, net};

use crate::{Stream, StreamError};

/// The bytes one send or receive moves: the size of the memory lent to the network server.
pub const LOAN_BYTES: usize = 64 * 1024;

/// A kernel refusal, in words.
pub fn kernel(error: Error) -> StreamError {
    StreamError::Other(format!("the kernel refused: {}", error.name()))
}

/// The network server's answer to a call, as a stream's error. A refusal by the client's policy
/// or a failed TLS handshake names itself.
pub fn failure(error: CallError<net::Error>) -> StreamError {
    match error {
        CallError::Remote(net::Error::Refused) => StreamError::Refused,
        CallError::Remote(net::Error::TimedOut) => StreamError::TimedOut,
        CallError::Remote(net::Error::NoRoute) => StreamError::Unreachable,
        CallError::Remote(net::Error::Reset) => StreamError::Reset,
        CallError::Remote(net::Error::NotPermitted) => {
            StreamError::Other(String::from("the network's policy does not permit it"))
        }
        CallError::Remote(net::Error::TlsFailed) => StreamError::Other(String::from(
            "TLS failed: the peer's certificate is not verified for its name",
        )),
        CallError::Remote(net::Error::TlsUnavailable) => StreamError::Other(String::from(
            "TLS is unavailable: the network server has no roots or the wall clock is unset",
        )),
        CallError::TimedOut => StreamError::Deadline,
        other => StreamError::Other(format!("the network server answered {other:?}")),
    }
}

/// Gives `client`'s next call until `deadline`.
fn arm(client: &mut net::Client, deadline: u64) -> Result<(), StreamError> {
    let left = match deadline {
        DEADLINE_INFINITE => DEADLINE_INFINITE,
        deadline => deadline.saturating_sub(now()),
    };
    if left == 0 {
        return Err(StreamError::Deadline);
    }
    client.core().set_timeout(left);
    Ok(())
}

/// One TCP connection through `client`. Dropping it closes the socket.
pub struct NetStream<'c> {
    client: &'c mut net::Client,
    loan: Memory,
    socket: u32,
}

impl<'c> NetStream<'c> {
    /// Connects to `to` through `client` by `deadline` and, with `tls`, upgrades the connection
    /// before any byte moves to TLS verified for that name, under the network server's roots
    /// and the node's wall clock. A refusal by the client's policy, a failed handshake and a
    /// server with no TLS are each an error; there is never a plaintext fallback.
    pub fn open(
        client: &'c mut net::Client,
        to: Target,
        tls: Option<&str>,
        deadline: u64,
    ) -> Result<NetStream<'c>, StreamError> {
        let loan = Memory::create_in(LOAN_BYTES as u64, &client.account().0).map_err(kernel)?;
        arm(client, deadline)?;
        let opened = client.open(to, None).map_err(failure)?;
        let stream = NetStream::adopt(client, loan, opened.socket);
        if let Some(name) = tls {
            arm(stream.client, deadline)?;
            stream
                .client
                .start_tls(stream.socket, String::from(name))
                .map_err(failure)?;
        }
        Ok(stream)
    }

    /// The connection `socket` of `client`'s, one it opened or accepted, moving its bytes through
    /// `loan`, memory of the client's own account at least [`LOAN_BYTES`] long.
    pub fn adopt(client: &'c mut net::Client, loan: Memory, socket: u32) -> NetStream<'c> {
        NetStream {
            client,
            loan,
            socket,
        }
    }

    /// A handle to the lent memory with `rights`, for the network server to keep for one call.
    fn lend(&self, rights: Rights) -> Result<Memory, StreamError> {
        self.loan.0.duplicate(rights).map(Memory).map_err(kernel)
    }
}

impl Stream for NetStream<'_> {
    fn write(&mut self, bytes: &[u8], deadline: u64) -> Result<usize, StreamError> {
        let count = bytes.len().min(LOAN_BYTES);
        arm(self.client, deadline)?;
        self.loan
            .map(0, LOAN_BYTES as u64, map::WRITE)
            .and_then(|mapping| mapping.write(0, &bytes[..count]))
            .map_err(kernel)?;
        let source = self.lend(Rights::READ | Rights::MAP | Rights::TRANSFER)?;
        let taken = self
            .client
            .send_from(self.socket, source, 0, count as u32)
            .map_err(failure)?;
        Ok(taken as usize)
    }

    fn read(&mut self, buffer: &mut [u8], deadline: u64) -> Result<usize, StreamError> {
        let most = buffer.len().min(LOAN_BYTES);
        arm(self.client, deadline)?;
        let destination =
            self.lend(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)?;
        let count = self
            .client
            .receive_into(self.socket, destination, 0, most as u32)
            .map_err(failure)? as usize;
        if count > most {
            return Err(StreamError::Other(String::from(
                "the network server gave more bytes than were asked for",
            )));
        }
        self.loan
            .map(0, LOAN_BYTES as u64, map::READ)
            .and_then(|mapping| mapping.read(0, &mut buffer[..count]))
            .map_err(kernel)?;
        Ok(count)
    }
}

impl Drop for NetStream<'_> {
    fn drop(&mut self) {
        // The connection is over either way; a socket the server already closed is no error.
        let _ = self.client.close(self.socket);
    }
}
