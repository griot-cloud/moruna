//! The `RunReport` object (PY-I5).
//!
//! `run` returns a report, it does not only print one. The attributes are exactly the fields of
//! `moruna_trace::RunReport` (04 d.1), `__str__` gives the fixed layout of at most forty lines,
//! `to_json()` is the trace crate's own serialisation, and `trace_path` is the file the trace was
//! written to when one was asked for.

use std::path::PathBuf;

use moruna_trace::{ExitReason, RunReport};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule};

/// The run report as Python sees it.
#[pyclass(frozen, module = "moruna._core", name = "RunReport")]
pub struct PyRunReport {
    pub(crate) inner: RunReport,
    pub(crate) trace_path: Option<PathBuf>,
}

impl PyRunReport {
    /// Wrap a report the facade produced.
    pub fn new(inner: RunReport, trace_path: Option<PathBuf>) -> PyRunReport {
        PyRunReport { inner, trace_path }
    }
}

#[pymethods]
impl PyRunReport {
    /// `moruna_trace::RunReport::run_id` (04 d.1).
    #[getter]
    fn run_id(&self) -> String {
        self.inner.run_id.clone()
    }

    /// `moruna_trace::RunReport::resumed` (04 d.1).
    #[getter]
    fn resumed(&self) -> bool {
        self.inner.resumed
    }

    /// `moruna_trace::RunReport::manifest` (04 d.1).
    #[getter]
    fn manifest(&self) -> Option<String> {
        self.inner.manifest.clone()
    }

    /// `moruna_trace::RunReport::wall_s` (04 d.1).
    #[getter]
    fn wall_s(&self) -> f64 {
        self.inner.wall_s
    }

    /// `moruna_trace::RunReport::peak_anon_bytes` (04 d.1).
    #[getter]
    fn peak_anon_bytes(&self) -> u64 {
        self.inner.peak_anon_bytes
    }

    /// `moruna_trace::RunReport::peak_fraction_of_ceiling` (04 d.1).
    #[getter]
    fn peak_fraction_of_ceiling(&self) -> f64 {
        self.inner.peak_fraction_of_ceiling
    }

    /// `moruna_trace::RunReport::worker_busy_fraction` (04 d.1).
    #[getter]
    fn worker_busy_fraction(&self) -> f64 {
        self.inner.worker_busy_fraction
    }

    /// `moruna_trace::RunReport::cpu_throttled_fraction` (04 d.1).
    #[getter]
    fn cpu_throttled_fraction(&self) -> f64 {
        self.inner.cpu_throttled_fraction
    }

    /// `moruna_trace::RunReport::source_bytes_per_s` (04 d.1).
    #[getter]
    fn source_bytes_per_s(&self) -> f64 {
        self.inner.source_bytes_per_s
    }

    /// `moruna_trace::RunReport::source_bandwidth` (04 d.1).
    #[getter]
    fn source_bandwidth(&self) -> f64 {
        self.inner.source_bandwidth
    }

    /// `moruna_trace::RunReport::staging_bandwidth` (04 d.1).
    #[getter]
    fn staging_bandwidth(&self) -> f64 {
        self.inner.staging_bandwidth
    }

    /// `moruna_trace::RunReport::staging_bytes_written` (04 d.1).
    #[getter]
    fn staging_bytes_written(&self) -> u64 {
        self.inner.staging_bytes_written
    }

    /// `moruna_trace::RunReport::staging_engaged` (04 d.1).
    #[getter]
    fn staging_engaged(&self) -> bool {
        self.inner.staging_engaged
    }

    /// `moruna_trace::RunReport::gil_serialised` (04 d.1).
    #[getter]
    fn gil_serialised(&self) -> bool {
        self.inner.gil_serialised
    }

    /// `moruna_trace::RunReport::sizer_used` (04 d.1).
    #[getter]
    fn sizer_used(&self) -> String {
        self.inner.sizer_used.clone()
    }

    /// `moruna_trace::RunReport::sizer_fallback_at` (04 d.1).
    #[getter]
    fn sizer_fallback_at(&self) -> Option<u64> {
        self.inner.sizer_fallback_at
    }

    /// `moruna_trace::RunReport::bottleneck_timeline` (04 d.1).
    #[getter]
    fn bottleneck_timeline(&self) -> Vec<(f64, String)> {
        self.inner.bottleneck_timeline.clone()
    }

