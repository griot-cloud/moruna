//! The worker heartbeat and the checker that turns a dead worker into a diagnostic (f.14,
//! SC-I11). A worker that dies outside `apply` must end the run, never hang it (G-I8).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use moruna_kernel::{MorunaError, Seq, StageId};

use crate::cputime::now_ns;
use crate::shared::Shared;

/// One worker's row: when it last reported, whether it is parked, and the task it is on.
pub(crate) struct HeartbeatRow {
    pub(crate) last_ns: AtomicU64,
    pub(crate) parked: AtomicBool,
    pub(crate) running: AtomicBool,
    pub(crate) exited_clean: AtomicBool,
    pub(crate) task_stage: AtomicU64,
    pub(crate) task_seq: AtomicU64,
    pub(crate) task_start_ns: AtomicU64,
}

/// The table, read without a lock by the checker (g).
pub(crate) struct HeartbeatTable {
    pub(crate) rows: Vec<HeartbeatRow>,
    /// Set once, so one dead worker produces one diagnostic.
    reported: AtomicBool,
}

impl HeartbeatTable {
    pub(crate) fn new(workers: usize) -> HeartbeatTable {
        HeartbeatTable {
            rows: (0..workers)
                .map(|_| HeartbeatRow {
                    last_ns: AtomicU64::new(now_ns()),
                    parked: AtomicBool::new(true),
                    running: AtomicBool::new(false),
                    exited_clean: AtomicBool::new(false),
                    task_stage: AtomicU64::new(0),
                    task_seq: AtomicU64::new(0),
                    task_start_ns: AtomicU64::new(0),
                })
                .collect(),
            reported: AtomicBool::new(false),
        }
    }

    pub(crate) fn touch(&self, worker: u16) {
        if let Some(row) = self.rows.get(worker as usize) {
            row.last_ns.store(now_ns(), Ordering::SeqCst);
        }
    }

    pub(crate) fn parked(&self, worker: u16, parked: bool) {
        if let Some(row) = self.rows.get(worker as usize) {
            row.parked.store(parked, Ordering::SeqCst);
        }
    }

    pub(crate) fn start_task(&self, worker: u16, stage: StageId, seq: Seq) {
        if let Some(row) = self.rows.get(worker as usize) {
            row.task_stage.store(stage as u64, Ordering::SeqCst);
            row.task_seq.store(seq, Ordering::SeqCst);
            row.task_start_ns.store(now_ns(), Ordering::SeqCst);
            row.running.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn end_task(&self, worker: u16) {
        if let Some(row) = self.rows.get(worker as usize) {
            row.running.store(false, Ordering::SeqCst);
            row.last_ns.store(now_ns(), Ordering::SeqCst);
        }
    }

    pub(crate) fn exited_clean(&self, worker: u16) {
        if let Some(row) = self.rows.get(worker as usize) {
            row.exited_clean.store(true, Ordering::SeqCst);
        }
    }
}

/// The check f.14 requires at least once per `heartbeat_interval_ms`: it runs on the checkpoint
/// thread when checkpointing is on and on the sink drive's bounded park when it is off.
///
/// A worker is dead when its thread handle reports finished and it did not leave the loop
/// through the stopping flag. A worker inside `apply` for longer than the interval is not dead,
/// it is slow, and is reported at debug level.
pub(crate) fn check(shared: &Shared) {
    if !shared.run_state().is_live() {
        return;
    }
    let interval_ns = shared.cfg.heartbeat_interval_ms.saturating_mul(1_000_000);
    let now = now_ns();
    let mut dead: Option<(u16, StageId, Seq)> = None;
    {
        let handles = shared
            .worker_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (index, row) in shared.heartbeat.rows.iter().enumerate() {
            if row.running.load(Ordering::SeqCst) {
                let started = row.task_start_ns.load(Ordering::SeqCst);
                if now.saturating_sub(started) > interval_ns {
                    tracing::debug!(
                        target: "sched.slow_task",
                        worker = index,
                        stage = row.task_stage.load(Ordering::SeqCst),
                        seq = row.task_seq.load(Ordering::SeqCst),
                        "a task has run longer than one heartbeat interval"
                    );
                }
                continue;
            }
            if row.exited_clean.load(Ordering::SeqCst) {
                continue;
            }
            let finished = handles
                .get(index)
                .and_then(|slot| slot.as_ref())
                .is_some_and(|handle| handle.is_finished());
            if finished && dead.is_none() {
                dead = Some((
                    index as u16,
                    row.task_stage.load(Ordering::SeqCst) as StageId,
                    row.task_seq.load(Ordering::SeqCst),
                ));
            }
        }
    }
    let Some((worker, stage, seq)) = dead else {
        return;
    };
    if shared.heartbeat.reported.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::error!(target: "sched.worker_dead", worker, stage, seq, "a worker died outside apply");
    crate::policy::terminate(
        shared,
        MorunaError::Kernel {
            stage,
            seq,
            msg: format!("worker {worker} died outside apply"),
            features: None,
        },
    );
}
