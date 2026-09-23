//! The sink contract (contracts d.8).

use crate::BoxFuture;
use crate::error::MorunaError;
use crate::ids::Seq;
use crate::payload::{Payload, PayloadSpec, SourceSchema};

/// What a sink wrote, for the run report.
#[derive(Clone, Debug, Default)]
pub struct SinkSummary {
    /// Rows written.
    pub rows: u64,
    /// Bytes written.
    pub bytes: u64,
    /// The files or objects produced.
    pub files: Vec<String>,
}

/// Where a run's output goes (component 8).
pub trait Sink: Send + Sync {
    /// Once, before the first write.
    fn open(&mut self, schema: &SourceSchema) -> crate::Result<()>;
    /// What this sink wants delivered.
    fn accepts(&self) -> PayloadSpec;
    /// True when the sink needs morsels in sequence order (the scheduler then reorders).
    fn requires_order(&self) -> bool {
        false
    }
    /// Takes ownership; runs on the reactor; completes when the bytes are handed
    /// to the store's client (not necessarily committed; see `committed_seq`).
    /// `seq` is the morsel's sequence number, which the sink records with the
    /// output it lands in so a resume can identify uncommitted output.
    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, crate::Result<()>>;
    /// Exactly once, after the last `write` completed.
    fn finish(&mut self) -> crate::Result<SinkSummary>;

    // Resume support. A sink that leaves the defaults in place is not resumable:
    // `Scheduler::apply_resume_point` calls `Sink::resume` first, and its `Resume` names the sink.

    /// Highest `seq` such that every morsel with a sequence number at or below
    /// it is committed (visible to a reader and safe against process loss) or was
    /// declared skipped through `skip`. `None` means nothing is committed yet, or
    /// the sink does not track commits.
    fn committed_seq(&self) -> Option<Seq> {
        None
    }
    /// The scheduler will never write `seq` (error policy `skip`); the sink counts
    /// it as committed for the watermark. Default: nothing, which is correct for
    /// a sink that does not track commits.
    fn skip(&self, seq: Seq) {
        let _ = seq;
    }
    /// Opaque sink state for the run manifest (for a file sink: committed file
    /// names and the next file index). Called at every manifest write, and once at
    /// startup: a resumable sink returns `Some` even before its first write (an
    /// empty file list), so `checkpoint()? == None` at startup is how the scheduler
    /// detects a non-resumable sink (SC f.11).
    fn checkpoint(&self) -> crate::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Called instead of `open` on resume. The sink must discard any output that
    /// holds a sequence number above `committed_seq` (an uncommitted file, a
    /// multipart upload) and continue numbering after the checkpointed state.
    fn resume(
        &mut self,
        schema: &SourceSchema,
        state: &[u8],
        committed_seq: Option<Seq>,
    ) -> crate::Result<()> {
        let _ = (schema, state, committed_seq);
        Err(MorunaError::Resume("sink does not support resume".into()))
    }
}
