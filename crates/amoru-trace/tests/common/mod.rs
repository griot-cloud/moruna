//! Test-only helpers shared by the TR tests: a synthetic `TraceRecord`, a temporary
//! directory and a resident-set reading for the memory bound (TR-T4).

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use amoru_kernel::{
    Device, DeviceId, GilState, IoPaths, LimitSource, Limits, Outcome, RunId, Seq, StageId,
    TraceRecord,
};
use amoru_trace::{ExitReason, RunMeta, TraceConfig};

/// A record with every field set to something distinguishable, so a decoder that mixes two
/// columns up is caught by the round trip.
pub fn record(seq: Seq, stage: StageId) -> TraceRecord {
    TraceRecord {
        seq,
        stage,
        worker: (seq % 8) as u16,
        instance: u16::MAX,
        t_start_ns: 1_000_000_000 + seq * 1_000,
        t_end_ns: 1_000_000_000 + seq * 1_000 + 500,
        rows_in: 100 + seq,
        bytes_in: 1_000 + seq,
        rows_out: 90 + seq,
        bytes_out: 900 + seq,
        tier_in: 2,
        tier_out: 2,
        feat_mean_string_len: 12.5,
        feat_null_ratio: 0.25,
        feat_column_bytes: vec![7, 8, 9],
        knob_morsel_target: 4 << 20,
        knob_active_workers: 4,
        knob_read_ahead: 2,
        mem_anon_before: 10_000,
        mem_anon_peak: 10_000 + 2 * (1_000 + seq),
        dev_mem_peak: 0,
        cpu_time_us: 400,
        throttled_delta_us: 3,
        q_bytes_before: vec![0, 0, 1_024, 0, 0],
        q_bytes_after: vec![0, 0, 2_048, 0, 0],
        staging_bytes_delta: 0,
        placement_miss_wait_us: 10,
        state_bytes: 0,
        sizer: 0,
        outcome: Outcome::Ok,
        error: None,
    }
}

/// A record with no list fields, for the volume tests: the record path must not allocate,
/// and the test must not spend its time allocating either.
pub fn lean_record(seq: Seq, stage: StageId) -> TraceRecord {
    TraceRecord {
        feat_column_bytes: Vec::new(),
        q_bytes_before: Vec::new(),
        q_bytes_after: Vec::new(),
        ..record(seq, stage)
    }
}

static DIRS: AtomicU64 = AtomicU64::new(0);

/// A temporary directory removed when it drops.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        let n = DIRS.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("amoru-trace-{tag}-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn config(dir: &std::path::Path) -> TraceConfig {
    TraceConfig {
        path: None,
        staging_dir: dir.to_path_buf(),
        channel_capacity: 4096,
        memory_limit: 64 * 1024 * 1024,
        run_id: RunId([0xab; 16]),
    }
}

pub fn limits() -> Limits {
    Limits {
        memory_ceiling: 8 * 1024 * 1024 * 1024,
        memory_kill: Some(10 * 1024 * 1024 * 1024),
        cpu_quota: 8.0,
        page_bytes: 4096,
        devices: vec![Device {
            id: DeviceId(0),
            total_bytes: 16 * 1024 * 1024 * 1024,
            free_bytes: 15 * 1024 * 1024 * 1024,
            name: "test device".to_string(),
        }],
        source: LimitSource::Cgroup,
    }
}

/// Meta with a fixed identity and fixed clock, so a report over a fixed trace is fixed
/// (TR-I3).
pub fn meta(exit: ExitReason) -> RunMeta {
    RunMeta {
        run_id: RunId([0xab; 16]),
        exit,
        start_ns: 1_000_000_000,
        end_ns: 11_000_000_000,
        resumed: false,
        manifest: None,
        notes: vec!["discovery: cgroup v2 ceiling".to_string()],
        gil: vec![(2, GilState::FreeThreaded)],
        io_paths: IoPaths {
            direct_io: true,
            io_uring: false,
            gds: false,
            pinned: true,
            rdma: false,
        },
        sizer: "rule",
        sizer_fallback_at: None,
        bottleneck_timeline: vec![(0.0, "Compute".to_string()), (4.0, "IoRead".to_string())],
        controller_notes: vec!["small dataset: no adaptation".to_string()],
    }
}

