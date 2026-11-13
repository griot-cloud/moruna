//! What a source's rows decode to in memory over what its splits' metadata says they are (f.5,
//! RB3).
//!
//! A Parquet split's "uncompressed" bytes are its encoded pages, several times smaller than the
//! decoded batch, so a read sized from them alone overshoots its target by that factor. The drive
//! therefore sizes a read's rows, and charges its admission, by what reads have actually come to.
//!
//! A payload is not proportional to its rows: every buffer takes at least one arena granule, so a
//! read carries a fixed cost on top of its rows. Measured on a small read, that cost reads as a
//! large ratio; a large ratio sizes the next read smaller, whose fixed cost reads as a larger
//! ratio still. Kept as "the most any read came to", the ratio runs away and reads shrink to a
//! few rows, each still paying for its whole split. So the ratio comes from the read with the
//! most rows seen, where the fixed cost weighs least, and a read at least half that size may
//! raise it (a denser part of the data keeps the bound honest). A read smaller than that never
//! moves it, so no read can shrink the reads after it below half the best sample.

use std::sync::Mutex;

#[derive(Clone, Copy)]
struct Sample {
    /// The rows of the read the ratio was taken from.
    rows: u64,
    /// What that read came to over its estimate.
    ratio: f64,
}

/// The learned ratio, at least 1.
pub(crate) struct DecodeRatio {
    sample: Mutex<Sample>,
}

impl Default for DecodeRatio {
    fn default() -> Self {
        DecodeRatio {
            sample: Mutex::new(Sample {
                rows: 0,
                ratio: 1.0,
            }),
        }
    }
}

impl DecodeRatio {
    /// The ratio to size and charge the next read by, at least 1.
    pub(crate) fn ratio(&self) -> f64 {
        self.sample
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ratio
            .max(1.0)
    }

    /// Take what a read of `rows` came to, `ratio` times its estimate, and return the ratio now
    /// in force.
    pub(crate) fn observe(&self, rows: u64, ratio: f64) -> f64 {
        let mut sample = self.sample.lock().unwrap_or_else(|e| e.into_inner());
        if ratio.is_finite() && rows > 0 {
            if rows >= sample.rows {
                *sample = Sample { rows, ratio };
            } else if rows.saturating_mul(2) >= sample.rows && ratio > sample.ratio {
                sample.ratio = ratio;
            }
        }
        sample.ratio.max(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_one_and_takes_the_first_read() {
        let decoded = DecodeRatio::default();
        assert_eq!(decoded.ratio(), 1.0);
        assert_eq!(decoded.observe(100_000, 6.0), 6.0);
        assert_eq!(decoded.ratio(), 6.0);
    }

    #[test]
    fn a_larger_read_replaces_the_sample() {
        let decoded = DecodeRatio::default();
        decoded.observe(10_000, 9.0);
        assert_eq!(
            decoded.observe(200_000, 6.0),
            6.0,
            "less fixed cost per row"
        );
    }

    #[test]
    fn a_read_half_the_sample_may_raise_it_but_not_lower_it() {
        let decoded = DecodeRatio::default();
        decoded.observe(200_000, 6.0);
        assert_eq!(decoded.observe(120_000, 7.5), 7.5, "denser data raises it");
        assert_eq!(
            decoded.observe(120_000, 5.0),
            7.5,
            "a smaller read never lowers it"
        );
    }

    /// The runaway this module exists to stop (RB3): a payload is `fixed + rows × per_row`, and
    /// each read is sized from the ratio the last one left. With the fixed cost above the
    /// target, keeping the most any read came to shrinks reads geometrically to one row.
    #[test]
    fn small_reads_cannot_ratchet_the_reads_after_them() {
        // Nineteen columns of up to three buffers, each at least one 64 KiB granule and rounded
        // up to its class: about 6 MiB fixed, over a 4 MiB target. Rows decode to 150 bytes and
        // are estimated at 26.
        let (fixed, per_row, estimate_per_row) = (96.0 * 65_536.0, 150.0, 26.0);
        let target = 4.0 * 1_048_576.0;
        let ratio_of =
            |rows: u64| (fixed + rows as f64 * per_row) / (rows as f64 * estimate_per_row);
        let next = |ratio: f64| ((target / ratio) / estimate_per_row).max(1.0) as u64;

        // The rule this replaces: the most any read came to. It collapses to one-row reads.
        let (mut most, mut rows) = (1.0f64, 1_000_000u64);
        for _ in 0..200 {
            most = most.max(ratio_of(rows));
            rows = next(most);
        }
        assert_eq!(rows, 1, "the old rule runs away");

        let decoded = DecodeRatio::default();
        let mut rows = 1_000_000u64;
        for _ in 0..200 {
            rows = next(decoded.observe(rows, ratio_of(rows)));
        }
        assert_eq!(
            rows,
            next(ratio_of(1_000_000)),
            "reads stay sized by the large first read"
        );
        assert!(decoded.ratio() < 6.1, "{}", decoded.ratio());
    }
}
