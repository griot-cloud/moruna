//! `StatsSource`: the counters the controller classifies bottlenecks from (j, contracts d.11).

use std::sync::atomic::Ordering;

use moruna_kernel::{SchedulerStats, StageStats};

use crate::instances;
use crate::shared::Shared;

/// The policy's figures, with what learning did and cost.
fn policy_stats(shared: &Shared) -> moruna_kernel::PolicyStats {
    let mut stats = shared
        .decider
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .stats();
    if let Some(learning) = shared.learning.get() {
        stats.learning = Some(learning.stats());
        let (learn_ns, learned, store_ns, save_ns, drain_ns) = learning.costs();
        let o = &mut stats.overhead;
        o.learn_ns = learn_ns;
        o.learned = learned;
        o.store_ns = store_ns;
        o.save_ns = save_ns;
        o.drain_ns = drain_ns;
    }
    stats
}

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
    let (stages_waiting, calls_waiting) = shared.waiting();
    SchedulerStats {
        policy: policy_stats(shared),
        per_stage,
        workers_waiting: u16::try_from(calls_waiting).unwrap_or(u16::MAX),
        stages_waiting,
        // f.9 parks every worker but one for the length of a probe, so while a probe runs the
        // count the controller reads is the one worker that may take work.
        workers_active: if shared.probing.load(Ordering::SeqCst) {
            1
        } else {
            shared.knobs.active_workers()
        },
        workers_busy: shared.knobs.slots.busy(),
        reads_in_flight: shared.reads_in_flight.load(Ordering::SeqCst),
        writes_in_flight: shared.writes_in_flight.load(Ordering::SeqCst),
        sink_concurrency: shared.cfg.sink_concurrency,
        source_exhausted: shared.source_exhausted.load(Ordering::SeqCst),
        // Over every source: the morsels read, less one (a chain's last number).
        seq_issued: shared
            .sources
            .iter()
            .map(|end| {
                end.cursor
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .next_seq
            })
            .sum::<u64>()
            .saturating_sub(1),
        // The least of the sinks' watermarks: what every sink has committed.
        committed_seq: shared
            .sinks
            .iter()
            .map(|end| {
                end.committed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .reached
            })
            .min()
            .flatten(),
        checkpoints: shared.checkpoints.load(Ordering::SeqCst),
        last_checkpoint_us: shared.last_checkpoint_us.load(Ordering::SeqCst),
        resumed: shared.resumed.load(Ordering::SeqCst),
        recomputed: shared.recomputed.load(Ordering::SeqCst),
        knob_clamps: shared.knobs.clamps(),
    }
}
