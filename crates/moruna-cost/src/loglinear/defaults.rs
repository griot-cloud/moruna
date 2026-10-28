//! The declared default (section 5.4, the third fallback): what a unit costs when neither the
//! function nor its kind has history. The kernel facts in [`Inputs`] answer where they are
//! supplied (warm-up's figures, today's profile store); these constants answer where they are
//! not.

use moruna_job::Role;

use crate::estimate::{Estimate, Target};
use crate::inputs::Inputs;

/// The figures a model falls back to, declared by whoever builds the model.
#[derive(Clone, Debug, PartialEq)]
pub struct Defaults {
    /// Fixed time per batch, seconds.
    pub per_batch_seconds: f64,
    /// Time per row, seconds.
    pub per_row_seconds: f64,
    /// Time per input byte, seconds.
    pub per_byte_seconds: f64,
    /// Peak memory per input byte.
    pub peak_per_byte: f64,
    /// Memory held after the unit per input byte.
    pub retained_per_byte: f64,
    /// State per distinct key, bytes, for a kernel declared stateful.
    pub state_per_key_bytes: f64,
    /// Worker time per wall second.
    pub worker_seconds_per_second: f64,
    /// Rows out per row in.
    pub selectivity: f64,
    /// Bytes per row where nothing says.
    pub bytes_per_row: f64,
    /// Rows where nothing says.
    pub rows: f64,
    /// p95 over p50 of a declared figure.
    pub spread: f64,
    /// The further factor on the spread when the unit's rows or bytes are unknown.
    pub unknown_size_spread: f64,
}

impl Defaults {
    /// The standard declaration: a millisecond per batch and a tenth of a microsecond per row,
    /// a peak of twice the input, the output held, one row out per row in, 64-byte rows, and a
    /// p95 four times the median (sixteen when the size is unknown too).
    pub fn standard() -> Defaults {
        Defaults {
            per_batch_seconds: 1e-3,
            per_row_seconds: 1e-7,
            per_byte_seconds: 0.0,
            peak_per_byte: 2.0,
            retained_per_byte: 1.0,
            state_per_key_bytes: 64.0,
            worker_seconds_per_second: 1.0,
            selectivity: 1.0,
            bytes_per_row: 64.0,
            rows: 10_000.0,
            spread: 4.0,
            unknown_size_spread: 4.0,
        }
    }

    /// The declared estimate of a regressed target for a unit of a kind of `role`.
    pub fn estimate(&self, target: Target, inputs: &Inputs, role: Role) -> Estimate {
        let k = &inputs.kernel;
        let known = inputs.data.rows.is_some() && inputs.bytes().is_some();
        let rows = inputs.data.rows.map_or(self.rows, |r| r as f64);
        let bytes = inputs
            .bytes()
            .unwrap_or_else(|| rows * inputs.bytes_per_row().unwrap_or(self.bytes_per_row));
        let single = k.per_batch_seconds.unwrap_or(self.per_batch_seconds)
            + rows * k.per_row_seconds.unwrap_or(self.per_row_seconds)
            + bytes * k.per_byte_seconds.unwrap_or(self.per_byte_seconds);
        let workers = inputs.resources.workers.unwrap_or(1);
        let speedup = match workers {
            0 | 1 => 1.0,
            2 | 3 => k.speedup_2.unwrap_or(1.0),
            _ => k.speedup_4.or(k.speedup_2).unwrap_or(1.0),
        }
        .max(1e-9);
        let duration = single / speedup;
        let value = match target {
            Target::Duration => duration,
            Target::WorkerTime => {
                duration
                    * k.worker_seconds_per_second
                        .unwrap_or(self.worker_seconds_per_second)
            }
            Target::PeakMemory => bytes * k.peak_per_byte.unwrap_or(self.peak_per_byte),
            Target::RetainedMemory => bytes * k.retained_per_byte.unwrap_or(self.retained_per_byte),
            Target::StateGrowth => {
                if k.stateful == Some(true) {
                    inputs.distinct().map_or(rows, |d| d as f64)
                        * k.state_per_key_bytes.unwrap_or(self.state_per_key_bytes)
                } else {
                    0.0
                }
            }
            Target::ReadBytes => {
                if role == Role::Source {
                    bytes
                } else {
                    0.0
                }
            }
            Target::SpillBytes => 0.0,
            Target::RowsOut => rows * self.selectivity,
            Target::BytesOut => rows * self.selectivity * self.bytes_per_row,
        };
        let spread = if known {
            self.spread
        } else {
            self.spread * self.unknown_size_spread
        };
        Estimate::declared(value, spread, target.unit())
    }
}

