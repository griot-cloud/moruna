//! What the data plane's boot tests share (SDD 29 k): the ports the engine answers, the
//! roles it gives its tasks, and how a role and a report are written.
//!
//! A task learns what to do from its first port call: the engine answers [`port::ROLE`]
//! with a [`Role`], its numbers and its bytes (a framing to write, a record to put). A task
//! tells the engine what it measured with [`port::REPORT`], a list of `u64`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};

use moruna_ipc_check::{
    frame,
    record::{self, Need},
};

/// Where a record's first column keeps its validity bitmap and its values, and how long
/// the record is, from its framing alone: what a task that writes a record or reads one
/// in place needs, taken with the engine's own check.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub record_len: u64,
    pub validity: (u64, u64),
    pub values: (u64, u64),
}

/// The layout of the record whose framing `head` begins with (a page is enough).
pub fn layout(head: &[u8]) -> Result<Layout, String> {
    let end = match record::framing_need(head)? {
        Need::Framing(n) if n <= head.len() => n,
        _ => return Err(String::from("a framing past what was given")),
    };
    let (_, batch) = record::stream_parts(&head[..end])?;
    let body = u64::try_from(frame::body_length(&head[batch])?)
        .map_err(|_| String::from("a negative body length"))?;
    let record_len = end as u64 + body;
    let checked = record::check_record(&head[..end], record_len)?;
    let column = checked
        .columns
        .first()
        .ok_or_else(|| String::from("a record of no columns"))?;
    match column.buffers.as_slice() {
        [validity, values, ..] => Ok(Layout {
            record_len,
            validity: *validity,
            values: *values,
        }),
        _ => Err(String::from(
            "a first column without a validity bitmap and values",
        )),
    }
}

/// The ports the test engine answers.
pub mod port {
    /// What this task is to do: the reply is a role.
    pub const ROLE: u64 = 1;
    /// What it measured: `u64`s, answered with nothing.
    pub const REPORT: u64 = 2;
    /// The request comes back as the reply.
    pub const ECHO: u64 = 3;
    /// Answered when the engine has offered what the task is to read.
    pub const WAIT: u64 = 4;
    /// A port the engine refuses (`AccessDenied`).
    pub const NONE: u64 = 99;
}

/// What a task does.
pub mod role {
    /// Writes a record of `a` bytes from the framing given, values `0, 1, ..` in its
    /// one `Int64` column, and puts it; reports the frames and copies (DP-T10).
    pub const PRODUCE_BIG: u8 = 1;
    /// Takes one batch and reports its frames, its copies and three values (DP-T10).
    pub const CONSUME_BIG: u8 = 2;
    /// Puts the `a` bytes of the record given (DP-T11, DP-T12).
    pub const PUT: u8 = 3;
    /// Takes one batch and reports the frame of its first page and its first value
    /// (DP-T11).
    pub const CONSUME_ONE: u8 = 4;
    /// Probes its plane's bounds (DP-T13).
    pub const ISOLATE: u8 = 5;
    /// `a` port calls to the echo port (DP-T14).
    pub const ECHO: u8 = 6;
    /// Puts `a` records of the framing given, each in its own window (DP-T15).
    pub const PRODUCE_MANY: u8 = 7;
    /// Takes `a` batches (DP-T15).
    pub const CONSUME_MANY: u8 = 8;
}

/// A role as the engine writes it: the role, two numbers and bytes.
pub struct Role<'a> {
    pub role: u8,
    pub a: u64,
    pub b: u64,
    pub bytes: &'a [u8],
}

impl Role<'_> {
    pub const HEADER: usize = 24;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER + self.bytes.len());
        out.extend_from_slice(&u64::from(self.role).to_le_bytes());
        out.extend_from_slice(&self.a.to_le_bytes());
        out.extend_from_slice(&self.b.to_le_bytes());
        out.extend_from_slice(self.bytes);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Role<'_>> {
        let word = |i: usize| -> Option<u64> {
            let w = bytes.get(i * 8..i * 8 + 8)?;
            let mut b = [0u8; 8];
            b.copy_from_slice(w);
            Some(u64::from_le_bytes(b))
        };
        Some(Role {
            role: u8::try_from(word(0)?).ok()?,
            a: word(1)?,
            b: word(2)?,
            bytes: bytes.get(Self::HEADER..)?,
        })
    }
}

/// A report: `u64`s, little endian.
pub fn report(values: &[u64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// The `u64`s of a report.
pub fn values(bytes: &[u8]) -> Vec<u64> {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect()
}

/// The least and the median of `samples`, which it sorts.
pub fn least_median(samples: &mut [u64]) -> (u64, u64) {
    samples.sort_unstable();
    match samples.len() {
        0 => (0, 0),
        n => (samples[0], samples[n / 2]),
    }
}
