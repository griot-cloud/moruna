//! The budget follows the machine (MH 4.4): the facade's limits watcher.
//!
//! One thread, started when the controller is running and stopped when the scheduler returns.
//! Every `controller.tick_ms` it polls discovery's `LimitsWatch`; when the limits moved it tells
//! the trace (the report's `limits_timeline`), the controller (the ceiling and the CPU limit, at
//! once rather than at its next tick) and the scheduler (the CPU limit), then re-runs the arena
//! arithmetic of 12 f.1 for the new ceiling, moves an engine source's operator memory to its new
//! share (MH 4.5), and grows or shrinks the arena by the difference.
//!
//! Nothing here decides anything the components do not already decide: the arena reserves what
//! it is told, the controller re-plans inside the limits it is given, and the scheduler parks what
//! the limit does not allow. The watcher is the one caller of `Arena::grow`, `Arena::shrink`,
//! `Controller::set_arena`, `Controller::set_engine`, `EngineMemory::set_limit` after the start,
//! and `Scheduler::set_cpu_limit`.
//!
//! Shrink is eventual: a region that is draining is unmapped when its last buffer comes
//! home, and the watcher does not grow while a drain is running (the pacing of MH 7.1), so the
//! process never holds a new region and the draining one at once. Each drain is recorded with its
//! duration for the report.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use moruna_arena::Arena;
use moruna_controller::Controller;
use moruna_discovery::{HUGE_PAGE_BYTES, LimitsWatch};
use moruna_kernel::{Tier, TraceSink};
use moruna_scheduler::Scheduler;
use moruna_trace::DrainSummary;

use crate::spec::EngineMemory;

/// The largest figures a run may follow the machine up to (MH 4.1 `budget.elastic`).
///
/// Both default to the run's starting figures, so a spec that says nothing never grows (MH H-Q4):
/// the arena stays one region and the worker pool stays the starting N, exactly as before. A
/// machine that shrinks is followed whatever these say. F8.1 maps the JSON spec's
/// `budget.elastic.memory_max_bytes` and `budget.elastic.cpu_max` onto these two fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ElasticBudget {
    /// The highest memory ceiling the run follows the machine to, in bytes. `None`: the starting
    /// ceiling.
    pub memory_max_bytes: Option<u64>,
    /// The most CPUs the run follows the machine to, which is also the worker thread pool the
    /// scheduler creates at start and parks. `None`: the starting N.
    pub cpu_max: Option<u16>,
}

/// What a ceiling is divided into: an engine source's plan memory and an engine sink's write
/// memory (each zero without one), and the arena, sized over what the engines leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shares {
    pub(crate) engine: u64,
    pub(crate) sink: u64,
    pub(crate) arena: u64,
}

/// The arena arithmetic of 12 f.1, closed over everything but the ceiling, so the watcher can
/// re-run it for each new ceiling.
pub(crate) type Sizing = Box<dyn Fn(u64) -> Shares + Send>;

/// What one watcher needs.
pub(crate) struct WatchCtx {
    pub(crate) watch: Arc<LimitsWatch>,
    pub(crate) trace: Arc<dyn TraceSink>,
    pub(crate) arena: Option<Arc<Arena>>,
    pub(crate) controller: Arc<Controller>,
    pub(crate) scheduler: Arc<Scheduler>,
    /// The source's engine memory, moved to its share of each new ceiling.
    pub(crate) engine: Option<Arc<dyn EngineMemory>>,
    /// The sink's engine memory, likewise.
    pub(crate) sink: Option<Arc<dyn EngineMemory>>,
    pub(crate) sizing: Sizing,
    pub(crate) start_ns: u64,
    pub(crate) workers_max: u16,
    pub(crate) tick: Duration,
}

/// The watcher's running state.
pub(crate) struct WatchState {
    /// The host budget and the draining bytes last handed to the controller.
    budget: (u64, u64),
    /// The engine shares last set.
    shares: Shares,
    /// Every shrink, and the drain in progress when there is one.
    drains: Vec<DrainSummary>,
    /// When the drain in progress started, nanoseconds.
    draining_since: Option<u64>,
    /// Notes for the report, each once.
    notes: Vec<String>,
}

impl WatchState {
    pub(crate) fn new(budget: u64, shares: Shares) -> WatchState {
        WatchState {
            budget: (budget, 0),
            shares,
            drains: Vec::new(),
            draining_since: None,
            notes: Vec::new(),
        }
    }

    fn note(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }
}

/// What the watcher hands the report when it stops.
pub(crate) struct WatchOutcome {
    pub(crate) drains: Vec<DrainSummary>,
    pub(crate) notes: Vec<String>,
}

/// The running watcher thread; stops and joins when dropped, so an error anywhere in the
/// lifecycle does not leave it running (PY-I1).
pub(crate) struct Watcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<WatchOutcome>>,
}

impl Watcher {
    /// Start polling every `ctx.tick`.
    pub(crate) fn start(ctx: WatchCtx, state: WatchState) -> Watcher {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("moruna-limits".to_string())
            .spawn(move || {
                let mut state = state;
                let slice = Duration::from_millis(5).min(ctx.tick);
                while !flag.load(Ordering::SeqCst) {
                    let mut waited = Duration::ZERO;
                    while waited < ctx.tick && !flag.load(Ordering::SeqCst) {
                        std::thread::sleep(slice);
                        waited += slice;
                    }
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    step(&ctx, &mut state);
                }
                finish(&ctx, state)
            })
            .ok();
        Watcher { stop, handle }
    }

    /// Stop, join, and return what the report needs.
    pub(crate) fn stop(mut self) -> WatchOutcome {
        self.stop_inner().unwrap_or(WatchOutcome {
            drains: Vec::new(),
            notes: vec!["the limits watcher could not be joined".to_string()],
        })
    }

