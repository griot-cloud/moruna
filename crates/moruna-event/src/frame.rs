//! The framed binary message of the host protocol that carries one event (MH 4.3, version 7).
//!
//! The protocol's messages are lines of JSON, each starting with `{`. An event is a record
//! (SDD 21), which is bytes, so it travels as a frame: [`FRAME`], one byte that no line starts
//! with, then the record exactly as `moruna_record::encode_into` wrote it. The record is its own
//! delimiter: its first four bytes are the number of bytes after them. A reader that sees
//! [`FRAME`] reads those four bytes, then that many more, and has one event; a reader that sees
//! anything else reads a line.

use std::io::{self, BufRead, Write};

use moruna_record::{FIXED, MAX_RECORD};

/// The first byte of a frame. NUL is not the start of a JSON line and not whitespace.
pub const FRAME: u8 = 0x00;

/// Write one event's record as a frame, whole in one write, and flush. `scratch` holds the frame
/// while it is written and keeps its capacity, so a writer that keeps one allocates nothing once
/// it has grown to the largest record.
pub fn write_frame(out: &mut dyn Write, scratch: &mut Vec<u8>, record: &[u8]) -> io::Result<()> {
    scratch.clear();
    scratch.push(FRAME);
    scratch.extend_from_slice(record);
    out.write_all(scratch)?;
    out.flush()
}

/// Read the record of a frame whose [`FRAME`] byte the reader is at (`reader.fill_buf()` begins
/// with it). A length no record has is an `InvalidData` error: a peer cannot make the reader hold
/// an unbounded buffer.
pub fn read_frame(reader: &mut dyn BufRead) -> io::Result<Vec<u8>> {
    let mut mark = [0u8; 1];
    reader.read_exact(&mut mark)?;
    if mark[0] != FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a frame does not start with its mark",
        ));
    }
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if !(FIXED - 4..=MAX_RECORD).contains(&n) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a frame's record says it has {n} bytes, which no record has"),
        ));
    }
    let mut record = vec![0u8; 4 + n];
    record[..4].copy_from_slice(&len);
    reader.read_exact(&mut record[4..])?;
    Ok(record)
}

/// Whether the next message the reader holds is a frame; `None` at the end of the stream.
pub fn next_is_frame(reader: &mut dyn BufRead) -> io::Result<Option<bool>> {
    let available = reader.fill_buf()?;
    Ok(available.first().map(|b| *b == FRAME))
}
