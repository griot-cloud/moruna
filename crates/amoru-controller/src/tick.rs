//! The tick thread, the record queue drain and the lock bound (g, RC-I10).
//!
//! Every tick is the same shape and the shape is what RC-I10 is about: measure with no lock
//! held, decide with the lock held and nothing else, write with the lock released. The
//! controller runs beside workers that must never wait on it, so the arithmetic of one tick is
//! the only thing the mutex ever covers.

use std::time::{Duration, Instant};

use amoru_kernel::{AmoruError, Outcome, StageId, TraceRecord};

use crate::{
    Actions, ControllerState, Inner, PROFILE_WRITE_AFTER, Phase, RecordSummary,
    SAMPLER_STALL_TICKS, SizerOutcome, TICK_LOCK_BOUND, WINDOW, apply, breach, classify, device,
    model, profile,
};

/// The exponential weight of one record on the shadow sizer's peak ratio (f.8).
const SHADOW_ALPHA: f64 = 0.2;
/// Records folded into the per-stage state under one acquisition of the controller's mutex
/// (RC-I10). The queue holds up to `RECORD_QUEUE_CAPACITY` of them, so the drain is cut into
/// pieces of this size rather than held as one. Sixteen is a hundred microseconds of work on a
/// quiet host and a few hundred under coverage instrumentation, which leaves the 5 ms bound an
/// order of magnitude of room for a scheduler that has other things to run.
const ABSORB_PER_LOCK: usize = 16;

/// One tick (f.6, f.5, f.9).
pub(crate) fn tick(ctl: &Inner) {
    let (phase, stages, tick_ms) = {
        let state = ctl.held();
        (
            state.phase,
            state.stages.iter().map(|s| s.stage).collect::<Vec<_>>(),
            state.cfg.tick_ms,
        )
    };
    if phase != Phase::Running {
        return;
    }

    // Measure with no lock held (RC-I10). The sample is the one call that can be slow, so it
    // is also the one that is timed.
    let at = Instant::now();
    let sample = ctl.peers().sampler.sample();
    let sample_took = at.elapsed();
    if sample_took > TICK_LOCK_BOUND {
        let mut state = ctl.held();
        state.ticks_skipped = state.ticks_skipped.saturating_add(1);
        tracing::warn!(
            target: "ctl.tick_skipped",
            micros = sample_took.as_micros() as u64,
            "sample over the tick bound; tick skipped"
        );
        return;
    }
    let stats = ctl.peers().stats.scheduler_stats();
    let mut tails: Vec<(StageId, Vec<TraceRecord>)> = Vec::with_capacity(stages.len());
    for stage in &stages {
        tails.push((*stage, ctl.peers().trace.tail(*stage, WINDOW)));
    }
    // `tail` answers out of the trace writer's in-memory chunks only (04 f.3), so a window can
    // be shorter than it was asked for or empty. An empty answer is an absence of evidence: it
    // is carried as `None` rather than as a queue of zero bytes, because a rule that read it as
    // zero would see an idle source every time the trace window rolled over.
    let q0_bytes: Option<u64> = tails
        .first()
        .and_then(|(_, records)| records.last())
        .map(|r| r.q_bytes_before.iter().copied().sum());
    let qn_bytes: Option<u64> = tails
        .last()
        .and_then(|(_, records)| records.last())
        .map(|r| r.q_bytes_after.iter().copied().sum());

    // The record queue is drained before the decision, in bounded chunks, each under its own
    // acquisition of the lock. A burst of completions can leave tens of thousands of records
    // queued, and folding all of them in one go is the one piece of a tick whose cost is not
    // bounded by the number of stages: RC-I10 is a bound on how long a worker can be made to
    // wait, so the work is cut into pieces rather than the queue left to grow.
    absorb_queue(ctl);

    let (actions, write_profile) = 'decide: {
        let mut state = ctl.held();
        let elapsed = state.last_tick.elapsed();
        state.last_tick = Instant::now();
        let mut actions = Actions::default();

        // A sampler that has stopped moving is not a slow sampler, it is a blind controller,
        // and a blind controller must not keep sizing (h, failures).
        if sample.at_ns == state.last_sample_at_ns {
            state.stall_ticks = state.stall_ticks.saturating_add(1);
            if state.stall_ticks >= SAMPLER_STALL_TICKS && !state.terminated {
                state.terminated = true;
                actions.terminate = Some(AmoruError::Config {
                    name: "sampler",
                    msg: format!(
                        "the sampler has not moved for {SAMPLER_STALL_TICKS} ticks of {tick_ms} ms"
                    ),
                });
                break 'decide (actions, false);
            }
        } else {
            state.stall_ticks = 0;
            state.last_sample_at_ns = sample.at_ns;
        }

        state.source_exhausted = stats.source_exhausted;
        for stage_stats in &stats.per_stage {
            if let Some(ctl_stage) = state.stage_mut(stage_stats.stage) {
                ctl_stage.instances_live = stage_stats.instances_live;
            }
        }

        state.last_state_total = state.state_total;
        model::recompute_state(&mut state);
        // RC-I1 is a property of every moment, not of every write: the state term may have
        // grown since the last tick, so the standing knob set is put right before anything
        // else this tick decides (apply::repair).
        apply::repair(&mut state, &mut actions);
        match qn_bytes {
            Some(bytes) => {
                state.qn_history.push_back(bytes);
                while state.qn_history.len() > 8 {
                    state.qn_history.pop_front();
                }
            }
            // A tick the tail could not answer is not a reading of the last queue, so the
            // history of f.6's sink row is cleared rather than continued across the gap.
            None => state.qn_history.clear(),
        }

        // The device high water f.11 took to zero is put back by the next tick (f.11).
        if let Some((queue, _)) = state.high_water_override.take() {
            let bytes = model::scale(state.budgets.device[0], 0.6)
                / u64::try_from(state.queue_count()).unwrap_or(1).max(1);
            actions.knobs.push(amoru_kernel::Knob::HighWater {
                stage: queue,
                tier: amoru_kernel::TierKind::Device,
                bytes,
            });
        }

        let inputs = classify::Inputs {
            stats,
            sample,
            elapsed,
            q0_bytes,
        };
        let class = classify::classify(&mut state, &inputs, &mut actions);
        tracing::debug!(target: "ctl.class", class = ?class, "tick");
        record_class(&mut state, class, elapsed);

        // The sizers run after the classification, so a stage that the class has just halved
        // is not immediately proposed back up in the same tick.
        if actions.terminate.is_none() && !state.tiny {
            for (at, (stage, records)) in tails.into_iter().enumerate() {
                if at >= state.stages.len() || state.stages[at].stage != stage {
                    continue;
                }
                let observation = apply::observation(&state, at, records);
                let envelope = state.stages[at].envelope;
                let proposal = state.stages[at].sizer.propose(&observation, &envelope);
                apply::apply_proposal(&mut state, at, proposal, &mut actions);
                let seq = state.stages[at].last.as_ref().map(|r| r.seq).unwrap_or(0);
                apply::maybe_fall_back(&mut state, at, seq);
            }
        }

        let due = state.cfg.checkpoint_enabled
            && state.last_profile_write.elapsed()
                >= Duration::from_millis(state.cfg.checkpoint_interval_ms)
            && state
                .stages
                .iter()
                .any(|ctl_stage| ctl_stage.records >= PROFILE_WRITE_AFTER);
        if due {
            state.last_profile_write = Instant::now();
        }
        (actions, due)
    };

    ctl.perform(actions);
    if write_profile {
        // f.9: a run that dies keeps what it learned, so the store is written while the run is
        // still going and not only at the end.
        profile::write_all(ctl);
    }
}

