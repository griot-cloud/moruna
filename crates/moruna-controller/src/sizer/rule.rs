//! `RuleSizer`, the additive-increase multiplicative-decrease sizer that ships in v1 (f.4).

use super::{CpuPerByte, Envelope, Observation, Proposal, Sizer, SizerOutcome, StageCpu};

/// The exponential weight of one observation on the peak ratio (f.4).
const ALPHA: f64 = 0.2;
/// Below this fraction of the allowed peak the sizer increases (f.4).
const INCREASE_BELOW: f64 = 0.85;
/// Above this fraction of the allowed peak the sizer halves (f.4).
const DECREASE_ABOVE: f64 = 1.0;
/// Records at one target before its CPU per byte is judged (f.4b).
const SEARCH_RECORDS: usize = 12;
/// How much less CPU per byte a smaller target must cost to be kept (f.4b): above the noise
/// of a dozen morsels of one kernel over one dataset.
const SEARCH_GAIN: f64 = 0.06;

/// Where the throughput search of f.4b stands.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Search {
    /// Growing under the memory rule; nothing compared yet.
    Growing,
    /// `at` is being measured against `base`, whose CPU per byte was `base_cost`.
    Trying {
        base: u64,
        base_cost: CpuPerByte,
        at: u64,
    },
    /// Settled at `best`: the memory rule grows no further than it. `cost` is what the stage
    /// spent per byte there, when the search measured it there.
    Settled { best: u64, cost: Option<CpuPerByte> },
}

/// The rule sizer: additive increase while the stage is using less of its allowance than
/// `target_fraction` asks for, multiplicative decrease when it is using more.
///
/// It predicts nothing (`Proposal::predicted_peak` is `None`) and is therefore also the
/// shadow the learned sizer's prediction error is compared against (f.8), where the shadow's
/// prediction is `peak_ewma x bytes_in`.
pub struct RuleSizer {
    target_fraction: f64,
    increase_step: f64,
    peak_ewma: f64,
    observations: u64,
    search: Search,
    /// The start of the newest record seen when the target last changed: a record that
    /// started after it was sized at the current target.
    changed_at_ns: u64,
    /// The target at the previous proposal: when the one observed differs, the target moved.
    last_target: u64,
    /// The stage's counters when the target last changed: the window the current target ran
    /// starts there (item 6.3ac).
    window_from: StageCpu,
}

impl RuleSizer {
    /// A rule sizer with `controller.target_fraction` and `controller.increase_step`.
    pub fn new(target_fraction: f32, increase_step: f32) -> RuleSizer {
        RuleSizer {
            target_fraction: f64::from(target_fraction),
            increase_step: f64::from(increase_step),
            peak_ewma: 0.0,
            observations: 0,
            search: Search::Growing,
            changed_at_ns: 0,
            last_target: 0,
            window_from: StageCpu::default(),
        }
    }

    /// The whole stage's CPU per input byte at `target`, once enough records sized at it have
    /// come since the target last changed to judge (f.4b). A morsel within a factor of two of
    /// the target counts: a split smaller than the target arrives as it is.
    ///
    /// The cost is the scheduler's counters over the window the target ran (item 6.3ac): the
    /// workers' CPU over all the stage's units, its ring task's, and the decode and encode
    /// charged to it, per input byte. A record carries only its unit's share of the worker's
    /// CPU, so a smaller morsel's extra reads, decodes and encodes, and its task's time, are in
    /// none of them.
    fn cost_at(&self, obs: &Observation, target: u64) -> Option<CpuPerByte> {
        let sized = obs
            .recent
            .iter()
            .filter(|record| {
                record.outcome == moruna_kernel::Outcome::Ok
                    && record.t_start_ns > self.changed_at_ns
                    && record.bytes_in > 0
                    && record.bytes_in.saturating_mul(2) >= target
                    && record.bytes_in <= target.saturating_mul(2)
            })
            .count();
        if sized < SEARCH_RECORDS {
            return None;
        }
        obs.cpu.since(&self.window_from)
    }

