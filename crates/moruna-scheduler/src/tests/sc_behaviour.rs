//! The edge cases and failures of section h, the configuration checks of section i and the
//! contract of section d.1, each of which the SDD states and none of which section k names as a
//! test of its own.

use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::{
    CancelToken, ErrorPolicy, Knob, Knobs, MorunaError, PayloadKind, PayloadSpec, Prober,
    ResumePoint, SourceSchema, StatsSource, TierPref,
};
use moruna_sinks::SinkHandle;
use moruna_testkit::{FakeKernel, FakePlacement, FakeSink, FakeSource};

use super::common::{RigBuilder, StatefulKernel, cfg, manifest_lock};
use crate::{Pipeline, RunOutcome, Scheduler, SchedulerConfig};

/// h: zero splits. Q0 closes at once, the queues close in order and the sink finishes empty.
#[test]
fn sc_h_zero_splits() {
    let rig = RigBuilder::new()
        .source(FakeSource::new().splits(0, 0, 0))
        .stages(2)
        .go();
    let RunOutcome::Completed { sink } = (match rig.scheduler.run(CancelToken::new()) {
        Ok(outcome) => outcome,
        Err(e) => panic!("run: {e}"),
    }) else {
        panic!("a run with no splits still completes");
    };
    assert_eq!(sink.rows, 0);
    assert_eq!(rig.sink.finish_calls(), 1);
    assert!(rig.trace.records().is_empty(), "no morsel, no record");
}

/// h: zero kernels. There are no stages, no probes and no instance pools; the source drive
/// reads at the stage 0 target and the sink drive drains Q0.
#[test]
fn sc_h_zero_kernels() {
    let rig = RigBuilder::new()
        .cfg(|cfg| cfg.initial_morsel_target = 8)
        .source(FakeSource::new().splits(2, 6, 48))
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(RunOutcome::Completed { sink }) => assert_eq!(sink.rows, 12),
        other => panic!("expected Completed, got {other:?}"),
    }
    assert!(
        rig.trace.records().is_empty(),
        "a chain with no stage records nothing"
    );
    // RC f.3: the controller does not probe a chain with no kernel, and the scheduler refuses.
    match rig.scheduler.probe(1, 8) {
        Err(MorunaError::Plan(msg)) => assert!(msg.contains("kernel stages"), "{msg}"),
        other => panic!("expected a Plan error, got {other:?}"),
    }
}

/// h: `read_ahead = 0` keeps a floor of one read, issued only when Q0 is empty.
#[test]
fn sc_h_read_ahead_zero_keeps_a_floor_of_one() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.read_ahead = 0;
            cfg.initial_morsel_target = 8;
        })
        .source(FakeSource::new().splits(1, 12, 96))
        .stages(1)
        .go();
    match rig.scheduler.run(CancelToken::new()) {
        Ok(RunOutcome::Completed { sink }) => assert_eq!(sink.rows, 12),
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(
        rig.source.reads().len(),
        12,
        "one read at a time, all twelve rows"
    );
}

/// h, failures: a source read that fails ends the run naming the split; there is no skip for a
/// source error.
#[test]
fn sc_h_source_failure_terminates() {
    let rig = RigBuilder::new()
        .cfg(|cfg| cfg.error_policy = ErrorPolicy::Skip)
        .source(FakeSource::new().splits(2, 8, 64).fail_split(1))
        .stages(1)
        .go();
    let outcome = match rig.scheduler.run(CancelToken::new()) {
        Ok(outcome) => outcome,
        Err(e) => panic!("run: {e}"),
    };
    let RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("a source error ends the run, got {outcome:?}");
    };
    match diagnostic {
        MorunaError::Source { split, msg } => {
            assert_eq!(split, 1);
            assert!(
                msg.contains("rows"),
                "the diagnostic names the row range: {msg}"
            );
        }
        other => panic!("expected a Source diagnostic, got {other}"),
    }
}

/// h, failures: a sink write that fails ends the run and `finish` is not called.
#[test]
fn sc_h_sink_failure_terminates() {
    let rig = RigBuilder::new()
        .cfg(|cfg| cfg.initial_morsel_target = 8)
        .source(FakeSource::new().splits(1, 8, 64))
        .sink(FakeSink::new().fail_at(3))
        .stages(1)
        .go();
    let outcome = match rig.scheduler.run(CancelToken::new()) {
        Ok(outcome) => outcome,
        Err(e) => panic!("run: {e}"),
    };
    let RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("a sink error ends the run, got {outcome:?}");
    };
    assert!(matches!(diagnostic, MorunaError::Sink(_)), "{diagnostic}");
    assert_eq!(
        rig.sink.finish_calls(),
        0,
        "a failed run does not finish the sink"
    );
}

