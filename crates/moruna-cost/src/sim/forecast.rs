//! Uncertainty (SDD 24 f.9): one run at the medians for the schedule, and Monte Carlo runs with
//! drawn durations for the job's spread.

use super::{Policy, Run, Simulator};
use crate::error::Result;
use crate::sample::Rng;

/// Monte Carlo runs `explain` makes (`architecture/adaptive-runtime.md` section 8).
pub const DRAWS: u32 = 200;
/// The seed of those runs, so the same job planned twice gets the same plan.
pub const SEED: u64 = 0x6d6f_7275_6e61_2d73;

/// A median and a 95th percentile over the runs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spread {
    /// The median.
    pub p50: f64,
    /// The 95th percentile.
    pub p95: f64,
}

/// One task's spread over the runs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TaskSpread {
    /// From its first unit's start to its last unit's end.
    pub span: Spread,
    /// Worker time it held.
    pub worker_seconds: Spread,
    /// The most memory it held at once.
    pub peak_memory: Spread,
    /// Its outputs spilled.
    pub spill_bytes: Spread,
}

/// The job's forecast: the medians' run, and the spread of every figure over the draws.
#[derive(Clone, Debug, PartialEq)]
pub struct Forecast {
    /// The run with every unit at its median: the schedule the plan shows.
    pub median: Run,
    /// The Monte Carlo runs behind the spreads; with none, each spread is the medians' run.
    pub draws: u32,
    /// When the job finishes.
    pub finish: Spread,
    /// The most memory held at once.
    pub peak_memory: Spread,
    /// Memory times time.
    pub memory_byte_seconds: Spread,
    /// Worker time held.
    pub worker_seconds: Spread,
    /// Bytes moved on disk.
    pub disk_bytes: Spread,
    /// Bytes spilled.
    pub spill_bytes: Spread,
    /// Every task, in the workload's order.
    pub tasks: Vec<TaskSpread>,
}

/// Figures recorded per run besides the tasks'.
const JOB: usize = 6;
/// Figures recorded per task per run.
const TASK: usize = 4;

impl Simulator {
    /// The medians' run, then `draws` runs with durations drawn from a generator seeded with
    /// `seed`; each figure's p50 and p95 over the draws (nearest rank).
    pub fn forecast(&mut self, policy: &mut dyn Policy, draws: u32, seed: u64) -> Result<Forecast> {
        let median = self.run(policy)?.clone();
        let n = median.tasks.len();
        let width = JOB + TASK * n;
        let count = draws.max(1) as usize;
        let mut samples = vec![0.0f64; width * count];
        let record = |row: &mut [f64], run: &Run| {
            row[0] = run.finish;
            row[1] = run.peak_memory;
            row[2] = run.memory_byte_seconds;
            row[3] = run.worker_seconds;
            row[4] = run.disk_bytes;
            row[5] = run.spill_bytes;
            for (t, task) in run.tasks.iter().enumerate() {
                let at = JOB + TASK * t;
                row[at] = task.end - task.start;
                row[at + 1] = task.worker_seconds;
                row[at + 2] = task.peak_memory;
                row[at + 3] = task.spill_bytes;
            }
        };
        if draws == 0 {
            record(&mut samples[..width], &median);
        } else {
            let mut rng = Rng::new(seed);
            for row in samples.chunks_mut(width) {
                let run = self.run_sampled(policy, &mut rng)?;
                record(row, run);
            }
        }
        let mut column = vec![0.0f64; count];
        let mut spread = |i: usize| {
            for (r, row) in samples.chunks(width).enumerate() {
                column[r] = row[i];
            }
            column.sort_by(f64::total_cmp);
            let rank = |q: f64| column[((q * count as f64).ceil() as usize).clamp(1, count) - 1];
            Spread {
                p50: rank(0.5),
                p95: rank(0.95),
            }
        };
        let finish = spread(0);
        let peak_memory = spread(1);
        let memory_byte_seconds = spread(2);
        let worker_seconds = spread(3);
        let disk_bytes = spread(4);
        let spill_bytes = spread(5);
        let tasks = (0..n)
            .map(|t| {
                let at = JOB + TASK * t;
                TaskSpread {
                    span: spread(at),
                    worker_seconds: spread(at + 1),
                    peak_memory: spread(at + 2),
                    spill_bytes: spread(at + 3),
                }
            })
            .collect();
        Ok(Forecast {
            median,
            draws,
            finish,
            peak_memory,
            memory_byte_seconds,
            worker_seconds,
            disk_bytes,
            spill_bytes,
            tasks,
        })
    }
}
