//! The CPU limit bounds the active workers (MH 4.4): threads are created up to `workers_max` and parked, and the
//! workers that take tasks are `min(workers.active, cpu_limit)`. A lowered limit parks workers at
//! their next pick without a knob write; a raised one lets the knob take effect up to it.
//! SC-I7, G-I5.
//!
//! A lowered limit binds from the moment it is stored: a worker that passed its active-slot
//! check before the lowering takes its task only if a busy slot under the new limit is free when
//! it claims one, and a task still running from before the lowering holds one of those slots.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, Knob, Knobs,
    NoState, Payload, PayloadKind, PayloadSpec, Result, SourceSchema, StatsSource, TierPref,
};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::{RigBuilder, wait_for};

#[test]
fn cpu_limit_bounds_active() {
    let kernel = FakeKernel::new().latency(Duration::from_millis(20));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            // The elastic pool: sixteen threads, four of them the starting N.
            cfg.workers_max = 16;
            cfg.workers_active = 16;
            cfg.initial_morsel_target = 8;
            cfg.read_ahead = 32;
        })
        .source(FakeSource::new().splits(1, 4_000, 32_000))
        .kernel(Arc::new(kernel))
        .go();

    // Before any limit is set the bound is the pool, so the knob alone decides (a run whose
    // limits never move behaves as it did).
    assert_eq!(rig.scheduler.cpu_limit(), (16, 16));
    assert_eq!(rig.scheduler.set_cpu_limit(4), 4);
    assert_eq!(
        rig.scheduler.set_cpu_limit(0),
        1,
        "a run always has one worker"
    );
    assert_eq!(
        rig.scheduler.set_cpu_limit(999),
        16,
        "no thread beyond the pool"
    );
    rig.scheduler.set_cpu_limit(4);
    assert_eq!(
        rig.scheduler.snapshot().active_workers,
        4,
        "the knob takes effect up to the limit"
    );

    let cancel = CancelToken::new();
    let token = cancel.clone();
    let result = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        assert!(
            wait_for(Duration::from_secs(5), || {
                rig.scheduler.scheduler_stats().workers_busy >= 4
            }),
            "four workers never became busy"
        );
        for _ in 0..30 {
            let stats = rig.scheduler.scheduler_stats();
            assert!(
                stats.workers_busy <= 4,
                "{} busy under a limit of 4",
                stats.workers_busy
            );
            assert_eq!(stats.workers_active, 4);
            std::thread::sleep(Duration::from_millis(5));
        }

        // Lowered: the workers above the new limit park at their next pick, with no knob write.
        rig.scheduler.set_cpu_limit(2);
        assert!(
            wait_for(Duration::from_secs(5), || {
                rig.scheduler.scheduler_stats().workers_busy <= 2
            }),
            "lowering the limit did not park workers"
        );
        for _ in 0..30 {
            let busy = rig.scheduler.scheduler_stats().workers_busy;
            assert!(busy <= 2, "{busy} busy under a limit of 2");
            std::thread::sleep(Duration::from_millis(5));
        }

        // Raised: the knob (still 16) takes effect up to the new limit.
        rig.scheduler.set_cpu_limit(12);
        let raised = wait_for(Duration::from_secs(5), || {
            rig.scheduler.scheduler_stats().workers_busy > 4
        });
        // And a knob write below the limit is still the knob's (G-I5).
        rig.scheduler.set(Knob::ActiveWorkers(3));
        let knob_rules = wait_for(Duration::from_secs(5), || {
            rig.scheduler.scheduler_stats().workers_active == 3
        });
        cancel.cancel();
        (raised, knob_rules, handle.join())
    });
    assert!(
        result.0,
        "raising the limit did not put more workers to work"
    );
    assert!(result.1, "a knob below the limit is the knob");
    assert!(result.2.is_ok(), "the run thread panicked");
}

