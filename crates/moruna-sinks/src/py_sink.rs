//! `PySink`: a user's `moruna.Sink` subclass, written to through the interpreter (08 f.10, e.6).
//!
//! The user's object receives each morsel as a `pyarrow.RecordBatch` through `write(batch)`, is
//! told the run is over through `finish()`, and may take part in resume through
//! `checkpoint() -> bytes | None` and `restore(state)`. A sink whose `checkpoint` returns `None`
//! (the base class's default) is not resumable, which the contract already spells as
//! `checkpoint()? == None` at startup (contracts d.8, SC f.11), and the run says so.
//!
//! The batch handed to Python is exported with `pyo3-arrow` over the payload's own arena
//! buffers: no copy (SI-I4), and a sink that keeps a batch after `write` returns keeps those
//! bytes under the run's budget until it lets go of it.
//!
//! Commit and replay. The user's state is opaque bytes, so this sink cannot remove output above a
//! watermark the way a file sink does (f.8). Instead, `write` returning is the commit (the batch
//! is in the user's hands), every call into the object is serialised under one mutex, and the
//! checkpoint records, beside the user's bytes, exactly which sequence numbers that state holds:
//! everything through the sink's watermark at that moment and the set above it. The scheduler
//! reads its watermark before the sink's state (SC f.12), so on resume every sequence the manifest
//! did not count as committed is delivered again; those the restored state already holds are
//! acknowledged without calling `write`, and the rest are written. The user sees each sequence
//! exactly once.

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};

use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyBytes};
use serde::{Deserialize, Serialize};

use moruna_kernel::{
    BoxFuture, MorunaError, Payload, PayloadKind, PayloadSpec, Result, Seq, Sink, SinkSummary,
    SourceSchema, TierPref,
};

use crate::{Phase, require_host};

/// The first bytes of this sink's checkpoint (e.6).
const MAGIC: &[u8; 4] = b"MPYS";
/// The version of the checkpoint header this build writes and accepts.
const VERSION: u32 = 1;

/// What a Python sink has done so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PySinkStats {
    /// Calls to the user's `write` that returned.
    pub writes: u64,
    /// Rows of those batches.
    pub rows: u64,
    /// Bytes of those batches, as `pyarrow.RecordBatch.nbytes` measures them.
    pub bytes: u64,
    /// Sequences a resumed sink acknowledged without calling `write`, because the restored
    /// state already held them.
    pub replayed: u64,
}

/// The header of this sink's checkpoint (e.6): which sequence numbers the user's state holds,
/// and the summary so far, so `SinkSummary` stays exact across a resume (SI-I7).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Header {
    version: u32,
    /// Every sequence at or below this was written or skipped when the state was taken.
    through: Option<Seq>,
    /// Sequences above `through` that were written or skipped when the state was taken.
    above: BTreeSet<Seq>,
    rows: u64,
    bytes: u64,
    writes: u64,
}

struct Inner {
    phase: Phase,
    /// The lowest sequence number neither written nor skipped.
    watermark_next: Seq,
    /// Sequences at or above `watermark_next` that are written or skipped.
    done: BTreeSet<Seq>,
    /// After a resume: the sequences the restored state already holds.
    holds: Option<Header>,
    stats: PySinkStats,
    /// Decided by the first `checkpoint` call: `Some(false)` when it returned `None`.
    resumable: Option<bool>,
}

impl Inner {
    fn mark(&mut self, seq: Seq) {
        if seq < self.watermark_next {
            return;
        }
        self.done.insert(seq);
        while self.done.remove(&self.watermark_next) {
            self.watermark_next += 1;
        }
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.watermark_next.checked_sub(1)
    }
}

/// A sink over a user's `moruna.Sink` subclass (d.1).
pub struct PySink {
    object: Py<PyAny>,
    /// The name of the user's class, for every message.
    name: String,
    /// Held across every call into the object, taken before attaching (g), so a checkpoint never
    /// interleaves with a write and the user's code is never entered from two threads at once.
    inner: Mutex<Inner>,
}

impl PySink {
    /// `object` must have a callable `write`; `finish`, `checkpoint` and `restore` are optional.
    pub fn new(object: Py<PyAny>) -> Result<PySink> {
        let name = Python::attach(|py| {
            let bound = object.bind(py);
            let name = class_name(bound);
            let callable = bound
                .getattr(pyo3::intern!(py, "write"))
                .map(|w| w.is_callable())
                .unwrap_or(false);
            if !callable {
                return Err(MorunaError::Plan(format!(
                    "{name} has no write(batch) method; a moruna.Sink subclass must define one"
                )));
            }
            Ok(name)
        })?;
        Ok(PySink {
            object,
            name,
            inner: Mutex::new(Inner {
                phase: Phase::Created,
                watermark_next: 0,
                done: BTreeSet::new(),
                holds: None,
                stats: PySinkStats::default(),
                resumable: None,
            }),
        })
    }

