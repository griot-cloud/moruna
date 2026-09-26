//! The budget follows the machine, end to end (MH 4.4, H3, H4):
//! memory_follows_the_machine, cpus_follow_the_machine, an_inelastic_run_is_as_before,
//! and , the same two gates under a real cgroup v2 whose limits are rewritten mid-run
//! (reference host, E1).
//!
//! This host is macOS and has no cgroup to rewrite, so to drive the watcher from a
//! scripted machine (`ManualLimitsSource`) through exactly the code path the host's files take:
//! the same `LimitsWatch::poll`, the same derivation, the same publish, the same arena, controller
//! and scheduler calls. Only the reading differs. and are that reading, and are
//! tagged for the reference host (preamble 6.6).
//!
//! Everything else here is real: the arena, the reactor, the placement engine, the scheduler, the
//! controller, the trace and the report.

#![allow(clippy::result_large_err)]

mod support;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moruna_discovery::DiscoveryInput;
use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, LimitSource,
    NoState, Payload, PayloadSpec, Sampler, Sink, Source, SourceSchema, TierPref,
};
use moruna_runtime::{
    Components, ElasticBudget, LimitsReading, ManualLimitsSource, RunReport, RunSpec, Runtime,
};
use moruna_testkit::{FakeSink, FakeSource};
use support::{Scratch, one_run_at_a_time};

const MIB: u64 = 1024 * 1024;

/// A kernel that passes its input through after `spin`, counting how many of it run at once and
/// when each apply started, so a test can see the worker count rather than infer it. It sleeps
/// by default and spins (CPU-bound) when `busy`: a development host shared with other builds
/// starves the drive threads of a run whose workers all spin, which measures the host and not
/// the runtime, so only the reference-host gates spin.
struct Spinner {
    spin: Duration,
    busy: bool,
    applies: Arc<AtomicU64>,
    running: Arc<AtomicU64>,
    starts: Arc<Mutex<Vec<(Instant, u64)>>>,
}

impl Spinner {
    fn new(spin: Duration) -> Spinner {
        Spinner {
            spin,
            busy: false,
            applies: Arc::new(AtomicU64::new(0)),
            running: Arc::new(AtomicU64::new(0)),
            starts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The most applies that were running at once among those that started after `from`.
    /// The CPU-bound variant, for the reference host.
    fn busy(mut self) -> Spinner {
        self.busy = true;
        self
    }

    fn max_concurrent_after(&self, from: Instant) -> u64 {
        self.starts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(at, _)| *at >= from)
            .map(|(_, n)| *n)
            .max()
            .unwrap_or(0)
    }
}

impl Kernel for Spinner {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-runtime::tests::elastic::Spinner", b"v1")
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            // About as much outside the arena as in it: the arena takes two thirds of the
            // allowance (12 f.1), which leaves the controller real headroom above it at every
            // ceiling a test sets, and the arena's growth is still most of the process's.
            expected_amplification: Some(1.0),
            ..Default::default()
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.starts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((Instant::now(), now));
        if self.busy {
            let until = Instant::now() + self.spin;
            let mut spins = 0u64;
            while Instant::now() < until {
                spins = spins.wrapping_add(1);
                std::hint::spin_loop();
            }
            std::hint::black_box(spins);
        } else {
            std::thread::sleep(self.spin);
        }
        self.running.fetch_sub(1, Ordering::SeqCst);
        self.applies.fetch_add(1, Ordering::SeqCst);
        Ok(input)
    }
}

/// A machine a test scripts: discovery's findings with the limits replaced by the scripted
/// reading, so the watcher's first poll sees no change.
struct Machine {
    discovered: moruna_discovery::Discovered,
    source: ManualLimitsSource,
    baseline: u64,
}

/// `total_ram` such that e.3's `0.9 x MemTotal` is `ceiling` to within a byte.
fn ram_for(ceiling: u64) -> u64 {
    (ceiling as f64 / 0.9).ceil() as u64
}

