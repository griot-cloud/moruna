//! `StatsSource`: the counters the controller classifies bottlenecks from (j, contracts d.11).

use std::sync::atomic::Ordering;

use amoru_kernel::{SchedulerStats, StageStats};

use crate::instances;
use crate::shared::Shared;

/// Every counter as of now. Cheap: atomics and one pool lock per stateful stage.
pub(crate) fn scheduler_stats(shared: &Shared) -> SchedulerStats {
    let per_stage = shared
        .stages
        .iter()
        .enumerate()
        .map(|(index, entry)| StageStats {
            stage: entry.stage,
            tasks: entry.tasks.load(Ordering::SeqCst),
            busy_ns: entry.busy_ns.load(Ordering::SeqCst),
            errors: entry.errors.load(Ordering::SeqCst),
            skipped: entry.skipped.load(Ordering::SeqCst),
            instances_live: instances::live(shared, index),
        })
        .collect();
    SchedulerStats {
        per_stage,
        // f.9 parks every worker but one for the length of a probe, so while a probe runs the
        // count the controller reads is the one worker that may take work.
        workers_active: if shared.probing.load(Ordering::SeqCst) {
            1
        } else {
            shared.knobs.active_workers()
        },
        workers_busy: shared.workers_busy.load(Ordering::SeqCst),
        reads_in_flight: shared.reads_in_flight.load(Ordering::SeqCst),
        writes_in_flight: shared.writes_in_flight.load(Ordering::SeqCst),
        sink_concurrency: shared.cfg.sink_concurrency,
        source_exhausted: shared.source_exhausted.load(Ordering::SeqCst),
        seq_issued: shared
            .cursor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_seq
            .saturating_sub(1),
        committed_seq: shared
            .committed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reached,
        checkpoints: shared.checkpoints.load(Ordering::SeqCst),
        last_checkpoint_us: shared.last_checkpoint_us.load(Ordering::SeqCst),
        resumed: shared.resumed.load(Ordering::SeqCst),
        recomputed: shared.recomputed.load(Ordering::SeqCst),
        knob_clamps: shared.knobs.clamps(),
    }
}