/// Drain the record queue and fold what it held into the per-stage state, in bounded chunks,
/// each under its own acquisition of the lock (RC-I10).
pub(crate) fn absorb_queue(ctl: &Inner) {
    for chunk in ctl.drain().chunks(ABSORB_PER_LOCK) {
        let mut state = ctl.held();
        for summary in chunk {
            absorb(&mut state, summary);
        }
        // The records just folded in moved `a_anon`, which is a term of the envelope (f.3), and
        // a stale envelope is not a harmless one: it clamps the sizer to the target it was
        // computed from, so a stage whose measured cost has come down would be held at the size
        // it had when the probe spoke and f.4's increase would never be accepted.
        model::refresh_envelopes(&mut state);
    }
}

/// Fold one record into its stage's state.
fn absorb(state: &mut ControllerState, summary: &RecordSummary) {
    let Some(at) = state.stages.iter().position(|s| s.stage == summary.stage) else {
        return;
    };
    let morsel_target = state.stages[at].target;
    let in_flight_morsels = crate::model::share(state, morsel_target);
    let resting = crate::model::resting_anon(state);
    // The state term is funded before the fit, so the fit is of what is left above it (f.3).
    let funded = state.state_total;
    let stage = summary.stage;
    let mut over_target = false;
    {
        let ctl = &mut state.stages[at];
        ctl.records = ctl.records.saturating_add(1);
        ctl.completions = ctl.completions.saturating_add(1);
        ctl.completions_since_adjust = ctl.completions_since_adjust.saturating_add(1);
        ctl.last = Some(summary.clone());
        if summary.instance != u16::MAX {
            let slot = ctl.state_by_instance.entry(summary.instance).or_insert(0);
            *slot = (*slot).max(summary.state_bytes);
        } else if summary.state_bytes > 0 {
            let slot = ctl.state_by_instance.entry(0).or_insert(0);
            *slot = (*slot).max(summary.state_bytes);
        }
        if summary.bytes_in > 0 {
            let ratio = summary.peak_delta as f64 / summary.bytes_in as f64;
            // f.3's anon fit. Two readings of the same record, and the model takes the larger,
            // because the inequality it feeds has to bound a peak and not track a mean:
            //
            //   * `peak_delta`, the process's growth across this morsel.
            //   * `mem_anon_peak` itself, above the resting figure and above the state the
            //     inequality has already funded. This is the reading the delta cannot give:
            //     allocator retention, a sink's buffers and anything else that is resident at
            //     the peak without being attributable to one morsel. It is the quantity S1
            //     measures, which is why f.3 is fitted to it.
            //
            // The two readings go to the fit side by side, with the bytes the model believed
            // were in flight beside them, and `observe_anon` fits f.3's two terms from them: the
            // residency before `apply` is the constant, measured and not inferred, and the growth
            // across `apply` carries the slope.
            //
            // Until 2026-09-23 the whole of `mem_anon_peak - resting_anon` was divided by the
            // bytes in flight, which charged a Python job's 92 MiB of allocator and interpreter
            // retention to every morsel byte: `a_anon x safety` reached 42 and rose further every
            // time the controller halved the target, because halving the bytes in flight doubles
            // a ratio whose numerator did not move. The refusal threshold then rose as the
            // morsel shrank, which is the opposite of a control loop.
            //
            // The fit bounds a window of records rather than the run (`observe_anon`): a fit
            // that could only rise would leave a kernel sized for its worst morsel forever and
            // f.4's margin, which tightens only on an accepted increase, would never tighten.
            let in_flight_bytes =
                (in_flight_morsels.max(1.0) * morsel_target.max(summary.bytes_in) as f64).max(1.0);
            let before = summary.mem_anon_peak.saturating_sub(summary.peak_delta);
            let fixed = before.saturating_sub(resting).saturating_sub(funded) as f64;
            ctl.observe_anon(in_flight_bytes, fixed, summary.peak_delta as f64);
            ctl.peak_ratios.push_back(ratio);
            while ctl.peak_ratios.len() > WINDOW {
                ctl.peak_ratios.pop_front();
            }
            ctl.peak_ewma = if ctl.var_count == 0 {
                ratio
            } else {
                SHADOW_ALPHA * ratio + (1.0 - SHADOW_ALPHA) * ctl.peak_ewma
            };
            // Welford, so the profile's variance is a function of every record and not of a
            // window (e.3).
            ctl.var_count += 1;
            let delta = ratio - ctl.var_mean;
            ctl.var_mean += delta / ctl.var_count as f64;
            ctl.var_m2 += delta * (ratio - ctl.var_mean);
            ctl.wall_ewma = if ctl.wall_ewma == 0.0 {
                summary.wall_ns as f64
            } else {
                SHADOW_ALPHA * summary.wall_ns as f64 + (1.0 - SHADOW_ALPHA) * ctl.wall_ewma
            };
            over_target = summary.bytes_in > morsel_target;
        }
    }
    apply::score_prediction(state, at, summary.peak_delta, summary.bytes_in);
    let observation = apply::observation(state, at, Vec::new());
    let outcome = SizerOutcome {
        peak_delta: summary.peak_delta,
        bytes_in: summary.bytes_in,
        wall_ns: summary.wall_ns,
    };
    state.stages[at].sizer.observe(&observation, &outcome);
    if over_target && state.row_note_done.insert(stage) {
        // A single row wider than the target is not a sizing failure, it is a fact about the
        // data; the allowance is computed from what arrived and the note says so once (h).
        state.note(format!("row larger than morsel target on stage {stage}"));
    }
}