/// Resident set of this process in bytes, or `None` when the platform has no cheap reading.
/// Linux reads `/proc/self/statm`; macOS asks `ps`, which needs no extra dependency.
pub fn resident_bytes() -> Option<u64> {
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        return Some(pages * 4096);
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let kib: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kib * 1024)
}

/// The trace f.2 is applied to. Times are exact seconds, byte counts are small, so every
/// formula can be checked by hand rather than by reimplementing it.
pub fn synthetic_trace() -> Vec<TraceRecord> {
    let s = 1_000_000_000u64;
    let base = |seq, stage| TraceRecord {
        knob_active_workers: 2,
        placement_miss_wait_us: 0,
        throttled_delta_us: 0,
        staging_bytes_delta: 0,
        state_bytes: 0,
        instance: u16::MAX,
        ..record(seq, stage)
    };
    vec![
        // Stage 1: three morsels, amplification 1, 2 and 3.
        TraceRecord {
            t_start_ns: 0,
            t_end_ns: s,
            rows_in: 100,
            bytes_in: 1_000,
            rows_out: 80,
            bytes_out: 800,
            mem_anon_before: 100,
            mem_anon_peak: 1_100,
            placement_miss_wait_us: 1_000_000,
            ..base(1, 1)
        },
        TraceRecord {
            t_start_ns: s,
            t_end_ns: 3 * s,
            rows_in: 200,
            bytes_in: 2_000,
            rows_out: 160,
            bytes_out: 1_600,
            mem_anon_before: 100,
            mem_anon_peak: 4_100,
            ..base(2, 1)
        },
        TraceRecord {
            t_start_ns: 3 * s,
            t_end_ns: 4 * s,
            rows_in: 300,
            bytes_in: 3_000,
            rows_out: 240,
            bytes_out: 2_400,
            mem_anon_before: 100,
            mem_anon_peak: 9_100,
            ..base(3, 1)
        },
        // Stage 2: one error and one skip, so neither counts as kernel busy time.
        TraceRecord {
            t_start_ns: 4 * s,
            t_end_ns: 5 * s,
            rows_in: 80,
            bytes_in: 800,
            rows_out: 80,
            bytes_out: 800,
            mem_anon_before: 100,
            mem_anon_peak: 500,
            throttled_delta_us: 2_000_000,
            staging_bytes_delta: 500,
            outcome: Outcome::Error,
            error: Some("boom".to_string()),
            ..base(4, 2)
        },
        TraceRecord {
            t_start_ns: 5 * s,
            t_end_ns: 6 * s,
            rows_in: 160,
            bytes_in: 1_600,
            rows_out: 0,
            bytes_out: 0,
            mem_anon_before: 100,
            mem_anon_peak: 900,
            staging_bytes_delta: -500,
            outcome: Outcome::Skipped,
            ..base(5, 2)
        },
        // Stage 3: two instances, one whose state grows and one whose state barely moves.
        TraceRecord {
            instance: 0,
            t_start_ns: 6 * s,
            t_end_ns: 6 * s + s / 2,
            bytes_in: 100,
            mem_anon_before: 0,
            mem_anon_peak: 100,
            state_bytes: 1_000,
            ..base(6, 3)
        },
        TraceRecord {
            instance: 0,
            t_start_ns: 7 * s,
            t_end_ns: 7 * s + s / 2,
            bytes_in: 100,
            mem_anon_before: 0,
            mem_anon_peak: 100,
            state_bytes: 3_000,
            ..base(7, 3)
        },
        TraceRecord {
            instance: 1,
            t_start_ns: 6 * s + s / 5,
            t_end_ns: 6 * s + 3 * s / 5,
            bytes_in: 100,
            mem_anon_before: 0,
            mem_anon_peak: 200,
            state_bytes: 500,
            ..base(8, 3)
        },
        TraceRecord {
            instance: 1,
            t_start_ns: 7 * s + s / 5,
            t_end_ns: 7 * s + 3 * s / 5,
            bytes_in: 100,
            mem_anon_before: 0,
            mem_anon_peak: 200,
            state_bytes: 600,
            ..base(9, 3)
        },
    ]
}
