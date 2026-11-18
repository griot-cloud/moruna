//! A function's own memory, measured (system design 3A; item 6.3g).
//!
//! A run's budget is the arena, which holds the batches in flight, and the room above it, where
//! a function keeps its own working memory. That room comes from measurement: what one call of
//! the function holds outside its batch, as a line over the batch's bytes, fitted from units
//! that were measured (warm-up's, on small batches) and extrapolated to the batches a run
//! gives it. Nothing here is declared, and no default factor stands in for it.
//!
//! - **The line** is least squares through the units: bytes held whatever the batch, and bytes
//!   held beside each byte of it. A line that would hold less than nothing at no input is
//!   taken through the origin, and one that falls as the input grows is taken flat, at the
//!   units' mean: neither a negative amount nor a negative rate is a measurement of memory.
//!   Units of one size cannot tell the two terms apart and are read as a rate alone.
//! - **The margin** is the measured spread and nothing else. Each unit departs from the line
//!   by its measured bytes over the line's value there, less one; the spread is the root mean
//!   square of those departures, and the margin is [`Z95`] times it, the 95th percentile of a
//!   normal spread of that size. A function that is exactly a line has no margin.

use serde::{Deserialize, Serialize};

use crate::sample::Z95;

/// The least the marks a unit's memory is read from tell apart: one granule of the arena,
/// 64 KiB. A unit's departure from the line is taken above it, so bytes that no mark can tell
/// from nothing are not counted as spread.
pub const GRAIN_BYTES: f64 = 65_536.0;

/// What one call of a function holds outside its batch, as a line over the batch's bytes, and
/// what it gives per byte it takes; with the units and the range of input it was fitted from,
/// so a reader knows how far it is extrapolated.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    /// Bytes held whatever the batch: the line's value at no input.
    pub fixed_bytes: f64,
    /// Bytes held beside each byte of the input batch: the line's slope.
    pub per_input_byte: f64,
    /// Bytes out per byte in, the median over the units; absent when no unit said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_input_byte: Option<f64>,
    /// The spread of the units about the line: the root mean square of each unit's departure,
    /// its measured bytes over the line's value there, less one.
    pub spread: f64,
    /// Units the line was fitted from.
    pub units: u64,
    /// The smallest input among them, in bytes.
    pub input_bytes_min: u64,
    /// The largest.
    pub input_bytes_max: u64,
}

impl Memory {
    /// The line through `points`, each a unit's input bytes and the bytes it held outside its
    /// batch, with `output_per_input_byte` as the units gave it. A point that is not a
    /// measurement (no input, a negative or non-finite amount) is left out; `None` when none
    /// is left.
    pub fn fit(points: &[(f64, f64)], output_per_input_byte: Option<f64>) -> Option<Memory> {
        let points: Vec<(f64, f64)> = points
            .iter()
            .copied()
            .filter(|(x, y)| x.is_finite() && y.is_finite() && *x > 0.0 && *y >= 0.0)
            .collect();
        let first = points.first()?;
        let (fixed_bytes, per_input_byte) = line(&points);
        let departures: f64 = points
            .iter()
            .map(|(x, y)| {
                let fitted = fixed_bytes + per_input_byte * x;
                let departure = (y + GRAIN_BYTES) / (fitted + GRAIN_BYTES) - 1.0;
                departure * departure
            })
            .sum();
        let (least, most) = points
            .iter()
            .fold((first.0, first.0), |(least, most), (x, _)| {
                (least.min(*x), most.max(*x))
            });
        Some(Memory {
            fixed_bytes,
            per_input_byte,
            output_per_input_byte: output_per_input_byte.filter(|o| o.is_finite() && *o >= 0.0),
            spread: (departures / points.len() as f64).sqrt(),
            units: points.len() as u64,
            input_bytes_min: least as u64,
            input_bytes_max: most as u64,
        })
    }

    /// The margin the room is widened by: [`Z95`] times the spread, the 95th percentile of a
    /// normal spread of that size about the line.
    pub fn margin(&self) -> f64 {
        Z95 * self.spread
    }

    /// The bytes one call needs outside a batch of `input_bytes`: the line there, widened by
    /// the margin.
    pub fn room(&self, input_bytes: f64) -> f64 {
        (self.fixed_bytes + self.per_input_byte * input_bytes) * (1.0 + self.margin())
    }

