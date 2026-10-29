//! What a model answers and what a unit measured (design section 4; SDD 24 e.3).

use std::fmt;

use serde::{Deserialize, Serialize};

/// What an estimate counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// Seconds of wall or worker time.
    Seconds,
    /// Bytes.
    Bytes,
    /// Rows.
    Rows,
}

impl Unit {
    /// The smallest amount worth telling from nothing: a nanosecond, a byte, a row. A model
    /// working on logarithms adds it so that an exact zero (no spill, no state) has a logarithm.
    pub fn floor(self) -> f64 {
        match self {
            Unit::Seconds => 1e-9,
            Unit::Bytes | Unit::Rows => 1.0,
        }
    }
}

/// Where an estimate's numbers came from (section 5.4). Ordered from most to least known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// This function's history, at least one unit of which ran in a real run.
    Measured,
    /// This function's history, all of it from warm-up.
    WarmUp,
    /// The kind's prior: every function of the kind, pooled.
    Prior,
    /// A declared default.
    Default,
}

/// A distribution, not a number (decision 2): its median, its 95th percentile, and what it
/// rests on.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    /// The median.
    pub p50: f64,
    /// The 95th percentile; never below the median.
    pub p95: f64,
    /// What it counts.
    pub unit: Unit,
    /// Which fallback answered.
    pub basis: Basis,
    /// The observations it rests on.
    pub observations: u32,
    /// Whether the inputs lie outside what the observations measured; the spread is wider when
    /// they do.
    pub extrapolated: bool,
}

impl Estimate {
    /// An estimate with nothing behind it but a declared figure and a spread factor.
    pub fn declared(p50: f64, spread: f64, unit: Unit) -> Estimate {
        let floor = unit.floor();
        let p50 = p50.max(0.0);
        Estimate {
            p50,
            p95: ((p50 + floor) * spread.max(1.0) - floor).max(p50),
            unit,
            basis: Basis::Default,
            observations: 0,
            extrapolated: false,
        }
    }
}

/// What a unit costs or measured, one per field of [`UnitCost`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Wall time.
    Duration,
    /// Worker time.
    WorkerTime,
    /// Peak memory.
    PeakMemory,
    /// Memory still held after the unit.
    RetainedMemory,
    /// Rows out.
    RowsOut,
    /// Bytes out.
    BytesOut,
    /// State added.
    StateGrowth,
    /// Bytes read from disk.
    ReadBytes,
    /// Bytes spilled to disk.
    SpillBytes,
}

impl Target {
    /// Every target, in the order of [`UnitCost`]'s fields.
    pub const ALL: [Target; 9] = [
        Target::Duration,
        Target::WorkerTime,
        Target::PeakMemory,
        Target::RetainedMemory,
        Target::RowsOut,
        Target::BytesOut,
        Target::StateGrowth,
        Target::ReadBytes,
        Target::SpillBytes,
    ];

    /// What it counts.
    pub fn unit(self) -> Unit {
        match self {
            Target::Duration | Target::WorkerTime => Unit::Seconds,
            Target::RowsOut => Unit::Rows,
            _ => Unit::Bytes,
        }
    }

    /// The field's name.
    pub fn as_str(self) -> &'static str {
        match self {
            Target::Duration => "duration",
            Target::WorkerTime => "worker_time",
            Target::PeakMemory => "peak_memory",
            Target::RetainedMemory => "retained_memory",
            Target::RowsOut => "rows_out",
            Target::BytesOut => "bytes_out",
            Target::StateGrowth => "state_growth",
            Target::ReadBytes => "read_bytes",
            Target::SpillBytes => "spill_bytes",
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One task applied to one batch: what it takes and what it gives (section 4).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnitCost {
    /// Wall time.
    pub duration: Estimate,
    /// Worker time.
    pub worker_time: Estimate,
    /// Peak memory.
    pub peak_memory: Estimate,
    /// Memory still held after the unit.
    pub retained_memory: Estimate,
    /// Rows out.
    pub rows_out: Estimate,
    /// Bytes out.
    pub bytes_out: Estimate,
    /// State added.
    pub state_growth: Estimate,
    /// Bytes read from disk.
    pub read_bytes: Estimate,
    /// Bytes spilled to disk.
    pub spill_bytes: Estimate,
}

impl UnitCost {
    /// Every field the same declared estimate of zero (of its target's unit), to be filled.
    pub fn zero() -> UnitCost {
        let at = |t: Target| Estimate::declared(0.0, 1.0, t.unit());
        UnitCost {
            duration: at(Target::Duration),
            worker_time: at(Target::WorkerTime),
            peak_memory: at(Target::PeakMemory),
            retained_memory: at(Target::RetainedMemory),
            rows_out: at(Target::RowsOut),
            bytes_out: at(Target::BytesOut),
            state_growth: at(Target::StateGrowth),
            read_bytes: at(Target::ReadBytes),
            spill_bytes: at(Target::SpillBytes),
        }
    }

    /// The estimate of one target.
    pub fn get(&self, target: Target) -> &Estimate {
        match target {
            Target::Duration => &self.duration,
            Target::WorkerTime => &self.worker_time,
            Target::PeakMemory => &self.peak_memory,
            Target::RetainedMemory => &self.retained_memory,
            Target::RowsOut => &self.rows_out,
            Target::BytesOut => &self.bytes_out,
            Target::StateGrowth => &self.state_growth,
            Target::ReadBytes => &self.read_bytes,
            Target::SpillBytes => &self.spill_bytes,
        }
    }

    /// The estimate of one target, to set.
    pub fn get_mut(&mut self, target: Target) -> &mut Estimate {
        match target {
            Target::Duration => &mut self.duration,
            Target::WorkerTime => &mut self.worker_time,
            Target::PeakMemory => &mut self.peak_memory,
            Target::RetainedMemory => &mut self.retained_memory,
            Target::RowsOut => &mut self.rows_out,
            Target::BytesOut => &mut self.bytes_out,
            Target::StateGrowth => &mut self.state_growth,
            Target::ReadBytes => &mut self.read_bytes,
            Target::SpillBytes => &mut self.spill_bytes,
        }
    }
}

/// Where a measured unit ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// A real run.
    Run,
    /// Warm-up, on generated data.
    WarmUp,
}

impl Origin {
    /// The basis history of this origin gives an estimate.
    pub fn basis(self) -> Basis {
        match self {
            Origin::Run => Basis::Measured,
            Origin::WarmUp => Basis::WarmUp,
        }
    }
}

/// What one unit measured: the values of [`UnitCost`]'s fields, each when it was measured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    /// Where it ran.
    pub origin: Origin,
    /// Wall time, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Worker time, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_time: Option<f64>,
    /// Peak memory, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_memory: Option<f64>,
    /// Memory held after, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_memory: Option<f64>,
    /// Rows out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_out: Option<f64>,
    /// Bytes out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_out: Option<f64>,
    /// State added, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_growth: Option<f64>,
    /// Bytes read from disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_bytes: Option<f64>,
    /// Bytes spilled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spill_bytes: Option<f64>,
}

