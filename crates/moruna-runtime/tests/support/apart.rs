//! One run per process, as in a guest (MH 4.8), and the whole process measured.
//!
//! A run's budget counts what its process already holds (12 f.1), and the figure the ceiling is
//! about is the whole process's, so a test that asserts a run stayed under its ceiling runs the
//! run in a process of its own: the test binary runs itself, one run per child, and the parent
//! checks what the child reports. The child answers with the run's report and, where the
//! platform keeps one, the operating system's own lifetime high-water mark of the process, which
//! no sampling interval can miss.

use std::path::PathBuf;

use serde_json::Value;

/// The budgets every whole-process test runs at: the smallest the range allows, and a gibibyte.
pub const BUDGETS: [u64; 2] = [256 << 20, 1 << 30];

/// One child at a time: the exchange directory is named by the parent's process id.
static APART: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where a child finds its job and leaves its outcome.
fn exchange(parent: u32) -> PathBuf {
    std::env::temp_dir().join(format!("moruna-apart-{parent}"))
}

/// Run `job` in a child process: this test binary, running the ignored test `child`, which calls
/// [`job`] and [`answer`]. The child's answer.
pub fn run(child: &str, job: &Value) -> Value {
    let _one = APART.lock().unwrap_or_else(|e| e.into_inner());
    let dir = exchange(std::process::id());
    std::fs::create_dir_all(&dir).expect("the exchange");
    std::fs::write(dir.join("job.json"), job.to_string()).expect("the job");
    let _ = std::fs::remove_file(dir.join("outcome.json"));
    let status = std::process::Command::new(std::env::current_exe().expect("this binary"))
        .args([
            "--exact",
            child,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .status()
        .expect("the child ran");
    assert!(status.success(), "the child failed: {status}");
    let outcome = std::fs::read_to_string(dir.join("outcome.json")).expect("an outcome");
    let _ = std::fs::remove_dir_all(&dir);
    serde_json::from_str(&outcome).expect("JSON")
}

/// In the child: the job its parent left, or `None` when the ignored test was run some other way.
pub fn job() -> Option<Value> {
    let text =
        std::fs::read_to_string(exchange(std::os::unix::process::parent_id()).join("job.json"))
            .ok()?;
    serde_json::from_str(&text).ok()
}

/// In the child: leave `outcome` for the parent, with the process's own high-water mark beside
/// it as `os_peak`.
pub fn answer(mut outcome: Value) {
    outcome["os_peak"] = serde_json::json!(os_peak());
    std::fs::write(
        exchange(std::os::unix::process::parent_id()).join("outcome.json"),
        outcome.to_string(),
    )
    .expect("the outcome");
}

/// The most anonymous memory this process has held, by the kernel's own ledger where the
/// platform keeps one: on macOS the lifetime maximum of `phys_footprint`, the quantity the run's
/// budget counts there (03 DS-I4). Linux keeps no such mark for a process, and there the
/// report's peak, sampled from `/proc/self/status`, is the evidence: every budget here is
/// explicit, so it is this child's own memory even inside a cgroup, never its parent's or
/// `cargo`'s beside it.
pub fn os_peak() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `proc_pid_rusage` fills one `rusage_info_v4` it is given for this process
        // and keeps no pointer; an all-zero value of that plain C struct is valid.
        unsafe {
            let mut info: libc::rusage_info_v4 = std::mem::zeroed();
            let got = libc::proc_pid_rusage(
                std::process::id() as i32,
                libc::RUSAGE_INFO_V4,
                (&mut info as *mut libc::rusage_info_v4).cast(),
            );
            (got == 0).then_some(info.ri_lifetime_max_phys_footprint)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// The report of a run that completed with the whole process under `budget`: the report's own
/// peak, which is the operating system's figure for the process whatever the run's shape, and the
/// child's lifetime high-water mark where the platform keeps one. The peaks are printed, as the
/// evidence they are.
pub fn within<'a>(outcome: &'a Value, what: &str, budget: u64) -> &'a Value {
    let report = outcome
        .get("report")
        .unwrap_or_else(|| panic!("{what}: {}", outcome["error"]));
    assert_eq!(
        report["exit"],
        serde_json::json!("Completed"),
        "{what}: {report}"
    );
    peaks_within(outcome, what, budget);
    report
}

/// The peaks of `outcome` (a completed run's or a stopped one's) are measured and under
/// `budget`.
pub fn peaks_within(outcome: &Value, what: &str, budget: u64) {
    let report = &outcome["report"];
    let peak = report["peak_anon_bytes"].as_u64().expect("a peak");
    let fraction = report["peak_fraction_of_ceiling"]
        .as_f64()
        .expect("a fraction");
    let os = outcome["os_peak"].as_u64();
    eprintln!(
        "{what} at {} MiB: report peak {peak} ({fraction:.3} of the ceiling), operating system \
         peak {os:?}",
        budget >> 20
    );
    assert!(peak > 0, "{what}: the peak was measured: {report}");
    assert!(
        fraction <= 1.0,
        "{what}: peak {peak}, {fraction} of the ceiling"
    );
    if let Some(os) = os {
        assert!(
            os <= budget,
            "{what}: the process reached {os} bytes against a {budget} byte ceiling"
        );
    }
}
