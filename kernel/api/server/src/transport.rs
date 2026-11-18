//! What the caller gives the server: each connection as a non-blocking byte stream, and the time.

/// Why a transport call moved no bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// Nothing can move now. The server tries again on its next poll.
    WouldBlock,
    /// The connection is broken. The server closes it and forgets it.
    Broken,
}

/// One connection, as a non-blocking byte stream: the TLS bytes from and to the client.
///
/// The server only reads, writes and closes. It never waits: a call that cannot move bytes
/// now answers [`TransportError::WouldBlock`], and the caller polls the server again once the
/// stream has changed.
pub trait Transport {
    /// Reads up to `buf.len()` bytes into `buf` and says how many. `Ok(0)` means the client has
    /// closed its side and will send nothing more.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;

    /// Writes bytes from the start of `buf` and says how many were taken.
    fn write(&mut self, buf: &[u8]) -> Result<usize, TransportError>;

    /// Closes the connection. The server calls it once, then drops the transport.
    fn close(&mut self);
}

/// The time, which the server asks the caller for.
pub trait Clock {
    /// A monotonic clock, in milliseconds from any fixed start. It times the idle limit.
    fn monotonic_ms(&self) -> u64;

    /// The node's one wall clock, in Unix milliseconds (system design 3D). The server reads it
    /// once a poll: a job's times are that reading, and all else the poll does is it in
    /// seconds.
    fn unix_ms(&self) -> u64;

    /// The wall clock, in Unix seconds: [`Clock::unix_ms`] divided by 1,000. The node
    /// document's `time`, a token's expiry, the audit record's time, and a job's times in
    /// seconds.
    fn unix_seconds(&self) -> u64 {
        self.unix_ms() / 1_000
    }
}