impl Outcome {
    /// An outcome of `origin` with nothing measured yet.
    pub fn new(origin: Origin) -> Outcome {
        Outcome {
            origin,
            duration: None,
            worker_time: None,
            peak_memory: None,
            retained_memory: None,
            rows_out: None,
            bytes_out: None,
            state_growth: None,
            read_bytes: None,
            spill_bytes: None,
        }
    }

    /// The measured value of one target.
    pub fn get(&self, target: Target) -> Option<f64> {
        match target {
            Target::Duration => self.duration,
            Target::WorkerTime => self.worker_time,
            Target::PeakMemory => self.peak_memory,
            Target::RetainedMemory => self.retained_memory,
            Target::RowsOut => self.rows_out,
            Target::BytesOut => self.bytes_out,
            Target::StateGrowth => self.state_growth,
            Target::ReadBytes => self.read_bytes,
            Target::SpillBytes => self.spill_bytes,
        }
    }

    /// Set the measured value of one target.
    pub fn set(&mut self, target: Target, value: f64) -> &mut Outcome {
        let slot = match target {
            Target::Duration => &mut self.duration,
            Target::WorkerTime => &mut self.worker_time,
            Target::PeakMemory => &mut self.peak_memory,
            Target::RetainedMemory => &mut self.retained_memory,
            Target::RowsOut => &mut self.rows_out,
            Target::BytesOut => &mut self.bytes_out,
            Target::StateGrowth => &mut self.state_growth,
            Target::ReadBytes => &mut self.read_bytes,
            Target::SpillBytes => &mut self.spill_bytes,
        };
        *slot = Some(value);
        self
    }

    /// Refuse a value that is not a finite, non-negative amount (CM-I6).
    pub fn check(&self) -> Result<(), String> {
        for target in Target::ALL {
            if let Some(value) = self.get(target)
                && !(value.is_finite() && value >= 0.0)
            {
                return Err(format!(
                    "`{target}` is {value}; a measured amount is finite and not negative"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_cover_every_target() {
        let mut cost = UnitCost::zero();
        let mut outcome = Outcome::new(Origin::Run);
        for (i, target) in Target::ALL.into_iter().enumerate() {
            cost.get_mut(target).p50 = i as f64;
            outcome.set(target, i as f64 * 2.0);
            assert!(!target.to_string().is_empty());
        }
        for (i, target) in Target::ALL.into_iter().enumerate() {
            assert_eq!(cost.get(target).p50, i as f64);
            assert_eq!(cost.get(target).unit, target.unit());
            assert_eq!(outcome.get(target), Some(i as f64 * 2.0));
        }
        assert!(outcome.check().is_ok());
        outcome.set(Target::SpillBytes, -1.0);
        assert!(outcome.check().unwrap_err().contains("spill_bytes"));
        outcome.set(Target::SpillBytes, f64::NAN);
        assert!(outcome.check().is_err());
    }

    #[test]
    fn declared_spread_and_bases() {
        let e = Estimate::declared(10.0, 4.0, Unit::Rows);
        assert_eq!(e.p50, 10.0);
        assert_eq!(e.p95, 43.0);
        assert_eq!(e.basis, Basis::Default);
        let zero = Estimate::declared(-3.0, 0.5, Unit::Bytes);
        assert_eq!((zero.p50, zero.p95), (0.0, 0.0));
        assert_eq!(Origin::Run.basis(), Basis::Measured);
        assert_eq!(Origin::WarmUp.basis(), Basis::WarmUp);
        assert!(Basis::Measured < Basis::Default);
        assert_eq!(Unit::Seconds.floor(), 1e-9);
    }
}