/// A test-local stateless `Kernel` (k): while `hold` is set every `apply` waits inside the
/// kernel, so the test decides when a task ends. It keeps how many applies run at once and the
/// most that have since `peak` was last set.
struct Held {
    hold: Mutex<bool>,
    released: Condvar,
    running: AtomicUsize,
    peak: AtomicUsize,
}

impl Held {
    fn new() -> Held {
        Held {
            hold: Mutex::new(true),
            released: Condvar::new(),
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    fn release(&self) {
        *self.hold.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.released.notify_all();
    }
}

impl Kernel for Held {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-scheduler::tests::sc_cpu_limit::Held", &[])
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        KernelHints::default()
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Either,
            tier: TierPref::Any,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let mut hold = self.hold.lock().unwrap_or_else(|e| e.into_inner());
        while *hold {
            hold = self.released.wait(hold).unwrap_or_else(|e| e.into_inner());
        }
        drop(hold);
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(input)
    }
}

/// Stops one worker, the first time it gets there, between its pick and its claim, until the
/// test lets it go.
struct PickGate {
    worker: u16,
    /// (arrived, released)
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl PickGate {
    fn new(worker: u16) -> Arc<PickGate> {
        Arc::new(PickGate {
            worker,
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        })
    }

    fn at_pick(&self, worker: u16) {
        if worker != self.worker {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 {
            return;
        }
        state.0 = true;
        while !state.1 {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn arrived(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    fn release(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.changed.notify_all();
    }
}

/// Two workers under a limit of two. Worker `at_pick` is stopped after its pick and before its
/// claim; the other worker, the only one that can be, is held inside `apply`; a morsel waits in
/// Q0. The limit is lowered to one and worker `at_pick` is let go. Returns the most applies the
/// kernel saw at once from the lowering on, which the limit says is one.
fn lowered_between_pick_and_claim(at_pick: u16) -> usize {
    let kernel = Arc::new(Held::new());
    let gate = PickGate::new(at_pick);
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 2;
            cfg.workers_active = 2;
            cfg.initial_morsel_target = 8;
            cfg.read_ahead = 8;
        })
        .source(FakeSource::new().splits(1, 4_000, 32_000))
        .kernel(Arc::clone(&kernel) as Arc<dyn Kernel>)
        .go();
    let hook = Arc::clone(&gate);
    rig.scheduler
        .test_pick_hook(Arc::new(move |worker| hook.at_pick(worker)));

    let cancel = CancelToken::new();
    let token = cancel.clone();
    let (reached, peak, joined) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        let reached = wait_for(Duration::from_secs(5), || {
            kernel.running.load(Ordering::SeqCst) == 1
                && gate.arrived()
                && rig.scheduler.shared().queue_count(0) > 0
        });
        rig.scheduler.set_cpu_limit(1);
        kernel.peak.store(1, Ordering::SeqCst);
        gate.release();
        // Every chance for the released worker to start a second apply beside the held one.
        std::thread::sleep(Duration::from_millis(200));
        let peak = kernel.peak.load(Ordering::SeqCst);
        kernel.release();
        cancel.cancel();
        (reached, peak, handle.join())
    });
    assert!(reached, "the workers never reached their places");
    assert!(joined.is_ok(), "the run thread panicked");
    peak
}

#[test]
fn a_lowered_limit_binds_a_worker_already_past_its_pick() {
    // Worker 1 passed its active-slot check under a limit of two. The limit is now one, so
    // worker 1 is outside it and must go back to park rather than run what it picked.
    let peak = lowered_between_pick_and_claim(1);
    assert_eq!(peak, 1, "{peak} applies at once under a limit of 1");
}

#[test]
fn a_task_running_at_the_lowering_holds_a_slot_under_the_new_limit() {
    // Worker 1 was running when the limit fell to one, and finishes its task (SC-I7). Worker 0
    // is inside the new limit, but it takes a task only once worker 1's slot is free: the
    // workers that take tasks are `min(workers.active, cpu_limit)` at any instant.
    let peak = lowered_between_pick_and_claim(0);
    assert_eq!(peak, 1, "{peak} applies at once under a limit of 1");
}
