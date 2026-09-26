//! A window onto a run in progress, for the host protocol's heartbeat and `checkpoint`
//! (MH 4.3).
//!
//! The facade attaches the components it built once they exist and detaches them when the run
//! ends; between the two, [`RunObserver::progress`] reads the same counters the controller
//! reads, and [`RunObserver::request_checkpoint`] asks the scheduler's checkpoint thread to
//! write the manifest now. Every handle is weak, so an observer outliving its run keeps nothing
//! of it alive: the arena in particular goes when the run's last strong reference goes (12 f.1).
//!
//! Nothing here can enlarge a run. An observer reads counters and can ask for a manifest; the
//! knobs are not reachable through it (MH 4.3: a hostile peer can cancel, not enlarge).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use moruna_controller::Controller;
use moruna_kernel::{Limits, Outcome, Placement, Sampler, StageId, TraceRecord};
use moruna_scheduler::Scheduler;

/// The run's state as a heartbeat reports it (MH 4.3). Every field is what is known now; a run
/// still starting has no limits and no counters yet.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// The limits the run was sized against, once discovery has run.
    pub limits: Option<Limits>,
    /// The sink's commit watermark.
    pub committed_seq: Option<u64>,
    /// Rows the last kernel stage produced so far; zero for a chain with no kernels.
    pub rows_out: u64,
    /// Bytes the last kernel stage produced so far.
    pub bytes_out: u64,
    /// Workers allowed to take tasks.
    pub active_workers: u16,
    /// The memory ceiling in force.
    pub ceiling_bytes: Option<u64>,
    /// The process's anonymous memory now.
    pub anon_bytes: Option<u64>,
    /// The controller's latest classification.
    pub bottleneck: Option<String>,
    /// Bytes on the disk tier across every queue.
    pub staging_bytes: u64,
}

struct Live {
    scheduler: Weak<Scheduler>,
    controller: Weak<Controller>,
    placement: Weak<dyn Placement>,
    sampler: Weak<dyn Sampler>,
}

/// Shared between the facade and whoever watches the run (MH 4.3).
pub struct RunObserver {
    live: Mutex<Option<Live>>,
    limits: Mutex<Option<Limits>>,
    last_stage: AtomicU64,
    rows_out: AtomicU64,
    bytes_out: AtomicU64,
}

impl RunObserver {
    /// An observer with nothing attached.
    pub fn new() -> Arc<RunObserver> {
        Arc::new(RunObserver {
            live: Mutex::new(None),
            limits: Mutex::new(None),
            last_stage: AtomicU64::new(0),
            rows_out: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
        })
    }

    /// The facade: discovery has run.
    pub(crate) fn discovered(&self, limits: &Limits, stages: usize) {
        *self.limits.lock().unwrap_or_else(|e| e.into_inner()) = Some(limits.clone());
        self.last_stage.store(stages as u64, Ordering::SeqCst);
    }

    /// The facade: the components exist.
    pub(crate) fn attach(
        &self,
        scheduler: &Arc<Scheduler>,
        controller: &Arc<Controller>,
        placement: &Arc<dyn Placement>,
        sampler: &Arc<dyn Sampler>,
    ) {
        *self.live.lock().unwrap_or_else(|e| e.into_inner()) = Some(Live {
            scheduler: Arc::downgrade(scheduler),
            controller: Arc::downgrade(controller),
            placement: Arc::downgrade(placement),
            sampler: Arc::downgrade(sampler),
        });
    }

    /// The facade: the run is over.
    pub(crate) fn detach(&self) {
        *self.live.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The facade's record hook: count what the last kernel stage produced.
    pub(crate) fn on_record(&self, record: &TraceRecord) {
        let last = self.last_stage.load(Ordering::SeqCst);
        if last > 0
            && u64::from(record.stage as StageId) == last
            && matches!(record.outcome, Outcome::Ok | Outcome::Probe)
        {
            self.rows_out.fetch_add(record.rows_out, Ordering::SeqCst);
            self.bytes_out.fetch_add(record.bytes_out, Ordering::SeqCst);
        }
    }

    /// What the run looks like now.
    pub fn progress(&self) -> Progress {
        let limits = self
            .limits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut progress = Progress {
            ceiling_bytes: limits.as_ref().map(|l| l.memory_ceiling),
            limits,
            rows_out: self.rows_out.load(Ordering::SeqCst),
            bytes_out: self.bytes_out.load(Ordering::SeqCst),
            ..Progress::default()
        };
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let Some(live) = live.as_ref() else {
            return progress;
        };
        if let Some(scheduler) = live.scheduler.upgrade() {
            let stats = moruna_kernel::StatsSource::scheduler_stats(scheduler.as_ref());
            progress.committed_seq = stats.committed_seq;
            progress.active_workers = stats.workers_active;
        }
        if let Some(controller) = live.controller.upgrade() {
            progress.bottleneck = controller
                .summary()
                .timeline
                .last()
                .map(|(_, b)| crate::report::bottleneck_name(*b));
        }
        if let Some(placement) = live.placement.upgrade() {
            let disk = moruna_kernel::TierKind::Disk.index();
            progress.staging_bytes = placement
                .stats()
                .queues
                .iter()
                .map(|q| q.bytes_by_tier.get(disk).copied().unwrap_or(0))
                .sum();
        }
        if let Some(sampler) = live.sampler.upgrade() {
            progress.anon_bytes = Some(sampler.sample().anon_bytes);
        }
        progress
    }

    /// Ask for a manifest now (MH 4.3 `checkpoint`). False when there is no run to checkpoint
    /// or the run is not checkpointing.
    pub fn request_checkpoint(&self) -> bool {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        match live.as_ref().and_then(|l| l.scheduler.upgrade()) {
            Some(scheduler) => scheduler.request_checkpoint(),
            None => false,
        }
    }
}
