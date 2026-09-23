//! Bottleneck classification (f.6).
//!
//! The table is evaluated in order and selects one action. RC-I8 is what keeps the controller
//! from fighting itself: read-ahead and the worker count are never both moved in one tick, so
//! two knobs can never chase the same symptom in opposite directions.

use std::time::Duration;

use moruna_kernel::{MorunaError, Knob, Sample, SchedulerStats};

use crate::{Actions, ControllerState, SAFETY_CAP, model, summary::Bottleneck};

/// Below this fraction of the active workers being busy the pipeline is not compute bound.
const BUSY_LOW: f64 = 0.7;
/// Above this fraction it is.
const BUSY_HIGH: f64 = 0.9;
/// The fraction of the reserve that anonymous memory reaching triggers the memory rows.
const RESERVE_TRIP: f64 = 0.5;
/// The fraction of a tick spent throttled that means the CPU quota is the bottleneck.
const THROTTLE_TRIP: f64 = 0.10;
/// How much a high water shrinks when a row calls for it.
const HIGH_WATER_SHRINK: f64 = 0.9;
/// How much the state term must grow in one tick to count as growing (f.6).
const STATE_GROWTH: f64 = 0.10;
/// The read-ahead ceiling (`readahead.splits`).
const READ_AHEAD_MAX: u16 = 64;
/// Ticks of a rising last queue that mean the sink is behind (f.6).
const RISING_TICKS: usize = 4;

/// What one tick measured, gathered before the lock was taken.
pub(crate) struct Inputs {
    pub stats: SchedulerStats,
    pub sample: Sample,
    pub elapsed: Duration,
    /// Bytes in Q0, from the trace tail's queue columns, or `None` when the tail had nothing
    /// to say. `TraceTail::tail` reads the trace writer's in-memory chunks only (04 f.3), so a
    /// window can come back shorter than it was asked for, or empty; that is an absence of
    /// evidence and never evidence of an empty queue, so the two rows that read Q0 do not fire
    /// on it (f, the bound at the head of the section).
    pub q0_bytes: Option<u64>,
}

/// f.6. Classify, take the one action the class calls for, and return the class.
pub(crate) fn classify(
    state: &mut ControllerState,
    inputs: &Inputs,
    actions: &mut Actions,
) -> Bottleneck {
    let ceiling = state.cfg.limits.memory_ceiling;
    let trip = ceiling.saturating_sub(model::scale(state.budgets.reserve, RESERVE_TRIP));
    let anon = inputs.sample.anon_bytes;
    let busy = if inputs.stats.workers_active == 0 {
        0.0
    } else {
        f64::from(inputs.stats.workers_busy) / f64::from(inputs.stats.workers_active)
    };
    let low_water = state.high_water / 2;
    let last_stage = u16::try_from(state.stage_count()).unwrap_or(u16::MAX);

    let state_grew = {
        let before = state.last_state_total;
        let now = state.state_total;
        now > before && (before == 0 || (now - before) as f64 / before as f64 > STATE_GROWTH)
    };

    if anon >= trip && state_grew {
        state_growth(state, actions);
        return Bottleneck::StateGrowth;
    }
    if anon >= trip {
        memory_pressure(state, last_stage, actions);
        return Bottleneck::Memory;
    }
    if throttled(state, inputs) {
        if state.active_workers > 1 {
            state.active_workers -= 1;
            actions
                .knobs
                .push(Knob::ActiveWorkers(state.active_workers));
        }
        return Bottleneck::CpuQuota;
    }
    if busy < BUSY_LOW
        && inputs.q0_bytes.is_some_and(|bytes| bytes < low_water)
        && inputs.stats.reads_in_flight == state.read_ahead
    {
        read_ahead_up(state, actions);
        return Bottleneck::IoRead;
    }
    if busy < BUSY_LOW && inputs.q0_bytes.is_some_and(|bytes| bytes >= low_water) {
        // The workers are idle and the queue is full, so what is parking them is the budget
        // and not the source: fund a worker out of the queues.
        shrink_high_waters(state, actions);
        let allowed = workers_allowed(state);
        if allowed > state.active_workers {
            state.active_workers += 1;
            actions
                .knobs
                .push(Knob::ActiveWorkers(state.active_workers));
        }
        return Bottleneck::Memory;
    }
    if busy > BUSY_HIGH
        && rising(state)
        && inputs.stats.writes_in_flight == inputs.stats.sink_concurrency
    {
        if state.staging_on.insert(last_stage) {
            actions.knobs.push(Knob::StagingTrigger {
                stage: last_stage,
                on: true,
            });
        }
        if state.read_ahead > 1 {
            state.read_ahead -= 1;
            actions.knobs.push(Knob::ReadAhead(state.read_ahead));
        }
        return Bottleneck::Sink;
    }
    if busy > BUSY_HIGH {
        return Bottleneck::Compute;
    }
    Bottleneck::Idle
}

/// Whether the tick spent more than a tenth of itself throttled (f.6).
fn throttled(state: &mut ControllerState, inputs: &Inputs) -> bool {
    let delta = inputs
        .sample
        .throttled_us
        .saturating_sub(state.last_throttled_us);
    state.last_throttled_us = inputs.sample.throttled_us;
    let tick_us = inputs.elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
    tick_us > 0 && delta as f64 / tick_us as f64 > THROTTLE_TRIP
}

