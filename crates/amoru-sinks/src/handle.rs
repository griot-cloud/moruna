//! `SinkHandle`, the one type the scheduler drives (08 f.9, d.1).

use amoru_kernel::{BoxFuture, Payload, PayloadSpec, Result, Seq, Sink, SinkSummary, SourceSchema};

use crate::reorder::ReorderBuffer;

/// What the scheduler drives and the facade builds. One shape whether ordering is on or off,
/// so the scheduler's admission rule reads one method in both cases (f.9).
pub enum SinkHandle {
    /// The sink as built.
    Plain(Box<dyn Sink>),
    /// The sink behind a reorder buffer.
    Ordered(ReorderBuffer<dyn Sink>),
}

impl SinkHandle {
    /// `Ordered` when `ordered` (the user's `ordered=True`) or `sink.requires_order()`;
    /// `Plain` otherwise. The facade calls this once, after constructing the sink.
    pub fn wrap(sink: Box<dyn Sink>, ordered: bool, buffer_bytes: u64) -> SinkHandle {
        if ordered || sink.requires_order() {
            SinkHandle::Ordered(ReorderBuffer::new(sink, buffer_bytes))
        } else {
            SinkHandle::Plain(sink)
        }
    }

    /// `false` for `Plain`.
    pub fn is_stalled(&self) -> bool {
        match self {
            SinkHandle::Plain(_) => false,
            SinkHandle::Ordered(buffer) => buffer.is_stalled(),
        }
    }

    /// `None` for `Plain`.
    pub fn next_expected(&self) -> Option<Seq> {
        match self {
            SinkHandle::Plain(_) => None,
            SinkHandle::Ordered(buffer) => Some(buffer.next_expected()),
        }
    }

    /// True when the handle reorders.
    pub fn is_ordered(&self) -> bool {
        matches!(self, SinkHandle::Ordered(_))
    }
}

impl Sink for SinkHandle {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        match self {
            SinkHandle::Plain(sink) => sink.open(schema),
            SinkHandle::Ordered(buffer) => buffer.open(schema),
        }
    }

    fn accepts(&self) -> PayloadSpec {
        match self {
            SinkHandle::Plain(sink) => sink.accepts(),
            SinkHandle::Ordered(buffer) => buffer.accepts(),
        }
    }

    fn requires_order(&self) -> bool {
        match self {
            SinkHandle::Plain(sink) => sink.requires_order(),
            SinkHandle::Ordered(buffer) => buffer.requires_order(),
        }
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        match self {
            SinkHandle::Plain(sink) => sink.write(seq, payload),
            SinkHandle::Ordered(buffer) => buffer.write(seq, payload),
        }
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        match self {
            SinkHandle::Plain(sink) => sink.finish(),
            SinkHandle::Ordered(buffer) => buffer.finish(),
        }
    }

    fn committed_seq(&self) -> Option<Seq> {
        match self {
            SinkHandle::Plain(sink) => sink.committed_seq(),
            SinkHandle::Ordered(buffer) => buffer.committed_seq(),
        }
    }

    fn skip(&self, seq: Seq) {
        match self {
            SinkHandle::Plain(sink) => sink.skip(seq),
            SinkHandle::Ordered(buffer) => buffer.skip(seq),
        }
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        match self {
            SinkHandle::Plain(sink) => sink.checkpoint(),
            SinkHandle::Ordered(buffer) => buffer.checkpoint(),
        }
    }

    fn resume(
        &mut self,
        schema: &SourceSchema,
        state: &[u8],
        committed_seq: Option<Seq>,
    ) -> Result<()> {
        match self {
            SinkHandle::Plain(sink) => sink.resume(schema, state, committed_seq),
            SinkHandle::Ordered(buffer) => buffer.resume(schema, state, committed_seq),
        }
    }
}
