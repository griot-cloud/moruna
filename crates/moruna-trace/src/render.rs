//! `Display` for the run report (e.3): a fixed layout of at most 40 lines, binary units for
//! bytes and three significant figures. Rendering only; every number it prints was computed
//! in `report.rs` (l, anti-patterns).

use core::fmt;

use crate::report::{RunReport, gil_name};

/// The most stage lines the layout prints before it summarises the rest, so that e.3's
/// "at most 40 lines" holds for any chain length.
const MAX_STAGE_LINES: usize = 20;
/// The most note lines, for the same reason.
const MAX_NOTE_LINES: usize = 6;

/// Bytes in binary units, three significant figures.
pub(crate) fn bytes_binary(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{} {}", sig3(v), UNITS[unit])
    }
}

/// A byte rate in binary units per second.
pub(crate) fn rate_binary(v: f64) -> String {
    if !v.is_finite() || v <= 0.0 {
        return "0 B/s".to_string();
    }
    format!("{}/s", bytes_binary(v as u64))
}

/// Three significant figures, with an exponent for values too small to show otherwise.
pub(crate) fn sig3(v: f64) -> String {
    if !v.is_finite() {
        return "n/a".to_string();
    }
    let a = v.abs();
    if a == 0.0 {
        "0.00".to_string()
    } else if a >= 100.0 {
        format!("{v:.0}")
    } else if a >= 10.0 {
        format!("{v:.1}")
    } else if a >= 1.0 {
        format!("{v:.2}")
    } else if a >= 0.001 {
        format!("{v:.3}")
    } else {
        format!("{v:.2e}")
    }
}

fn percent(v: f64) -> String {
    format!("{}%", sig3(v * 100.0))
}

fn exit_text(r: &RunReport) -> String {
    match &r.exit {
        crate::ExitReason::Completed => "completed".to_string(),
        crate::ExitReason::Cancelled => "cancelled".to_string(),
        crate::ExitReason::Terminated { diagnostic } => format!("terminated: {diagnostic}"),
    }
}