fn reading(ceiling: u64, cpus: f64) -> LimitsReading {
    LimitsReading {
        total_ram: Some(ram_for(ceiling)),
        cpus_online: cpus,
        ..LimitsReading::default()
    }
}

impl Machine {
    /// A guest with no cgroup whose ceiling is the process's resident memory plus `room`, and
    /// `cpus` CPUs online.
    fn new(room: u64, cpus: f64) -> Machine {
        let mut discovered =
            moruna_discovery::discover(&DiscoveryInput::default()).expect("discovery");
        let sampler = moruna_discovery::Sampler::new(&discovered).expect("a sampler");
        let baseline = sampler.sample().anon_bytes;
        let ceiling = (ram_for(baseline + room) as f64 * 0.9) as u64;
        discovered.limits.memory_ceiling = ceiling;
        discovered.limits.memory_kill = None;
        discovered.limits.cpu_quota = cpus;
        discovered.limits.source = LimitSource::Os;
        discovered.limits.observed_at = 0;
        Machine {
            discovered,
            source: ManualLimitsSource::new(reading(ceiling, cpus)),
            baseline,
        }
    }

    fn ceiling(&self) -> u64 {
        self.discovered.limits.memory_ceiling
    }
}

fn spec(scratch: &Scratch, kernel: Arc<dyn Kernel>, splits: u32) -> RunSpec {
    let source: Arc<dyn Source> = Arc::new(FakeSource::new().splits(splits, 131_072, 131_072 * 8));
    let sink: Box<dyn Sink> = Box::new(FakeSink::new());
    let mut spec = RunSpec::new(source, vec![kernel], sink);
    spec.staging_dir = Some(scratch.path().join("staging"));
    spec.staging_limit = Some(256 * MIB);
    spec.profiles_dir = Some(scratch.path().join("profiles"));
    spec.checkpoint = false;
    std::fs::create_dir_all(scratch.path().join("staging")).expect("the staging directory");
    spec
}

/// Run `spec` on `machine` while `script` changes the machine from another thread. Returns the
/// report and the wall clock just before the run started, nanoseconds since the epoch, which is
/// at or before the run's own start (the trace's clock).
fn run_on(
    machine: &Machine,
    spec: RunSpec,
    script: impl FnOnce(&Script) + Send,
) -> (RunReport, u64) {
    let start_ns = epoch_ns();
    let components = Components {
        discovered: Some(machine.discovered.clone()),
        limits_source: Some(Arc::new(machine.source.clone())),
        ..Components::default()
    };
    let done = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let driver = {
            let script_ctx = Script {
                source: machine.source.clone(),
                done: Arc::clone(&done),
            };
            scope.spawn(move || script(&script_ctx))
        };
        let outcome = Runtime::run_with(spec, CancelToken::new(), components);
        done.store(true, Ordering::SeqCst);
        driver.join().expect("the script thread");
        match outcome {
            Ok(report) => (report, start_ns),
            Err(error) => panic!(
                "the run did not complete: {error}; notes: {:?}",
                error.shutdown_notes
            ),
        }
    })
}

/// What a script drives: the scripted machine, and whether the run has already ended.
struct Script {
    source: ManualLimitsSource,
    done: Arc<AtomicBool>,
}

impl Script {
    fn set(&self, reading: LimitsReading) {
        self.source.set(reading);
    }