/// f.8: `Knobs::terminate` takes the terminate path from the controller's thread, whatever the
/// error policy says.
#[test]
fn sc_h_controller_terminate() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.error_policy = ErrorPolicy::Skip;
            cfg.initial_morsel_target = 8;
        })
        .source(FakeSource::new().splits(4, 400, 3_200))
        .kernel(Arc::new(
            FakeKernel::new().latency(Duration::from_millis(2)),
        ))
        .go();
    let outcome = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(CancelToken::new()));
        std::thread::sleep(Duration::from_millis(20));
        rig.scheduler
            .terminate(MorunaError::Staging("the controller said so".into()));
        match handle.join() {
            Ok(outcome) => outcome,
            Err(_) => panic!("the run thread panicked"),
        }
    });
    let Ok(RunOutcome::Terminated { diagnostic, .. }) = outcome else {
        panic!("expected Terminated, got {outcome:?}");
    };
    assert_eq!(diagnostic.to_string(), "staging: the controller said so");
}

/// i: the configuration rows this component owns are checked once, at `new`.
#[test]
fn sc_i_configuration_is_checked_at_new() {
    let bad = [
        (
            SchedulerConfig {
                workers_max: 0,
                ..cfg()
            },
            "workers.max",
        ),
        (
            SchedulerConfig {
                sink_concurrency: 0,
                ..cfg()
            },
            "sink.concurrency",
        ),
        (
            SchedulerConfig {
                morsel_min: 0,
                ..cfg()
            },
            "morsel.min_bytes",
        ),
        (
            SchedulerConfig {
                heartbeat_interval_ms: 0,
                ..cfg()
            },
            "heartbeat_interval_ms",
        ),
        (
            SchedulerConfig {
                checkpoint_interval_ms: 0,
                ..cfg()
            },
            "checkpoint.interval_ms",
        ),
    ];
    for (config, row) in bad {
        let outcome = RigBuilder::new()
            .cfg(|slot| *slot = config.clone())
            .build()
            .err();
        match outcome {
            Some(MorunaError::Config { name, .. }) => assert_eq!(name, row),
            other => panic!("expected a Config error for {row}, got {other:?}"),
        }
    }
    // The defaults of d.1 are within their own ranges.
    assert!(SchedulerConfig::default().validate().is_ok());
}

/// f.1: the chain is validated before anything is opened, and a schema that cannot cross is a
/// plan error naming the stage or the sink (CT-I5).
#[test]
fn sc_f1_chain_validation() {
    /// A kernel that wants a tensor, which a table of strings cannot become.
    struct TensorKernel;

    impl moruna_kernel::Kernel for TensorKernel {
        fn fingerprint(&self) -> moruna_kernel::Fingerprint {
            moruna_kernel::Fingerprint::compute("tests::TensorKernel", &[])
        }

        fn kind(&self) -> moruna_kernel::KernelKind {
            moruna_kernel::KernelKind::Stateless
        }

        fn accepts(&self) -> PayloadSpec {
            PayloadSpec {
                kind: PayloadKind::Tensor,
                tier: TierPref::Any,
            }
        }

        fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
            Ok(input.clone())
        }

        fn init(
            &self,
            _ctx: &moruna_kernel::InitCtx,
        ) -> moruna_kernel::Result<Box<dyn moruna_kernel::KernelState>> {
            Ok(Box::new(moruna_kernel::NoState))
        }

        fn apply(
            &self,
            _state: &mut dyn moruna_kernel::KernelState,
            input: moruna_kernel::Payload,
        ) -> moruna_kernel::Result<moruna_kernel::Payload> {
            Ok(input)
        }
    }

    let strings = SourceSchema::Table(Arc::new(moruna_kernel::arrow::datatypes::Schema::new(
        vec![moruna_kernel::arrow::datatypes::Field::new(
            "text",
            moruna_kernel::arrow::datatypes::DataType::Utf8,
            false,
        )],
    )));
    let outcome = RigBuilder::new()
        .source(FakeSource::new().schema(strings))
        .kernel(Arc::new(TensorKernel))
        .build();
    match outcome.err() {
        Some(MorunaError::Plan(msg)) => assert!(msg.starts_with("stage 1:"), "{msg}"),
        other => panic!("expected a Plan error naming the stage, got {other:?}"),
    }
    assert!(
        RigBuilder::new().stages(1).build().is_ok(),
        "a chain whose schemas cross is accepted"
    );
}

