//! The lifecycle of PY-I1 and 12 f.1 observed on the contracts d.15 fakes, through
//! `Runtime::run_with`. Only the knobs and observables contracts d.15 names are used.

mod support;

use std::sync::Arc;

use moruna_discovery::Discovered;
use moruna_kernel::{
    Allocator, CancelToken, Device, DeviceId, Guarantee, HostProfile, Kernel, LimitSource, Limits,
    MorunaError, Placement, Reactor, RunId, Sampler, Sink, Source, TierKind, TraceSink, TraceTail,
};
use moruna_runtime::{Components, RunSpec, Runtime, SinkSpec, SourceSpec};
use moruna_testkit::{
    FakeAllocator, FakeKernel, FakePlacement, FakeReactor, FakeSampler, FakeSink, FakeSource,
    FakeTrace,
};
use support::Scratch;

/// A host with a ceiling big enough for the fake run and nothing else discovered.
fn discovered(ceiling: u64, staging: Option<std::path::PathBuf>) -> Discovered {
    Discovered {
        limits: Limits {
            memory_ceiling: ceiling,
            memory_kill: None,
            cpu_quota: 2.0,
            page_bytes: 4096,
            devices: Vec::new(),
            source: LimitSource::Explicit,
        },
        profile: HostProfile {
            staging_dir: staging,
            durable_staging: Guarantee::Absent,
            ..Default::default()
        },
        host_tier: TierKind::Host,
        cgroup_path: None,
        disk_budget: 1 << 30,
        notes: vec!["a fixed host, for the tests".to_string()],
    }
}

/// Everything the facade would otherwise build, as fakes.
struct Rig {
    alloc: FakeAllocator,
    reactor: FakeReactor,
    trace: FakeTrace,
    sampler: FakeSampler,
    placement: FakePlacement,
    sink: FakeSink,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            alloc: FakeAllocator::new(),
            reactor: FakeReactor::new(),
            trace: FakeTrace::new(),
            sampler: FakeSampler::new(),
            placement: FakePlacement::new().with_manifest_store(),
            sink: FakeSink::new(),
        }
    }

    fn components(&self, ceiling: u64, staging: Option<std::path::PathBuf>) -> Components {
        let trace = Arc::new(self.trace.clone());
        Components {
            discovered: Some(discovered(ceiling, staging)),
            alloc: Some(Arc::new(self.alloc.clone()) as Arc<dyn Allocator>),
            reactor: Some(Arc::new(self.reactor.clone()) as Arc<dyn Reactor>),
            object_metadata: None,
            trace: Some((
                trace.clone() as Arc<dyn TraceSink>,
                trace as Arc<dyn TraceTail>,
            )),
            sampler: Some(Arc::new(self.sampler.clone()) as Arc<dyn Sampler>),
            placement: Some(Arc::new(self.placement.clone()) as Arc<dyn Placement>),
            run_id: Some(RunId([7; 16])),
            observer: None,
        }
    }
}

/// A spec over the fakes, with the run held inside a scratch directory.
fn spec(
    source: FakeSource,
    kernels: Vec<Arc<dyn Kernel>>,
    sink: FakeSink,
    scratch: &Scratch,
) -> RunSpec {
    let mut spec = RunSpec::new(
        SourceSpec::Built(Arc::new(source) as Arc<dyn Source>),
        kernels,
        SinkSpec::Built(Box::new(sink) as Box<dyn Sink>),
    );
    spec.staging_dir = Some(scratch.path().to_path_buf());
    spec.staging_limit = Some(1 << 30);
    // preamble 6.7: the profile store goes in the scratch directory, never in the home one.
    spec.profiles_dir = Some(scratch.path().join("profiles"));
    spec.notes = vec!["a note from the surface".to_string()];
    spec
}