/// Whether the last queue has been rising for four ticks (f.6).
///
/// Only ticks whose tail answered are in the history, so four rising readings are four
/// readings and not four silences (f, the bound at the head of the section).
fn rising(state: &ControllerState) -> bool {
    if state.qn_history.len() < RISING_TICKS {
        return false;
    }
    state
        .qn_history
        .iter()
        .zip(state.qn_history.iter().skip(1))
        .all(|(before, after)| after > before)
}

/// The StateGrowth row. A kernel whose state grows with morsels seen is not described by
/// `a_k x bytes_in` at all, so the answer is to fund the state out of the morsels and the
/// workers, and to say so when the state alone has taken more than the half it may have.
fn state_growth(state: &mut ControllerState, actions: &mut Actions) {
    let half = state.budgets.host / 2;
    if state.state_total > half
        && let Some((stage, footprint, seq)) = worst_state(state)
    {
        state.terminated = true;
        actions.terminate = Some(MorunaError::Budget {
            seq,
            stage,
            footprint,
            budget: half,
            features: state
                .stage(stage)
                .and_then(|ctl| ctl.last.as_ref())
                .map(|last| moruna_kernel::MorselFeatures {
                    rows: last.rows_in,
                    bytes: last.bytes_in,
                    column_bytes: last.column_bytes.clone(),
                    mean_string_len: Some(last.mean_string_len),
                    null_ratio: Some(last.null_ratio),
                    shape: None,
                    dtype: None,
                })
                .unwrap_or_default(),
        });
        state.note(format!(
            "state growth on stage {stage} exceeds half the budget"
        ));
        return;
    }
    state.note("state growth: targets and workers reduced to fund it".into());
    model::resize(state, actions);
}

/// The stage holding the most state, with the sequence number of its last record.
fn worst_state(state: &ControllerState) -> Option<(u16, u64, u64)> {
    state
        .stages
        .iter()
        .max_by_key(|ctl| ctl.state_bytes)
        .map(|ctl| {
            (
                ctl.stage,
                ctl.state_bytes,
                ctl.last.as_ref().map(|last| last.seq).unwrap_or(0),
            )
        })
}

/// The Memory row: halve the largest stage, raise its safety and turn staging on.
fn memory_pressure(state: &mut ControllerState, last_stage: u16, actions: &mut Actions) {
    let largest = state
        .stages
        .iter()
        .enumerate()
        .max_by_key(|(_, ctl)| ctl.target)
        .map(|(at, _)| at);
    if let Some(at) = largest {
        let morsel_min = state.cfg.morsel_min;
        let ctl = &mut state.stages[at];
        let bytes = (ctl.target / 2).max(morsel_min);
        if bytes != ctl.target {
            ctl.target = bytes;
            ctl.completions_since_adjust = 0;
            let stage = ctl.stage;
            actions.knobs.push(Knob::MorselTarget { stage, bytes });
        }
        let ctl = &mut state.stages[at];
        ctl.safety = (ctl.safety + 0.2).min(SAFETY_CAP);
        model::refresh_envelopes(state);
    }
    if state.staging_on.insert(last_stage) {
        actions.knobs.push(Knob::StagingTrigger {
            stage: last_stage,
            on: true,
        });
    }
}

/// The IoRead row: another split in flight when the working set allows it, and otherwise the
/// bytes to pay for one, taken from the queues.
fn read_ahead_up(state: &mut ControllerState, actions: &mut Actions) {
    let next = state.read_ahead.saturating_add(1).min(READ_AHEAD_MAX);
    if next == state.read_ahead {
        return;
    }
    let targets: Vec<u64> = state.stages.iter().map(|s| s.target).collect();
    let was = state.read_ahead;
    state.read_ahead = next;
    if model::fits(state, &targets) {
        actions.knobs.push(Knob::ReadAhead(next));
        return;
    }
    state.read_ahead = was;
    shrink_high_waters(state, actions);
}

/// Shrink every queue's high water by a tenth (f.6, rows 4 and 5).
fn shrink_high_waters(state: &mut ControllerState, actions: &mut Actions) {
    let shrunk = model::scale(state.high_water, HIGH_WATER_SHRINK);
    if shrunk == state.high_water {
        return;
    }
    state.high_water = shrunk;
    let tier = model::host_tier(state);
    for queue in 0..state.queue_count() {
        actions.knobs.push(Knob::HighWater {
            stage: u16::try_from(queue).unwrap_or(u16::MAX),
            tier,
            bytes: shrunk,
        });
    }
}

/// RC-I7, re-evaluated: the workers the budget can feed one morsel each.
pub(crate) fn workers_allowed(state: &ControllerState) -> u16 {
    let max_allowance = state
        .stages
        .iter()
        .map(|ctl| ctl.allowance(ctl.target))
        .max()
        .unwrap_or(0);
    if max_allowance == 0 {
        return state.cfg.workers_max.max(1);
    }
    let allowed = model::worker_half(state) / max_allowance;
    u16::try_from(allowed)
        .unwrap_or(u16::MAX)
        .clamp(1, state.cfg.workers_max.max(1))
}