    /// The proposal.
    fn propose_target(&mut self, _obs: &Observation, target: u64) -> Proposal {
        Proposal {
            morsel_target: target,
            predicted_peak: None,
        }
    }

    /// The exponentially weighted `peak_delta / bytes_in` this sizer has seen (f.4, f.8).
    pub fn peak_ewma(&self) -> f64 {
        self.peak_ewma
    }
}

impl Sizer for RuleSizer {
    fn propose(&mut self, obs: &Observation, envelope: &Envelope) -> Proposal {
        // RC-I3: the sizer honours damping itself, so a proposal never arrives early.
        if obs.completions_since_adjust < obs.damping {
            return Proposal {
                morsel_target: obs.target,
                predicted_peak: None,
            };
        }
        // How much of the growth this stage is allowed the stage is actually using, in the
        // quantity the records measure: the process's growth per morsel, `peak_ewma` per byte.
        // The allowance at the top of the envelope is what `target_fraction` aims at, so a stage
        // whose measured growth is well under it has room to grow.
        //
        // Both sides are the part that scales with the morsel. The fixed term is paid per morsel
        // in flight whatever the target, so halving cannot buy it back: counted, a kernel whose
        // growth is mostly fixed read as over its allowance at every size and was halved to the
        // floor. And both are the anonymous slope (2026-11-14): RB3 made `a_k` the in-arena
        // ratio, about 1.2, while `peak_ewma` stayed the anonymous growth, 2 to 8 for a Python
        // kernel, so every stage read as several times over and was halved while the process
        // sat at a sixth of its ceiling (Audit on the VM: 22 MB morsels, 3.8 of 21.6 GiB).
        let allowance = self.target_fraction * envelope.max as f64 * obs.a_anon;
        if allowance <= 0.0 || self.observations == 0 {
            return Proposal {
                morsel_target: obs.target,
                predicted_peak: None,
            };
        }
        let using = (self.peak_ewma * obs.target as f64 - obs.anon_fixed as f64).max(0.0);
        let ratio = using / allowance;
        if let (Search::Growing, Some(learned)) = (self.search, obs.learned_target) {
            // An earlier run of this kernel searched and settled: start where it did.
            self.search = Search::Settled {
                best: learned,
                cost: None,
            };
        }
        // Memory first: over the allowance halves, whatever the search was doing, and the
        // halved target is the most the search may come back to.
        if ratio > DECREASE_ABOVE {
            let halved = obs.target / 2;
            self.search = match self.search {
                // Nothing was measured at the half: what the search measured stands only where
                // it settled.
                Search::Settled { best, cost } if best <= halved => Search::Settled { best, cost },
                Search::Settled { .. } => Search::Settled {
                    best: halved,
                    cost: None,
                },
                _ => Search::Growing,
            };
            return self.propose_target(obs, halved);
        }
        let grown = crate::model::scale(obs.target, 1.0 + self.increase_step);
        if self.last_target != 0 && obs.target != self.last_target {
            // The target moved: what ran before now was sized at the old one. A proposal that
            // was not taken moves nothing, and the first proposal finds the target as it was.
            self.changed_at_ns = obs
                .recent
                .iter()
                .map(|r| r.t_start_ns)
                .max()
                .unwrap_or(self.changed_at_ns);
            self.window_from = obs.cpu;
        }
        self.last_target = obs.target;
        let can_grow = ratio < INCREASE_BELOW && grown <= envelope.max;
        // f.4b: the memory rule says how large a morsel may be; the kernel's CPU per byte says
        // whether as large is better. A Python kernel that walks its columns value by value is
        // cheaper on morsels that stay in the cache (Occupancy: 421 ns a row at 30k rows, 490 at
        // 250k), one that calls vectorised compute is cheaper on large ones (Enrich: 64 ns at
        // 30k, 27 at 500k). Once the memory rule stops growing, half the target is tried; it is
        // kept, and its half tried in turn, while it costs at least `SEARCH_GAIN` less per byte.
        match self.search {
            Search::Growing if can_grow => self.propose_target(obs, grown),
            Search::Growing => match self.cost_at(obs, obs.target) {
                Some(base_cost) if obs.target / 2 >= envelope.min => {
                    let at = obs.target / 2;
                    self.search = Search::Trying {
                        base: obs.target,
                        base_cost,
                        at,
                    };
                    self.propose_target(obs, at)
                }
                Some(cost) => {
                    self.search = Search::Settled {
                        best: obs.target,
                        cost: Some(cost),
                    };
                    self.propose_target(obs, obs.target)
                }
                None => self.propose_target(obs, obs.target),
            },
            Search::Trying {
                base,
                base_cost,
                at,
            } => {
                if obs.target != at {
                    // Something else moved the target (a breach, the freeze): start again.
                    self.search = Search::Growing;
                    return self.propose_target(obs, obs.target);
                }
                match self.cost_at(obs, at) {
                    None => self.propose_target(obs, at),
                    Some(cost) if cost.total() < base_cost.total() * (1.0 - SEARCH_GAIN) => {
                        if at / 2 >= envelope.min {
                            self.search = Search::Trying {
                                base: at,
                                base_cost: cost,
                                at: at / 2,
                            };
                            self.propose_target(obs, at / 2)
                        } else {
                            self.search = Search::Settled {
                                best: at,
                                cost: Some(cost),
                            };
                            self.propose_target(obs, at)
                        }
                    }
                    Some(_) => {
                        self.search = Search::Settled {
                            best: base,
                            cost: Some(base_cost),
                        };
                        self.propose_target(obs, base)
                    }
                }
            }
            Search::Settled { best, .. } if can_grow && obs.target < best => {
                self.propose_target(obs, grown.min(best))
            }
            Search::Settled { .. } => self.propose_target(obs, obs.target),
        }
    }

