//! SC-T10: the probe protocol. One worker active, one read for stage 1 at the cursor with the
//! cursor advanced, no read for a later stage, a `ProbeResult` whose peak comes from the
//! sampler, the output pushed downstream and one peak reset per probe. f.9.
//!
//! `FakeKnobs` has no `probes()` observable and its `terminated()` returns `Option<String>`
//! rather than `Option<MorunaError>`, so this test asserts on the scheduler's own output rather
//! than on the fake, exactly as the controller agent's tests do. That is a gap in the fake,
//! reported rather than worked around in the testkit.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, NoState,
    Payload, PayloadKind, PayloadSpec, Prober, Result, Sample, SourceSchema, StatsSource, TierPref,
};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::{Latch, PickGate, RigBuilder, StatefulKernel, scripted_sampler, wait_for};

fn sample(anon: u64, peak: u64) -> Sample {
    Sample {
        anon_bytes: anon,
        peak_anon_bytes: peak,
        ..Sample::default()
    }
}

#[test]
fn sc_t10_probe_protocol() {
    let sampler = scripted_sampler(vec![
        sample(100, 100),
        sample(900, 900),
        sample(200, 200),
        sample(700, 700),
    ]);
    // The probe's own `apply` is held at a latch, so the count below is read at a moment that is
    // inside the probe by construction. Sampling the count in a loop and taking the minimum, as
    // this did, asserts that a polling thread was scheduled at least once inside a window it
    // does not control: under a loaded host it never runs there and the test reports the initial
    // `u16::MAX`, which says nothing about the scheduler (f.9).
    let gate = Latch::after(0);
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 4;
            cfg.workers_active = 4;
            cfg.initial_morsel_target = 16;
        })
        .source(FakeSource::new().splits(1, 64, 512))
        .sampler(sampler)
        .kernel(Arc::new(StatefulKernel::new(0).gated(gate.clone())))
        .kernel(Arc::new(FakeKernel::new()))
        .go();

    let first = moruna_kernel::StatsSource::scheduler_stats(&rig.scheduler).workers_active;
    assert_eq!(first, 4, "four workers are active before the probe");

    let (one, seen) = {
        let scheduler = &rig.scheduler;
        let gate = &gate;
        std::thread::scope(|scope| {
            let watcher = scope.spawn(move || {
                // The count the controller would read, read while the probe is inside `apply`.
                let held = gate.wait_holding(Duration::from_secs(30));
                let active = scheduler.scheduler_stats().workers_active;
                gate.open();
                (held, active)
            });
            let result = scheduler.probe(1, 16);
            let seen = match watcher.join() {
                Ok(seen) => seen,
                Err(_) => panic!("the watching thread panicked"),
            };
            (result, seen)
        })
    };
    let one = match one {
        Ok(result) => result,
        Err(e) => panic!("probe(1): {e}"),
    };
    assert!(
        seen.0,
        "the probe never reached the kernel, so the count was never read inside it"
    );
    assert_eq!(seen.1, 1, "the probe runs with exactly one worker active");
    assert_eq!(
        rig.source.reads().len(),
        1,
        "stage 1 costs exactly one read"
    );
    let reads = rig.source.reads();
    let range = match reads[0].1 {
        Some(range) => range,
        None => panic!("the probe read the whole split rather than a range at the cursor"),
    };
    assert_eq!(range.start, 0, "the probe reads at the cursor");
    assert_eq!(
        one.peak_delta, 800,
        "peak_delta is the scripted peak above the sample before"
    );
    assert!(one.rows_in > 0 && one.bytes_in > 0);

    // The probe's output went downstream as a normal morsel, so Q1 holds it.
    assert_eq!(
        rig.fake_placement.pushed(1).len(),
        1,
        "the probe output is in Q1"
    );

    let two = match rig.scheduler.probe(2, 16) {
        Ok(result) => result,
        Err(e) => panic!("probe(2): {e}"),
    };
    assert_eq!(rig.source.reads().len(), 1, "a later stage costs no read");
    assert_eq!(rig.fake_placement.popped(1).len(), 1, "it popped Q1's head");
    assert_eq!(
        rig.fake_placement.pushed(2).len(),
        1,
        "and pushed its output to Q2"
    );
    assert!(two.wall_ns > 0 || two.cpu_ns == two.cpu_ns);
    assert_eq!(rig.sampler.peak_resets(), 2, "one peak reset per probe");

    // h: a chain has no stage 0 and no stage beyond its last kernel.
    assert!(rig.scheduler.probe(0, 16).is_err());
    assert!(rig.scheduler.probe(3, 16).is_err());
}

