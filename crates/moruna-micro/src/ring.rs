//! A task's text (SDD 29 f.7): the ring of `moruna-record` records the personality writes a
//! `run_plane` task's lines into, one record a line, its `stream` header `stdout` or `stderr`.
//! The engine formats the ring in a memory object of its own, hands the personality a
//! duplicate, and takes the lines out as they come.

use linux_protocol::log;
use moruna_record::{record::Decoded, ring::Ring};
use moruna_sys::{Handle, Memory, Rights, map};

/// Which stream a line was written to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// The task's standard output (its text when it has no output slot).
    Stdout,
    /// Its standard error.
    Stderr,
}

/// One task's log ring.
pub struct LogRing {
    memory: Memory,
    bytes: u64,
}

impl LogRing {
    /// A ring of `bytes` (whole pages), formatted.
    pub fn new(bytes: u64) -> Result<LogRing, String> {
        let memory = Memory::create(bytes).map_err(|e| format!("memory_create: {}", e.name()))?;
        {
            let mut mapping = memory
                .map(0, bytes, map::READ | map::WRITE)
                .map_err(|e| format!("mapping the log ring: {}", e.name()))?;
            let region = mapping
                .shared_region()
                .map_err(|e| format!("the log ring's region: {}", e.name()))?;
            Ring::init(region).map_err(|e| format!("formatting the log ring: {e:?}"))?;
        }
        Ok(LogRing { memory, bytes })
    }

    /// The ring's size in bytes.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The handle the personality writes the task's lines through.
    pub fn give(&self) -> Result<Handle, String> {
        self.memory
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .map_err(|e| format!("duplicating the log ring: {}", e.name()))
    }

    /// Every line in the ring now, in order, taken out of it.
    pub fn drain(&self) -> Result<Vec<(Stream, Vec<u8>)>, String> {
        let mut mapping = self
            .memory
            .map(0, self.bytes, map::READ | map::WRITE)
            .map_err(|e| format!("mapping the log ring: {}", e.name()))?;
        let region = mapping
            .shared_region()
            .map_err(|e| format!("the log ring's region: {}", e.name()))?;
        let ring = Ring::attach(region).map_err(|e| format!("attaching the log ring: {e:?}"))?;
        let mut consumer = ring
            .consumer()
            .map_err(|e| format!("the log ring's consumer: {e:?}"))?;
        let mut out = Vec::new();
        while let Some(bytes) = consumer.pop() {
            match moruna_record::record::decode(bytes) {
                Decoded::Record(r, _) => {
                    let stream = match r.headers.iter().find(|(k, _)| *k == log::STREAM) {
                        Some((_, v)) if *v == log::STDERR => Stream::Stderr,
                        _ => Stream::Stdout,
                    };
                    out.push((stream, r.body.to_vec()));
                }
                other => return Err(format!("a log record that does not decode: {other:?}")),
            }
        }
        Ok(out)
    }
}