    /// What this sink has done so far (j).
    pub fn stats(&self) -> PySinkStats {
        self.lock().stats.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write_now(&self, seq: Seq, payload: Payload) -> Result<()> {
        let mut inner = self.lock();
        inner.phase.require_open()?;
        let already = inner.holds.as_ref().is_some_and(|h| {
            h.through.is_some_and(|through| seq <= through) || h.above.contains(&seq)
        });
        if already {
            inner.stats.replayed += 1;
            inner.mark(seq);
            return Ok(());
        }
        require_host(&payload)?;
        let batch = match payload {
            Payload::Table(batch, _) => batch,
            Payload::Tensor(_, _) => {
                return Err(MorunaError::Sink(format!(
                    "{} is a Python sink, which takes tables; a tensor reached it",
                    self.name
                )));
            }
        };
        let rows = batch.num_rows() as u64;
        let fallback = batch.get_array_memory_size() as u64;
        let outcome = Python::attach(|py| {
            let exported = pyo3_arrow::PyRecordBatch::new(batch)
                .into_pyarrow(py)
                .map_err(|e| format!("the batch could not be handed to Python: {e}"))?;
            let bytes = exported
                .getattr(pyo3::intern!(py, "nbytes"))
                .and_then(|n| n.extract::<u64>())
                .unwrap_or(fallback);
            self.object
                .bind(py)
                .call_method1(pyo3::intern!(py, "write"), (exported,))
                .map_err(|e| format!("{}.write(): {e}", self.name))?;
            Ok::<u64, String>(bytes)
        });
        match outcome {
            Ok(bytes) => {
                inner.stats.writes += 1;
                inner.stats.rows += rows;
                inner.stats.bytes += bytes;
                inner.mark(seq);
                Ok(())
            }
            Err(msg) => {
                inner.phase = Phase::Failed(msg.clone());
                Err(MorunaError::Sink(msg))
            }
        }
    }
}

impl Sink for PySink {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        let mut inner = self.lock();
        inner.phase.require_created()?;
        if let SourceSchema::Tensor { .. } = schema {
            return Err(MorunaError::Sink(format!(
                "{} is a Python sink, which takes tables; the chain gives it tensors",
                self.name
            )));
        }
        // f.1: the chain's schema is provisional and every batch carries its own, so the user's
        // object is not told it; the first batch it receives is the schema it writes.
        inner.phase = Phase::Open;
        Ok(())
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        let outcome = self.write_now(seq, payload);
        Box::pin(async move { outcome })
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        let mut inner = self.lock();
        match &inner.phase {
            Phase::Created => return Err(MorunaError::Sink("the sink is not open".into())),
            Phase::Finished => return Err(MorunaError::Sink("finish ran twice".into())),
            Phase::Failed(msg) => {
                let msg = msg.clone();
                inner.phase = Phase::Finished;
                return Err(MorunaError::Sink(msg));
            }
            Phase::Open => {}
        }
        let outcome = Python::attach(|py| {
            let bound = self.object.bind(py);
            if !bound.hasattr(pyo3::intern!(py, "finish")).unwrap_or(false) {
                return Ok(());
            }
            bound
                .call_method0(pyo3::intern!(py, "finish"))
                .map(|_| ())
                .map_err(|e| format!("{}.finish(): {e}", self.name))
        });
        inner.phase = Phase::Finished;
        outcome.map_err(MorunaError::Sink)?;
        Ok(SinkSummary {
            rows: inner.stats.rows,
            bytes: inner.stats.bytes,
            files: Vec::new(),
        })
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.lock().committed_seq()
    }

    fn skip(&self, seq: Seq) {
        self.lock().mark(seq);
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        let mut inner = self.lock();
        let state = Python::attach(|py| {
            let bound = self.object.bind(py);
            if !bound
                .hasattr(pyo3::intern!(py, "checkpoint"))
                .unwrap_or(false)
            {
                return Ok(None);
            }
            let returned = bound
                .call_method0(pyo3::intern!(py, "checkpoint"))
                .map_err(|e| MorunaError::Sink(format!("{}.checkpoint(): {e}", self.name)))?;
            if returned.is_none() {
                return Ok(None);
            }
            returned.extract::<Vec<u8>>().map(Some).map_err(|_| {
                MorunaError::Sink(format!(
                    "{}.checkpoint() returned {}, not bytes or None",
                    self.name,
                    class_name(&returned)
                ))
            })
        })?;
        let Some(state) = state else {
            if inner.resumable == Some(true) {
                return Err(MorunaError::Sink(format!(
                    "{}.checkpoint() returned None after returning bytes; a sink is resumable \
                     for the whole run or not at all",
                    self.name
                )));
            }
            inner.resumable = Some(false);
            return Ok(None);
        };
        inner.resumable = Some(true);
        let header = Header {
            version: VERSION,
            through: inner.committed_seq(),
            above: inner.done.clone(),
            rows: inner.stats.rows,
            bytes: inner.stats.bytes,
            writes: inner.stats.writes,
        };
        encode(&header, &state).map(Some)
    }