    /// `moruna_trace::RunReport::notes` (04 d.1).
    #[getter]
    fn notes(&self) -> Vec<String> {
        self.inner.notes.clone()
    }

    /// `moruna_trace::RunReport::overflow_failed` (04 d.1).
    #[getter]
    fn overflow_failed(&self) -> bool {
        self.inner.overflow_failed
    }

    /// `moruna_trace::RunReport::late_records` (04 d.1).
    #[getter]
    fn late_records(&self) -> u64 {
        self.inner.late_records
    }

    /// How the run ended: `"Completed"`, `"Cancelled"`, or `{"Terminated": {"diagnostic": ...}}`,
    /// which is the shape `to_json` gives the same field.
    #[getter]
    fn exit<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        Ok(match &self.inner.exit {
            ExitReason::Completed => "Completed".into_pyobject(py)?.into_any(),
            ExitReason::Cancelled => "Cancelled".into_pyobject(py)?.into_any(),
            ExitReason::Terminated { diagnostic } => {
                let inner = PyDict::new(py);
                inner.set_item("diagnostic", diagnostic)?;
                let outer = PyDict::new(py);
                outer.set_item("Terminated", inner)?;
                outer.into_any()
            }
        })
    }

    /// The discovered limits, field by field.
    #[getter]
    fn limits<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let l = &self.inner.limits;
        let d = PyDict::new(py);
        d.set_item("memory_ceiling", l.memory_ceiling)?;
        d.set_item("memory_kill", l.memory_kill)?;
        d.set_item("cpu_quota", l.cpu_quota)?;
        d.set_item("source", &l.source)?;
        let devices = PyList::empty(py);
        for dev in &l.devices {
            let e = PyDict::new(py);
            e.set_item("id", dev.id)?;
            e.set_item("name", &dev.name)?;
            e.set_item("total_bytes", dev.total_bytes)?;
            e.set_item("free_bytes", dev.free_bytes)?;
            devices.append(e)?;
        }
        d.set_item("devices", devices)?;
        Ok(d)
    }

    /// Which direct paths the run took (G-I7).
    #[getter]
    fn io_paths<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let p = &self.inner.io_paths;
        let d = PyDict::new(py);
        d.set_item("direct_io", p.direct_io)?;
        d.set_item("io_uring", p.io_uring)?;
        d.set_item("gds", p.gds)?;
        d.set_item("pinned", p.pinned)?;
        d.set_item("rdma", p.rdma)?;
        Ok(d)
    }

    /// One `(stage, state)` pair per Python stage, the state being `"FreeThreaded"` or
    /// `"Serialised"` (05 d.1).
    #[getter]
    fn gil(&self) -> Vec<(u16, String)> {
        self.inner
            .gil
            .iter()
            .map(|(stage, state)| (*stage, format!("{state:?}")))
            .collect()
    }

    /// One entry per stage the trace saw.
    #[getter]
    fn stages<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        for s in &self.inner.stages {
            let d = PyDict::new(py);
            d.set_item("stage", s.stage)?;
            d.set_item("morsels", s.morsels)?;
            d.set_item("rows_in", s.rows_in)?;
            d.set_item("rows_out", s.rows_out)?;
            d.set_item("bytes_in", s.bytes_in)?;
            d.set_item("bytes_out", s.bytes_out)?;
            d.set_item("wall_s", s.wall_s)?;
            d.set_item("kernel_busy_s", s.kernel_busy_s)?;
            d.set_item("rows_per_s", s.rows_per_s)?;
            d.set_item("bytes_per_s", s.bytes_per_s)?;
            d.set_item("amplification_p50", s.amplification_p50)?;
            d.set_item("amplification_p95", s.amplification_p95)?;
            d.set_item("placement_miss_wait_s", s.placement_miss_wait_s)?;
            d.set_item("errors", s.errors)?;
            d.set_item("skipped", s.skipped)?;
            d.set_item("state_bytes_max", s.state_bytes_max)?;
            d.set_item("state_growth", s.state_growth)?;
            list.append(d)?;
        }
        Ok(list)
    }

    /// The trace file, when `trace=` asked for one.
    #[getter]
    fn trace_path(&self) -> Option<String> {
        self.trace_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// The report as JSON, exactly `moruna_trace::RunReport::to_json`.
    fn to_json(&self) -> String {
        self.inner.to_json()
    }

    fn __str__(&self) -> String {
        self.render()
    }

    fn __repr__(&self) -> String {
        format!(
            "RunReport(run_id={}, exit={}, wall_s={:.3})",
            self.inner.run_id,
            exit_name(&self.inner.exit),
            self.inner.wall_s
        )
    }
}