    /// The bytes one process of the function holds at a piece of `piece` bytes, when this is the
    /// node's own reading of the process's account (system design 3A; PM, 2026-11-18): its whole
    /// memory, its interpreter or program included, widened by the margin. Within the measured
    /// range, and past it for a line with a slope, the line; past it for a line with none (the
    /// node's readings of one size, which say nothing of how the need grows), the line's value
    /// at the top of the range scaled up in proportion to the piece.
    pub fn account(&self, piece: f64) -> f64 {
        let top = self.input_bytes_max as f64;
        let at = if piece > top && self.per_input_byte == 0.0 && top > 0.0 {
            self.fixed_bytes * piece / top
        } else {
            self.fixed_bytes + self.per_input_byte * piece
        };
        at * (1.0 + self.margin())
    }

    /// The largest piece one process of the function takes inside an account of `account`
    /// bytes, by [`Memory::account`]: piece <= (account / (1 + margin) - fixed) / per-byte, zero
    /// when the fixed term alone does not fit, and unbounded for a line that does not grow
    /// (system design 3A; PM, 2026-11-18). A line with no slope grows past the top of its range
    /// in proportion, as `account` says it does.
    pub fn piece_within(&self, account: f64) -> f64 {
        let held = account / (1.0 + self.margin());
        if held < self.fixed_bytes {
            return 0.0;
        }
        if self.per_input_byte > 0.0 {
            return (held - self.fixed_bytes) / self.per_input_byte;
        }
        let top = self.input_bytes_max as f64;
        if self.fixed_bytes > 0.0 && top > 0.0 {
            held * top / self.fixed_bytes
        } else {
            f64::INFINITY
        }
    }

    /// Refuse what is not a measurement of memory: an amount or a rate that is negative or not
    /// finite, no units, or a range that is not one.
    pub fn check(&self) -> Result<(), String> {
        let amounts = [
            ("fixed_bytes", self.fixed_bytes),
            ("per_input_byte", self.per_input_byte),
            ("spread", self.spread),
            (
                "output_per_input_byte",
                self.output_per_input_byte.unwrap_or(0.0),
            ),
        ];
        for (name, value) in amounts {
            if !value.is_finite() || value < 0.0 {
                return Err(format!(
                    "{name} is {value}, not a finite amount of zero or more"
                ));
            }
        }
        if self.units == 0 {
            return Err("it was fitted from no units".into());
        }
        if self.input_bytes_min == 0 || self.input_bytes_min > self.input_bytes_max {
            return Err(format!(
                "its units' input runs from {} to {} bytes, which is not a range",
                self.input_bytes_min, self.input_bytes_max
            ));
        }
        Ok(())
    }
}