    fn resume(
        &mut self,
        schema: &SourceSchema,
        state: &[u8],
        committed_seq: Option<Seq>,
    ) -> Result<()> {
        let mut inner = self.lock();
        inner.phase.require_created()?;
        if let SourceSchema::Tensor { .. } = schema {
            return Err(MorunaError::Sink(format!(
                "{} is a Python sink, which takes tables; the chain gives it tensors",
                self.name
            )));
        }
        let (header, user) = decode(state)?;
        // The watermark is read before the state (SC f.12), so the state holds at least every
        // sequence the manifest counts as committed. One that holds less would lose the rows in
        // between, which a correct checkpoint cannot produce; it is refused rather than guessed at.
        if committed_seq > header.through {
            return Err(MorunaError::Resume(format!(
                "{}'s checkpoint holds sequences through {:?}, below the manifest's watermark \
                 {committed_seq:?}; the rows in between would be lost",
                self.name, header.through
            )));
        }
        Python::attach(|py| {
            let bound = self.object.bind(py);
            if !bound.hasattr(pyo3::intern!(py, "restore")).unwrap_or(false) {
                return Err(MorunaError::Resume(format!(
                    "{} returned checkpoint state but defines no restore(state)",
                    self.name
                )));
            }
            bound
                .call_method1(pyo3::intern!(py, "restore"), (PyBytes::new(py, user),))
                .map(|_| ())
                .map_err(|e| MorunaError::Resume(format!("{}.restore(): {e}", self.name)))
        })?;
        inner.watermark_next = committed_seq.map_or(0, |w| w + 1);
        inner.done.clear();
        inner.stats = PySinkStats {
            writes: header.writes,
            rows: header.rows,
            bytes: header.bytes,
            replayed: 0,
        };
        inner.holds = Some(header);
        inner.resumable = Some(true);
        inner.phase = Phase::Open;
        Ok(())
    }
}

/// `MPYS`, the version, the header's length (both little-endian u32), the header as JSON, then
/// the user's bytes as they were returned (e.6).
fn encode(header: &Header, state: &[u8]) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(header)
        .map_err(|e| MorunaError::Sink(format!("sink checkpoint could not be serialised: {e}")))?;
    let len = u32::try_from(json.len())
        .map_err(|_| MorunaError::Sink("sink checkpoint header is too large".into()))?;
    let mut out = Vec::with_capacity(12 + json.len() + state.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(state);
    Ok(out)
}

/// The inverse of [`encode`]; anything else is `Resume` naming what is wrong (f.8).
fn decode(state: &[u8]) -> Result<(Header, &[u8])> {
    let bad = |what: &str| MorunaError::Resume(format!("python sink checkpoint: {what}"));
    if state.len() < 12 || &state[..4] != MAGIC {
        return Err(bad("not a python sink checkpoint"));
    }
    let word =
        |at: usize| u32::from_le_bytes([state[at], state[at + 1], state[at + 2], state[at + 3]]);
    let version = word(4);
    if version != VERSION {
        return Err(bad(&format!("version {version} is not {VERSION}")));
    }
    let len = word(8) as usize;
    let Some(json) = state.get(12..12 + len) else {
        return Err(bad("the header is cut short"));
    };
    let header: Header = serde_json::from_slice(json)
        .map_err(|e| bad(&format!("the header is not readable: {e}")))?;
    Ok((header, &state[12 + len..]))
}

/// The Python class name of a value, for messages.
fn class_name(value: &Bound<'_, PyAny>) -> String {
    value
        .get_type()
        .name()
        .map_or_else(|_| "object".to_string(), |n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checkpoint_round_trips_and_refuses_what_is_not_one() {
        let header = Header {
            version: VERSION,
            through: Some(4),
            above: [6, 9].into_iter().collect(),
            rows: 12,
            bytes: 96,
            writes: 7,
        };
        let bytes = encode(&header, b"user state").expect("encodes");
        let (back, user) = decode(&bytes).expect("decodes");
        assert_eq!(back.through, Some(4));
        assert_eq!(back.above, header.above);
        assert_eq!((back.rows, back.bytes, back.writes), (12, 96, 7));
        assert_eq!(user, b"user state");

        assert!(decode(b"short").is_err());
        assert!(decode(b"MPYQ\x01\0\0\0\0\0\0\0").is_err());
        let mut wrong_version = bytes.clone();
        wrong_version[4] = 9;
        assert!(
            decode(&wrong_version)
                .unwrap_err()
                .to_string()
                .contains("version")
        );
        let mut cut = bytes[..14].to_vec();
        cut[8] = 200;
        assert!(decode(&cut).unwrap_err().to_string().contains("cut short"));
        let mut garbled = bytes.clone();
        garbled[12] = b'!';
        assert!(
            decode(&garbled)
                .unwrap_err()
                .to_string()
                .contains("not readable")
        );
    }
}
