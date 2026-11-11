//! Pre-dispatch fixed commitments use the same anonymous model as live control.
use crate::{INITIAL_ANON_AMPLIFICATION, KernelInfo, Result, model};
use moruna_kernel::{KernelKind, MorunaError};

/// The runtime's already-funded allocations and initial controller configuration.
pub struct FixedCommitmentInput {
    /// Effective run ceiling, unchanged from ControllerConfig.
    pub ceiling_bytes: u64,
    /// Sample before the arena exists.
    pub baseline_bytes: u64,
    /// Full resident arena, including any sink buffer.
    pub arena_bytes: u64,
    /// Source and sink operator shares outside the arena.
    pub engine_bytes: u64,
    /// The controller's minimum morsel size.
    pub morsel_min: u64,
    /// The controller's initial safety multiplier.
    pub safety_initial: f32,
}

/// Fixed native commitments that fit after declared instance state and one
/// initial minimum morsel per stage are funded. This grants no physical memory;
/// the runtime must additionally cap it by its actual remaining allowance.
pub fn fixed_commitment_budget(
    input: &FixedCommitmentInput,
    kernels: &[KernelInfo],
) -> Result<u64> {
    if !input.safety_initial.is_finite() || input.safety_initial <= 0.0 || input.morsel_min == 0 {
        return Err(MorunaError::Config {
            name: "budget.native_children",
            msg: "invalid initial controller morsel/safety configuration".into(),
        });
    }
    let headroom = input
        .ceiling_bytes
        .saturating_sub(input.baseline_bytes)
        .saturating_sub(input.arena_bytes)
        .saturating_sub(input.engine_bytes);
    let state = kernels.iter().fold(0u64, |sum, kernel| {
        let instances = match &kernel.kind {
            KernelKind::Stateful { max_instances } => {
                u64::try_from(max_instances.get()).unwrap_or(u64::MAX)
            }
            KernelKind::Stateless => 1,
        };
        sum.saturating_add(
            kernel
                .hints
                .state_bytes
                .unwrap_or(0)
                .saturating_mul(instances),
        )
    });
    let work = kernels.iter().fold(0u64, |sum, _| {
        sum.saturating_add(model::anon_work(
            input.morsel_min,
            INITIAL_ANON_AMPLIFICATION,
            input.safety_initial,
        ))
    });
    Ok(model::kernel_envelope(headroom, state).saturating_sub(work))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::{Fingerprint, KernelHints};
    fn input() -> FixedCommitmentInput {
        FixedCommitmentInput {
            ceiling_bytes: 402653184,
            baseline_bytes: 10026192,
            arena_bytes: 117440512,
            engine_bytes: 0,
            morsel_min: 4 << 20,
            safety_initial: 1.5,
        }
    }
    fn kernel(stage: u16, state: u64, instances: usize) -> KernelInfo {
        KernelInfo {
            state_uses_arena: true,
            stage,
            fingerprint: Fingerprint([0; 32]),
            schema_hash: [0; 32],
            hints: KernelHints {
                state_bytes: Some(state),
                ..Default::default()
            },
            kind: KernelKind::Stateful {
                max_instances: std::num::NonZeroUsize::new(instances).unwrap(),
            },
        }
    }
    #[test]
    fn measured_two_stage_admission_leaves_minimum_work_funded() {
        let cfg = input();
        let pool = fixed_commitment_budget(&cfg, &[kernel(1, 0, 1), kernel(2, 0, 1)]).unwrap();
        let work = 2 * model::anon_work(
            cfg.morsel_min,
            INITIAL_ANON_AMPLIFICATION,
            cfg.safety_initial,
        );
        let headroom = cfg.ceiling_bytes - cfg.baseline_bytes - cfg.arena_bytes - cfg.engine_bytes;
        let envelope = model::kernel_envelope(headroom, 0);
        assert_eq!(pool, 114780240);
        assert_eq!(pool + work, envelope);
        assert!(pool + work <= envelope);
    }
    #[test]
    fn declared_state_counts_every_instance_and_minimum_work_every_stage() {
        let baseline = fixed_commitment_budget(&input(), &[kernel(1, 0, 1)]).unwrap();
        let funded = fixed_commitment_budget(&input(), &[kernel(1, 10 << 20, 3)]).unwrap();
        assert_eq!(baseline - funded, 18874368);
        let two = fixed_commitment_budget(&input(), &[kernel(1, 0, 1), kernel(2, 0, 1)]).unwrap();
        assert_eq!(baseline - two, 25165824);
    }
    #[test]
    fn mixed_stages_fund_all_instances_and_source_sink_shares() {
        let mut cfg = input();
        // engine_bytes is the runtime's combined source and sink operator funding.
        let source = 8 << 20;
        let sink = 4 << 20;
        cfg.engine_bytes = source + sink;
        let mut stateless = kernel(2, 2 << 20, 1);
        stateless.kind = KernelKind::Stateless;
        let kernels = [kernel(1, 3 << 20, 2), stateless, kernel(3, 1 << 20, 3)];
        let state = (3 << 20) * 2 + (2 << 20) + (1 << 20) * 3;
        let work = 3 * model::anon_work(
            cfg.morsel_min,
            INITIAL_ANON_AMPLIFICATION,
            cfg.safety_initial,
        );
        let headroom = cfg.ceiling_bytes - cfg.baseline_bytes - cfg.arena_bytes - source - sink;
        let envelope = model::kernel_envelope(headroom, state);
        let pool = fixed_commitment_budget(&cfg, &kernels).unwrap();
        assert_eq!(pool + work, envelope);
        assert!(pool + work + state <= headroom);
        cfg.engine_bytes = 0;
        assert!(fixed_commitment_budget(&cfg, &kernels).unwrap() > pool);
    }

    #[test]
    fn exhausted_headroom_and_overflowed_commitments_cannot_wrap() {
        let mut cfg = input();
        cfg.arena_bytes = u64::MAX;
        assert_eq!(
            fixed_commitment_budget(&cfg, &[kernel(1, 0, 1)]).unwrap(),
            0
        );
        assert_eq!(
            fixed_commitment_budget(&input(), &[kernel(1, u64::MAX, 3)]).unwrap(),
            0
        );
        cfg.safety_initial = f32::NAN;
        assert!(fixed_commitment_budget(&cfg, &[]).is_err());
    }
}