/// The least-squares line `y = a + b x` through `points` (not empty, every `x` above zero),
/// kept to what memory can be: through the origin when `a` would be negative or the points
/// have one `x`, flat at their mean when `b` would be negative.
fn line(points: &[(f64, f64)]) -> (f64, f64) {
    let n = points.len() as f64;
    let (mx, my) = (
        points.iter().map(|p| p.0).sum::<f64>() / n,
        points.iter().map(|p| p.1).sum::<f64>() / n,
    );
    let through_origin = || {
        let rate = points.iter().map(|p| p.0 * p.1).sum::<f64>()
            / points.iter().map(|p| p.0 * p.0).sum::<f64>();
        (0.0, rate)
    };
    let sxx: f64 = points.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
    if sxx <= 0.0 {
        return through_origin();
    }
    let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let b = sxy / sxx;
    let a = my - b * mx;
    if b < 0.0 {
        (my, 0.0)
    } else if a < 0.0 {
        through_origin()
    } else {
        (a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: f64 = 1_048_576.0;

    #[test]
    fn a_function_that_is_a_line_is_fitted_exactly_and_has_no_margin() {
        // 3 MiB whatever the batch, and a byte and a quarter beside each byte of it.
        let points: Vec<(f64, f64)> = [1.0, 2.0, 8.0, 16.0]
            .iter()
            .map(|mib| (mib * MIB, 3.0 * MIB + 1.25 * mib * MIB))
            .collect();
        let memory = Memory::fit(&points, Some(1.17)).expect("fitted");
        assert!((memory.fixed_bytes - 3.0 * MIB).abs() < 1.0, "{memory:?}");
        assert!((memory.per_input_byte - 1.25).abs() < 1e-9, "{memory:?}");
        assert_eq!(memory.output_per_input_byte, Some(1.17));
        assert!(memory.spread < 1e-9 && memory.margin() < 1e-9);
        assert_eq!(memory.units, 4);
        assert_eq!(
            (memory.input_bytes_min, memory.input_bytes_max),
            (MIB as u64, 16 * MIB as u64)
        );
        // Extrapolated past what was measured: the line, and nothing added to it.
        assert!((memory.room(64.0 * MIB) - (3.0 + 80.0) * MIB).abs() < 64.0);
        memory.check().expect("a measurement");
    }

    #[test]
    fn the_margin_is_the_units_spread_about_the_line() {
        // Two units at each size, a tenth above and a tenth below ten bytes a byte.
        let mut points = Vec::new();
        for mib in [1.0, 4.0, 16.0] {
            points.push((mib * MIB, 11.0 * mib * MIB));
            points.push((mib * MIB, 9.0 * mib * MIB));
        }
        let memory = Memory::fit(&points, None).expect("fitted");
        assert!((memory.per_input_byte - 10.0).abs() < 1e-6, "{memory:?}");
        assert!(memory.fixed_bytes.abs() < 1.0);
        // Every unit departs by about a tenth, so the spread is about a tenth and the margin
        // 1.645 of it; the room is the line widened by exactly that.
        assert!((memory.spread - 0.1).abs() < 0.005, "{}", memory.spread);
        assert!((memory.margin() - Z95 * memory.spread).abs() < 1e-12);
        let line = 10.0 * 8.0 * MIB;
        assert!((memory.room(8.0 * MIB) - line * (1.0 + memory.margin())).abs() < 1.0);
        assert_eq!(memory.output_per_input_byte, None);
    }

    #[test]
    fn a_line_is_kept_to_what_memory_can_be() {
        // A negative intercept: taken through the origin.
        let below = Memory::fit(&[(MIB, 0.0), (2.0 * MIB, 2.0 * MIB)], None).expect("fitted");
        assert_eq!(below.fixed_bytes, 0.0);
        assert!((below.per_input_byte - 0.8).abs() < 1e-9, "{below:?}");
        // A falling line: flat, at the mean.
        let falling =
            Memory::fit(&[(MIB, 6.0 * MIB), (2.0 * MIB, 4.0 * MIB)], None).expect("fitted");
        assert_eq!(falling.per_input_byte, 0.0);
        assert!((falling.fixed_bytes - 5.0 * MIB).abs() < 1.0);
        // One size: a rate alone, and the range says it is a point.
        let one =
            Memory::fit(&[(4.0 * MIB, 8.0 * MIB), (4.0 * MIB, 8.0 * MIB)], None).expect("fitted");
        assert_eq!((one.fixed_bytes, one.per_input_byte), (0.0, 2.0));
        assert_eq!(one.input_bytes_min, one.input_bytes_max);
        // A function that holds nothing: no rate, no amount, and no spread out of noise below a
        // granule.
        let nothing =
            Memory::fit(&[(MIB, 0.0), (4.0 * MIB, 4096.0), (16.0 * MIB, 0.0)], None).expect("fit");
        assert!(nothing.room(64.0 * MIB) < GRAIN_BYTES, "{nothing:?}");
        assert!(nothing.spread < 0.1, "{nothing:?}");
    }

    #[test]
    fn an_account_follows_the_line_and_past_a_line_of_one_size_scales_in_proportion() {
        // The node's readings of one size: 4.5 MiB at 16 MiB pieces, no slope.
        let one = Memory {
            fixed_bytes: 4.5 * MIB,
            per_input_byte: 0.0,
            output_per_input_byte: None,
            spread: 0.0,
            units: 4,
            input_bytes_min: 16 * MIB as u64,
            input_bytes_max: 16 * MIB as u64,
        };
        // At or below the range, the largest peak; above it, in proportion.
        assert_eq!(one.account(MIB), 4.5 * MIB);
        assert_eq!(one.account(16.0 * MIB), 4.5 * MIB);
        assert_eq!(one.account(32.0 * MIB), 9.0 * MIB);
        // With the client's slope, the line, inside the range and past it.
        let sloped = Memory {
            fixed_bytes: 3.6 * MIB,
            per_input_byte: 0.054,
            ..one.clone()
        };
        assert!((sloped.account(32.0 * MIB) - (3.6 + 0.054 * 32.0) * MIB).abs() < 1.0);
        // Widened by the margin.
        let spread = Memory {
            spread: 0.1,
            ..one.clone()
        };
        assert!((spread.account(MIB) - 4.5 * MIB * (1.0 + Z95 * 0.1)).abs() < 1.0);
    }

    #[test]
    fn the_piece_an_account_holds_is_the_line_s_inverse() {
        let line = Memory {
            fixed_bytes: 30.0 * MIB,
            per_input_byte: 6.0,
            output_per_input_byte: None,
            spread: 0.0,
            units: 6,
            input_bytes_min: MIB as u64,
            input_bytes_max: 16 * MIB as u64,
        };
        // A Python function's 30 MiB and six bytes a byte: an instance of 128 MiB takes pieces
        // of (128 - 30) / 6 MiB, and one of 30 MiB none.
        assert!((line.piece_within(128.0 * MIB) - 98.0 / 6.0 * MIB).abs() < 1.0);
        assert_eq!(line.piece_within(29.0 * MIB), 0.0);
        assert!((line.account(line.piece_within(128.0 * MIB)) - 128.0 * MIB).abs() < 1.0);
        // Widened by the margin, as the account is.
        let spread = Memory {
            spread: 0.1,
            ..line.clone()
        };
        assert!(spread.piece_within(128.0 * MIB) < line.piece_within(128.0 * MIB));
        assert!((spread.account(spread.piece_within(128.0 * MIB)) - 128.0 * MIB).abs() < 1.0);
        // A line of one size grows in proportion past its range; one that holds nothing, never.
        let one = Memory {
            fixed_bytes: 4.0 * MIB,
            per_input_byte: 0.0,
            ..line.clone()
        };
        assert!((one.piece_within(8.0 * MIB) - 32.0 * MIB).abs() < 1.0);
        let nothing = Memory {
            fixed_bytes: 0.0,
            per_input_byte: 0.0,
            ..line
        };
        assert!(nothing.piece_within(MIB).is_infinite());
    }

    #[test]
    fn what_is_not_a_measurement_is_left_out_or_refused() {
        assert_eq!(Memory::fit(&[], None), None);
        assert_eq!(
            Memory::fit(
                &[
                    (0.0, 1.0),
                    (f64::NAN, 1.0),
                    (MIB, -1.0),
                    (MIB, f64::INFINITY)
                ],
                None
            ),
            None
        );
        let kept = Memory::fit(&[(0.0, 9.0), (MIB, MIB)], Some(f64::NAN)).expect("one unit");
        assert_eq!((kept.units, kept.output_per_input_byte), (1, None));

        let good = Memory::fit(&[(MIB, MIB), (2.0 * MIB, 2.0 * MIB)], Some(0.5)).expect("fitted");
        good.check().expect("a measurement");
        let refused = |change: fn(&mut Memory)| {
            let mut memory = good.clone();
            change(&mut memory);
            memory.check().expect_err("refused")
        };
        assert!(refused(|m| m.fixed_bytes = -1.0).contains("fixed_bytes"));
        assert!(refused(|m| m.per_input_byte = f64::NAN).contains("per_input_byte"));
        assert!(refused(|m| m.spread = f64::INFINITY).contains("spread"));
        assert!(
            refused(|m| m.output_per_input_byte = Some(-0.5)).contains("output_per_input_byte")
        );
        assert!(refused(|m| m.units = 0).contains("no units"));
        assert!(refused(|m| m.input_bytes_min = 0).contains("not a range"));
        assert!(refused(|m| m.input_bytes_max = 1).contains("not a range"));
    }

    #[test]
    fn it_is_written_and_read_as_its_fields() {
        let memory = Memory::fit(&[(MIB, MIB), (2.0 * MIB, 2.0 * MIB)], None).expect("fitted");
        let text = serde_json::to_string(&memory).expect("serialises");
        assert!(!text.contains("output_per_input_byte"), "{text}");
        assert_eq!(
            serde_json::from_str::<Memory>(&text).expect("reads"),
            memory
        );
        // A record that leaves a term out is not a measurement.
        assert!(serde_json::from_str::<Memory>(r#"{"fixed_bytes": 1.0}"#).is_err());
    }
}