    /// Wait for `condition`; false when the run ended first, so the script stops and the run's
    /// own outcome is what the test reports.
    fn wait(&self, what: &str, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !condition() {
            if self.done.load(Ordering::SeqCst) {
                return false;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// memory_follows_the_machine (H3). The machine's RAM is raised mid-run: the ceiling rises
/// within one tick, the arena grows, and the process holds more than the starting ceiling, which
/// the report measures against the ceiling in force. Then the RAM is lowered back: the ceiling
/// falls within one tick, the grown region drains and is unmapped, the drain's duration is in the
/// report, and once it is complete the process is back under the new ceiling.
#[test]
fn memory_follows_the_machine() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("rt_t9");
    let machine = Machine::new(256 * MIB, 2.0);
    let low = machine.ceiling();
    let high = (ram_for(machine.baseline + 768 * MIB) as f64 * 0.9) as u64;
    let kernel = Arc::new(Spinner::new(Duration::from_millis(15)));
    let applies = Arc::clone(&kernel.applies);
    let mut spec = spec(&scratch, kernel.clone(), 240);
    spec.elastic = ElasticBudget {
        memory_max_bytes: Some(high),
        cpu_max: None,
    };
    spec.trace_path = Some(scratch.path().join("trace.arrow"));

    let (report, start_ns) = run_on(&machine, spec, move |script| {
        if !script.wait("the run to start", || applies.load(Ordering::SeqCst) >= 20) {
            return;
        }
        script.set(reading(high, 2.0));
        let raised = applies.load(Ordering::SeqCst);
        if !script.wait("the run to work in the grown arena", || {
            applies.load(Ordering::SeqCst) >= raised + 80
        }) {
            return;
        }
        script.set(reading(low, 2.0));
    });

    assert_eq!(report.exit, moruna_runtime::ExitReason::Completed);
    assert_eq!(report.limits_initial.memory_ceiling, low);
    assert_eq!(
        report.limits_timeline.len(),
        2,
        "raised and lowered: {:?}",
        report.limits_timeline
    );
    assert_eq!(report.limits_timeline[0].1.memory_ceiling, high);
    assert_eq!(report.limits_timeline[1].1.memory_ceiling, low);
    assert!(
        report.peak_anon_bytes > low,
        "H3: the grown arena is resident, so the run held more than the starting ceiling \
         ({} against {low})",
        report.peak_anon_bytes
    );
    assert!(
        report.peak_fraction_of_ceiling <= 1.0,
        "and it stayed under the ceiling in force at its peak ({} of {})",
        report.peak_fraction_of_ceiling,
        report.peak_ceiling_bytes
    );
    assert_eq!(report.drains.len(), 1, "one shrink: {:?}", report.drains);
    let drain = &report.drains[0];
    assert!(drain.bytes > 0);
    let drain_ms = drain
        .drain_ms
        .expect("the drain completed before the run ended");

    // Once the drain was complete the process was back under the lowered ceiling: every record
    // that started after it. The test's start is at or before the run's, so a margin covers the
    // difference.
    let after_drain_ns = start_ns + (drain.at_ms + drain_ms + 50) * 1_000_000;
    let records = read_trace(&scratch.path().join("trace.arrow"));
    let late: Vec<_> = records
        .iter()
        .filter(|(start, _)| *start > after_drain_ns)
        .collect();
    assert!(!late.is_empty(), "the run went on after the drain");
    for (_, peak) in &late {
        assert!(
            *peak <= low,
            "H3: after the drain the process holds {peak} bytes against the lowered ceiling {low}"
        );
    }
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("memory ceiling moved")),
        "the controller says it followed: {:?}",
        report.notes
    );
}

/// cpus_follow_the_machine (H4). CPUs are added mid-run: more workers run at once than the
/// starting N, up to the new limit. Then CPUs are taken away: within a tick the workers above the
/// new limit are parked.
#[test]
fn cpus_follow_the_machine() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("rt_t10");
    let machine = Machine::new(512 * MIB, 2.0);
    let ceiling = machine.ceiling();
    let kernel = Arc::new(Spinner::new(Duration::from_millis(20)));
    let applies = Arc::clone(&kernel.applies);
    let watched = Arc::clone(&kernel);
    let mut spec = spec(&scratch, kernel.clone(), 600);
    spec.elastic = ElasticBudget {
        memory_max_bytes: None,
        cpu_max: Some(6),
    };