    fn observe(&mut self, _obs: &Observation, outcome: &SizerOutcome) {
        // Never divide by a zero input: a record with no input bytes says nothing about
        // amplification and is skipped for sizing (l, numerics).
        if outcome.bytes_in == 0 {
            return;
        }
        let ratio = outcome.peak_delta as f64 / outcome.bytes_in as f64;
        self.peak_ewma = if self.observations == 0 {
            ratio
        } else {
            ALPHA * ratio + (1.0 - ALPHA) * self.peak_ewma
        };
        self.observations += 1;
    }

    fn confidence(&self) -> f32 {
        // The rule sizer is a rule, not a model: it is as confident as it has evidence, and
        // the figure gates nothing.
        self.observations.min(100) as f32 / 100.0
    }

    fn name(&self) -> &'static str {
        "rule"
    }

    fn settled_target(&self) -> Option<u64> {
        match self.search {
            Search::Settled { best, .. } => Some(best),
            _ => None,
        }
    }

    fn settled_cost(&self) -> Option<CpuPerByte> {
        match self.search {
            Search::Settled { cost, .. } => cost,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::MorselFeatures;

    const MIB: u64 = 1 << 20;

    fn observation(target: u64, a_anon: f64, anon_fixed: u64) -> Observation {
        Observation {
            stage: 1,
            features: MorselFeatures::default(),
            active_workers: 8,
            // The in-arena ratio of RB3, a fraction of the anonymous one for a Python kernel.
            a_k: 1.2,
            a_anon,
            anon_fixed,
            learned_target: None,
            safety: 1.5,
            target,
            completions_since_adjust: 8,
            damping: 8,
            recent: Vec::new(),
            cpu: StageCpu::default(),
        }
    }

    fn after(peak_delta: u64, obs: &Observation) -> Proposal {
        let mut sizer = RuleSizer::new(0.85, 0.25);
        sizer.observe(
            obs,
            &SizerOutcome {
                peak_delta,
                bytes_in: obs.target,
                wall_ns: 1,
            },
        );
        sizer.propose(
            obs,
            &Envelope {
                min: MIB,
                max: 64 * MIB,
            },
        )
    }

    /// RC-T23 (f.4, 2026-11-14): growth within the anonymous allowance grows the morsel, however
    /// small the in-arena ratio. Audit's records grew about 2.7 bytes per byte against a fitted
    /// slope of 3.5; compared with `a_k`, 1.2, a 32 MiB morsel read as a third over its allowance
    /// and was halved, and the run settled at 22 MB with the process at a sixth of its ceiling.
    #[test]
    fn rc_t23_growth_within_the_anonymous_allowance_grows_the_morsel() {
        let obs = observation(32 * MIB, 3.5, 0);
        let proposal = after((2.7 * (32 * MIB) as f64) as u64, &obs);
        assert!(proposal.morsel_target > 32 * MIB, "{proposal:?}");
    }

    /// RC-T23: what a morsel costs whatever its size is not halved away; only the part that
    /// scales with the morsel is weighed against the allowance.
    #[test]
    fn rc_t23_a_fixed_cost_does_not_halve_the_morsel() {
        let obs = observation(16 * MIB, 0.5, 400 * MIB);
        let proposal = after(400 * MIB + 8 * MIB, &obs);
        assert!(proposal.morsel_target >= 16 * MIB, "{proposal:?}");
    }

    /// `n` records of morsels of `bytes`, costing `ns_per_byte` of CPU each, started after `t`.
    fn records(n: u64, bytes: u64, ns_per_byte: f64, t: u64) -> Vec<moruna_kernel::TraceRecord> {
        (0..n)
            .map(|i| moruna_kernel::TraceRecord {
                seq: i,
                stage: 1,
                worker: 0,
                instance: u16::MAX,
                t_start_ns: t + 1 + i,
                t_end_ns: t + 2 + i,
                rows_in: bytes / 100,
                bytes_in: bytes,
                rows_out: bytes / 100,
                bytes_out: bytes,
                tier_in: 2,
                tier_out: 2,
                feat_mean_string_len: 0.0,
                feat_null_ratio: 0.0,
                feat_column_bytes: vec![bytes],
                knob_morsel_target: bytes,
                knob_active_workers: 8,
                knob_read_ahead: 2,
                mem_anon_before: 0,
                mem_anon_peak: bytes,
                dev_mem_peak: 0,
                cpu_time_us: (bytes as f64 * ns_per_byte / 1000.0) as u64,
                throttled_delta_us: 0,
                q_bytes_before: vec![0; 5],
                q_bytes_after: vec![0; 5],
                staging_bytes_delta: 0,
                placement_miss_wait_us: 0,
                state_bytes: 0,
                sizer: 0,
                outcome: moruna_kernel::Outcome::Ok,
                error: None,
                alloc: moruna_kernel::KernelAlloc::default(),
            })
            .collect()
    }

    /// `n` morsels of `bytes` more on the counters of a stage with no ring task, costing
    /// `ns_per_byte` of the workers' CPU each (6.3ac): what `records` gives them.
    fn ran(cpu: &mut StageCpu, n: u64, bytes: u64, ns_per_byte: f64) -> StageCpu {
        cpu.worker_ns += n * (bytes as f64 * ns_per_byte / 1000.0) as u64 * 1000;
        cpu.bytes_in += n * bytes;
        *cpu
    }

    /// A sizer that has seen growth well inside its allowance, at the envelope's top.
    fn at_the_top() -> (RuleSizer, Envelope) {
        let mut sizer = RuleSizer::new(0.85, 0.25);
        let obs = observation(64 * MIB, 3.5, 0);
        sizer.observe(
            &obs,
            &SizerOutcome {
                peak_delta: 64 * MIB,
                bytes_in: 64 * MIB,
                wall_ns: 1,
            },
        );
        (
            sizer,
            Envelope {
                min: MIB,
                max: 64 * MIB,
            },
        )
    }

    /// RC-T24 (f.4b, 2026-11-14): at the memory rule's top, half the target is tried; it is kept
    /// while it costs at least 6% less CPU per byte, and the search settles on the cheapest.
    #[test]
    fn rc_t24_a_smaller_target_that_costs_less_is_kept() {
        let (mut sizer, envelope) = at_the_top();
        let mut cpu = StageCpu::default();
        let mut obs = observation(64 * MIB, 3.5, 0);
        obs.recent = records(12, 64 * MIB, 4.0, 0);
        obs.cpu = ran(&mut cpu, 12, 64 * MIB, 4.0);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 32 * MIB);
        // The sizer sees each new target first beside the records that ran before it.
        let mut obs = observation(32 * MIB, 3.5, 0);
        obs.recent = records(12, 64 * MIB, 4.0, 0);
        obs.cpu = cpu;
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 32 * MIB);
        obs.recent = records(12, 32 * MIB, 3.0, 100);
        obs.cpu = ran(&mut cpu, 12, 32 * MIB, 3.0);
        assert_eq!(
            sizer.propose(&obs, &envelope).morsel_target,
            16 * MIB,
            "25% cheaper: kept, and its half tried"
        );
        let mut obs = observation(16 * MIB, 3.5, 0);
        obs.recent = records(12, 32 * MIB, 3.0, 100);
        obs.cpu = cpu;
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 16 * MIB);
        obs.recent = records(12, 16 * MIB, 2.95, 200);
        obs.cpu = ran(&mut cpu, 12, 16 * MIB, 2.95);
        assert_eq!(
            sizer.propose(&obs, &envelope).morsel_target,
            32 * MIB,
            "2% cheaper is noise: back to the half that was"
        );
        assert_eq!(sizer.settled_target(), Some(32 * MIB));
        // Settled, the memory rule grows back to the settled target and no further.
        let obs = observation(24 * MIB, 3.5, 0);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 30 * MIB);
        let obs = observation(30 * MIB, 3.5, 0);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 32 * MIB);
    }

    /// RC-T24: a smaller target that costs more is not kept; the search settles where it began.
    #[test]
    fn rc_t24_a_smaller_target_that_costs_more_is_not_kept() {
        let (mut sizer, envelope) = at_the_top();
        let mut cpu = StageCpu::default();
        let mut obs = observation(64 * MIB, 3.5, 0);
        obs.recent = records(12, 64 * MIB, 2.0, 0);
        obs.cpu = ran(&mut cpu, 12, 64 * MIB, 2.0);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 32 * MIB);
        let mut obs = observation(32 * MIB, 3.5, 0);
        obs.recent = records(12, 64 * MIB, 2.0, 0);
        obs.cpu = cpu;
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 32 * MIB);
        obs.recent = records(12, 32 * MIB, 2.6, 100);
        obs.cpu = ran(&mut cpu, 12, 32 * MIB, 2.6);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 64 * MIB);
        assert_eq!(sizer.settled_target(), Some(64 * MIB));
    }

    /// RC-T24: too few records at a target is no verdict; the sizer waits at it.
    #[test]
    fn rc_t24_too_few_records_wait() {
        let (mut sizer, envelope) = at_the_top();
        let mut obs = observation(64 * MIB, 3.5, 0);
        obs.recent = records(5, 64 * MIB, 2.0, 0);
        obs.cpu = ran(&mut StageCpu::default(), 5, 64 * MIB, 2.0);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 64 * MIB);
        assert_eq!(sizer.settled_target(), None);
    }

    /// RC-T24: an earlier run's settled target is where the sizer starts, settled, so the memory
    /// rule grows no further than it.
    #[test]
    fn rc_t24_a_learned_target_is_where_the_search_starts() {
        let (mut sizer, envelope) = at_the_top();
        let mut obs = observation(16 * MIB, 3.5, 0);
        obs.learned_target = Some(16 * MIB);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 16 * MIB);
        assert_eq!(sizer.settled_target(), Some(16 * MIB));
    }

    /// One size's CPU per byte in the ring stage of RC-T28, in nanoseconds: its function's
    /// (the task's), and the engine's three parts.
    #[derive(Clone, Copy)]
    struct Spent {
        task: f64,
        worker: f64,
        decode: f64,
        encode: f64,
    }

    /// The ring stage of RC-T28 at `bytes` a morsel. Its function, which runs in the task, is
    /// cheaper per byte on smaller morsels (they stay in the cache); the engine pays a fixed cost
    /// a morsel, posting and taking its pieces on the worker and decoding its read, so it costs
    /// more per byte on smaller ones. The sink's encode is the same per byte at every size.
    fn ring_stage(bytes: u64) -> Spent {
        let at_64 = (64 * MIB) as f64 / bytes as f64;
        let task = match bytes / MIB {
            64.. => 3.0,
            32.. => 2.4,
            16.. => 2.2,
            _ => 2.1,
        };
        Spent {
            task,
            worker: 0.3 + 0.05 * at_64,
            decode: 0.4 + 0.15 * at_64,
            encode: 0.1,
        }
    }

    /// RC-T28 (f.4b, item 6.3ac): the search's cost at a size is the whole stage's CPU per byte,
    /// from the scheduler's counters over the window the size ran: the function's and the
    /// engine's. The stage above spends, per byte, 4.0 ns at 64 MiB (the function's 3.0), 3.6 at
    /// 32 (2.4) and 3.8 at 16 (2.2): the least at 32 MiB, though the function alone is cheaper
    /// at 16 and the worker's unit CPU alone, which the records carry, at 64.
    #[test]
    fn rc_t28_the_search_settles_where_the_whole_stage_costs_least() {
        let (mut sizer, envelope) = at_the_top();
        let mut cpu = StageCpu::default();
        let mut target = 64 * MIB;
        let mut t = 0;
        let mut tried = Vec::new();
        for _ in 0..16 {
            // Twelve morsels at the target, as the counters and the records see them.
            let spent = ring_stage(target);
            let ns = |per_byte: f64| (per_byte * target as f64) as u64 * 12;
            cpu.worker_ns += ns(spent.worker);
            cpu.task_ns += ns(spent.task);
            cpu.decode_ns += ns(spent.decode);
            cpu.encode_ns += ns(spent.encode);
            cpu.bytes_in += 12 * target;
            let recent = records(12, target, spent.worker, t);
            t += 100;
            tried.push(target);
            let mut obs = observation(target, 3.5, 0);
            obs.recent = recent.clone();
            obs.cpu = cpu;
            let next = sizer.propose(&obs, &envelope).morsel_target;
            if next != target {
                // The sizer sees each new target first beside what ran before it.
                let mut obs = observation(next, 3.5, 0);
                obs.recent = recent;
                obs.cpu = cpu;
                assert_eq!(sizer.propose(&obs, &envelope).morsel_target, next);
                target = next;
            }
            if sizer.settled_target().is_some() {
                break;
            }
        }
        assert_eq!(
            sizer.settled_target(),
            Some(32 * MIB),
            "tried {:?} MiB",
            tried.iter().map(|b| b / MIB).collect::<Vec<_>>()
        );
        let cost = sizer.settled_cost().expect("measured where it settled");
        assert!(
            (cost.function - 2.4).abs() < 1e-6 && (cost.engine - 1.2).abs() < 1e-6,
            "{cost:?}"
        );
    }

    /// RC-T28: a stage with no ring task runs its function on the worker, so the function's
    /// part is the workers' unit CPU and the engine's the decode and the encode beside it.
    #[test]
    fn rc_t28_a_stage_with_no_ring_task_is_its_units_and_its_ends() {
        let from = StageCpu {
            worker_ns: 100,
            task_ns: 0,
            decode_ns: 10,
            encode_ns: 5,
            bytes_in: 1_000,
        };
        let now = StageCpu {
            worker_ns: 2_100,
            task_ns: 0,
            decode_ns: 510,
            encode_ns: 505,
            bytes_in: 2_000,
        };
        let cost = now.since(&from).expect("bytes came in");
        assert_eq!((cost.function, cost.engine), (2.0, 1.0));
        assert_eq!(cost.total(), 3.0);
        assert_eq!(now.since(&now), None, "no bytes, no verdict");
    }

    /// RC-T28: a memory cut below the settled target keeps it settled there, with nothing
    /// measured at the new one to say.
    #[test]
    fn rc_t28_a_memory_cut_below_the_settled_target_has_no_cost() {
        let (mut sizer, envelope) = at_the_top();
        let mut obs = observation(16 * MIB, 3.5, 0);
        obs.learned_target = Some(16 * MIB);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 16 * MIB);
        assert_eq!(sizer.settled_cost(), None, "learned, not measured");
        // Growth far past the allowance halves the target and the settled one with it.
        sizer.observe(
            &obs,
            &SizerOutcome {
                peak_delta: 64 * 16 * MIB,
                bytes_in: 16 * MIB,
                wall_ns: 1,
            },
        );
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 8 * MIB);
        assert_eq!(sizer.settled_target(), Some(8 * MIB));
        assert_eq!(sizer.settled_cost(), None);
    }

    /// RC-T28: the search stops at the envelope's floor, settled with what it measured there:
    /// a half that would pass the floor is not tried, whether the search is starting or keeping
    /// a cheaper half. A sizer with no search has no settled cost.
    #[test]
    fn rc_t28_the_search_stops_at_the_floor_with_what_it_measured() {
        // Starting at the floor's double: the half is the floor, tried and kept, and its half
        // passes the floor, so it settles there.
        let (mut sizer, _) = at_the_top();
        let envelope = Envelope {
            min: 32 * MIB,
            max: 64 * MIB,
        };
        let mut cpu = StageCpu::default();
        let mut obs = observation(64 * MIB, 3.5, 0);
        obs.recent = records(12, 64 * MIB, 4.0, 0);
        obs.cpu = ran(&mut cpu, 12, 64 * MIB, 4.0);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 32 * MIB);
        let mut obs = observation(32 * MIB, 3.5, 0);
        obs.recent = records(12, 64 * MIB, 4.0, 0);
        obs.cpu = cpu;
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 32 * MIB);
        obs.recent = records(12, 32 * MIB, 3.0, 100);
        obs.cpu = ran(&mut cpu, 12, 32 * MIB, 3.0);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 32 * MIB);
        assert_eq!(sizer.settled_target(), Some(32 * MIB));
        let cost = sizer.settled_cost().expect("measured there");
        assert!((cost.total() - 3.0).abs() < 1e-4, "{cost:?}");

        // Starting at the floor itself: no half to try, settled where it is.
        let (mut sizer, _) = at_the_top();
        let envelope = Envelope {
            min: 64 * MIB,
            max: 64 * MIB,
        };
        let mut obs = observation(64 * MIB, 3.5, 0);
        obs.recent = records(12, 64 * MIB, 2.0, 0);
        obs.cpu = ran(&mut StageCpu::default(), 12, 64 * MIB, 2.0);
        assert_eq!(sizer.propose(&obs, &envelope).morsel_target, 64 * MIB);
        assert_eq!(sizer.settled_target(), Some(64 * MIB));
        let cost = sizer.settled_cost().expect("measured there");
        assert!(
            (cost.function - 2.0).abs() < 1e-4 && cost.engine == 0.0,
            "{cost:?}"
        );

        assert_eq!(
            super::super::LearnedSizer::new(0.85, 0.25).settled_cost(),
            None
        );
    }

    /// RC-T23: growth past the allowance still halves.
    #[test]
    fn rc_t23_growth_past_the_allowance_halves() {
        let obs = observation(16 * MIB, 3.5, 0);
        let proposal = after(20 * 16 * MIB, &obs);
        assert_eq!(proposal.morsel_target, 8 * MIB, "{proposal:?}");
    }
}