impl PyRunReport {
    /// The fixed layout (PY-I5). At most forty lines: the head is eleven, each stage is one, and
    /// the notes are folded into a count with the first three shown, so a run with many stages or
    /// many notes still prints a page (PY-T5 asserts the bound).
    fn render(&self) -> String {
        let r = &self.inner;
        let mut out = Vec::new();
        out.push(format!(
            "moruna run {} {}{}",
            r.run_id,
            exit_name(&r.exit),
            if r.resumed { " (resumed)" } else { "" }
        ));
        out.push(format!(
            "wall {:.2}s   peak {} ({:.0}% of ceiling {})   workers busy {:.0}%",
            r.wall_s,
            bytes(r.peak_anon_bytes),
            r.peak_fraction_of_ceiling * 100.0,
            bytes(r.limits.memory_ceiling),
            r.worker_busy_fraction * 100.0
        ));
        out.push(format!(
            "cpu quota {:.2} ({})   throttled {:.1}%",
            r.limits.cpu_quota,
            r.limits.source,
            r.cpu_throttled_fraction * 100.0
        ));
        out.push(format!(
            "source {}/s   staging {}/s, {} written{}",
            bytes(r.source_bandwidth as u64),
            bytes(r.staging_bandwidth as u64),
            bytes(r.staging_bytes_written),
            if r.staging_engaged { "" } else { " (unused)" }
        ));
        out.push(format!(
            "io: direct_io={} io_uring={} gds={} pinned={}",
            r.io_paths.direct_io, r.io_paths.io_uring, r.io_paths.gds, r.io_paths.pinned
        ));
        out.push(format!(
            "sizer {}{}   gil {}",
            r.sizer_used,
            match r.sizer_fallback_at {
                Some(seq) => format!(" (fell back at morsel {seq})"),
                None => String::new(),
            },
            if r.gil.is_empty() {
                "no python stage".to_string()
            } else if r.gil_serialised {
                "serialised".to_string()
            } else {
                "free threaded".to_string()
            }
        ));
        if let Some(m) = &r.manifest {
            out.push(format!("manifest {m}"));
        }
        if r.overflow_failed || r.late_records > 0 {
            out.push(format!(
                "trace incomplete: overflow_failed={} late_records={}",
                r.overflow_failed, r.late_records
            ));
        }
        out.push(String::new());
        out.push(format!(
            "{:>5}  {:>9}  {:>12}  {:>12}  {:>8}  {:>6}  {:>7}",
            "stage", "morsels", "rows in", "rows out", "rows/s", "errors", "skipped"
        ));
        // Twelve head lines are already used; forty is the bound, and the tail below takes four.
        const MAX_STAGE_LINES: usize = 22;
        for s in r.stages.iter().take(MAX_STAGE_LINES) {
            out.push(format!(
                "{:>5}  {:>9}  {:>12}  {:>12}  {:>8.3e}  {:>6}  {:>7}",
                s.stage, s.morsels, s.rows_in, s.rows_out, s.rows_per_s, s.errors, s.skipped
            ));
        }
        if r.stages.len() > MAX_STAGE_LINES {
            out.push(format!(
                "... {} more stages (report.stages has them all)",
                r.stages.len() - MAX_STAGE_LINES
            ));
        }
        if !r.notes.is_empty() {
            out.push(String::new());
            out.push(format!("notes ({}):", r.notes.len()));
            for note in r.notes.iter().take(2) {
                out.push(format!("  {note}"));
            }
            if r.notes.len() > 2 {
                out.push(format!(
                    "  ... {} more (report.notes has them all)",
                    r.notes.len() - 2
                ));
            }
        }
        out.join("\n")
    }
}