    let marks = Arc::new(Mutex::new((None::<Instant>, None::<Instant>)));
    let marked = Arc::clone(&marks);
    let (report, _) = run_on(&machine, spec, move |script| {
        if !script.wait("the run to start", || applies.load(Ordering::SeqCst) >= 20) {
            return;
        }
        script.set(reading(ceiling, 6.0));
        let raised_at = Instant::now();
        // Lower again once the raise has been seen to take effect (or the run ended, which the
        // assertions below then report), plus a few more applies at six.
        if !script.wait("more than two workers at six CPUs", || {
            watched.max_concurrent_after(raised_at) > 2
        }) {
            return;
        }
        let raised = applies.load(Ordering::SeqCst);
        if !script.wait("work at six CPUs", || {
            applies.load(Ordering::SeqCst) >= raised + 30
        }) {
            return;
        }
        script.set(reading(ceiling, 1.0));
        let lowered_at = Instant::now();
        *marked.lock().unwrap_or_else(|e| e.into_inner()) = (Some(raised_at), Some(lowered_at));
    });

    assert_eq!(report.exit, moruna_runtime::ExitReason::Completed);
    let (raised_at, lowered_at) = *marks.lock().unwrap_or_else(|e| e.into_inner());
    let (raised_at, lowered_at) = (raised_at.expect("raised"), lowered_at.expect("lowered"));
    let before = kernel
        .starts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(at, _)| *at < raised_at)
        .map(|(_, n)| *n)
        .max()
        .unwrap_or(0);
    assert!(
        before <= 2,
        "at the starting two CPUs, {before} ran at once"
    );
    let during = kernel
        .starts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(at, _)| *at >= raised_at && *at < lowered_at)
        .map(|(_, n)| *n)
        .max()
        .unwrap_or(0);
    assert!(
        during > 2 && during <= 6,
        "H4: with six CPUs, more than the starting two ran at once and no more than six: {during}"
    );
    // One tick to see the change and one pick for a worker to park, then at most one runs.
    let settled = lowered_at + Duration::from_millis(600);
    assert!(
        kernel.max_concurrent_after(settled) <= 1,
        "the workers above the lowered limit were parked"
    );
    assert_eq!(report.limits_timeline.len(), 2);
    assert!((report.limits_timeline[0].1.cpu_quota - 6.0).abs() < f64::EPSILON);
    assert!((report.limits_timeline[1].1.cpu_quota - 1.0).abs() < f64::EPSILON);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("6 worker threads created and 2 active")),
        "{:?}",
        report.notes
    );
}

/// an_inelastic_run_is_as_before (MH H-Q4). With no `elastic`, a machine that grows is not
/// followed up: no timeline entry, the arena stays one region, the pool stays the starting N. A
/// machine that shrinks under it is still followed down.
#[test]
fn an_inelastic_run_is_as_before() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("rt_t11");
    let machine = Machine::new(192 * MIB, 2.0);
    let start = machine.ceiling();
    let kernel = Arc::new(Spinner::new(Duration::from_millis(5)));
    let applies = Arc::clone(&kernel.applies);
    let spec = spec(&scratch, kernel.clone(), 200);

    let (report, _) = run_on(&machine, spec, move |script| {
        if !script.wait("the run to start", || applies.load(Ordering::SeqCst) >= 10) {
            return;
        }
        // A machine four times the size, with four times the CPUs: not the run's to take.
        script.set(reading(start * 4, 8.0));
        let at = applies.load(Ordering::SeqCst);
        script.wait("a few ticks at the bigger machine", || {
            applies.load(Ordering::SeqCst) >= at + 100
        });
    });
    assert_eq!(report.exit, moruna_runtime::ExitReason::Completed);
    assert!(
        report.limits_timeline.is_empty(),
        "an inelastic run does not follow the machine up: {:?}",
        report.limits_timeline
    );
    assert!(report.drains.is_empty());
    assert!(kernel.max_concurrent_after(Instant::now() - Duration::from_secs(3600)) <= 2);
    assert!(
        !report
            .notes
            .iter()
            .any(|n| n.contains("worker threads created")),
        "the pool is the starting N"
    );
    assert_eq!(report.peak_ceiling_bytes, start);
}