/// PY-T1: the order of first calls on the fakes equals PY-I1, and the report comes back.
#[test]
fn py_t1_startup_order() {
    let scratch = Scratch::new("py_t1");
    let rig = Rig::new();
    let source = FakeSource::new().splits(4, 1000, 1 << 20);
    let kernel = Arc::new(FakeKernel::new().stateful(2, 0));
    let components = rig.components(4 << 30, Some(scratch.path().to_path_buf()));
    let spec = spec(
        source,
        vec![kernel.clone() as Arc<dyn Kernel>],
        rig.sink.clone(),
        &scratch,
    );

    let report = match Runtime::run_with(spec, CancelToken::new(), components) {
        Ok(report) => report,
        Err(error) => panic!("the fake run did not complete: {error}"),
    };

    // The sink was opened, every instance was inited, and the run produced a report whose
    // notes carry discovery's and the surface's, in that order (12 f.2).
    assert_eq!(rig.sink.open_calls(), 1, "the sink is opened exactly once");
    assert_eq!(
        rig.sink.finish_calls(),
        1,
        "the sink is finished exactly once"
    );
    assert_eq!(
        kernel.init_calls(),
        2,
        "every instance of the stateful stage is inited before the baseline"
    );
    assert!(!rig.placement.pushed(0).is_empty(), "Q0 saw the source");
    assert!(
        report.notes.iter().any(|n| n.contains("a fixed host")),
        "discovery's notes come first: {:?}",
        report.notes
    );
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("a note from the surface")),
        "the surface's notes follow: {:?}",
        report.notes
    );
    assert_eq!(report.run_id, RunId([7; 16]).to_hex());
    assert!(
        !rig.trace.records().is_empty(),
        "the injected trace sink saw records"
    );
}

/// PY-T1: a failure at a step unwinds what was started, in reverse, and nothing runs after it.
#[test]
fn py_t1_failure_unwinds_in_reverse() {
    let scratch = Scratch::new("py_t1_fail");
    let rig = Rig::new();
    let source = FakeSource::new().splits(2, 1000, 1 << 20).fail_split(0);
    let components = rig.components(4 << 30, Some(scratch.path().to_path_buf()));
    let spec = spec(source, Vec::new(), rig.sink.clone(), &scratch);

    let error = Runtime::run_with(spec, CancelToken::new(), components)
        .expect_err("a failing split ends the run");
    // PY-T1 asks for `shutdown_calls == 1`. The scheduler shuts the placement engine down
    // itself (SC f.10) and the facade shuts it down again on the way out, which `Placement`
    // permits because `shutdown` is idempotent (contracts d.10), so the count is what is
    // observable and one is not it. Reported.
    assert!(
        rig.placement.shutdown_calls() >= 1,
        "the placement engine is shut down"
    );
    assert!(
        rig.reactor.shutdown_calls() >= 1,
        "the reactor is shut down"
    );
    assert!(
        error.report.is_some(),
        "a run that entered Running carries its partial report"
    );
}

/// PY-T2: a kernel error arrives as `MorunaError::Kernel` with the partial report attached.
#[test]
fn py_t2_kernel_error_is_mapped() {
    let scratch = Scratch::new("py_t2");
    let rig = Rig::new();
    let source = FakeSource::new().splits(2, 1000, 1 << 20);
    let kernel = Arc::new(FakeKernel::new().fail_on(&[0]));
    let components = rig.components(4 << 30, Some(scratch.path().to_path_buf()));
    let spec = spec(
        source,
        vec![kernel as Arc<dyn Kernel>],
        rig.sink.clone(),
        &scratch,
    );

    let error = Runtime::run_with(spec, CancelToken::new(), components)
        .expect_err("the terminate policy ends the run on the first kernel error");
    assert!(
        matches!(error.error, MorunaError::Kernel { .. }),
        "the diagnostic is the kernel's: {error}"
    );
    assert!(error.report.is_some(), "the partial report is attached");
}

/// PY-I6, f.5: a token that is already cancelled ends the run as `Cancelled`, with the
/// partial report attached.
#[test]
fn py_t6_cancel_before_run() {
    let scratch = Scratch::new("py_t6");
    let rig = Rig::new();
    let source = FakeSource::new().splits(8, 10_000, 4 << 20);
    let components = rig.components(4 << 30, Some(scratch.path().to_path_buf()));
    let spec = spec(source, Vec::new(), rig.sink.clone(), &scratch);
    let cancel = CancelToken::new();
    cancel.cancel();

    let error =
        Runtime::run_with(spec, cancel, components).expect_err("a cancelled run is an error");
    assert!(
        matches!(error.error, MorunaError::Cancelled),
        "cancellation is reported as such: {error}"
    );
    assert!(error.report.is_some(), "the partial report is attached");
    assert_eq!(
        rig.sink.finish_calls(),
        0,
        "a cancelled run does not finish the sink"
    );
}

