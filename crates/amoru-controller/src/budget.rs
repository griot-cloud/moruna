//! Budget arithmetic: `prepare` (f.1).

use amoru_kernel::{AmoruError, TierBudgets};

use crate::{Budgets, Inner, Phase, Result, model};

/// The fraction of free device memory the runtime may hold (f.1).
const DEVICE_FRACTION: f64 = 0.9;
/// The share of the host budget the placement engine's queues get; the rest is the workers'
/// in-flight allowance (f.1).
pub(crate) const PLACEMENT_SHARE: f64 = 0.5;
/// The share of the device budget the queues get (f.1).
const DEVICE_QUEUE_SHARE: f64 = 0.6;

/// f.1. Take the arena's capacity as the host budget, derive the device budgets, and hand the
/// placement engine the tier budgets it starts with. The only placement call the controller
/// ever makes is `set_budgets`, here and when the state term changes (f.3, f.6).
///
/// The host budget is `cfg.arena_bytes` and nothing is subtracted from it: the arena is where
/// morsels live and its accounting is what enforces G-I1, and the facade has already taken the
/// ceiling, the pre-arena baseline, the reserve and the expected kernel state out of it
/// (12 f.1, 02 f.1). A sample is still taken, for the tick clock and the throttling counter.
pub(crate) fn prepare(ctl: &Inner) -> Result<Budgets> {
    // Sampling is a call into another component, so it happens with no lock held (RC-I10).
    let sample = ctl.peers().sampler.sample();

    let (budgets, tier_budgets) = {
        let mut state = ctl.held();
        if state.phase != Phase::Created {
            return Err(AmoruError::Config {
                name: "controller",
                msg: format!("prepare called in phase {:?}", state.phase),
            });
        }
        let ceiling = state.cfg.limits.memory_ceiling;
        let baseline = state.cfg.baseline_bytes;
        let reserve = model::scale(ceiling, f64::from(state.cfg.reserve_fraction));
        let host = state.cfg.arena_bytes;
        if host <= state.cfg.morsel_min.saturating_mul(2) {
            return Err(AmoruError::Config {
                name: "budget.host",
                msg: format!(
                    "the arena holds {host} bytes, which is not two morsels of {}; the ceiling \
                     is {ceiling}, the process already held {baseline} bytes before the arena \
                     existed and the reserve is {reserve}",
                    state.cfg.morsel_min
                ),
            });
        }

        // A kernel that allocates device memory on a host with no device cannot be sized;
        // that is a configuration error at prepare, not a failure on morsel 40,000 (h).
        if state.cfg.limits.devices.is_empty()
            && state.kernels.iter().any(|k| k.hints.uses_device_memory)
        {
            return Err(AmoruError::Config {
                name: "budget.device",
                msg: "a kernel declares uses_device_memory and the host has no device".into(),
            });
        }

        let mut device = [0u64; 8];
        for dev in &state.cfg.limits.devices {
            let at = usize::from(dev.id.0);
            if at < device.len() {
                device[at] = model::scale(dev.free_bytes, DEVICE_FRACTION);
            }
        }

        let budgets = Budgets {
            host,
            device,
            baseline,
            reserve,
        };
        let tier_budgets = tier_budgets(&state, &budgets, model::scale(host, PLACEMENT_SHARE));

        state.budgets = budgets;
        state.tier_budgets = tier_budgets.clone();
        state.last_sample_at_ns = sample.at_ns;
        state.last_throttled_us = sample.throttled_us;
        state.phase = Phase::Prepared;
        // Build one sizer per kernel stage now, so `on_record` before `start` has somewhere
        // to put what it sees (e.2).
        let factory = ctl.factory();
        let stages: Vec<_> = state.kernels.iter().map(|k| k.stage).collect();
        let cfg = state.cfg.clone();
        for stage in stages {
            let mut sizer_ctl = crate::StageCtl::new(stage, &cfg, factory(stage));
            sizer_ctl.instances_live = state.max_instances(stage);
            state.stages.push(sizer_ctl);
        }
        (budgets, tier_budgets)
    };

    tracing::info!(
        target: "ctl.budgets",
        host = budgets.host,
        baseline = budgets.baseline,
        reserve = budgets.reserve,
        "budgets"
    );
    ctl.peers().placement.set_budgets(tier_budgets);
    Ok(budgets)
}

/// The placement engine's share of every tier (f.1). `placement_host` is the host half, which
/// f.3 and f.6 shrink when the state term grows.
pub(crate) fn tier_budgets(
    state: &crate::ControllerState,
    budgets: &Budgets,
    placement_host: u64,
) -> TierBudgets {
    let mut device = [0u64; 8];
    for (at, slot) in device.iter_mut().enumerate() {
        *slot = model::scale(budgets.device[at], DEVICE_QUEUE_SHARE);
    }
    // One host pool, on the tier the arena actually has (contracts e.1): a pinned arena means
    // the queues live in pinned host memory and the ordinary host pool is zero, and the other
    // way round. Two non-zero host pools would double-count the same bytes.
    let (pinned_host, host) = if state.cfg.pinned {
        (placement_host, 0)
    } else {
        (0, placement_host)
    };
    TierBudgets {
        device,
        pinned_host,
        host,
        disk: state.cfg.disk_budget,
    }
}