impl Default for Defaults {
    fn default() -> Defaults {
        Defaults::standard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::Basis;
    use crate::inputs::Column;

    #[test]
    fn declared_figures_from_the_kernel_and_the_constants() {
        let d = Defaults::standard();
        let mut inputs = Inputs::batch(1_000, 64_000);
        let e = d.estimate(Target::Duration, &inputs, Role::Kernel);
        assert!((e.p50 - (1e-3 + 1e-4)).abs() < 1e-12);
        assert_eq!(e.basis, Basis::Default);
        assert!((e.p95 / e.p50 - 4.0).abs() < 1e-5);
        assert_eq!(
            d.estimate(Target::ReadBytes, &inputs, Role::Kernel).p50,
            0.0
        );
        assert_eq!(
            d.estimate(Target::ReadBytes, &inputs, Role::Source).p50,
            64_000.0
        );
        assert_eq!(d.estimate(Target::SpillBytes, &inputs, Role::Sink).p50, 0.0);
        assert_eq!(
            d.estimate(Target::StateGrowth, &inputs, Role::Kernel).p50,
            0.0
        );
        assert_eq!(
            d.estimate(Target::RowsOut, &inputs, Role::Kernel).p50,
            1_000.0
        );
        assert_eq!(
            d.estimate(Target::BytesOut, &inputs, Role::Kernel).p50,
            64_000.0
        );
        inputs.kernel.per_row_seconds = Some(1e-6);
        inputs.kernel.peak_per_byte = Some(3.0);
        inputs.kernel.retained_per_byte = Some(0.5);
        inputs.kernel.worker_seconds_per_second = Some(2.0);
        inputs.kernel.stateful = Some(true);
        inputs.kernel.speedup_2 = Some(2.0);
        inputs.kernel.speedup_4 = Some(4.0);
        inputs.resources.workers = Some(4);
        inputs.data.columns = vec![Column {
            name: "k".into(),
            distinct: Some(10),
            key: true,
            ..Column::default()
        }];
        let duration = d.estimate(Target::Duration, &inputs, Role::Kernel).p50;
        assert!((duration - (1e-3 + 1e-3) / 4.0).abs() < 1e-12);
        let worker = d.estimate(Target::WorkerTime, &inputs, Role::Kernel).p50;
        assert!((worker - 2.0 * duration).abs() < 1e-12);
        assert_eq!(
            d.estimate(Target::PeakMemory, &inputs, Role::Kernel).p50,
            192_000.0
        );
        assert_eq!(
            d.estimate(Target::RetainedMemory, &inputs, Role::Kernel)
                .p50,
            32_000.0
        );
        assert_eq!(
            d.estimate(Target::StateGrowth, &inputs, Role::Kernel).p50,
            640.0
        );
        inputs.resources.workers = Some(2);
        let two = d.estimate(Target::Duration, &inputs, Role::Kernel).p50;
        assert!((two - 1e-3).abs() < 1e-12);
    }

    #[test]
    fn an_unknown_size_is_wider() {
        let d = Defaults::default();
        let e = d.estimate(Target::Duration, &Inputs::default(), Role::Kernel);
        assert!((e.p50 - (1e-3 + 1e-3)).abs() < 1e-12, "{}", e.p50);
        assert!(e.p95 / e.p50 > 15.0);
    }
}