impl fmt::Display for RunReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "moruna run {} {}{} in {} s",
            self.run_id,
            exit_text(self),
            if self.resumed { " (resumed)" } else { "" },
            sig3(self.wall_s)
        )?;
        writeln!(
            f,
            "limits: ceiling {}, kill {}, cpu {}, source {}{}",
            bytes_binary(self.limits.memory_ceiling),
            self.limits
                .memory_kill
                .map_or_else(|| "none".to_string(), bytes_binary),
            sig3(self.limits.cpu_quota),
            self.limits.source,
            if self.limits.devices.is_empty() {
                String::new()
            } else {
                format!(", devices {}", self.limits.devices.len())
            }
        )?;
        writeln!(
            f,
            "memory: peak {} of ceiling {} ({} of ceiling)",
            bytes_binary(self.peak_anon_bytes),
            bytes_binary(self.limits.memory_ceiling),
            percent(self.peak_fraction_of_ceiling)
        )?;
        writeln!(
            f,
            "cpu: workers busy {}, throttled {}",
            percent(self.worker_busy_fraction),
            percent(self.cpu_throttled_fraction)
        )?;
        writeln!(
            f,
            "io: source {}, paths direct_io {} io_uring {} gds {} pinned {} rdma {}",
            rate_binary(self.source_bytes_per_s),
            self.io_paths.direct_io,
            self.io_paths.io_uring,
            self.io_paths.gds,
            self.io_paths.pinned,
            self.io_paths.rdma
        )?;
        writeln!(
            f,
            "staging: {}, written {}, staging bandwidth {} beside source bandwidth {}",
            if self.staging_engaged {
                "engaged"
            } else {
                "not engaged"
            },
            bytes_binary(self.staging_bytes_written),
            rate_binary(self.staging_bandwidth),
            rate_binary(self.source_bandwidth)
        )?;
        if self.stages.is_empty() {
            writeln!(f, "stages: no kernel stages")?;
        }
        for s in self.stages.iter().take(MAX_STAGE_LINES) {
            writeln!(
                f,
                "stage {}: {} morsels, {} rows/s, {}, amplification p50 {} p95 {}, miss {} s{}{}",
                s.stage,
                s.morsels,
                sig3(s.rows_per_s),
                rate_binary(s.bytes_per_s),
                sig3(s.amplification_p50),
                sig3(s.amplification_p95),
                sig3(s.placement_miss_wait_s),
                if s.errors > 0 || s.skipped > 0 {
                    format!(", {} errors, {} skipped", s.errors, s.skipped)
                } else {
                    String::new()
                },
                if s.state_bytes_max > 0 {
                    format!(
                        ", state {} (growth {})",
                        bytes_binary(s.state_bytes_max),
                        s.state_growth
                    )
                } else {
                    String::new()
                }
            )?;
        }
        if self.stages.len() > MAX_STAGE_LINES {
            writeln!(
                f,
                "stages: {} more not shown",
                self.stages.len() - MAX_STAGE_LINES
            )?;
        }
        writeln!(
            f,
            "sizer: {}{}",
            self.sizer_used,
            match self.sizer_fallback_at {
                Some(seq) => format!(", fell back to the rule sizer at morsel {seq}"),
                None => String::new(),
            }
        )?;
        if !self.gil.is_empty() {
            let stages: Vec<String> = self
                .gil
                .iter()
                .map(|(stage, g)| format!("{stage}:{}", gil_name(*g)))
                .collect();
            writeln!(
                f,
                "python: {} ({})",
                if self.gil_serialised {
                    "serialised on at least one stage"
                } else {
                    "free threaded"
                },
                stages.join(" ")
            )?;
        }
        if let Some(m) = &self.manifest {
            writeln!(f, "manifest: {m}")?;
        }
        if self.overflow_failed || self.late_records > 0 {
            writeln!(
                f,
                "trace: overflow_failed {}, late records {}",
                self.overflow_failed, self.late_records
            )?;
        }
        if !self.bottleneck_timeline.is_empty() {
            let entries: Vec<String> = self
                .bottleneck_timeline
                .iter()
                .take(8)
                .map(|(at, class)| format!("{} s {}", sig3(*at), class))
                .collect();
            writeln!(f, "bottlenecks: {}", entries.join(", "))?;
        }
        for note in self.notes.iter().take(MAX_NOTE_LINES) {
            writeln!(f, "note: {note}")?;
        }
        if self.notes.len() > MAX_NOTE_LINES {
            writeln!(
                f,
                "note: {} more not shown",
                self.notes.len() - MAX_NOTE_LINES
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// e.3: bytes in binary units. The layout never prints a decimal SI unit, so a reader
    /// comparing a figure against a budget is comparing like with like.
    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(bytes_binary(0), "0 B");
        assert_eq!(bytes_binary(1), "1 B");
        assert_eq!(bytes_binary(1023), "1023 B");
        assert_eq!(bytes_binary(1024), "1.00 KiB");
        assert_eq!(bytes_binary(1536), "1.50 KiB");
        assert_eq!(bytes_binary(1024 * 1024), "1.00 MiB");
        assert_eq!(bytes_binary(214_748_364), "205 MiB");
        assert_eq!(bytes_binary(1024 * 1024 * 1024), "1.00 GiB");
        assert_eq!(bytes_binary(1024u64.pow(4)), "1.00 TiB");
        assert_eq!(bytes_binary(1024u64.pow(5)), "1.00 PiB");
        // Beyond the largest unit the figure grows rather than the unit.
        assert_eq!(bytes_binary(2 * 1024u64.pow(6)), "2048 PiB");
    }

    /// e.3: a rate is the same figure with a unit of time, and no rate is ever negative or
    /// not a number.
    #[test]
    fn rates_use_binary_units() {
        assert_eq!(rate_binary(0.0), "0 B/s");
        assert_eq!(rate_binary(-1.0), "0 B/s");
        assert_eq!(rate_binary(f64::NAN), "0 B/s");
        assert_eq!(rate_binary(f64::INFINITY), "0 B/s");
        assert_eq!(rate_binary(1024.0), "1.00 KiB/s");
        assert_eq!(rate_binary(214_748_364.8), "205 MiB/s");
    }

    /// e.3: three significant figures, at every magnitude, with an exponent where three
    /// figures would otherwise read as zero.
    #[test]
    fn numbers_use_three_significant_figures() {
        assert_eq!(sig3(0.0), "0.00");
        assert_eq!(sig3(1.0), "1.00");
        assert_eq!(sig3(1.2345), "1.23");
        assert_eq!(sig3(12.345), "12.3");
        assert_eq!(sig3(123.45), "123");
        assert_eq!(sig3(1234.5), "1234");
        assert_eq!(sig3(0.1234), "0.123");
        assert_eq!(sig3(0.00012345), "1.23e-4");
        assert_eq!(sig3(-12.345), "-12.3");
        assert_eq!(sig3(f64::NAN), "n/a");
        assert_eq!(sig3(f64::NEG_INFINITY), "n/a");
        assert_eq!(percent(0.5), "50.0%");
        assert_eq!(percent(0.0), "0.00%");
    }
}