/// , the other half: an inelastic run is still followed down. The machine loses a CPU and a
/// third of its memory mid-run; the timeline has the change and the pool parks down to one.
#[test]
fn an_inelastic_run_is_followed_down() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("down");
    let machine = Machine::new(384 * MIB, 2.0);
    let start = machine.ceiling();
    // Five percent: inside the reserve the arena left, so the region `new` made still fits under
    // the lowered ceiling (an inelastic run is one region, and that region never drains).
    let lower = start - start / 20;
    let kernel = Arc::new(Spinner::new(Duration::from_millis(10)));
    let applies = Arc::clone(&kernel.applies);
    let spec = spec(&scratch, kernel.clone(), 140);
    let lowered_at = Arc::new(Mutex::new(None::<Instant>));
    let lowered = Arc::clone(&lowered_at);
    let (report, _) = run_on(&machine, spec, move |script| {
        if !script.wait("the run to start", || applies.load(Ordering::SeqCst) >= 20) {
            return;
        }
        script.set(reading(lower, 1.0));
        *lowered.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    });
    assert_eq!(report.exit, moruna_runtime::ExitReason::Completed);
    assert_eq!(
        report.limits_timeline.len(),
        1,
        "{:?}",
        report.limits_timeline
    );
    assert!(report.limits_timeline[0].1.memory_ceiling < start);
    let lowered_at = lowered_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("lowered");
    assert!(kernel.max_concurrent_after(lowered_at + Duration::from_millis(600)) <= 1);
}

/// The inelastic default with the host's own files: nothing is injected but the discovery
/// findings, the machine does not move, and the report has no timeline and no drain.
#[test]
fn the_host_source_on_a_still_machine() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("host");
    let kernel = Arc::new(Spinner::new(Duration::from_millis(1)));
    let mut spec = spec(&scratch, kernel, 40);
    spec.budget = Some(512 * MIB);
    spec.cpu = Some(2.0);
    let report = match Runtime::run(spec, CancelToken::new()) {
        Ok(report) => report,
        Err(error) => panic!("the run did not complete: {error}"),
    };
    assert!(report.limits_timeline.is_empty());
    assert!(report.drains.is_empty());
    assert_eq!(
        report.peak_ceiling_bytes,
        report.limits_initial.memory_ceiling
    );
}

/// memory_follows_a_cgroup (reference host, E1). H3 under a real cgroup v2 whose
/// `memory.max` is rewritten mid-run. `MORUNA_ELASTIC_CGROUP` names the cgroup directory this test
/// process runs in, writable by it (the container job delegates one).
#[test]
#[ignore = "(reference host, E1): needs a delegated cgroup v2 named by MORUNA_ELASTIC_CGROUP whose memory.max this process may rewrite"]
fn memory_follows_a_cgroup() {
    let _serial = one_run_at_a_time();
    let dir = std::path::PathBuf::from(
        std::env::var("MORUNA_ELASTIC_CGROUP").expect("MORUNA_ELASTIC_CGROUP names the cgroup"),
    );
    let scratch = Scratch::new("rt_t12");
    let resident = resident_now();
    let low = resident + 512 * MIB;
    let high = resident + 1536 * MIB;
    std::fs::write(dir.join("memory.max"), low.to_string()).expect("memory.max");
    let kernel = Arc::new(Spinner::new(Duration::from_millis(15)));
    let applies = Arc::clone(&kernel.applies);
    let mut spec = spec(&scratch, kernel, 600);
    spec.elastic = ElasticBudget {
        memory_max_bytes: Some(high),
        cpu_max: None,
    };
    let report = std::thread::scope(|scope| {
        let cgroup = dir.clone();
        scope.spawn(move || {
            wait_until("the run to start", Duration::from_secs(60), || {
                applies.load(Ordering::SeqCst) >= 20
            });
            std::fs::write(cgroup.join("memory.max"), high.to_string()).expect("raise");
            let at = applies.load(Ordering::SeqCst);
            wait_until("work in the grown arena", Duration::from_secs(60), || {
                applies.load(Ordering::SeqCst) >= at + 150
            });
            std::fs::write(cgroup.join("memory.max"), low.to_string()).expect("lower");
        });
        Runtime::run(spec, CancelToken::new())
    })
    .expect("the run completes under the rewritten cgroup");
    assert_eq!(report.limits_timeline.len(), 2);
    assert!(report.peak_anon_bytes > report.limits_initial.memory_ceiling);
    assert!(report.peak_fraction_of_ceiling <= 1.0);
    assert!(report.drains.iter().all(|d| d.drain_ms.is_some()));
}