/// The timeline of j, compressed: consecutive equal classes are one entry with a duration.
fn record_class(state: &mut ControllerState, class: crate::Bottleneck, elapsed: Duration) {
    let seconds = elapsed.as_secs_f64();
    match state.last_class {
        Some(last) if last == class => {
            if let Some(entry) = state.timeline.last_mut() {
                entry.0 += seconds;
            }
        }
        _ => {
            state.timeline.push((seconds, class));
            state.last_class = Some(class);
        }
    }
}

/// The scheduler's record hook (contracts d.11).
pub(crate) fn on_record(ctl: &Inner, r: &TraceRecord) {
    // The queue is the normal path: the tick thread does the arithmetic (g).
    if ctl.enqueue(RecordSummary::of(r)) {
        let mut state = ctl.held();
        state.records_dropped = state.records_dropped.saturating_add(1);
    }
    // The two paths that cannot wait for a tick: a device allocation failure, whose retry the
    // scheduler is about to make (f.11), and a breach, which must be smaller by the time the
    // next morsel is picked up (f.7).
    let actions = {
        let mut state = ctl.held();
        if state.terminated {
            return;
        }
        let mut actions = Actions::default();
        if r.outcome == Outcome::Error {
            device::check(&mut state, r, &mut actions);
        }
        if actions.terminate.is_none() {
            breach::check(&mut state, r, &mut actions);
        }
        actions
    };
    ctl.perform(actions);
}
