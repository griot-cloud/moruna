//! How big a morsel is (SDD 24 f.10; `architecture/adaptive-runtime.md` section 0 and G4): the
//! rows per batch that keep a unit's fixed overhead small within its share of the memory, while
//! every worker still has a batch.

use crate::estimate::Target;
use crate::inputs::Inputs;
use crate::key::CostKey;
use crate::model::{CostModel, Marginal};

/// The share of a unit's median time its fixed overhead may take: the size where the overhead is
/// this share is the size the overhead asks for.
pub const OVERHEAD_SHARE: f64 = 0.05;
/// The fewest rows a morsel is given while nothing else bounds it lower.
pub const MIN_MORSEL_ROWS: u64 = 1_024;
/// The most rows a morsel is given.
pub const MAX_MORSEL_ROWS: u64 = 1 << 22;
/// The batch size the model is first asked about; the answer is asked again at the size chosen.
pub const PROBE_ROWS: u64 = 65_536;

/// What set a morsel's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    /// The overhead: the size where the fixed part is [`OVERHEAD_SHARE`] of a unit.
    Overhead,
    /// [`MAX_MORSEL_ROWS`].
    Largest,
    /// [`MIN_MORSEL_ROWS`].
    Smallest,
    /// The memory: a larger unit's 95th-percentile peak passes its share.
    Memory,
    /// The workers: a larger morsel leaves a worker without a batch.
    Workers,
    /// The input: every row in one morsel.
    Rows,
}

/// A task's morsel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Morsel {
    /// Rows per batch.
    pub rows: u64,
    /// Batches over the task's rows.
    pub units: u32,
    /// What set the size.
    pub bound: Bound,
    /// A unit's time near that size, as a line in rows.
    pub duration: Marginal,
    /// A unit's peak memory near that size.
    pub peak: Marginal,
}

/// The morsel of a task of `key` over `total_rows` rows, given `inputs` (its row width, its
/// columns, its kernel's figures; its rows are set here), `memory_per_unit` bytes for each unit
/// at once and `workers` workers (SDD 24 f.10):
///
/// 1. the overhead asks for `fixed · (1 - s) / (s · per_row)` rows, `s` = [`OVERHEAD_SHARE`],
///    held to [`MIN_MORSEL_ROWS`] and [`MAX_MORSEL_ROWS`];
/// 2. the memory allows `(memory_per_unit - peak_fixed) / peak_per_row` rows at the 95th
///    percentile, one at least;
/// 3. the input allows `total_rows`;
/// 4. the workers allow `total_rows / workers` rows, rounded up, so each has a batch.
///
/// The smallest wins. The model's cost is read through [`CostModel::marginal`] at
/// [`PROBE_ROWS`], then again at the size chosen, and the size chosen again from that.
pub fn morsel(
    model: &dyn CostModel,
    key: &CostKey,
    inputs: &Inputs,
    total_rows: u64,
    memory_per_unit: f64,
    workers: u32,
) -> Morsel {
    let total = total_rows.max(1);
    let mut probe = PROBE_ROWS.min(total);
    let mut chosen = (probe, Bound::Rows);
    let mut lines = (Marginal::default(), Marginal::default());
    for _ in 0..2 {
        let duration = model.marginal(key, inputs, Target::Duration, probe);
        let peak = model.marginal(key, inputs, Target::PeakMemory, probe);
        lines = (duration, peak);
        let time = duration.p50;
        let for_overhead = if time.per_row > 0.0 {
            time.fixed * (1.0 - OVERHEAD_SHARE) / (OVERHEAD_SHARE * time.per_row)
        } else if time.fixed > 0.0 {
            f64::INFINITY
        } else {
            0.0
        };
        let (mut rows, mut bound) = if for_overhead >= MAX_MORSEL_ROWS as f64 {
            (MAX_MORSEL_ROWS, Bound::Largest)
        } else if for_overhead <= MIN_MORSEL_ROWS as f64 {
            (MIN_MORSEL_ROWS, Bound::Smallest)
        } else {
            (for_overhead.ceil() as u64, Bound::Overhead)
        };
        let room = peak.p95;
        if room.per_row > 0.0 {
            let fits = ((memory_per_unit - room.fixed) / room.per_row)
                .floor()
                .max(1.0);
            if fits < rows as f64 {
                rows = fits as u64;
                bound = Bound::Memory;
            }
        }
        if total < rows {
            rows = total;
            bound = Bound::Rows;
        }
        let each = total.div_ceil(u64::from(workers.max(1)));
        if each < rows {
            rows = each;
            bound = Bound::Workers;
        }
        chosen = (rows.max(1), bound);
        probe = chosen.0;
    }
    let units = total.div_ceil(chosen.0).min(u64::from(u32::MAX)) as u32;
    Morsel {
        rows: chosen.0,
        units,
        bound: chosen.1,
        duration: lines.0,
        peak: lines.1,
    }
}