/// cpus_follow_a_cgroup (reference host, E1). H4 under a real cgroup v2 whose `cpu.max` is
/// rewritten mid-run, as .
#[test]
#[ignore = "(reference host, E1): needs a delegated cgroup v2 named by MORUNA_ELASTIC_CGROUP whose cpu.max this process may rewrite"]
fn cpus_follow_a_cgroup() {
    let _serial = one_run_at_a_time();
    let dir = std::path::PathBuf::from(
        std::env::var("MORUNA_ELASTIC_CGROUP").expect("MORUNA_ELASTIC_CGROUP names the cgroup"),
    );
    let scratch = Scratch::new("rt_t13");
    std::fs::write(dir.join("cpu.max"), "200000 100000").expect("cpu.max");
    let kernel = Arc::new(Spinner::new(Duration::from_millis(20)).busy());
    let applies = Arc::clone(&kernel.applies);
    let mut spec = spec(&scratch, kernel.clone(), 600);
    spec.elastic = ElasticBudget {
        memory_max_bytes: None,
        cpu_max: Some(6),
    };
    let raised_at = Arc::new(Mutex::new(None::<Instant>));
    let raised = Arc::clone(&raised_at);
    let report = std::thread::scope(|scope| {
        let cgroup = dir.clone();
        scope.spawn(move || {
            wait_until("the run to start", Duration::from_secs(60), || {
                applies.load(Ordering::SeqCst) >= 20
            });
            std::fs::write(cgroup.join("cpu.max"), "600000 100000").expect("raise");
            *raised.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        });
        Runtime::run(spec, CancelToken::new())
    })
    .expect("the run completes under the rewritten cgroup");
    let raised_at = raised_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("raised");
    assert!(kernel.max_concurrent_after(raised_at) > 2);
    assert_eq!(report.limits_timeline.len(), 1);
}

/// The process's resident anonymous memory now, as discovery measures it.
fn resident_now() -> u64 {
    let discovered = moruna_discovery::discover(&DiscoveryInput::default()).expect("discovery");
    moruna_discovery::Sampler::new(&discovered)
        .expect("sampler")
        .sample()
        .anon_bytes
}

/// Nanoseconds since the epoch, the trace's clock.
fn epoch_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// `(t_start_ns, mem_anon_peak)` of every record in the trace file.
fn read_trace(path: &std::path::Path) -> Vec<(u64, u64)> {
    use arrow::array::{Array, UInt64Array};
    let file = std::fs::File::open(path).expect("the trace file");
    let reader = arrow::ipc::reader::FileReader::try_new(file, None).expect("a trace reader");
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.expect("a trace batch");
        let starts = batch
            .column_by_name("t_start_ns")
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>().cloned())
            .expect("t_start_ns");
        let peaks = batch
            .column_by_name("mem_anon_peak")
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>().cloned())
            .expect("mem_anon_peak");
        for row in 0..batch.num_rows() {
            if !starts.is_null(row) {
                out.push((starts.value(row), peaks.value(row)));
            }
        }
    }
    out
}