/// f.1: a source that cannot be re-read forces checkpointing off, named in the log.
#[test]
fn sc_f1_a_source_that_cannot_be_reread_forces_checkpointing_off() {
    let _guard = manifest_lock();
    let rig = RigBuilder::new()
        .cfg(|cfg| cfg.checkpoint_enabled = true)
        .source(FakeSource::new().repeatable(false))
        .sink(FakeSink::new().resumable(true))
        .placement(FakePlacement::new().with_manifest_store())
        .stages(1)
        .go();
    assert!(!rig.scheduler.checkpoint_enabled());
    match rig.scheduler.apply_resume_point(ResumePoint::default()) {
        Err(MorunaError::Resume(msg)) => assert!(msg.contains("source"), "{msg}"),
        other => panic!("expected Resume naming the source, got {other:?}"),
    }
}

/// d.1, e.2: `run` is valid once, from `Init`, and `run_resumed` only after
/// `apply_resume_point`.
#[test]
fn sc_e2_run_is_valid_once() {
    let rig = RigBuilder::new().stages(1).go();
    match rig.scheduler.run_resumed(CancelToken::new()) {
        Err(MorunaError::Resume(msg)) => assert!(msg.contains("apply_resume_point"), "{msg}"),
        other => panic!("expected a Resume refusal, got {other:?}"),
    }
    match rig.scheduler.run(CancelToken::new()) {
        Ok(RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    match rig.scheduler.run(CancelToken::new()) {
        Err(MorunaError::Plan(msg)) => assert!(msg.contains("only valid once"), "{msg}"),
        other => panic!("a second run is refused, got {other:?}"),
    }
    match rig.scheduler.apply_resume_point(ResumePoint::default()) {
        Err(MorunaError::Resume(msg)) => assert!(msg.contains("before the run starts"), "{msg}"),
        other => panic!("resume after a run is refused, got {other:?}"),
    }
}

/// f.4: a kernel error retires its instance, and the next acquire rebuilds it with `init`; that
/// is the only `init` after `init_instances`.
#[test]
fn sc_f4_a_failed_instance_is_rebuilt() {
    let kernel = Arc::new(FakeKernel::new().stateful(1, 0).fail_on(&[2]));
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.initial_morsel_target = 8;
            cfg.error_policy = ErrorPolicy::Skip;
        })
        .source(FakeSource::new().splits(1, 8, 64))
        .kernel(kernel.clone())
        .go();
    if let Err(e) = rig.scheduler.init_instances() {
        panic!("init_instances: {e}");
    }
    assert_eq!(kernel.init_calls(), 1, "one instance, built eagerly");
    match rig.scheduler.run(CancelToken::new()) {
        Ok(RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(
        kernel.init_calls(),
        2,
        "the retired instance was rebuilt once, on the next acquire"
    );
    let stats = rig.scheduler.scheduler_stats();
    assert_eq!(stats.per_stage[0].skipped, 1);
    assert_eq!(stats.per_stage[0].errors, 1);
    assert_eq!(stats.per_stage[0].instances_live, 1);
    assert!(stats.source_exhausted);
    assert_eq!(
        stats.sink_concurrency,
        rig.scheduler.snapshot().active_workers.max(2)
    );
}

/// j: the counters the controller reads, over a run that did nothing yet.
#[test]
fn sc_j_stats_before_a_run() {
    let rig = RigBuilder::new().stages(2).go();
    let stats = rig.scheduler.scheduler_stats();
    assert_eq!(stats.per_stage.len(), 2);
    assert_eq!(stats.workers_active, 2);
    assert_eq!(stats.workers_busy, 0);
    assert!(!stats.source_exhausted);
    assert_eq!(stats.committed_seq, None);
    assert_eq!(stats.checkpoints, 0);
    assert!(!stats.resumed);
    assert_eq!(stats.recomputed, 0);
    assert_eq!(rig.scheduler.kernels().len(), 2);
    assert_eq!(rig.scheduler.node(), moruna_kernel::LOCAL_NODE);
    // A knob for a stage the chain does not have is stored nowhere and breaks nothing.
    rig.scheduler.set(Knob::MorselTarget {
        stage: 9,
        bytes: 64,
    });
    assert_eq!(rig.scheduler.snapshot().morsel_target.len(), 3);
}

/// d.1: `Scheduler::new` refuses a chain whose sink cannot take the last stage's output, and
/// the `SinkHandle` the facade builds is the one type the scheduler drives (08 d.1).
#[test]
fn sc_d1_ordered_sink_handle() {
    let sink = FakeSink::new().requires_order(true);
    let handle = SinkHandle::wrap(Box::new(sink.clone()), false, 1 << 20);
    assert!(handle.is_ordered(), "a sink that requires order is wrapped");
    let scheduler = Scheduler::new(
        SchedulerConfig {
            initial_morsel_target: 8,
            morsel_min: 8,
            morsel_max: 1 << 20,
            ..cfg()
        },
        Pipeline {
            source: Arc::new(FakeSource::new().splits(1, 8, 64)),
            kernels: vec![Arc::new(FakeKernel::new())],
            sink: handle,
        },
        Arc::new(FakePlacement::new()),
        Arc::new(moruna_testkit::FakeAllocator::new()),
        Arc::new(moruna_testkit::FakeTrace::new()),
        Arc::new(moruna_testkit::FakeSampler::new()),
    );
    let scheduler = match scheduler {
        Ok(scheduler) => scheduler,
        Err(e) => panic!("new: {e}"),
    };
    match scheduler.run(CancelToken::new()) {
        Ok(RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let mut written = sink.written();
    written.sort_unstable();
    assert_eq!(written, (0..8).collect::<Vec<u64>>());
}

/// f.4, f.13: a `Forbid` kernel cannot be resumed, and a manifest that holds no state for an
/// instance of a `Checkpoint` stage is a resume error.
#[test]
fn sc_f13_resume_refusals() {
    let _guard = manifest_lock();
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.checkpoint_enabled = true;
            cfg.resuming = true;
        })
        .sink(FakeSink::new().resumable(true))
        .placement(FakePlacement::new().with_manifest_store())
        .kernel(Arc::new(StatefulKernel::new(1).checkpointing()))
        .go();
    match rig.scheduler.apply_resume_point(ResumePoint::default()) {
        Err(MorunaError::Resume(msg)) => assert!(msg.contains("no state"), "{msg}"),
        other => panic!("expected Resume naming the missing state, got {other:?}"),
    }
    assert_eq!(
        rig.sink.resume_calls(),
        1,
        "the sink was resumed before the pools"
    );
}

/// f.12: manifest writes are counted and logged, and three consecutive failures end the run
/// (placement h). Here the engine has no staging directory, so every write fails.
#[test]
fn sc_f12_three_failed_manifests_end_the_run() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.initial_morsel_target = 8;
            cfg.checkpoint_enabled = true;
            cfg.checkpoint_interval_ms = 10;
        })
        .source(FakeSource::new().splits(4, 400, 3_200))
        .sink(FakeSink::new().resumable(true))
        .kernel(Arc::new(
            FakeKernel::new().latency(Duration::from_millis(2)),
        ))
        .go();
    assert!(
        rig.scheduler.checkpoint_enabled(),
        "a resumable sink leaves checkpointing on"
    );
    let outcome = match rig.scheduler.run(CancelToken::new()) {
        Ok(outcome) => outcome,
        Err(e) => panic!("run: {e}"),
    };
    let RunOutcome::Terminated {
        diagnostic,
        manifest,
    } = outcome
    else {
        panic!("three failed manifest writes end the run, got {outcome:?}");
    };
    assert!(matches!(diagnostic, MorunaError::Resume(_)), "{diagnostic}");
    assert!(manifest.is_none(), "no manifest was ever written");
}