fn exit_name(exit: &ExitReason) -> String {
    match exit {
        ExitReason::Completed => "completed".to_string(),
        ExitReason::Cancelled => "cancelled".to_string(),
        ExitReason::Terminated { diagnostic } => format!("terminated: {diagnostic}"),
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Add the report class to the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyRunReport>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::{GilState, IoPaths};
    use moruna_trace::{LimitsSummary, StageReport};

    fn stage(stage: u16) -> StageReport {
        StageReport {
            stage,
            morsels: 12,
            rows_in: 1_000,
            rows_out: 1_000,
            bytes_in: 1 << 20,
            bytes_out: 1 << 20,
            wall_s: 1.0,
            kernel_busy_s: 0.9,
            rows_per_s: 1_000.0,
            bytes_per_s: 1.0,
            amplification_p50: 1.0,
            amplification_p95: 1.1,
            placement_miss_wait_s: 0.0,
            errors: 0,
            skipped: 0,
            state_bytes_max: 0,
            state_growth: 0,
        }
    }

    fn report(stages: usize, notes: usize) -> RunReport {
        RunReport {
            run_id: "0".repeat(32),
            exit: ExitReason::Completed,
            resumed: false,
            manifest: Some("/tmp/run/manifest.json".into()),
            wall_s: 12.5,
            limits: LimitsSummary {
                memory_ceiling: 8 << 30,
                memory_kill: None,
                cpu_quota: 8.0,
                source: "cgroup".into(),
                devices: Vec::new(),
            },
            io_paths: IoPaths {
                direct_io: true,
                io_uring: false,
                gds: false,
                pinned: false,
                rdma: false,
            },
            peak_anon_bytes: 4 << 30,
            peak_fraction_of_ceiling: 0.5,
            worker_busy_fraction: 0.9,
            cpu_throttled_fraction: 0.0,
            source_bytes_per_s: 1.0,
            source_bandwidth: 1.0,
            staging_bandwidth: 0.0,
            staging_bytes_written: 0,
            staging_engaged: false,
            gil: vec![(1, GilState::FreeThreaded)],
            gil_serialised: false,
            sizer_used: "rule".into(),
            sizer_fallback_at: None,
            bottleneck_timeline: Vec::new(),
            stages: (0..stages).map(|i| stage(i as u16 + 1)).collect(),
            notes: (0..notes).map(|i| format!("note {i}")).collect(),
            overflow_failed: false,
            late_records: 0,
        }
    }

    /// PY-T5 report_object: `__str__` is at most forty lines however many stages and notes the
    /// run produced, and `to_json` round-trips.
    #[test]
    fn py_t5_report_object() {
        for (stages, notes) in [(0, 0), (3, 2), (60, 40)] {
            let r = PyRunReport::new(report(stages, notes), None);
            let text = r.render();
            let lines = text.lines().count();
            assert!(lines <= 40, "{stages} stages, {notes} notes: {lines} lines");
            assert!(text.contains("moruna run"));
        }
        let r = PyRunReport::new(report(2, 1), Some(PathBuf::from("/tmp/t.arrow")));
        let json: serde_json::Value =
            serde_json::from_str(&r.to_json()).expect("to_json is valid JSON");
        assert_eq!(json["run_id"], "0".repeat(32));
        assert_eq!(json["stages"].as_array().map(|a| a.len()), Some(2));
        assert_eq!(json["io_paths"]["direct_io"], true);
    }

    #[test]
    fn byte_rendering_and_exit_names() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2048), "2.0 KiB");
        assert_eq!(bytes(3 << 30), "3.0 GiB");
        assert_eq!(exit_name(&ExitReason::Cancelled), "cancelled");
        assert_eq!(
            exit_name(&ExitReason::Terminated {
                diagnostic: "kernel stage 1".into()
            }),
            "terminated: kernel stage 1"
        );
        let mut r = report(1, 0);
        r.exit = ExitReason::Cancelled;
        r.resumed = true;
        r.overflow_failed = true;
        r.late_records = 3;
        r.gil_serialised = true;
        r.sizer_fallback_at = Some(9);
        let text = PyRunReport::new(r, None).render();
        assert!(text.contains("(resumed)"));
        assert!(text.contains("trace incomplete"));
        assert!(text.contains("fell back at morsel 9"));
        assert!(text.contains("serialised"));
    }
}
