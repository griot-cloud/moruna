//! What the scheduler says about the run's tasks as each fact becomes true (MH 4.3 task
//! events): each source's first read, the first morsel each queue receives, each kernel stage's
//! first task, each queue's close, each sink's writing, commit and finish, and each kernel
//! stage's waits on its ports (MH 4.10). Queues, sources and sinks are named by their numbers
//! in the run's topology (d.16). The facade turns these into the host protocol's task lifecycle; nothing here knows a
//! task id.
//!
//! Every event is told once, by the thread on which the fact became true, and none is told by
//! parsing a record, a log or a report.

use std::sync::Arc;

use crate::ids::StageId;

/// What one end or stage of the chain did, measured where it happened. A kernel stage's
/// counters count every morsel it took, probes included: a probe's output is pushed downstream
/// as a normal morsel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StageCounters {
    /// Morsels taken (a kernel stage, the sink) or produced (the source).
    pub morsels: u64,
    /// Rows taken; zero for the source.
    pub rows_in: u64,
    /// Rows produced; for the sink, rows written.
    pub rows_out: u64,
    /// Bytes taken; zero for the source.
    pub bytes_in: u64,
    /// Bytes produced; for the sink, bytes written.
    pub bytes_out: u64,
    /// Morsels the error policy skipped.
    pub skipped: u64,
    /// Morsels whose kernel failed, skipped or not.
    pub errors: u64,
}

/// One fact about the run (MH 4.3 task events). In a linear chain queue `k` is stage `k`'s
/// output: queue 0 is the source's, queue `n` (the last) the sink's input (d.16).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageEvent {
    /// The scheduler entered `Running`: the drives drive and workers take tasks. Everything told
    /// before it happened while the controller calibrated the chain.
    Running,
    /// Source `source`'s drive issued its first read.
    SourceReading {
        /// The source.
        source: u16,
    },
    /// Queue `queue` received its first morsel: its consumer has input.
    Fed {
        /// The queue.
        queue: StageId,
    },
    /// A worker took the first morsel of kernel stage `stage` (1..=n).
    StageStarted {
        /// The kernel stage.
        stage: StageId,
    },
    /// Queue `queue` was closed: its producer has produced everything it will into it, and its
    /// consumer drains what is left.
    QueueClosed {
        /// The queue.
        queue: StageId,
        /// The producer's counters, final.
        counters: StageCounters,
    },
    /// Sink `sink` was handed its first morsel.
    SinkWriting {
        /// The sink.
        sink: u16,
    },
    /// Every morsel was written and sink `sink`'s `finish` began: the commit.
    SinkCommitting {
        /// The sink.
        sink: u16,
    },
    /// Sink `sink` finished: its output is committed.
    SinkFinished {
        /// The sink.
        sink: u16,
        /// What the sink wrote.
        counters: StageCounters,
    },
    /// Kernel stage `stage` called a port and waits for its answer (MH 4.10).
    PortWaiting {
        /// The kernel stage.
        stage: StageId,
        /// The name its entry lists the port under.
        port: String,
    },
    /// Kernel stage `stage` has no call waiting any more: it is working again.
    PortAnswered {
        /// The kernel stage.
        stage: StageId,
    },
}

/// The hook the facade installs to hear [`StageEvent`]s; called on the scheduler's own threads,
/// so it must not block.
pub type StageHook = Arc<dyn Fn(&StageEvent) + Send + Sync>;