/// f.9: a probe asked for when the plan is exhausted has nothing to read, and says so rather
/// than waiting.
#[test]
fn sc_f9_probe_with_an_exhausted_plan() {
    let rig = RigBuilder::new()
        .source(FakeSource::new().splits(0, 0, 0))
        .stages(1)
        .go();
    match rig.scheduler.probe(1, 8) {
        Err(MorunaError::Plan(msg)) => assert!(msg.contains("exhausted"), "{msg}"),
        other => panic!("expected a Plan error, got {other:?}"),
    }
}

/// f.5, 08 f.4: an ordered sink that is holding bytes out of order stalls source admission
/// until the sequence it waits for arrives; the run still completes, in order.
#[test]
fn sc_f5_ordered_sink_completes_in_order() {
    let sink = FakeSink::new();
    let handle = SinkHandle::wrap(Box::new(sink.clone()), true, 1 << 20);
    let scheduler = Scheduler::new(
        SchedulerConfig {
            workers_max: 4,
            workers_active: 4,
            read_ahead: 4,
            initial_morsel_target: 8,
            morsel_min: 8,
            morsel_max: 1 << 20,
            ..cfg()
        },
        Pipeline {
            source: Arc::new(FakeSource::new().splits(2, 20, 160)),
            kernels: vec![Arc::new(FakeKernel::new()), Arc::new(FakeKernel::new())],
            sink: handle,
        },
        Arc::new(FakePlacement::new()),
        Arc::new(moruna_testkit::FakeAllocator::new()),
        Arc::new(moruna_testkit::FakeTrace::new().capacity(1 << 14)),
        Arc::new(moruna_testkit::FakeSampler::new()),
    );
    let scheduler = match scheduler {
        Ok(scheduler) => scheduler,
        Err(e) => panic!("new: {e}"),
    };
    match scheduler.run(CancelToken::new()) {
        Ok(RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(
        sink.written(),
        (0..40).collect::<Vec<u64>>(),
        "a reorder buffer delivers in sequence order"
    );
}

/// f.9: a probe asked for while the run is going holds every other worker off until it is done.
#[test]
fn sc_f9_probe_during_a_run() {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 4;
            cfg.workers_active = 4;
            cfg.initial_morsel_target = 8;
        })
        .source(FakeSource::new().splits(4, 200, 1_600))
        .kernel(Arc::new(
            FakeKernel::new().latency(Duration::from_millis(2)),
        ))
        .go();
    // Hold Q0 near empty so the drive is paced by the kernel and the plan is not exhausted
    // before the probe is asked for.
    rig.scheduler.set(Knob::HighWater {
        stage: 0,
        tier: moruna_kernel::TierKind::Host,
        bytes: 64,
    });
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let probed = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.scheduler.run(token));
        std::thread::sleep(Duration::from_millis(30));
        let result = rig.scheduler.probe(1, 8);
        cancel.cancel();
        let _ = handle.join();
        result
    });
    match probed {
        Ok(result) => assert!(result.rows_in > 0, "the probe measured a morsel"),
        Err(e) => panic!("probe during a run: {e}"),
    }
    let probes = rig
        .trace
        .records()
        .into_iter()
        .filter(|r| r.outcome == moruna_kernel::Outcome::Probe)
        .count();
    assert_eq!(probes, 1, "the probe left exactly one probe record");
}

