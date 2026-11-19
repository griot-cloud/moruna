//! RC-T28 through the controller (f.4b, item 6.3ac): the sizer of each stage is given the
//! stage's CPU counters as the scheduler reports them, a source's decode charged to the stage it
//! feeds and a sink's encode to the stage that feeds it, and a settled search's note says what
//! the stage spent there, its function's and the engine's. The search itself is
//! `sizer::rule::tests::rc_t28_*`.

mod common;

use std::sync::{Arc, Mutex};

use common::{GIB, MIB, config, kernel, probe, steady};
use moruna_controller::{
    CpuPerByte, Envelope, Observation, Proposal, Sizer, SizerOutcome, StageCpu,
};
use moruna_kernel::{CodecStats, KernelHints, SchedulerStats, StageId, StageStats};
use moruna_testkit::{FakeKnobs, FakeSampler};

/// A sizer that keeps its stage's target, says it settled there, and keeps what it was given.
struct Settled {
    seen: Arc<Mutex<Vec<(StageId, StageCpu)>>>,
    cost: Option<CpuPerByte>,
    at: u64,
}

impl Sizer for Settled {
    fn propose(&mut self, obs: &Observation, _envelope: &Envelope) -> Proposal {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((obs.stage, obs.cpu));
        self.at = obs.target;
        Proposal {
            morsel_target: obs.target,
            predicted_peak: None,
        }
    }

    fn observe(&mut self, _obs: &Observation, _outcome: &SizerOutcome) {}

    fn confidence(&self) -> f32 {
        1.0
    }

    fn name(&self) -> &'static str {
        "settled"
    }

    fn settled_target(&self) -> Option<u64> {
        (self.at > 0).then_some(self.at)
    }

    fn settled_cost(&self) -> Option<CpuPerByte> {
        self.cost
    }
}

/// Stage `stage`'s counters.
fn counted(stage: StageId, worker: u64, task: u64, bytes: u64) -> StageStats {
    StageStats {
        stage,
        instances_live: 1,
        worker_cpu_ns: worker,
        task_cpu_ns: task,
        bytes_in: bytes,
        ..StageStats::default()
    }
}

#[test]
fn rc_t28_each_stage_is_given_its_counters_and_the_note_says_its_parts() {
    let cfg = config(8 * GIB, 4);
    let probe_bytes = cfg.probe_bytes;
    let stats = SchedulerStats {
        per_stage: vec![
            counted(1, 1_000, 7_000, 4 * MIB),
            counted(2, 3_000, 0, 2 * MIB),
        ],
        workers_active: 4,
        workers_busy: 4,
        decodes: vec![CodecStats {
            cpu_ns: 500,
            stages: vec![1],
        }],
        encodes: vec![CodecStats {
            cpu_ns: 70,
            stages: vec![2],
        }],
        ..SchedulerStats::default()
    };
    let rig = common::Rig::new(
        cfg,
        vec![
            kernel(1, KernelHints::default()),
            kernel(2, KernelHints::default()),
        ],
        FakeKnobs::new()
            .probe_result(1, probe(probe_bytes, 2.0))
            .probe_result(2, probe(probe_bytes, 2.0))
            .stats(stats),
        FakeSampler::new().scripted(steady(400 * MIB, 64)),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&seen);
    rig.controller.set_sizer_factory(Arc::new(move |stage| {
        Box::new(Settled {
            seen: Arc::clone(&shared),
            // Stage 1's search measured where it settled; stage 2's was set by a memory cut.
            cost: (stage == 1).then_some(CpuPerByte {
                function: 2.4,
                engine: 1.2,
            }),
            at: 0,
        })
    }));
    rig.run_up();
    rig.controller.tick_once();

    let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let given = |stage: StageId| {
        seen.iter()
            .rev()
            .find(|(s, _)| *s == stage)
            .map(|(_, cpu)| *cpu)
            .expect("the stage's sizer was asked")
    };
    assert_eq!(
        given(1),
        StageCpu {
            worker_ns: 1_000,
            task_ns: 7_000,
            decode_ns: 500,
            encode_ns: 0,
            bytes_in: 4 * MIB,
        },
        "the source's decode is the first stage's"
    );
    assert_eq!(
        given(2),
        StageCpu {
            worker_ns: 3_000,
            task_ns: 0,
            decode_ns: 0,
            encode_ns: 70,
            bytes_in: 2 * MIB,
        },
        "the sink's encode is the last stage's"
    );

    let notes = rig.controller.stop().notes;
    let settled: Vec<&String> = notes.iter().filter(|n| n.contains("settled at")).collect();
    assert_eq!(settled.len(), 2, "{notes:?}");
    assert!(
        settled[0].starts_with("stage 1 settled at ")
            && settled[0].ends_with(
                "bytes a morsel, where the stage spent the least CPU per byte (its function's \
                 2.40 ns, the engine's 1.20 ns a byte)"
            ),
        "{}",
        settled[0]
    );
    assert!(
        settled[1].starts_with("stage 2 settled at ")
            && settled[1].ends_with("where the stage spent the least CPU per byte"),
        "{}",
        settled[1]
    );
}
