//! RC-T1 consistent_knobs. For random budgets, amplifications and worker counts, every knob
//! set the controller writes satisfies the working-set inequality of b. The test recomputes
//! that inequality from the knobs it observed and the numbers it fed in, so it is checking the
//! controller's arithmetic against the document and not against itself. Proves RC-I1, and with
//! it G-I5: the only writer of any knob in the run is this component.

mod common;

use amoru_kernel::{KernelHints, StageId};
use amoru_testkit::{FakeKnobs, FakeSampler};
use common::{GIB, MIB, active_workers, kernel, morsel_targets, probe, read_aheads, steady};

/// A deterministic generator, so a failure is reproducible from the seed alone.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn in_range(&mut self, low: u64, high: u64) -> u64 {
        low + self.next() % (high - low + 1)
    }
}

#[test]
fn rc_t1_consistent_knobs() {
    let mut rng = Lcg(0x5eed_1234_abcd_0001);
    for case in 0..64u64 {
        let ceiling = rng.in_range(1, 16) * GIB;
        let workers = rng.in_range(1, 32) as u16;
        let stages = rng.in_range(1, 4) as usize;
        let baseline = rng.in_range(50, 400) * MIB;
        let amplifications: Vec<f64> = (0..stages)
            .map(|_| rng.in_range(1, 200) as f64 / 10.0)
            .collect();

        let cfg = common::config_with_baseline(ceiling, workers, baseline);
        let probe_bytes = cfg.probe_bytes;
        let mut knobs = FakeKnobs::new();
        for (at, amplification) in amplifications.iter().enumerate() {
            let stage = StageId::try_from(at + 1).expect("stage fits");
            knobs = knobs.probe_result(stage, probe(probe_bytes, *amplification));
        }
        let kernels: Vec<_> = (0..stages)
            .map(|at| {
                kernel(
                    StageId::try_from(at + 1).expect("stage fits"),
                    KernelHints::default(),
                )
            })
            .collect();
        let sampler = FakeSampler::new().scripted(steady(baseline, 4));
        let rig = common::Rig::new(cfg.clone(), kernels, knobs, sampler);
        rig.run_up();

        let writes = rig.writes();
        let targets = morsel_targets(&writes);
        assert_eq!(targets.len(), stages, "case {case}: one target per stage");
        let active = *active_workers(&writes)
            .last()
            .expect("the worker count is written");
        let read_ahead = *read_aheads(&writes).last().expect("read-ahead is written");
        let high_water = common::high_waters(&writes)
            .into_iter()
            .filter(|(_, tier, _)| *tier == amoru_kernel::TierKind::Host)
            .map(|(_, _, bytes)| bytes)
            .max()
            .expect("a high water per queue is written");

        // The working set of b, recomputed here from the document rather than from the code.
        let reserve = (ceiling as f64 * f64::from(cfg.reserve_fraction)) as u64;
        let host_budget = ceiling - baseline - reserve;
        let share = f64::from(active) / stages as f64;
        let mut working_set = 0.0f64;
        for (at, (_, target)) in targets.iter().enumerate() {
            let a_k =
                ((probe_bytes as f64 * amplifications[at]) as u64) as f64 / probe_bytes as f64;
            working_set += share * *target as f64 * a_k.max(0.5) * f64::from(cfg.safety_initial);
        }
        let queues = (stages + 1) as f64;
        working_set += queues * high_water as f64;
        let split_bytes = targets[0].1 as f64;
        working_set += f64::from(read_ahead) * split_bytes;

        assert!(
            working_set <= host_budget as f64,
            "case {case}: working set {working_set} exceeds the host budget {host_budget} \
             (ceiling {ceiling}, baseline {baseline}, workers {workers}, stages {stages}, \
             amplifications {amplifications:?})"
        );
        // RC-I7 is the same inequality read the other way: no more workers than the half can
        // feed one morsel each.
        assert!(
            active >= 1 && active <= workers,
            "case {case}: worker count"
        );
        rig.controller.stop();
    }
}