/// f.8: a device out of memory from `apply` is the one error that reaches the record hook
/// before the policy, and the trace still sees exactly one record for that morsel and stage
/// (SC-I4, TR-I2).
#[test]
fn sc_f8_device_out_of_memory_reaches_the_hook_first() {
    /// A kernel that fails every apply with a device allocation error.
    struct DeviceOom;

    impl moruna_kernel::Kernel for DeviceOom {
        fn fingerprint(&self) -> moruna_kernel::Fingerprint {
            moruna_kernel::Fingerprint::compute("tests::DeviceOom", &[])
        }

        fn kind(&self) -> moruna_kernel::KernelKind {
            moruna_kernel::KernelKind::Stateless
        }

        fn accepts(&self) -> PayloadSpec {
            PayloadSpec {
                kind: PayloadKind::Either,
                tier: TierPref::Any,
            }
        }

        fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
            Ok(input.clone())
        }

        fn init(
            &self,
            _ctx: &moruna_kernel::InitCtx,
        ) -> moruna_kernel::Result<Box<dyn moruna_kernel::KernelState>> {
            Ok(Box::new(moruna_kernel::NoState))
        }

        fn apply(
            &self,
            _state: &mut dyn moruna_kernel::KernelState,
            _input: moruna_kernel::Payload,
        ) -> moruna_kernel::Result<moruna_kernel::Payload> {
            Err(MorunaError::Alloc {
                bytes: 1,
                tier: moruna_kernel::Tier::Device(moruna_kernel::DeviceId(0)),
                budget: 0,
                in_use: 0,
            })
        }
    }

    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.initial_morsel_target = 8;
            cfg.error_policy = ErrorPolicy::Skip;
        })
        .source(FakeSource::new().splits(1, 3, 24))
        .kernel(Arc::new(DeviceOom))
        .go();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_of_records = Arc::clone(&seen);
    rig.scheduler
        .set_record_hook(Arc::new(move |record: &moruna_kernel::TraceRecord| {
            sink_of_records
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((record.seq, record.stage, record.outcome));
        }));
    match rig.scheduler.run(CancelToken::new()) {
        Ok(RunOutcome::Completed { .. }) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
    let hooked = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let traced = rig.trace.records();
    assert_eq!(traced.len(), 3, "one record per morsel per stage");
    assert_eq!(
        hooked.len(),
        6,
        "the hook saw the device error's record as well as the trace's"
    );
    for record in &traced {
        assert_eq!(record.outcome, moruna_kernel::Outcome::Skipped);
    }
    assert_eq!(rig.sink.skipped().len(), 3);
}