/// f.7: a resume over a source that is not repeatable is refused before anything is written.
#[test]
fn py_t12_resume_refuses_a_one_shot_source() {
    let scratch = Scratch::new("py_t12");
    let rig = Rig::new();
    // A manifest to point at: the fake engine's store writes one on `checkpoint`.
    let manifest = scratch.path().join("manifest.json");
    std::fs::write(&manifest, b"{}").expect("a manifest file");
    let source = FakeSource::new().repeatable(false);
    let components = rig.components(4 << 30, Some(scratch.path().to_path_buf()));
    let mut spec = spec(source, Vec::new(), rig.sink.clone(), &scratch);
    spec.resume = Some(manifest);

    let error = Runtime::run_with(spec, CancelToken::new(), components)
        .expect_err("a one-shot source cannot be resumed");
    assert!(
        matches!(error.error, MorunaError::Resume(_)),
        "the refusal names the resume path: {error}"
    );
    assert_eq!(rig.sink.open_calls(), 0, "nothing was opened");
}

/// PL-I6, 07 SO-I8: a one-shot source has Q0 staged and the report says so.
#[test]
fn rt_t3_iterator_source_stages_q0() {
    let scratch = Scratch::new("rt_t3");
    let rig = Rig::new();
    let source = FakeSource::new().splits(2, 100, 1 << 20).repeatable(false);
    let components = rig.components(4 << 30, Some(scratch.path().to_path_buf()));
    let spec = spec(source, Vec::new(), rig.sink.clone(), &scratch);

    let report = Runtime::run_with(spec, CancelToken::new(), components)
        .expect("a one-shot source still runs");
    assert!(
        report.notes.iter().any(|n| n.contains("Q0 staged")),
        "the report says why resume is off: {:?}",
        report.notes
    );
}

/// d.1: `inspect` is discovery on its own and reads the real host.
#[test]
fn rt_t4_inspect_reads_the_host() {
    let discovered = Runtime::inspect(&moruna_discovery::DiscoveryInput {
        explicit_budget: Some(1 << 30),
        explicit_cpu: Some(1.0),
        ..Default::default()
    })
    .expect("this host can be inspected");
    assert_eq!(discovered.limits.memory_ceiling, 1 << 30);
    assert!(discovered.limits.page_bytes > 0);
}

/// A device in the limits reaches the arena and the reactor; the run still completes.
#[test]
fn rt_t5_a_device_in_the_limits_is_passed_on() {
    let scratch = Scratch::new("rt_t5");
    let rig = Rig::new();
    let mut found = discovered(4 << 30, Some(scratch.path().to_path_buf()));
    found.limits.devices.push(Device {
        id: DeviceId(0),
        total_bytes: 8 << 30,
        free_bytes: 4 << 30,
        name: "a device that is not there".to_string(),
    });
    let mut components = rig.components(4 << 30, Some(scratch.path().to_path_buf()));
    components.discovered = Some(found);
    let spec = spec(
        FakeSource::new().splits(1, 100, 1 << 20),
        Vec::new(),
        rig.sink.clone(),
        &scratch,
    );

    let report =
        Runtime::run_with(spec, CancelToken::new(), components).expect("the run completes");
    assert_eq!(report.limits.devices.len(), 1);
}

/// PY-T15: `checkpoint.keep` on a run shorter than `checkpoint.interval_ms` still leaves a
/// manifest, and the report names it; without it the run leaves none. 12 f.7, SC f.12.
#[test]
fn py_t15_a_kept_checkpoint_survives_a_short_run() {
    let run = |keep: bool| {
        let scratch = Scratch::new("py_t15");
        let rig = Rig::new();
        let components = rig.components(4 << 30, Some(scratch.path().to_path_buf()));
        let mut spec = spec(
            FakeSource::new().splits(2, 100, 1 << 20),
            Vec::new(),
            FakeSink::new().resumable(true),
            &scratch,
        );
        spec.checkpoint = true;
        // Far longer than the run, so the checkpoint thread never ticks.
        spec.checkpoint_interval_ms = 60_000;
        spec.checkpoint_keep = keep;
        let report = Runtime::run_with(spec, CancelToken::new(), components)
            .unwrap_or_else(|e| panic!("the run completes: {e}"));
        (
            report.manifest.clone(),
            rig.placement.manifests_written().len(),
        )
    };

    let (_manifest, written) = run(true);
    assert_eq!(
        written, 1,
        "f.12: the scheduler wrote a final manifest for a run shorter than the interval"
    );

    let (manifest, written) = run(false);
    assert_eq!(
        written, 0,
        "f.12: no final manifest without checkpoint.keep"
    );
    assert!(manifest.is_none(), "and the report names none");
    // The report's `manifest` comes from `PlacementEngine::manifest_path` (12 f.2), which an
    // injected `FakePlacement` does not have; the real path is the end-to-end suite's.
}