    fn stop_inner(&mut self) -> Option<WatchOutcome> {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().and_then(|handle| handle.join().ok())
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}

/// The watcher's last word: its notes, the discovery watcher's own, and the drain still running
/// (if any) left open.
fn finish(ctx: &WatchCtx, mut state: WatchState) -> WatchOutcome {
    check_drain(ctx, &mut state);
    let mut notes = ctx.watch.notes();
    notes.extend(state.notes);
    WatchOutcome {
        drains: state.drains,
        notes,
    }
}

/// One poll. Public to the crate so a test drives it deterministically.
pub(crate) fn step(ctx: &WatchCtx, state: &mut WatchState) {
    if let Some(change) = ctx.watch.poll() {
        let new = change.new.clone();
        ctx.trace.limits_changed(change);
        ctx.controller
            .follow_limits(new.memory_ceiling, new.cpu_quota);
        ctx.scheduler
            .set_cpu_limit(workers_for(new.cpu_quota, ctx.workers_max));
    }
    check_drain(ctx, state);
    resize_arena(ctx, state);
}

/// A CPU quota as a worker count: rounded up, at least one, at most the pool (preamble 5,
/// `workers.max`).
pub(crate) fn workers_for(cpu_quota: f64, workers_max: u16) -> u16 {
    let ceil = cpu_quota.ceil();
    let workers = if !ceil.is_finite() || ceil < 1.0 {
        1
    } else {
        ceil.min(1024.0) as u16
    };
    workers.clamp(1, workers_max.max(1))
}

/// Close the drain in progress once the arena says nothing is draining.
fn check_drain(ctx: &WatchCtx, state: &mut WatchState) {
    let (Some(arena), Some(since)) = (&ctx.arena, state.draining_since) else {
        return;
    };
    if arena.draining_bytes() > 0 {
        return;
    }
    let took_ms = crate::report::now_ns().saturating_sub(since) / 1_000_000;
    if let Some(open) = state.drains.iter_mut().rev().find(|d| d.drain_ms.is_none()) {
        open.drain_ms = Some(took_ms);
    }
    state.draining_since = None;
    tracing::info!(target: "runtime.drain", ms = took_ms, "arena drain complete");
}

/// Re-run the arena arithmetic for the ceiling in force and move the arena toward it:
/// shrink when it is a huge page or more too large, grow when it is a huge page or more too small
/// and no drain is running, and hand the controller the budget it may plan inside.
fn resize_arena(ctx: &WatchCtx, state: &mut WatchState) {
    let ceiling = ctx.watch.current().memory_ceiling;
    let shares = (ctx.sizing)(ceiling);
    if (shares.engine, shares.sink) != (state.shares.engine, state.shares.sink) {
        // Before the arena moves: a lowered share refuses the engines' growth at once, so their
        // operators spill rather than hold what the ceiling no longer allows.
        if let Some(engine) = &ctx.engine {
            engine.set_limit(shares.engine);
        }
        if let Some(sink) = &ctx.sink {
            sink.set_limit(shares.sink);
        }
        state.shares = shares;
        ctx.controller
            .set_engine(shares.engine.saturating_add(shares.sink));
    }
    let target = shares.arena;
    let Some(arena) = &ctx.arena else {
        // An injected allocator is not the facade's to resize; the controller still follows the
        // ceiling through its sampler.
        return;
    };
    let capacity = arena.host_capacity();
    if capacity >= target.saturating_add(HUGE_PAGE_BYTES) {
        let marked = arena.shrink(capacity - target);
        if marked > 0 {
            let now = crate::report::now_ns();
            state.drains.push(DrainSummary {
                at_ms: now.saturating_sub(ctx.start_ns) / 1_000_000,
                bytes: marked,
                drain_ms: None,
            });
            state.draining_since.get_or_insert(now);
            // A region that was already empty is unmapped at once.
            check_drain(ctx, state);
        }
        if arena.host_capacity() > target {
            state.note(format!(
                "the ceiling fell to {ceiling} bytes, which is below the arena's first region: \
                 that region is not given back, and the controller holds less of it instead"
            ));
        }
    } else if target >= capacity.saturating_add(HUGE_PAGE_BYTES) {
        if arena.draining_bytes() > 0 {
            // Pacing (MH 7.1): no new region while an old one is still resident.
            return;
        }
        match arena.grow(target - capacity) {
            Ok(_) => {}
            Err(error) => state.note(format!("the arena could not grow: {error}")),
        }
    }
    let told = (target.min(arena.host_capacity()), arena.draining_bytes());
    if told != state.budget {
        state.budget = told;
        ctx.controller.set_arena(told.0, told.1);
    }
    debug_assert!(arena.region_bytes(Tier::Host) >= arena.host_capacity());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worker count of a CPU quota: rounded up, one at least, the pool at most.
    #[test]
    fn a_quota_is_a_worker_count() {
        assert_eq!(workers_for(0.0, 8), 1);
        assert_eq!(workers_for(0.5, 8), 1);
        assert_eq!(workers_for(2.0, 8), 2);
        assert_eq!(workers_for(2.1, 8), 3);
        assert_eq!(workers_for(64.0, 8), 8);
        assert_eq!(workers_for(f64::NAN, 8), 1);
        assert_eq!(workers_for(4.0, 0), 1);
    }

    /// The default elastic budget is the starting figures, which is what H-Q4 asks for.
    #[test]
    fn the_default_elastic_budget_never_grows() {
        let elastic = ElasticBudget::default();
        assert_eq!(elastic.memory_max_bytes, None);
        assert_eq!(elastic.cpu_max, None);
    }
}