/// A test-local stateless `Kernel` (k): the first apply waits for `release(0)` and the second for
/// `release(1)`; every later one returns at once. It keeps how many applies have started, how
/// many run now and the most that have run at once.
struct Ticketed {
    open: Mutex<[bool; 2]>,
    opened: Condvar,
    entered: AtomicUsize,
    running: AtomicUsize,
    peak: AtomicUsize,
}

impl Ticketed {
    fn new() -> Ticketed {
        Ticketed {
            open: Mutex::new([false; 2]),
            opened: Condvar::new(),
            entered: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    fn release(&self, ticket: usize) {
        self.open.lock().unwrap_or_else(|e| e.into_inner())[ticket] = true;
        self.opened.notify_all();
    }
}

impl Kernel for Ticketed {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-scheduler::tests::sc_t10::Ticketed", &[])
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
        let ticket = self.entered.fetch_add(1, Ordering::SeqCst);
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        if ticket < 2 {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            while !open[ticket] {
                open = self.opened.wait(open).unwrap_or_else(|e| e.into_inner());
            }
        }
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(input)
    }
}

/// f.9: a worker that passed `pick`'s gate check before a probe raised the gate does not run
/// beside the probe. Worker 1 is stopped between its pick and its claim; worker 0 runs the first
/// apply and is held in it while the probe raises the gate and waits for quiet; worker 0 is let
/// go, the probe's apply (the second) starts and is held, and only then is worker 1 let go. The
/// claim it makes is after the gate went up, so it is refused and the probe's apply runs alone.
#[test]
fn sc_f9_a_worker_past_its_pick_does_not_run_beside_the_probe() {
    let kernel = Arc::new(Ticketed::new());
    let gate = PickGate::new(1);
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
    let (placed, probing, beside, probed, joined) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        // Worker 0 in the first apply, worker 1 past its pick, and morsels waiting in Q0.
        let placed = wait_for(Duration::from_secs(5), || {
            kernel.entered.load(Ordering::SeqCst) == 1
                && gate.arrived()
                && rig.scheduler.shared().queue_count(0) > 1
        });
        let prober = scope.spawn(|| rig.scheduler.probe(1, 8));
        let raised = wait_for(Duration::from_secs(5), || rig.scheduler.shared().gated());
        kernel.release(0);
        // The probe saw quiet and its own apply is the second.
        let probing = raised
            && wait_for(Duration::from_secs(5), || {
                kernel.entered.load(Ordering::SeqCst) == 2
            });
        gate.release();
        // Every chance for worker 1 to start its task beside the probe's.
        std::thread::sleep(Duration::from_millis(200));
        let beside = kernel.peak.load(Ordering::SeqCst);
        kernel.release(1);
        let probed = prober.join();
        cancel.cancel();
        (placed, probing, beside, probed, handle.join())
    });
    assert!(placed, "the workers never reached their places");
    assert!(probing, "the probe's apply never started");
    assert_eq!(beside, 1, "{beside} applies at once while the probe ran");
    match probed {
        Ok(Ok(result)) => assert!(result.rows_in > 0, "the probe measured a morsel"),
        Ok(Err(e)) => panic!("probe during a run: {e}"),
        Err(_) => panic!("the probing thread panicked"),
    }
    assert!(joined.is_ok(), "the run thread panicked");
}
